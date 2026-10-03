//! Boxed scalar objects.
//!
//! Managed collections (`List<T>`, `Map<K, V>`, options in due course) hold
//! `*mut PickleObject` references, which only works for values that are
//! already objects (strings). Plain `int`/`float`/`bool` scalars go through
//! these tiny 32-byte boxes instead: `{ header, bits: i64 }`, where `bits` is
//! the raw `i64`, the `f64` bit pattern, or `0`/`1` for `bool`.
//!
//! The box classes have no managed payload (slot count 0), so the collector
//! marks the box itself but never chases the scalar. Layout constants live in
//! `layout.rs`; descriptors are registered at `pickle_runtime_init`.

use crate::layout::{BOX_OBJECT_SIZE, BOX_PAYLOAD_OFF};
use crate::object::{
    PickleObject, PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_FLOAT,
    PICKLE_CLASS_BOX_INT,
};

#[inline]
unsafe fn payload(obj: *mut PickleObject) -> *mut i64 {
    (obj as *mut u8).add(BOX_PAYLOAD_OFF) as *mut i64
}

fn new_box(class_id: u32, bits: i64) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let obj = gc.alloc(BOX_OBJECT_SIZE as u32, class_id);
    unsafe {
        *payload(obj) = bits;
    }
    #[cfg(feature = "nostd")]
    if crate::nostd::getenv_present("PKL_GC_TRACE") {
        eprintln!("GCBOX + {:p} class={} bits={}", obj, class_id, bits);
    }
    #[cfg(not(feature = "nostd"))]
    if std::env::var_os("PKL_GC_TRACE").is_some() {
        eprintln!("GCBOX + {:p} class={} bits={}", obj, class_id, bits);
    }
    obj
}

/// The raw 8 payload bits of a box (print helper).
#[inline]
pub fn box_bits(obj: *const PickleObject) -> i64 {
    unsafe { ((obj as *const u8).add(BOX_PAYLOAD_OFF) as *const i64).read() }
}

/// Reinterpret a boxed float's payload bits back to an `f64`.
///
/// Added for the `pickle_print_obj` port (`pickle/runtime/abi_console.pkl`).
/// That body's `BOX_FLOAT` arm holds the raw bits and must turn them back into a
/// float to hand to `pickle_print_f64`.
///
/// Pickle has no bitcast: `x as float` is a *numeric* conversion, so
/// `f64FromBits` has no source-level spelling and needs this symbol. It is the
/// exact inverse of `pickle_box_f64`, which stores `v.to_bits()`.
///
/// Takes the bits as `i64` because that is what `box_bits` yields; the cast
/// through `u64` restores the bit pattern without changing it.
#[no_mangle]
pub extern "C" fn pickle_f64_from_bits(bits: i64) -> f64 {
    f64::from_bits(bits as u64)
}

/// Box an `int` as a managed object.
#[no_mangle]
pub extern "C" fn pickle_box_i64(v: i64) -> *mut PickleObject {
    new_box(PICKLE_CLASS_BOX_INT, v)
}

/// Box a `float` as a managed object (bits stored bit-for-bit).
#[no_mangle]
pub extern "C" fn pickle_box_f64(v: f64) -> *mut PickleObject {
    new_box(PICKLE_CLASS_BOX_FLOAT, v.to_bits() as i64)
}

/// Box a `bool` as a managed object (`0`/`1` payload).
#[no_mangle]
pub extern "C" fn pickle_box_bool(v: bool) -> *mut PickleObject {
    new_box(PICKLE_CLASS_BOX_BOOL, v as i64)
}

/// Box a `char` as a managed object. `char` is an i32 scalar at the IR
/// boundary (see the Compiler's `IrTy::Char`); only the low 8 bits carry the
/// code point for ASCII, and the printed form is a single byte.
#[no_mangle]
pub extern "C" fn pickle_box_char(v: i32) -> *mut PickleObject {
    new_box(PICKLE_CLASS_BOX_CHAR, v as i64)
}

/// Unboxing reads the raw 8 payload bits. The null/type checks live in a plain
/// Rust helper so the value logic can be unit-tested; the `extern "C"` entry
/// points below delegate here. (An `extern "C"` function cannot unwind, so a
/// panicking read from compiled code aborts via the panic hook — a hard error,
/// matching the language's no-exceptions model.)
#[inline]
pub(crate) fn unbox_bits(obj: *const PickleObject, what: &str) -> i64 {
    if obj.is_null() {
        panic!("pickle: cannot unbox null as {what}");
    }
    #[cfg(feature = "nostd")]
    if crate::nostd::getenv_present("PKL_GC_TRACE") {
        eprintln!("GCBOX ! unbox {:p} as {}", obj, what);
    }
    #[cfg(not(feature = "nostd"))]
    if std::env::var_os("PKL_GC_TRACE").is_some() {
        eprintln!("GCBOX ! unbox {:p} as {}", obj, what);
    }
    box_bits(obj)
}

/// Unbox a managed `int` object back to a raw `i64`.
#[no_mangle]
pub extern "C" fn pickle_unbox_i64(obj: *const PickleObject) -> i64 {
    unbox_bits(obj, "int")
}

/// Unbox a managed `float` object back to a raw `f64`.
#[no_mangle]
pub extern "C" fn pickle_unbox_f64(obj: *const PickleObject) -> f64 {
    f64::from_bits(unbox_bits(obj, "float") as u64)
}

/// Unbox a managed `bool` object back to a raw `bool`.
#[no_mangle]
pub extern "C" fn pickle_unbox_bool(obj: *const PickleObject) -> bool {
    unbox_bits(obj, "bool") != 0
}

/// Unbox a managed `char` object back to an `i32` scalar.
#[no_mangle]
pub extern "C" fn pickle_unbox_char(obj: *const PickleObject) -> i32 {
    unbox_bits(obj, "char") as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{
        PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_FLOAT, PICKLE_CLASS_BOX_INT,
        PICKLE_CLASS_STREAM,
    };

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn roundtrip_scalars() {
        let _guard = setup();
        let i = pickle_box_i64(-1234567890123);
        assert_eq!(pickle_unbox_i64(i), -1234567890123);
        assert_eq!(crate::layout::BOX_OBJECT_SIZE, 32);
        let f = pickle_box_f64(3.5);
        assert_eq!(pickle_unbox_f64(f), 3.5);
        let b = pickle_box_bool(true);
        assert!(pickle_unbox_bool(b));
        let c = pickle_box_char(0x41);
        assert_eq!(pickle_unbox_char(c), 0x41);
        assert_eq!(crate::boxscalar::box_bits(c), 0x41);
    }

    #[test]
    fn class_ids_and_layout() {
        let _guard = setup();
        crate::pickle_runtime_init();
        let i = pickle_box_i64(7);
        assert_eq!(unsafe { (*i).class_id }, PICKLE_CLASS_BOX_INT);
        assert_eq!(
            crate::gc::gc_mut().class_name(PICKLE_CLASS_BOX_INT),
            Some(&b"int"[..])
        );
        assert_eq!(
            crate::gc::gc_mut().class_name(PICKLE_CLASS_BOX_FLOAT),
            Some(&b"float"[..])
        );
        assert_eq!(
            crate::gc::gc_mut().class_name(PICKLE_CLASS_BOX_BOOL),
            Some(&b"bool"[..])
        );
        assert_eq!(
            crate::gc::gc_mut().class_name(PICKLE_CLASS_BOX_CHAR),
            Some(&b"char"[..])
        );
        let c = pickle_box_char(b'x' as i32);
        assert_eq!(unsafe { (*c).class_id }, PICKLE_CLASS_BOX_CHAR);
        // Class ids must match the descriptor registration order: the enum,
        // tuple, and stream hold placeholder descriptors, so the user-class
        // range begins at `PICKLE_CLASS_STREAM + 1` (`PICKLE_CLASS_USER_BASE`).
        assert_eq!(
            crate::gc::gc_mut().descriptors.len(),
            PICKLE_CLASS_STREAM as usize + 1
        );
        // Payload sits directly after the header.
        assert_eq!(i as usize + 24, unsafe { payload(i) } as usize);
    }

    #[test]
    fn unbox_null_panics() {
        let err = std::panic::catch_unwind(|| unbox_bits(core::ptr::null(), "int"));
        assert!(err.is_err());
    }
}
