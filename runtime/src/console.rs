//! Console bridge: `print`/`println` and value-to-text formatting for the
//! simple builtins. The compiler emits calls to these primitives; richer
//! formatting is left to `std.text` later.

use alloc::string::ToString;
use alloc::vec::Vec;
use crate::layout::{enum_tag, list_len, map_len, str_bytes, str_len, tuple_field_count};
use crate::object::{
    PickleObject, PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_FLOAT,
    PICKLE_CLASS_BOX_INT, PICKLE_CLASS_ENUM, PICKLE_CLASS_LIST, PICKLE_CLASS_MAP,
    PICKLE_CLASS_STRING, PICKLE_CLASS_TUPLE,
};
#[cfg(all(not(test), not(feature = "nostd")))]
use std::io::Write;

#[cfg(test)]
pub(crate) mod testio {
    use core::cell::{Cell, RefCell};

    thread_local! {
        pub static CAPTURE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        pub static HAS_CAPTURE: Cell<bool> = const { Cell::new(false) };
    }
}

/// Write bytes either to a test capture buffer (when enabled) or stdout.
/// Errors are ignored on purpose: a closed stdout should not unwind GC paths.
pub(crate) fn write_to_con(bytes: &[u8]) {
    #[cfg(test)]
    if testio::HAS_CAPTURE.with(|c| c.get()) {
        testio::CAPTURE.with(|buf| buf.borrow_mut().extend_from_slice(bytes));
        return;
    }
    write_stdout(bytes);
}

fn write_stdout(bytes: &[u8]) {
    #[cfg(feature = "nostd")]
    {
        crate::nostd::console_write(bytes);
    }
    #[cfg(not(feature = "nostd"))]
    {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(bytes);
        let _ = lock.flush();
    }
}

/// Whether ANSI escapes will be honoured on stdout. Color is enabled for a
/// real terminal unless `NO_COLOR` (or `PICKLE_NO_COLOR`) is set. A
/// freestanding build never emits colors.
pub(crate) fn terminal_color_enabled() -> bool {
    #[cfg(feature = "nostd")]
    {
        false
    }
    #[cfg(not(feature = "nostd"))]
    {
        use std::io::IsTerminal;
        if std::env::var_os("NO_COLOR").is_some() || std::env::var_os("PICKLE_NO_COLOR").is_some() {
            return false;
        }
        std::io::stdout().is_terminal()
    }
}

/// Print raw bytes (from a string literal or the contents of a string).
fn print_raw_bytes(ptr: *const u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        write_to_con(bytes);
    }
}

#[no_mangle]
pub extern "C" fn pickle_print_bytes(ptr: *const u8, len: usize) {
    print_raw_bytes(ptr, len);
}

/// Print a NUL-terminated static string.
fn print_raw_cstr(ptr: *const u8) {
    if ptr.is_null() {
        return;
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
    }
    print_raw_bytes(ptr, len);
}

#[no_mangle]
pub extern "C" fn pickle_print_cstr(ptr: *const u8) {
    print_raw_cstr(ptr);
}

/// Print a signed 64-bit integer as text.
#[no_mangle]
pub extern "C" fn pickle_print_i64(v: i64) {
    let s = crate::strings::int64_to_string(v);
    emit_string(s);
}

/// Print an unsigned 64-bit integer as text.
#[no_mangle]
pub extern "C" fn pickle_print_u64(v: u64) {
    let s = crate::strings::int64_to_string(v as i64);
    emit_string(s);
}

/// Print a double as text.
#[no_mangle]
pub extern "C" fn pickle_print_f64(v: f64) {
    let s = crate::strings::float64_to_string(v);
    emit_string(s);
}

/// Print a boolean as `true` / `false`.
#[no_mangle]
pub extern "C" fn pickle_print_bool(v: bool) {
    if v {
        pickle_print_cstr(b"true\0".as_ptr());
    } else {
        pickle_print_cstr(b"false\0".as_ptr());
    }
}

/// Print a single raw byte (for `byte` values). Writes the byte verbatim, so a
/// `byte` mid-copy is preserved even inside multi-byte UTF-8 text.
#[no_mangle]
pub extern "C" fn pickle_print_byte(v: i64) {
    write_to_con(&[v as u8]);
}

/// Print a `char` as text, matching `"{c}"`: the code point is lowered to
/// UTF-8 first (a non-ASCII `char` is multiple bytes, never a single byte).
#[no_mangle]
pub extern "C" fn pickle_print_char(v: i32) {
    let mut buf = [0u8; 4];
    let s = char::from_u32(v as u32).unwrap_or('\u{FFFD}');
    let bytes = s.encode_utf8(&mut buf);
    write_to_con(bytes.as_bytes());
}

fn emit_string(s: *mut PickleObject) {
    let len = str_len(s);
    let ptr = str_bytes(s);
    pickle_print_bytes(ptr, len);
}

/// Print the text form of a managed object. Strings print their contents;
/// collections print their length; user objects print their class name.
fn print_obj_raw(obj: *mut PickleObject) {
    if obj.is_null() {
        pickle_print_cstr(b"none\0".as_ptr());
        return;
    }
    let class_id = unsafe { (*obj).class_id };
    match class_id {
        PICKLE_CLASS_STRING => emit_string(obj),
        PICKLE_CLASS_LIST => {
            pickle_print_cstr(b"List(len=\0".as_ptr());
            pickle_print_i64(list_len(obj) as i64);
            pickle_print_byte(b')' as i64);
        }
        PICKLE_CLASS_MAP => {
            pickle_print_cstr(b"Map(len=\0".as_ptr());
            pickle_print_i64(map_len(obj) as i64);
            pickle_print_byte(b')' as i64);
        }
        PICKLE_CLASS_ENUM => {
            pickle_print_cstr(b"Enum(tag=\0".as_ptr());
            pickle_print_i64(enum_tag(obj));
            pickle_print_byte(b')' as i64);
        }
        PICKLE_CLASS_TUPLE => {
            pickle_print_cstr(b"Tuple(len=\0".as_ptr());
            pickle_print_i64(tuple_field_count(obj) as i64);
            pickle_print_byte(b')' as i64);
        }
        PICKLE_CLASS_BOX_INT => pickle_print_i64(crate::boxscalar::box_bits(obj)),
        PICKLE_CLASS_BOX_FLOAT => {
            pickle_print_f64(f64::from_bits(crate::boxscalar::box_bits(obj) as u64))
        }
        PICKLE_CLASS_BOX_BOOL => pickle_print_cstr(if crate::boxscalar::box_bits(obj) != 0 {
            b"true\0".as_ptr()
        } else {
            b"false\0".as_ptr()
        }),
        PICKLE_CLASS_BOX_CHAR => pickle_print_char(crate::boxscalar::box_bits(obj) as i32),
        #[cfg(not(feature = "nostd"))]
        _ => {
            let gc = crate::gc::gc_mut();
            let name = gc.class_name(class_id);
            match name {
                Some(bytes) => pickle_print_bytes(bytes.as_ptr(), bytes.len()),
                None => pickle_print_cstr(b"<object>\0".as_ptr()),
            }
        }
        #[cfg(feature = "nostd")]
        _ => pickle_print_cstr(b"<object>\0".as_ptr()),
    }
}

/// Format the value of a managed object into `buf` (used by `expect` to build
/// `Expected:/Received:` lines). Lists are printed element-wise with brackets.
pub(crate) fn fmt_obj_to(buf: &mut Vec<u8>, obj: *mut PickleObject) {
    if obj.is_null() {
        buf.extend_from_slice(b"none");
        return;
    }
    let class_id = unsafe { (*obj).class_id };
    match class_id {
        PICKLE_CLASS_STRING => {
            // SAFETY: pointer/length come straight from the managed string.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    crate::strings::string_bytes_ptr(obj),
                    crate::strings::string_bytes_len(obj),
                )
            };
            buf.extend_from_slice(bytes);
        }
        PICKLE_CLASS_LIST => {
            buf.push(b'[');
            let n = list_len(obj);
            for i in 0..n {
                if i > 0 {
                    buf.extend_from_slice(b", ");
                }
                let elem = crate::list::pickle_list_get(obj, i);
                fmt_obj_to(buf, elem);
            }
            buf.push(b']');
        }
        PICKLE_CLASS_MAP => {
            let n = map_len(obj);
            buf.extend_from_slice(b"Map(");
            buf.extend_from_slice(n.to_string().as_bytes());
            buf.push(b')');
        }
        PICKLE_CLASS_ENUM => {
            buf.extend_from_slice(b"Enum(");
            buf.extend_from_slice(enum_tag(obj).to_string().as_bytes());
            buf.push(b')');
        }
        PICKLE_CLASS_TUPLE => {
            buf.push(b'(');
            let n = tuple_field_count(obj);
            for i in 0..n {
                if i > 0 {
                    buf.extend_from_slice(b", ");
                }
                let elem = crate::tuple::pickle_tuple_field(obj, i);
                fmt_obj_to(buf, elem);
            }
            buf.push(b')');
        }
        PICKLE_CLASS_BOX_INT => {
            buf.extend_from_slice(crate::boxscalar::box_bits(obj).to_string().as_bytes())
        }
        PICKLE_CLASS_BOX_FLOAT => {
            let s = crate::strings::float64_to_string(f64::from_bits(
                crate::boxscalar::box_bits(obj) as u64,
            ));
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    crate::strings::string_bytes_ptr(s),
                    crate::strings::string_bytes_len(s),
                )
            };
            buf.extend_from_slice(bytes);
        }
        PICKLE_CLASS_BOX_BOOL => buf.extend_from_slice(if crate::boxscalar::box_bits(obj) != 0 {
            b"true"
        } else {
            b"false"
        }),
        PICKLE_CLASS_BOX_CHAR => buf.push(crate::boxscalar::box_bits(obj) as u8),
        #[cfg(not(feature = "nostd"))]
        _ => {
            let gc = crate::gc::gc_mut();
            match gc.class_name(class_id) {
                Some(bytes) => buf.extend_from_slice(bytes),
                None => buf.extend_from_slice(b"<object>"),
            }
        }
        #[cfg(feature = "nostd")]
        _ => buf.extend_from_slice(b"<object>"),
    }
}

#[no_mangle]
pub extern "C" fn pickle_print_obj(obj: *mut PickleObject) {
    print_obj_raw(obj);
}

/// Write a newline.
#[no_mangle]
pub extern "C" fn pickle_print_newline() {
    write_to_con(b"\n");
}

/// Flush stdout. Kept for explicit checkpointing from compiled code.
#[no_mangle]
pub extern "C" fn pickle_flush() {
    #[cfg(all(not(test), not(feature = "nostd")))]
    {
        let stdout = std::io::stdout();
        let _ = stdout.lock().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::testio::{CAPTURE, HAS_CAPTURE};
    use super::*;

    fn capture() -> Vec<u8> {
        HAS_CAPTURE.with(|c| c.set(true));
        CAPTURE.with(|b| {
            let mut b = b.borrow_mut();
            b.clear();
            b.clone()
        })
    }

    fn stop_capture() -> String {
        let out = CAPTURE.with(|b| b.borrow().clone());
        HAS_CAPTURE.with(|c| c.set(false));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prints_values() {
        let _guard = crate::gc::test_begin();
        capture();
        pickle_print_cstr(b"hi\0".as_ptr());
        pickle_print_bytes(b" world".as_ptr(), 6);
        pickle_print_i64(42);
        pickle_print_f64(1.25);
        pickle_print_bool(true);
        pickle_print_newline();
        let out = stop_capture();
        assert_eq!(out, "hi world421.25true\n");
    }
}
