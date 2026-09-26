//! Builtin enum support.
//!
//! Layout per `docs/05-memory.md`: `PEnum { header, tag: i64, fields: [] }`.
//! The tag at payload slot 0 is *not* a managed pointer; the payload fields
//! (payload slot 1..) are managed and get traced by the collector. Fields are
//! boxed scalars or object pointers, exactly like list elements, so the field
//! count is recovered at runtime from the object's stored `size`.

use crate::gc::Gc;
use crate::layout::{enum_field_count, enum_object_size, enum_set_tag, enum_tag, ENUM_FIELDS_OFF};
use crate::object::{
    PickleObject, PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_FLOAT,
    PICKLE_CLASS_BOX_INT, PICKLE_CLASS_ENUM, PICKLE_CLASS_STRING,
};

/// Create an enum object holding `tag` and `field_count` null payload fields.
pub fn enum_new(tag: i64, field_count: usize, gc: &mut Gc) -> *mut PickleObject {
    let size = enum_object_size(field_count);
    let obj = gc.alloc(size as u32, PICKLE_CLASS_ENUM);
    enum_init(obj, tag, field_count);
    obj
}

/// Write `tag` and zero every payload field of an already-allocated enum.
pub(crate) fn enum_init(obj: *mut PickleObject, tag: i64, field_count: usize) {
    unsafe {
        enum_set_tag(obj, tag);
        // Zero the payload fields so uninitialised slots are never traced.
        // `ENUM_FIELDS_OFF` is measured from the object start, and
        // `size - ENUM_FIELDS_OFF` is exactly the byte length of the field
        // region (the tag occupies the 8 bytes before it).
        if field_count > 0 {
            let size = enum_object_size(field_count);
            let fields = (obj as *mut u8).add(ENUM_FIELDS_OFF);
            core::ptr::write_bytes(fields, 0, size - ENUM_FIELDS_OFF);
        }
    }
}

/// Write a payload field (unchecked bounds; callers pass constant indices into
/// a freshly constructed enum of matching width).
pub fn enum_set_field(obj: *mut PickleObject, index: usize, value: *mut PickleObject) {
    unsafe {
        let n = enum_field_count(obj);
        assert!(index < n, "pickle: enum field index out of bounds");
        let base = (*obj).payload_mut() as *mut *mut PickleObject;
        base.add(1 + index).write(value);
    }
}

/// Read a payload field; null when the index is out of bounds.
pub fn enum_field(obj: *const PickleObject, index: usize) -> *mut PickleObject {
    unsafe {
        if index >= enum_field_count(obj) {
            return core::ptr::null_mut();
        }
        let base = (*obj).payload() as *const *mut PickleObject;
        base.add(1 + index).read()
    }
}

/// The variant tag of an enum object.
pub fn enum_tag_of(obj: *const PickleObject) -> i64 {
    enum_tag(obj)
}

/// The runtime class id of an object (must be non-null).
fn enum_class_of(obj: *const PickleObject) -> u32 {
    unsafe { (*obj).class_id }
}

// ---- Exported ABI ----------------------------------------------------------
// Thin `extern "C"` wrappers over the internal Rust APIs so compiled programs
// can construct enum values and inspect their tag/fields. Payload fields must
// be boxed first (`boxscalar::pickle_box_*`), matching list-element rules.

/// Create an enum object with `tag` and `field_count` null payload fields.
#[no_mangle]
pub extern "C" fn pickle_enum_new(tag: i64, field_count: usize) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    enum_new(tag, field_count, gc)
}

/// Write a payload field.
#[no_mangle]
pub extern "C" fn pickle_enum_set_field(
    obj: *mut PickleObject,
    index: usize,
    value: *mut PickleObject,
) {
    enum_set_field(obj, index, value)
}

/// The variant tag.
#[no_mangle]
pub extern "C" fn pickle_enum_tag(obj: *const PickleObject) -> i64 {
    enum_tag_of(obj)
}

/// A payload field; null when out of bounds.
#[no_mangle]
pub extern "C" fn pickle_enum_field(obj: *const PickleObject, index: usize) -> *mut PickleObject {
    enum_field(obj, index)
}

/// Structural equality of two enum objects: same tag and equal payload fields.
/// Fields compare by their runtime class — boxed scalars by value (floats by
/// `f64` value, not bit pattern, so `-0.0 == 0.0` and `NaN != NaN`), strings by
/// content, nested enums recursively, and any other managed payload (class
/// instances, lists, maps, tuples, options, ...) by pointer identity.
#[no_mangle]
pub extern "C" fn pickle_enum_eq(a: *const PickleObject, b: *const PickleObject) -> bool {
    if core::ptr::eq(a, b) {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    if enum_class_of(a) != PICKLE_CLASS_ENUM || enum_class_of(b) != PICKLE_CLASS_ENUM {
        return false;
    }
    if enum_tag(a) != enum_tag(b) {
        return false;
    }
    let na = enum_field_count(a);
    let nb = enum_field_count(b);
    if na != nb {
        return false;
    }
    for i in 0..na {
        if !enum_field_eq(enum_field(a, i), enum_field(b, i)) {
            return false;
        }
    }
    true
}

/// Equality of a single managed payload slot, used by `pickle_enum_eq`.
fn enum_field_eq(a: *const PickleObject, b: *const PickleObject) -> bool {
    if core::ptr::eq(a, b) {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    let ca = enum_class_of(a);
    let cb = enum_class_of(b);
    if ca != cb {
        return false;
    }
    match ca {
        PICKLE_CLASS_BOX_INT | PICKLE_CLASS_BOX_BOOL | PICKLE_CLASS_BOX_CHAR => {
            crate::boxscalar::box_bits(a) == crate::boxscalar::box_bits(b)
        }
        PICKLE_CLASS_BOX_FLOAT => {
            f64::from_bits(crate::boxscalar::box_bits(a) as u64)
                == f64::from_bits(crate::boxscalar::box_bits(b) as u64)
        }
        PICKLE_CLASS_STRING => crate::strings::string_cmp(a, b) == 0,
        PICKLE_CLASS_ENUM => pickle_enum_eq(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn new_zeroes_payload_and_stores_tag() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        unsafe {
            let e = enum_new(2, 3, gc);
            assert_eq!(enum_tag_of(e), 2);
            assert_eq!(enum_field_count(e), 3);
            for i in 0..3 {
                assert!(enum_field(e, i).is_null());
            }
            assert!(enum_field(e, 5).is_null(), "out of range reads null");
            assert_eq!((*e).class_id, PICKLE_CLASS_ENUM);
        }
    }

    #[test]
    fn new_zeroes_reused_dirty_memory() {
        // Regression: the field-zeroing write must start at the object-relative
        // `ENUM_FIELDS_OFF`, not at `payload + ENUM_FIELDS_OFF` (which is
        // HEADER bytes too far and leaves the whole field region dirty). We
        // poison the region first so the check is deterministic regardless of
        // whether the allocator hands back zeroed pages.
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        unsafe {
            let size = crate::layout::enum_object_size(3);
            let e = gc.alloc(size as u32, PICKLE_CLASS_ENUM);
            let fields = (e as *mut u8).add(ENUM_FIELDS_OFF);
            core::ptr::write_bytes(fields, 0xAB, size - ENUM_FIELDS_OFF);
            enum_init(e, 7, 3);
            assert_eq!(enum_tag_of(e), 7);
            for i in 0..3 {
                assert!(enum_field(e, i).is_null(), "field {i} not re-zeroed");
            }
        }
    }

    #[test]
    fn set_and_read_fields() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let a = crate::boxscalar::pickle_box_i64(10);
        let b = crate::boxscalar::pickle_box_i64(20);
        let e = enum_new(0, 2, gc);
        enum_set_field(e, 0, a);
        enum_set_field(e, 1, b);
        assert_eq!(crate::boxscalar::box_bits(enum_field(e, 0)), 10);
        assert_eq!(crate::boxscalar::box_bits(enum_field(e, 1)), 20);
    }

    #[test]
    fn enum_eq_compares_tag_and_fields() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        {
            // Same tag, no fields: equal regardless of address.
            let a = enum_new(3, 0, gc);
            let b = enum_new(3, 0, gc);
            assert!(pickle_enum_eq(a, b));
            // Different tag: unequal.
            let c = enum_new(4, 0, gc);
            assert!(!pickle_enum_eq(a, c));
            // Payload fields compare by value.
            let p1 = enum_new(1, 2, gc);
            let p2 = enum_new(1, 2, gc);
            enum_set_field(p1, 0, crate::boxscalar::pickle_box_i64(10));
            enum_set_field(
                p1,
                1,
                crate::strings::pickle_str_from_bytes("x".as_ptr(), 1),
            );
            enum_set_field(p2, 0, crate::boxscalar::pickle_box_i64(10));
            enum_set_field(
                p2,
                1,
                crate::strings::pickle_str_from_bytes("x".as_ptr(), 1),
            );
            assert!(pickle_enum_eq(p1, p2));
            // Mismatched scalar payload: unequal.
            let p3 = enum_new(1, 2, gc);
            enum_set_field(p3, 0, crate::boxscalar::pickle_box_i64(11));
            enum_set_field(
                p3,
                1,
                crate::strings::pickle_str_from_bytes("x".as_ptr(), 1),
            );
            assert!(!pickle_enum_eq(p1, p3));
            // Mismatched string payload: unequal.
            let p4 = enum_new(1, 2, gc);
            enum_set_field(p4, 0, crate::boxscalar::pickle_box_i64(10));
            enum_set_field(
                p4,
                1,
                crate::strings::pickle_str_from_bytes("y".as_ptr(), 1),
            );
            assert!(!pickle_enum_eq(p1, p4));
            // Self-comparison short-circuits.
            assert!(pickle_enum_eq(p1, p1));
        }
    }

    #[test]
    fn enum_eq_nested_enums_recurse() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        {
            let inner_target = enum_new(7, 0, gc);
            let outer1 = enum_new(9, 1, gc);
            let outer2 = enum_new(9, 1, gc);
            enum_set_field(outer1, 0, inner_target);
            enum_set_field(outer2, 0, inner_target);
            // Same inner pointer (or structurally equal) -> equal.
            assert!(pickle_enum_eq(outer1, outer1));
            let inner2 = enum_new(7, 0, gc);
            let outer3 = enum_new(9, 1, gc);
            enum_set_field(outer3, 0, inner2);
            assert!(pickle_enum_eq(outer1, outer3));
        }
    }

    #[test]
    fn enum_eq_mismatched_payload_class_false() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        {
            let e = enum_new(2, 1, gc);
            let other = enum_new(2, 1, gc);
            // One payload is a float box, the other an int box: unequal even
            // though both happen to carry the same bit pattern.
            let f = crate::boxscalar::pickle_box_f64(1.0);
            let i = crate::boxscalar::pickle_box_i64(0x3FF0_0000_0000_0000);
            enum_set_field(e, 0, f);
            enum_set_field(other, 0, i);
            assert!(!pickle_enum_eq(e, other));
            // Float boxes compare by value: NaN != NaN, -0.0 == 0.0.
            let nan1 = crate::boxscalar::pickle_box_f64(f64::NAN);
            let nan2 = crate::boxscalar::pickle_box_f64(f64::NAN);
            let e1 = enum_new(4, 1, gc);
            let e2 = enum_new(4, 1, gc);
            enum_set_field(e1, 0, nan1);
            enum_set_field(e2, 0, nan2);
            assert!(
                !pickle_enum_eq(e1, e2),
                "NaN != NaN under f64 value compare"
            );
            let nz1 = crate::boxscalar::pickle_box_f64(-0.0);
            let nz2 = crate::boxscalar::pickle_box_f64(0.0);
            let e3 = enum_new(5, 1, gc);
            let e4 = enum_new(5, 1, gc);
            enum_set_field(e3, 0, nz1);
            enum_set_field(e4, 0, nz2);
            assert!(
                pickle_enum_eq(e3, e4),
                "-0.0 == 0.0 under f64 value compare"
            );
        }
    }

    #[test]
    fn enum_eq_null_or_mismatched_objects_false() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let e = enum_new(0, 1, gc);
        assert!(!pickle_enum_eq(e, core::ptr::null_mut()));
        assert!(!pickle_enum_eq(core::ptr::null_mut(), e));
        // A non-enum managed object is never equal to an enum.
        let s = crate::strings::pickle_str_from_bytes("x".as_ptr(), 1) as *const PickleObject;
        assert!(!pickle_enum_eq(e, s));
    }

    #[test]
    fn gc_marks_through_payload_fields() {
        // A unit variant (no fields) holds no refs; a payload variant must be
        // traced through its field slots but never through the tag slot.
        let _guard = crate::gc::test_begin();
        use crate::heap::Heap;
        use crate::object::DescriptorTable;
        let heap = Heap::new();
        let desc = DescriptorTable::new();
        let gc = crate::gc::gc_mut();
        unsafe {
            let payload = crate::boxscalar::pickle_box_i64(99);
            let e = enum_new(1, 1, gc);
            enum_set_field(e, 0, payload);
            assert!(!(*e).is_marked());
            assert!(!(*payload).is_marked());

            let root: *mut PickleObject = e;
            let roots: Vec<*mut *mut PickleObject> =
                vec![(&root as *const *mut PickleObject) as *mut _];
            crate::trace::trace_from_roots(&desc, &roots, &[], &[]);

            assert!((*e).is_marked());
            assert!((*payload).is_marked(), "payload field must be traced");
        }
        let _ = heap;
        let _ = gc;
    }
}
