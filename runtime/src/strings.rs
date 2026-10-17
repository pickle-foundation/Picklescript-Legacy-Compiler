//! Builtin `string` support: allocation, concat, and number-to-text.

use alloc::vec::Vec;

use crate::gc::Gc;
use crate::layout::{builtin_string_total, str_bytes, str_len, str_set_len};
use crate::object::{PickleObject, PICKLE_CLASS_STRING};

/// Allocate a `string` object copying `bytes[0..len]`.
pub fn string_from_bytes(bytes: *const u8, len: usize, gc: &mut Gc) -> *mut PickleObject {
    let total = builtin_string_total(len);
    let obj = gc.alloc(total as u32, PICKLE_CLASS_STRING);
    unsafe {
        str_set_len(obj, len);
        if len > 0 {
            core::ptr::copy_nonoverlapping(bytes, str_bytes(obj) as *mut u8, len);
        }
    }
    obj
}

/// Allocate a `string` from a NUL-terminated C string.
pub fn string_from_cstr(cs: *const u8) -> *mut PickleObject {
    let mut len = 0usize;
    unsafe {
        while *cs.add(len) != 0 {
            len += 1;
        }
    }
    let gc = crate::gc::gc_mut();
    string_from_bytes(cs, len, gc)
}

/// Raw bytes of a string object.
pub fn string_bytes_ptr(obj: *const PickleObject) -> *const u8 {
    str_bytes(obj)
}

/// Byte length of a string object.
pub fn string_bytes_len(obj: *const PickleObject) -> usize {
    str_len(obj)
}

/// Concatenate two strings into a fresh third. A null operand (an
/// uninitialized string) is treated as the empty string, mirroring the
/// compiled code's `""` default for absent string values.
pub fn string_concat(a: *const PickleObject, b: *const PickleObject) -> *mut PickleObject {
    let la = if a.is_null() { 0 } else { string_bytes_len(a) };
    let lb = if b.is_null() { 0 } else { string_bytes_len(b) };
    let total = la + lb;
    let gc = crate::gc::gc_mut();
    let obj = gc.alloc(builtin_string_total(total) as u32, PICKLE_CLASS_STRING);
    unsafe {
        str_set_len(obj, total);
        if la > 0 {
            core::ptr::copy_nonoverlapping(str_bytes(a), str_bytes(obj) as *mut u8, la);
        }
        if lb > 0 {
            core::ptr::copy_nonoverlapping(str_bytes(b), (str_bytes(obj) as *mut u8).add(la), lb);
        }
    }
    obj
}

/// Format an `i64` as a string.
pub fn int64_to_string(v: i64) -> *mut PickleObject {
    let mut buf = [0u8; 24];
    let (neg, digits) = if v == i64::MIN {
        (true, i64::MIN.unsigned_abs())
    } else {
        (v < 0, v.unsigned_abs())
    };
    let mut n = digits;
    let mut idx = 0usize;
    loop {
        buf[idx] = b'0' + (n % 10) as u8;
        idx += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let total_len = idx + if neg { 1 } else { 0 };
    let gc = crate::gc::gc_mut();
    let obj = gc.alloc(builtin_string_total(total_len) as u32, PICKLE_CLASS_STRING);
    unsafe {
        str_set_len(obj, total_len);
        let base = str_bytes(obj) as *mut u8;
        let mut o = total_len;
        if neg {
            *base = b'-';
        }
        for &b in buf.iter().take(idx) {
            o -= 1;
            *base.add(o) = b;
        }
    }
    obj
}

/// Format an `f64` as a string (Rust's shortest round-trip formatting).
pub fn float64_to_string(v: f64) -> *mut PickleObject {
    let text = format!("{}", v);
    let gc = crate::gc::gc_mut();
    string_from_bytes(text.as_ptr(), text.len(), gc)
}

/// Compare two strings (contents); returns -1/0/1.
pub fn string_cmp(a: *const PickleObject, b: *const PickleObject) -> i32 {
    let la = string_bytes_len(a);
    let lb = string_bytes_len(b);
    unsafe {
        let common = la.min(lb);
        for i in 0..common {
            let x = *str_bytes(a).add(i);
            let y = *str_bytes(b).add(i);
            if x != y {
                return if x < y { -1 } else { 1 };
            }
        }
        if la == lb {
            0
        } else {
            if la < lb {
                -1
            } else {
                1
            }
        }
    }
}

// ---- compiler ABI ------------------------------------------------
//
// Symbols the emitted `pkl_*` code calls for `string` operations. All take and
// return managed `string` pointers directly (the codegen mangles the caller
// side); `pickle_str_cmp`/`pickle_str_len` widen to `i64` for the compiler's
// `int` ABI.

/// Allocate a `string` object from raw bytes.
#[no_mangle]
pub extern "C" fn pickle_str_from_bytes(ptr: *const u8, len: usize) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    string_from_bytes(ptr, len, gc)
}

/// Concatenate two strings.
#[no_mangle]
pub extern "C" fn pickle_str_concat(
    a: *const PickleObject,
    b: *const PickleObject,
) -> *mut PickleObject {
    string_concat(a, b)
}

/// Byte length of a string.
#[no_mangle]
pub extern "C" fn pickle_str_len(obj: *const PickleObject) -> i64 {
    string_bytes_len(obj) as i64
}

/// Lexicographic comparison; -1/0/1 as `i64`.
#[no_mangle]
pub extern "C" fn pickle_str_cmp(a: *const PickleObject, b: *const PickleObject) -> i64 {
    string_cmp(a, b) as i64
}

/// Whether `needle` is a substring of `hay` (the `in` operator's string arm).
#[no_mangle]
pub extern "C" fn pickle_str_contains(hay: *const PickleObject, needle: *const PickleObject) -> bool {
    string_contains(hay, needle)
}

/// Whether the bytes of `needle` occur as a contiguous run in `hay`. An empty
/// needle is a substring of every string (the `in` operator's string arm).
pub fn string_contains(hay: *const PickleObject, needle: *const PickleObject) -> bool {
    let hay_len = string_bytes_len(hay);
    let needle_len = string_bytes_len(needle);
    if needle_len == 0 {
        return true;
    }
    if needle_len > hay_len {
        return false;
    }
    unsafe {
        let h = string_bytes_ptr(hay);
        let n = string_bytes_ptr(needle);
        let mut i = 0usize;
        while i + needle_len <= hay_len {
            let mut k = 0usize;
            while k < needle_len && *h.add(i + k) == *n.add(k) {
                k += 1;
            }
            if k == needle_len {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// Copy of `obj` with byte `index` replaced by `v` (a `byte` on the compiler's
/// `i64` ABI, 0..=255); panics on out-of-range. Strings are immutable, so the
/// bytes are copied into a fresh object (copy-on-write).
#[no_mangle]
pub extern "C" fn pickle_str_set(
    obj: *const PickleObject,
    index: i64,
    v: i64,
) -> *mut PickleObject {
    let len = string_bytes_len(obj);
    if index < 0 || index as u64 >= len as u64 {
        panic!("pickle: string index {index} out of range (len {len})");
    }
    let gc = crate::gc::gc_mut();
    let out = gc.alloc(builtin_string_total(len) as u32, PICKLE_CLASS_STRING);
    unsafe {
        str_set_len(out, len);
        if len > 0 {
            core::ptr::copy_nonoverlapping(str_bytes(obj), str_bytes(out) as *mut u8, len);
        }
        *(str_bytes(out) as *mut u8).add(index as usize) = v as u8;
    }
    out
}

/// Byte at `index` of a string, widened to the compiler's `byte` ABI (`i64`,
/// always 0..=255); panics on out-of-range.
#[no_mangle]
pub extern "C" fn pickle_str_get(obj: *const PickleObject, index: i64) -> i64 {
    let len = string_bytes_len(obj);
    if index < 0 || index as u64 >= len as u64 {
        panic!("pickle: string index {index} out of range (len {len})");
    }
    unsafe { *str_bytes(obj).add(index as usize) as i64 }
}

/// Format an `i64` as a string (string-interpolation helper).
#[no_mangle]
pub extern "C" fn pickle_str_from_i64(v: i64) -> *mut PickleObject {
    int64_to_string(v)
}

/// Format an `f64` as a string (string-interpolation helper).
#[no_mangle]
pub extern "C" fn pickle_str_from_f64(v: f64) -> *mut PickleObject {
    float64_to_string(v)
}

/// Format a `bool` as a string (string-interpolation helper).
#[no_mangle]
pub extern "C" fn pickle_str_from_bool(v: bool) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    if v {
        string_from_bytes(b"true".as_ptr(), 4, gc)
    } else {
        string_from_bytes(b"false".as_ptr(), 5, gc)
    }
}

/// Format a `char` (as a UCS-4 code point) as a string (string-interpolation
/// helper). Non-ASCII code points are lowered as UTF-8.
#[no_mangle]
pub extern "C" fn pickle_str_from_char(v: u32) -> *mut PickleObject {
    let mut buf = [0u8; 4];
    let s = char::from_u32(v).unwrap_or('\u{FFFD}');
    let bytes = s.encode_utf8(&mut buf);
    let gc = crate::gc::gc_mut();
    string_from_bytes(bytes.as_ptr(), bytes.len(), gc)
}

/// Format a `byte` as a one-byte string (string-interpolation helper). The
/// value is written raw: the checker keeps a `byte` in 0..=255, and a re-encode
/// would corrupt multi-byte UTF-8 text ("é" must copy back as "é", not "é").
#[no_mangle]
pub extern "C" fn pickle_str_from_byte(v: i64) -> *mut PickleObject {
    let b = v as u8;
    let gc = crate::gc::gc_mut();
    string_from_bytes(&b, 1, gc)
}

/// Snapshot a string's raw bytes as a fresh `List<byte>` (boxed per byte).
/// Supports the `std.text` model: `bytes(s)<->str(bs)` round-trips exactly.
#[no_mangle]
pub extern "C" fn pickle_str_to_bytes(obj: *const PickleObject) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let len = string_bytes_len(obj);
    let out = crate::list::list_new(len, gc);
    // The in-flight list is only a Rust local while the per-byte box_i64
    // allocations below can trigger a nested collection; the lease keeps it
    // alive until it is returned and the caller shadows it.
    let _lease = gc.temp_lease(out);
    unsafe {
        let data = str_bytes(obj);
        for i in 0..len {
            crate::list::list_push(out, crate::boxscalar::pickle_box_i64(*data.add(i) as i64));
        }
    }
    out
}

/// Build a `string` from a `List<byte>`'s raw bytes (inverse of `bytes`).
/// Panics on any element outside 0..=255 so a corrupted list can't sneak
/// invalid bytes into the string model.
#[no_mangle]
pub extern "C" fn pickle_str_from_list(list: *const PickleObject) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    unsafe {
        let len = crate::layout::list_len(list);
        if len == 0 {
            return string_from_bytes(core::ptr::null(), 0, gc);
        }
        let data = crate::layout::list_data(list);
        let mut buf: Vec<u8> = Vec::with_capacity(len);
        for i in 0..len {
            let v = crate::boxscalar::pickle_unbox_i64(*data.add(i));
            if !(0..=255).contains(&v) {
                crate::panic::pickle_panic_cstr(b"str(bytes): byte out of 0..=255\0".as_ptr());
            }
            buf.push(v as u8);
        }
        string_from_bytes(buf.as_ptr(), buf.len(), gc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn roundtrip_bytes() {
        let _guard = setup();
        let s = string_from_bytes(b"hello".as_ptr(), 5, crate::gc::gc_mut());
        assert_eq!(string_bytes_len(s), 5);
        let bytes =
            unsafe { core::slice::from_raw_parts(string_bytes_ptr(s), string_bytes_len(s)) };
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn concat_contents() {
        let _guard = setup();
        let a = string_from_bytes(b"foo".as_ptr(), 3, crate::gc::gc_mut());
        let b = string_from_bytes(b"bar".as_ptr(), 3, crate::gc::gc_mut());
        let c = string_concat(a, b);
        let bytes =
            unsafe { core::slice::from_raw_parts(string_bytes_ptr(c), string_bytes_len(c)) };
        assert_eq!(bytes, b"foobar");
    }

    #[test]
    fn byte_at_roundtrip() {
        let _guard = setup();
        let s = string_from_bytes(b"hello".as_ptr(), 5, crate::gc::gc_mut());
        for (i, expect) in b"hello".iter().enumerate() {
            assert_eq!(pickle_str_get(s, i as i64), *expect as i64);
        }
        assert_eq!(pickle_str_get(s, 4), b'o' as i64);
    }

    #[test]
    fn int_format() {
        let _guard = setup();
        for (v, expect) in [
            (0i64, &b"0"[..]),
            (42, &b"42"[..]),
            (-7, &b"-7"[..]),
            (i64::MAX, &b"9223372036854775807"[..]),
            (i64::MIN, &b"-9223372036854775808"[..]),
        ] {
            let s = int64_to_string(v);
            let bytes =
                unsafe { core::slice::from_raw_parts(string_bytes_ptr(s), string_bytes_len(s)) };
            assert_eq!(bytes, expect, "int64 {v}");
        }
    }

    #[test]
    fn float_format() {
        let _guard = setup();
        let s = float64_to_string(1.5);
        let bytes =
            unsafe { core::slice::from_raw_parts(string_bytes_ptr(s), string_bytes_len(s)) };
        assert_eq!(bytes, b"1.5");
    }

    #[test]
    fn contains_finds_substrings() {
        let _guard = setup();
        let mk = |b: &[u8]| string_from_bytes(b.as_ptr(), b.len(), crate::gc::gc_mut());
        let hay = mk(b"hello");
        assert!(string_contains(hay, mk(b"ell")));
        assert!(string_contains(hay, mk(b"hello")));
        assert!(string_contains(hay, mk(b"")));
        assert!(!string_contains(hay, mk(b"xyz")));
        assert!(!string_contains(hay, mk(b"hellos")));
        assert!(!string_contains(hay, mk(b"ol")));
        assert!(string_contains(hay, mk(b"lo")));
    }
}
