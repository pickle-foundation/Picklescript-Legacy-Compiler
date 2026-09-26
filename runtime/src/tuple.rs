//! Builtin tuple support.
//!
//! Layout per `docs/05-memory.md`: `PTuple { header, len: usize, fields: [] }`.
//! The element count lives at `TUPLE_LEN_OFF` (a `usize`, never a managed
//! pointer); the payload fields after it are managed pointers — boxed scalars
//! or object pointers, exactly like list elements — and get traced by the
//! collector. The count is stored explicitly so an empty tuple is
//! distinguishable from a one-element one even though the allocator's minimum
//! object size is 32 bytes.

use crate::gc::Gc;
use crate::layout::{tuple_field_count, tuple_object_size, tuple_set_len, TUPLE_FIELDS_OFF};
use crate::object::{PickleObject, PICKLE_CLASS_TUPLE};

/// Create a tuple object holding `field_count` null payload fields.
pub fn tuple_new(field_count: usize, gc: &mut Gc) -> *mut PickleObject {
    let size = tuple_object_size(field_count);
    let obj = gc.alloc(size as u32, PICKLE_CLASS_TUPLE);
    tuple_init(obj, field_count);
    obj
}

/// Write `field_count` and zero every payload field of an already-allocated
/// tuple.
pub(crate) fn tuple_init(obj: *mut PickleObject, field_count: usize) {
    unsafe {
        tuple_set_len(obj, field_count);
        // Zero the payload fields so uninitialised slots are never traced.
        if field_count > 0 {
            let size = tuple_object_size(field_count);
            let fields = (obj as *mut u8).add(TUPLE_FIELDS_OFF);
            core::ptr::write_bytes(fields, 0, size - TUPLE_FIELDS_OFF);
        }
    }
}

/// Write a payload field (unchecked bounds; callers pass constant indices into
/// a freshly constructed tuple of matching width).
pub fn tuple_set_field(obj: *mut PickleObject, index: usize, value: *mut PickleObject) {
    unsafe {
        let n = tuple_field_count(obj);
        assert!(index < n, "pickle: tuple field index out of bounds");
        let base = (*obj).payload_mut() as *mut *mut PickleObject;
        base.add(1 + index).write(value);
    }
}

/// Read a payload field; null when the index is out of bounds.
pub fn tuple_field(obj: *const PickleObject, index: usize) -> *mut PickleObject {
    unsafe {
        if index >= tuple_field_count(obj) {
            return core::ptr::null_mut();
        }
        let base = (*obj).payload() as *const *mut PickleObject;
        base.add(1 + index).read()
    }
}

/// The element count of a tuple object.
pub fn tuple_len_of(obj: *const PickleObject) -> usize {
    tuple_field_count(obj)
}

// ---- Exported ABI ----------------------------------------------------------
// Thin `extern "C"` wrappers over the internal Rust APIs so compiled programs
// can construct tuple values and read their elements. Payload fields must be
// boxed first (`boxscalar::pickle_box_*`), matching list-element rules.

/// Create a tuple object with `field_count` null payload fields.
#[no_mangle]
pub extern "C" fn pickle_tuple_new(field_count: usize) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    tuple_new(field_count, gc)
}

/// The element count.
#[no_mangle]
pub extern "C" fn pickle_tuple_len(obj: *const PickleObject) -> usize {
    tuple_len_of(obj)
}

/// Write a payload field.
#[no_mangle]
pub extern "C" fn pickle_tuple_set_field(
    obj: *mut PickleObject,
    index: usize,
    value: *mut PickleObject,
) {
    tuple_set_field(obj, index, value)
}

/// A payload field; null when out of bounds.
#[no_mangle]
pub extern "C" fn pickle_tuple_field(obj: *const PickleObject, index: usize) -> *mut PickleObject {
    tuple_field(obj, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn new_zeroes_payload_and_stores_count() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        unsafe {
            let t = tuple_new(3, gc);
            assert_eq!(tuple_len_of(t), 3);
            for i in 0..3 {
                assert!(tuple_field(t, i).is_null());
            }
            assert!(tuple_field(t, 5).is_null(), "out of range reads null");
            assert_eq!((*t).class_id, PICKLE_CLASS_TUPLE);
        }
    }

    #[test]
    fn empty_tuple_has_zero_fields() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let t = tuple_new(0, gc);
        assert_eq!(tuple_len_of(t), 0, "empty tuple must read as 0 elements");
        assert!(tuple_field(t, 0).is_null());
    }

    #[test]
    fn new_zeroes_reused_dirty_memory() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        unsafe {
            let size = crate::layout::tuple_object_size(3);
            let t = gc.alloc(size as u32, PICKLE_CLASS_TUPLE);
            let fields = (t as *mut u8).add(TUPLE_FIELDS_OFF);
            core::ptr::write_bytes(fields, 0xAB, size - TUPLE_FIELDS_OFF);
            tuple_init(t, 3);
            assert_eq!(tuple_len_of(t), 3);
            for i in 0..3 {
                assert!(tuple_field(t, i).is_null(), "field {i} not re-zeroed");
            }
        }
    }

    #[test]
    fn set_and_read_fields() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let a = crate::boxscalar::pickle_box_i64(10);
        let b = crate::boxscalar::pickle_box_i64(20);
        let t = tuple_new(2, gc);
        tuple_set_field(t, 0, a);
        tuple_set_field(t, 1, b);
        assert_eq!(crate::boxscalar::box_bits(tuple_field(t, 0)), 10);
        assert_eq!(crate::boxscalar::box_bits(tuple_field(t, 1)), 20);
    }

    #[test]
    fn gc_marks_through_payload_fields() {
        let _guard = crate::gc::test_begin();
        use crate::heap::Heap;
        use crate::object::DescriptorTable;
        let heap = Heap::new();
        let desc = DescriptorTable::new();
        let gc = crate::gc::gc_mut();
        unsafe {
            let payload = crate::boxscalar::pickle_box_i64(99);
            let t = tuple_new(1, gc);
            tuple_set_field(t, 0, payload);
            assert!(!(*t).is_marked());
            assert!(!(*payload).is_marked());

            let root: *mut PickleObject = t;
            let roots: Vec<*mut *mut PickleObject> =
                vec![(&root as *const *mut PickleObject) as *mut _];
            crate::trace::trace_from_roots(&desc, &roots, &[], &[]);

            assert!((*t).is_marked());
            assert!((*payload).is_marked(), "payload field must be traced");
        }
        let _ = heap;
        let _ = gc;
    }
}
