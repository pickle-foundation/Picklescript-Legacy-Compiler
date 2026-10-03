//! Class static-field storage.
//!
//! Every static field of a user class gets one stable cell
//! (`*mut *mut PickleObject`) indexed by `(class_id, static_slot)`. Cells are
//! allocated lazily on first access and registered as GC roots, so objects
//! referenced by static fields stay alive across collections. Values follow the
//! same representation rules as class instance slots and list elements: every
//! cell holds a managed pointer (boxed scalars, strings, class instances…).

#[cfg(feature = "nostd")]
use crate::nostd::sync::Mutex;
#[cfg(not(feature = "nostd"))]
use std::sync::Mutex;

use crate::gc::pickle_gc_root_add;
use crate::object::PickleObject;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// `(class_id, slot) -> cell` as a raw pointer stored as `usize` bits (0 means
/// "no cell yet"). `usize` keeps the table `Send`; the pointers themselves are
/// only ever dereferenced on the runtime thread.
static CELLS: Mutex<Vec<Vec<usize>>> = Mutex::new(Vec::new());

/// Look up (allocating on first use) the cell for `(class_id, slot)`, returning
/// its address as `usize`.
fn cell_bits(class_id: u32, slot: u32) -> usize {
    let mut cells = CELLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cid = class_id as usize;
    let s = slot as usize;
    if cells.len() <= cid {
        cells.resize_with(cid + 1, Vec::new);
    }
    if cells[cid].len() <= s {
        cells[cid].resize(s + 1, 0);
    }
    let mut bits = cells[cid][s];
    if bits == 0 {
        let fresh: *mut *mut PickleObject = Box::into_raw(Box::new(core::ptr::null_mut()));
        // Register the cell as a static root so its pointee is traced at every
        // collection. The handle is deliberately leaked: the cell lives for the
        // whole program, like a compiler-emitted global.
        pickle_gc_root_add(fresh);
        bits = fresh as usize;
        cells[cid][s] = bits;
    }
    bits
}

/// Drop every static-field cell. `pickle test` calls this between files so a
/// new module's `(class_id, slot)` statics never collide with the previous
/// module's registry: class ids restart at `PICKLE_CLASS_USER_BASE`, so the
/// old cells must not be reused. Cells are leaked (they were turned into raw
/// boxes and registered as GC roots), matching the existing cell lifecycle.
pub(crate) fn reset() {
    CELLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

/// Read the managed value held in static cell `(class_id, slot)`; null when the
/// field was never assigned.
#[no_mangle]
pub extern "C" fn pickle_static_get(class_id: u32, slot: u32) -> *mut PickleObject {
    let cell = cell_bits(class_id, slot) as *mut *mut PickleObject;
    // SAFETY: `cell_bits` returns a live heap cell, and the caller owns the
    // world lock (see `crate::world`), so no task thread is mutating it.
    unsafe { *cell }
}

/// Store `value` into static cell `(class_id, slot)`.
#[no_mangle]
pub extern "C" fn pickle_static_set(class_id: u32, slot: u32, value: *mut PickleObject) {
    let cell = cell_bits(class_id, slot) as *mut *mut PickleObject;
    // SAFETY: `cell_bits` returns a live heap cell, and the caller owns the
    // world lock (see `crate::world`), so no task thread is mutating it.
    unsafe {
        *cell = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_static_reads_null_and_roundtrips() {
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        assert!(pickle_static_get(8, 0).is_null(), "unset static is null");
        let boxed = crate::boxscalar::pickle_box_i64(42);
        pickle_static_set(8, 0, boxed);
        let got = pickle_static_get(8, 0);
        assert_eq!(crate::boxscalar::box_bits(got), 42);
    }

    #[test]
    fn static_cell_is_a_gc_root() {
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        let boxed = crate::boxscalar::pickle_box_i64(7);
        pickle_static_set(8, 1, boxed);
        // Force a collection: the boxed value must survive because the static
        // cell is registered as a root.
        crate::gc::gc_mut().collect();
        let got = pickle_static_get(8, 1);
        assert_eq!(crate::boxscalar::box_bits(got), 7);
    }
}
