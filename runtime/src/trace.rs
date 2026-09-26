//! Marking: conservative, worklist-based tracing of the reference graph.
//!
//! `trace_from_roots` marks everything reachable from the root cells and each
//! registered thread's shadow-stack slots. Builtin objects (string/list/map)
//! use their known layouts; user objects use their registered class
//! descriptor's managed-field bitmask.

use alloc::vec::Vec;

use crate::layout::{
    enum_field_count, list_data, list_len, map_cap, map_entries, tuple_field_count,
};
use crate::object::{
    DescriptorTable, PickleObject, PICKLE_CLASS_ENUM, PICKLE_CLASS_LIST, PICKLE_CLASS_MAP,
    PICKLE_CLASS_STRING, PICKLE_CLASS_TUPLE,
};
use crate::shadow::all_threads;

/// Mark `obj` (and transitively everything it references) from the given
/// roots. `roots` is typically the list of registered static cells. `manual`
/// holds the `#[manualAlloc]` objects, which are always treated as roots so
/// they survive until explicitly freed.
pub fn trace_from_roots(
    descriptors: &DescriptorTable,
    roots: &[*mut *mut PickleObject],
    manual: &[*mut PickleObject],
    temp_roots: &[*mut PickleObject],
) {
    let mut worklist: Vec<*mut PickleObject> = Vec::new();
    for cell in roots {
        let r = unsafe { **cell };
        if !r.is_null() {
            enqueue(&mut worklist, r);
        }
    }
    for obj in manual {
        if !obj.is_null() {
            enqueue(&mut worklist, *obj);
        }
    }
    for obj in temp_roots {
        if !obj.is_null() {
            enqueue(&mut worklist, *obj);
        }
    }
    // Shadow-stack slots of every registered thread. Hold the registry lock for
    // the whole walk: without it, a concurrent `register_thread`/
    // `unregister_thread`/`pickle_shadow_push` in another test (OS) thread can
    // free a node or install a frame while we iterate, and `slots_roundtrip`
    // runs in parallel with the collector on Linux CI, dereferencing garbage.
    let _guard = crate::shadow::lock_registry();
    let mut thread = all_threads();
    unsafe {
        while !thread.is_null() {
            let mut frame = (*thread).frames;
            while !frame.is_null() {
                for i in 0..(*frame).slot_count {
                    let slot = (*frame).slot(i);
                    if !slot.is_null() {
                        enqueue(&mut worklist, slot);
                    }
                }
                frame = (*frame).prev;
            }
            thread = (*thread).next;
        }
    }

    while let Some(obj) = worklist.pop() {
        trace_one(descriptors, obj, &mut worklist);
    }
}

#[inline]
fn enqueue(worklist: &mut Vec<*mut PickleObject>, obj: *mut PickleObject) {
    unsafe {
        if (*obj).is_marked() {
            return;
        }
        (*obj).set_marked();
    }
    worklist.push(obj);
}

#[inline]
fn trace_one(
    descriptors: &DescriptorTable,
    obj: *mut PickleObject,
    worklist: &mut Vec<*mut PickleObject>,
) {
    unsafe {
        let class_id = (*obj).class_id;
        match class_id {
            PICKLE_CLASS_STRING => {}
            PICKLE_CLASS_LIST => {
                let data = list_data(obj);
                let len = list_len(obj);
                if !data.is_null() {
                    for i in 0..len {
                        let e = *data.add(i);
                        if !e.is_null() {
                            enqueue(worklist, e);
                        }
                    }
                }
            }
            PICKLE_CLASS_MAP => {
                let entries = map_entries(obj);
                // Open addressing scatters live slots across the whole table,
                // so scan the full capacity (skipping the tombstone sentinel);
                // scanning only `len` slots would strand live entries.
                let cap = map_cap(obj);
                if !entries.is_null() {
                    for i in 0..cap {
                        let e = entries.add(i);
                        if !(*e).key.is_null()
                            && !core::ptr::eq((*e).key, crate::object::nil_sentinel())
                            && !core::ptr::eq((*e).key, crate::object::none_key())
                        {
                            enqueue(worklist, (*e).key);
                        }
                        if !(*e).value.is_null() {
                            enqueue(worklist, (*e).value);
                        }
                    }
                }
            }
            PICKLE_CLASS_ENUM => {
                // Payload slot 0 is the (unmanaged) tag; fields follow.
                let n = enum_field_count(obj);
                let base = (*obj).payload() as *const *mut PickleObject;
                for i in 0..n {
                    let e = base.add(1 + i).read();
                    if !e.is_null() {
                        enqueue(worklist, e);
                    }
                }
            }
            PICKLE_CLASS_TUPLE => {
                // Payload slot 0 holds the (unmanaged) element count; fields
                // follow as managed pointers.
                let n = tuple_field_count(obj);
                let base = (*obj).payload() as *const *mut PickleObject;
                for i in 0..n {
                    let e = base.add(1 + i).read();
                    if !e.is_null() {
                        enqueue(worklist, e);
                    }
                }
            }
            _ => {
                if let Some(d) = descriptors.get(class_id) {
                    let mask_words = d.mask_words as usize;
                    let slots = d.slot_count as usize;
                    let slot_ptr = (*obj).payload_mut() as *mut *mut PickleObject;
                    for w in 0..mask_words {
                        let mask = d.managed_mask.add(w).read();
                        if mask == 0 {
                            continue;
                        }
                        let base_slot = w * 32;
                        for bit in 0..32 {
                            if mask & (1 << bit) != 0 {
                                let idx = base_slot + bit;
                                if idx >= slots {
                                    break;
                                }
                                let e = slot_ptr.add(idx).read();
                                if !e.is_null() {
                                    enqueue(worklist, e);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{list_data, list_set_data, list_set_len};
    use crate::object::PICKLE_CLASS_LIST;

    #[test]
    fn marks_through_builtin_list() {
        // Serialise with `shadow::tests::slots_roundtrip`: this walker reads
        // every registered thread's frame slots, and that test pushes fake
        // pointers (0x11/0x22) which would be dereferenced here.
        let _guard = crate::gc::test_begin();
        use crate::heap::Heap;
        let mut heap = Heap::new();
        let desc = DescriptorTable::new();
        unsafe {
            let list = heap.alloc_object(crate::layout::LIST_PAYLOAD + 24);
            let elem = heap.alloc_object(48);
            (*list).init(PICKLE_CLASS_LIST, (crate::layout::LIST_PAYLOAD + 24) as u32);
            (*elem).init(9, 48);
            list_set_len(list, 1);
            let data = heap.raw_alloc(8);
            (data as *mut *mut PickleObject).write(elem);
            list_set_data(list, data as *mut *mut PickleObject);
            assert!(!(*elem).is_marked());

            let root: *mut PickleObject = list;
            let roots: Vec<*mut *mut PickleObject> =
                vec![(&root as *const *mut PickleObject) as *mut _];
            trace_from_roots(&desc, &roots, &[], &[]);

            assert!((*list).is_marked());
            assert!((*elem).is_marked());
            assert_eq!(list_data(list) as usize, data as usize);
        }
        heap.destroy();
    }
}
