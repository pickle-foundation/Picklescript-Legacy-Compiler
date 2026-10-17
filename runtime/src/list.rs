//! Builtin `List<T>` support.
//!
//! Layout per `docs/05-memory.md`: `{ header, len, cap, data: *mut void }`,
//! where `data` holds `cap * 8` bytes of `*mut PickleObject` slots. Elements
//! are scanned as managed references by the collector.

use crate::gc::Gc;
use crate::layout::{
    list_cap, list_data, list_len, list_set_cap, list_set_data, list_set_len, LIST_OBJECT_SIZE,
};
use crate::object::{PickleObject, PICKLE_CLASS_LIST};

const INITIAL_CAP: usize = 8;

/// Maximum list length to keep arithmetic sound.
const MAX_CAP: usize = isize::MAX as usize / 8;

fn require_capacity(gc: &mut Gc, list: *mut PickleObject, need: usize) {
    unsafe {
        let cap = list_cap(list);
        if cap >= need {
            return;
        }
        let mut new_cap = cap.max(INITIAL_CAP);
        while new_cap < need {
            new_cap = new_cap.saturating_mul(2);
            if new_cap >= MAX_CAP {
                panic!("pickle: list capacity overflow");
            }
        }
        let old_data = list_data(list);
        let fresh = gc.heap.raw_alloc(new_cap * 8);
        if !old_data.is_null() {
            let preserve = cap.min(new_cap) * 8;
            if preserve > 0 {
                core::ptr::copy_nonoverlapping(old_data as *const u8, fresh, preserve);
            }
            crate::heap::raw_free(old_data as *mut u8);
        }
        list_set_cap(list, new_cap);
        list_set_data(list, fresh as *mut *mut PickleObject);
    }
}

/// Create an empty list with at least `cap` capacity.
pub fn list_new(cap: usize, gc: &mut Gc) -> *mut PickleObject {
    let obj = gc.alloc(LIST_OBJECT_SIZE as u32, PICKLE_CLASS_LIST);
    list_set_len(obj, 0);
    let cap = cap.max(INITIAL_CAP);
    list_set_cap(obj, cap);
    list_set_data(obj, gc.heap.raw_alloc(cap * 8) as *mut *mut PickleObject);
    obj
}

/// Bound-checked element read; returns null when out of bounds.
pub fn list_get(list: *const PickleObject, index: usize) -> *mut PickleObject {
    unsafe {
        if index >= list_len(list) {
            return core::ptr::null_mut();
        }
        *list_data(list).add(index)
    }
}

/// Element write (unchecked bounds in release; callers must check `len`).
pub fn list_set(list: *mut PickleObject, index: usize, value: *mut PickleObject) {
    unsafe {
        assert!(index < list_len(list), "pickle: list index out of bounds");
        *list_data(list).add(index) = value;
    }
}

/// Append an element.
pub fn list_push(list: *mut PickleObject, value: *mut PickleObject) {
    let gc = crate::gc::gc_mut();
    unsafe {
        let n = list_len(list);
        require_capacity(gc, list, n + 1);
        *list_data(list).add(n) = value;
        list_set_len(list, n + 1);
    }
}

/// Remove and return the last element (null when empty).
pub fn list_pop(list: *mut PickleObject) -> *mut PickleObject {
    unsafe {
        let n = list_len(list);
        if n == 0 {
            return core::ptr::null_mut();
        }
        let v = *list_data(list).add(n - 1);
        list_set_len(list, n - 1);
        v
    }
}

/// Remove the element at `index`, shifting the tail left; returns the element.
/// Panics with a pickle error when `index` is out of bounds.
pub fn list_remove_at(list: *mut PickleObject, index: usize) -> *mut PickleObject {
    unsafe {
        let n = list_len(list);
        if index >= n {
            crate::panic::pickle_panic_cstr(b"pickle: remove index out of bounds\0".as_ptr());
        }
        let data = list_data(list);
        let v = *data.add(index);
        for i in index..n - 1 {
            *data.add(i) = *data.add(i + 1);
        }
        list_set_len(list, n - 1);
        v
    }
}

/// Insert `value` at `index`, shifting the tail right. Panics with a pickle
/// error when `index` is past the end (`index > len`).
pub fn list_insert(list: *mut PickleObject, index: usize, value: *mut PickleObject) {
    let gc = crate::gc::gc_mut();
    unsafe {
        let n = list_len(list);
        if index > n {
            crate::panic::pickle_panic_cstr(b"pickle: insert index out of bounds\0".as_ptr());
        }
        require_capacity(gc, list, n + 1);
        let data = list_data(list);
        let mut i = n;
        while i > index {
            *data.add(i) = *data.add(i - 1);
            i -= 1;
        }
        *data.add(index) = value;
        list_set_len(list, n + 1);
    }
}

/// Ascending order test between two boxed elements for a sort kind:
/// `0` int/byte/char/bool payloads, `1` float bits, `2` strings.
fn elem_lt(kind: i64, a: *const PickleObject, b: *const PickleObject) -> bool {
    match kind {
        0 => crate::boxscalar::box_bits(a) < crate::boxscalar::box_bits(b),
        1 => crate::boxscalar::pickle_unbox_f64(a) < crate::boxscalar::pickle_unbox_f64(b),
        2 => crate::strings::pickle_str_cmp(a, b) < 0,
        other => panic!("pickle: invalid sort kind {other}"),
    }
}

/// In-place ascending order sort over boxed elements (insertion sort; stable
/// for equal keys, O(n^2) worst case — fine for the compiler's symbol tables).
/// `kind` picks the comparison: see [`elem_lt`].
pub fn list_sort(list: *mut PickleObject, kind: i64) {
    unsafe {
        let n = list_len(list);
        if n < 2 {
            return;
        }
        let data = list_data(list);
        for i in 1..n {
            let key = *data.add(i);
            let mut j = i;
            while j > 0 && elem_lt(kind, key, *data.add(j - 1)) {
                *data.add(j) = *data.add(j - 1);
                j -= 1;
            }
            *data.add(j) = key;
        }
    }
}

/// Number of elements.
pub fn list_len_of(list: *const PickleObject) -> usize {
    list_len(list)
}

/// Borrow the element array (compiler-facing; length via `list_len`).
pub fn list_elements(list: *const PickleObject) -> *mut *mut PickleObject {
    list_data(list)
}

// ---- Exported ABI ----------------------------------------------------------
// Thin `extern "C"` wrappers over the internal Rust APIs so compiled programs
// can build and manipulate lists. Scalars must be boxed first
// (`boxscalar::pickle_box_*`) before being stored, and unboxed after reading.

/// Create an empty list with at least `cap` capacity.
#[no_mangle]
pub extern "C" fn pickle_list_new(cap: usize) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    list_new(cap, gc)
}

/// Number of elements.
#[no_mangle]
pub extern "C" fn pickle_list_len(list: *const PickleObject) -> usize {
    list_len_of(list)
}

/// Bound-checked element read; returns null when out of bounds.
#[no_mangle]
pub extern "C" fn pickle_list_get(list: *const PickleObject, index: usize) -> *mut PickleObject {
    list_get(list, index)
}

/// Element write; panics on index out of bounds.
#[no_mangle]
pub extern "C" fn pickle_list_set(list: *mut PickleObject, index: usize, value: *mut PickleObject) {
    list_set(list, index, value)
}

/// Append an element.
#[no_mangle]
pub extern "C" fn pickle_list_push(list: *mut PickleObject, value: *mut PickleObject) {
    list_push(list, value);
}

/// Remove and return the last element (null when empty).
#[no_mangle]
pub extern "C" fn pickle_list_pop(list: *mut PickleObject) -> *mut PickleObject {
    list_pop(list)
}

/// Remove the element at `index`, shifting the tail left; returns the element.
/// Panics with a pickle error when `index` is out of bounds.
#[no_mangle]
pub extern "C" fn pickle_list_remove(list: *mut PickleObject, index: usize) -> *mut PickleObject {
    list_remove_at(list, index)
}

/// Insert `value` at `index`, shifting the tail right. Panics with a pickle
/// error when `index` is past the end.
#[no_mangle]
pub extern "C" fn pickle_list_insert(
    list: *mut PickleObject,
    index: usize,
    value: *mut PickleObject,
) {
    list_insert(list, index, value);
}

/// Whether `value` occurs in `list`, comparing elements structurally (the
/// `in` operator's list arm).
#[no_mangle]
pub extern "C" fn pickle_list_contains(
    list: *const PickleObject,
    value: *const PickleObject,
) -> bool {
    unsafe {
        let len = list_len(list);
        let data = list_data(list);
        let mut i = 0usize;
        while i < len {
            if crate::map::value_eq(data.add(i).read(), value) {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// Sort a list of boxed elements in place, ascending. `kind` picks the
/// comparison: `0` int/byte/char/bool payloads, `1` float, `2` string.
#[no_mangle]
pub extern "C" fn pickle_list_sort(list: *mut PickleObject, kind: i64) {
    list_sort(list, kind);
}

/// Build a `List<int>` from an arithmetic sequence `[start, end)` with the
/// given step. A zero step yields an empty list. Bounds are inclusive of
/// `start`, exclusive of `end`.
#[no_mangle]
pub extern "C" fn pickle_range(start: i64, end: i64, step: i64) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let list = list_new(0, gc);
    let _lease = gc.temp_lease(list);
    if step == 0 {
        return list;
    }
    let mut v = start;
    if step > 0 {
        while v < end {
            let elem = crate::boxscalar::pickle_box_i64(v);
            list_push(list, elem);
            v += step;
        }
    } else {
        while v > end {
            let elem = crate::boxscalar::pickle_box_i64(v);
            list_push(list, elem);
            v += step;
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn push_grow_and_get() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let l = list_new(0, gc);
        let _lease = gc.temp_lease(l);
        assert_eq!(list_len_of(l), 0);
        let mut ids: Vec<usize> = Vec::new();
        for i in 0..100usize {
            let s = crate::strings::int64_to_string(i as i64);
            ids.push(s as usize);
            list_push(l, s);
        }
        assert_eq!(list_len_of(l), 100);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(list_get(l, i) as usize, *id);
        }
        assert!(list_get(l, 100).is_null());
        list_pop(l);
        assert_eq!(list_len_of(l), 99);
    }

    #[test]
    fn grow_capacity() {
        let _guard = setup();
        let mut gc = Gc::new();
        let l = list_new(8, &mut gc);
        assert!(list_cap(l) >= 8);
        for i in 0..200usize {
            let s = crate::strings::int64_to_string(i as i64);
            list_push(l, s);
        }
        assert!(list_cap(l) >= 200);
    }

    #[test]
    fn abi_roundtrip_boxed_ints() {
        let _guard = setup();
        crate::pickle_runtime_init();
        let l = crate::list::pickle_list_new(0);
        assert_eq!(crate::list::pickle_list_len(l), 0);
        for v in [3i64, -7, 1000, 0] {
            crate::list::pickle_list_push(l, crate::boxscalar::pickle_box_i64(v));
        }
        assert_eq!(crate::list::pickle_list_len(l), 4);
        for (i, v) in [3, -7, 1000, 0].iter().enumerate() {
            let e = crate::list::pickle_list_get(l, i);
            assert!(!e.is_null());
            assert_eq!(crate::boxscalar::pickle_unbox_i64(e), *v);
        }
        assert!(crate::list::pickle_list_get(l, 99).is_null());
        crate::list::pickle_list_set(l, 0, crate::boxscalar::pickle_box_i64(42));
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 0)),
            42
        );
        let popped = crate::list::pickle_list_pop(l);
        assert_eq!(crate::boxscalar::pickle_unbox_i64(popped), 0);
        assert_eq!(crate::list::pickle_list_len(l), 3);
    }

    #[test]
    fn remove_shift_and_insert() {
        let _guard = setup();
        crate::pickle_runtime_init();
        let l = crate::list::pickle_list_new(0);
        for v in [10i64, 20, 30, 40] {
            crate::list::pickle_list_push(l, crate::boxscalar::pickle_box_i64(v));
        }
        let removed = crate::list::pickle_list_remove(l, 1);
        assert_eq!(crate::boxscalar::pickle_unbox_i64(removed), 20);
        assert_eq!(crate::list::pickle_list_len(l), 3);
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 1)),
            30
        );
        crate::list::pickle_list_insert(l, 1, crate::boxscalar::pickle_box_i64(20));
        assert_eq!(crate::list::pickle_list_len(l), 4);
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 1)),
            20
        );
        crate::list::pickle_list_insert(l, 0, crate::boxscalar::pickle_box_i64(0));
        crate::list::pickle_list_insert(l, 5, crate::boxscalar::pickle_box_i64(99));
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 0)),
            0
        );
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 5)),
            99
        );
    }

    #[test]
    fn sort_boxed_scalars_and_strings() {
        let _guard = setup();
        crate::pickle_runtime_init();
        let l = crate::list::pickle_list_new(0);
        for v in [3i64, 1, 2] {
            crate::list::pickle_list_push(l, crate::boxscalar::pickle_box_i64(v));
        }
        crate::list::pickle_list_sort(l, 0);
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 0)),
            1
        );
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::list::pickle_list_get(l, 2)),
            3
        );
        let s = crate::list::pickle_list_new(0);
        for t in ["pear", "apple", "mango"] {
            crate::list::pickle_list_push(
                s,
                crate::strings::string_from_bytes(t.as_ptr(), t.len(), crate::gc::gc_mut()),
            );
        }
        crate::list::pickle_list_sort(s, 2);
        assert_eq!(
            crate::strings::string_bytes_len(crate::list::pickle_list_get(s, 0)),
            5
        );
        assert_eq!(
            crate::strings::pickle_str_cmp(
                crate::list::pickle_list_get(s, 0),
                crate::strings::string_from_bytes(b"apple".as_ptr(), 5, crate::gc::gc_mut())
            ),
            0
        );
    }

    #[test]
    fn contains_compares_elements_structurally() {
        // `in` on a list is structural membership, not pointer identity: the
        // needle is a distinct box from every element, and a nested list must
        // match another nested list with the same contents.
        let _guard = setup();
        crate::pickle_runtime_init();
        let l = crate::list::pickle_list_new(0);
        for v in [3i64, -7, 1000] {
            crate::list::pickle_list_push(l, crate::boxscalar::pickle_box_i64(v));
        }
        assert!(pickle_list_contains(l, crate::boxscalar::pickle_box_i64(-7)));
        assert!(pickle_list_contains(l, crate::boxscalar::pickle_box_i64(1000)));
        assert!(!pickle_list_contains(l, crate::boxscalar::pickle_box_i64(42)));

        let inner = crate::list::pickle_list_new(0);
        crate::list::pickle_list_push(inner, crate::boxscalar::pickle_box_i64(1));
        crate::list::pickle_list_push(inner, crate::boxscalar::pickle_box_i64(2));
        crate::list::pickle_list_push(l, inner);

        let same = crate::list::pickle_list_new(0);
        crate::list::pickle_list_push(same, crate::boxscalar::pickle_box_i64(1));
        crate::list::pickle_list_push(same, crate::boxscalar::pickle_box_i64(2));
        assert!(pickle_list_contains(l, same));

        let diff = crate::list::pickle_list_new(0);
        crate::list::pickle_list_push(diff, crate::boxscalar::pickle_box_i64(1));
        crate::list::pickle_list_push(diff, crate::boxscalar::pickle_box_i64(9));
        assert!(!pickle_list_contains(l, diff));
    }
}
