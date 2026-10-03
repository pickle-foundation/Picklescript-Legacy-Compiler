//! User class (object) support.
//!
//! Instances are plain objects whose payload is a dense array of 8-byte slots,
//! one per instance field, at `HEADER + 8*i`. Every slot holds a managed
//! pointer (boxed scalars or object references) exactly like a list element,
//! so class descriptors mark *all* `slot_count` slots as managed. The
//! descriptor for each user class is registered once at program start by
//! `pickle_class_register` (emitted by the compiler ahead of `main`), which
//! copies the class name and the managed-mask words into stable memory.

use crate::gc::Gc;
use crate::object::pickle_header_size;
use crate::object::{
    builtin_nop_finalizer, ClassDescriptor, PickleObject, PICKLE_CLASS_FLAG_FINALIZER,
    PICKLE_CLASS_USER_BASE,
};
use alloc::boxed::Box;

/// Total byte size of a class instance with `field_count` managed slots.
pub fn class_object_size(field_count: usize) -> usize {
    pickle_header_size() + 8 * field_count
}

/// Create an object of user class `class_id` holding `field_count` null field
/// slots. Slots are zeroed so uninitialised fields are never traced.
pub fn class_new(class_id: u64, field_count: usize, gc: &mut Gc) -> *mut PickleObject {
    let size = class_object_size(field_count);
    let obj = gc.alloc(size as u32, class_id as u32);
    unsafe {
        if field_count > 0 {
            core::ptr::write_bytes((*obj).payload_mut(), 0, 8 * field_count);
        }
    }
    obj
}

/// Read field slot `index`; null when out of bounds (the slot must hold a
/// managed pointer, so an in-range zeroed slot reads as null).
pub fn class_slot(obj: *const PickleObject, index: i64) -> *mut PickleObject {
    unsafe {
        if obj.is_null() || index < 0 {
            return core::ptr::null_mut();
        }
        let idx = index as u64;
        let size = (*obj).size as u64;
        if size < (pickle_header_size() as u64 + 8) {
            return core::ptr::null_mut();
        }
        let slots = (size - pickle_header_size() as u64 - 8) / 8;
        if idx > slots {
            return core::ptr::null_mut();
        }
        (*obj).slot(idx as usize)
    }
}

/// Write field slot `index`; silently ignored when out of bounds (the
/// compiler emits only statically-known indices within the field count).
pub fn class_set_slot(obj: *mut PickleObject, index: i64, value: *mut PickleObject) {
    unsafe {
        if obj.is_null() || index < 0 {
            return;
        }
        let idx = index as u64;
        let size = (*obj).size as u64;
        if size < (pickle_header_size() as u64 + 8) {
            return;
        }
        let slots = (size - pickle_header_size() as u64 - 8) / 8;
        if idx > slots {
            return;
        }
        (*obj).set_slot(idx as usize, value);
    }
}

// ---- Exported ABI ----------------------------------------------------------
// Thin `extern "C"` wrappers so compiled programs can allocate class instances
// and read/write their fields. Values must be boxed first
// (`boxscalar::pickle_box_*`), matching list-element rules.

/// Register one user-class descriptor (copies the name and mask into stable
/// heap memory) and return its assigned class id. `finalizer` is the raw
/// address of the compiled `pkl_<T>_deinit` function, or 0 when the class has
/// no `deinit` block. `parent` is the superclass id, or 0 when the class has no
/// superclass (builtins are never a superclass of a user class). `owned_mask`
/// marks payload slots holding `#[manualAlloc]` fields, freed recursively with
/// the object.
#[no_mangle]
pub extern "C" fn pickle_class_register(
    name_ptr: *const u8,
    name_len: usize,
    slot_count: usize,
    mask: u64,
    owned_mask: u64,
    finalizer: usize,
    parent: u32,
) -> u32 {
    class_register(
        name_ptr, name_len, slot_count, mask, owned_mask, finalizer, parent,
    )
}

/// Name of a registered class, as a managed `string`, or null when `class_id` has
/// no descriptor yet.
///
/// Added for the `pickle_print_obj` port (`pickle/runtime/abi_console.pkl`).
/// That body needs the user-class fallback arm, which asks the collector for a
/// class name; without a symbol to call, the arm had nothing to invoke. The
/// `nostd` build already degraded this arm to `<object>`, so behaviour there is
/// unchanged - this makes the name available to both builds.
///
/// The returned string is freshly allocated, so the caller owns it and the GC
    /// may collect it like any other managed value.
    ///
    /// An unregistered id yields the literal `<object>` rather than null. That
    /// folds `print_obj_raw`'s two-arm user-class fallback (name, else
    /// `<object>`) into the symbol, so callers get the finished text and need no
    /// null test. It also keeps the result a plain non-optional `string`: a
    /// nullable `string` from this builtin cannot be consumed by `if (let
    /// some(..) = ..)` in the port backend today (the AOT COFF stage fails
    /// silently on an `Option<String>` produced by a `flatBuiltin` call).
    #[no_mangle]
    pub extern "C" fn pickle_class_name(class_id: u32) -> *mut PickleObject {
        let gc = crate::gc::gc_mut();
        let text = match gc.class_name(class_id) {
            Some(bytes) => bytes,
            None => &b"<object>"[..],
        };
        crate::strings::string_from_bytes(text.as_ptr(), text.len(), gc)
    }

fn class_register(
    name_ptr: *const u8,
    name_len: usize,
    slot_count: usize,
    mask: u64,
    owned_mask: u64,
    finalizer: usize,
    parent: u32,
) -> u32 {
    let name: Box<[u8]> = if name_ptr.is_null() || name_len == 0 {
        Box::default()
    } else {
        // SAFETY: the caller (compiler-emitted code) passes a module string's
        // bytes with a matching length.
        unsafe { core::slice::from_raw_parts(name_ptr, name_len) }
            .to_vec()
            .into_boxed_slice()
    };
    let nwords = slot_count.div_ceil(32);
    let mut words = vec![0u32; nwords];
    for i in 0..slot_count {
        // The compiler emits a *saturating* managed mask: every slot of a user
        // class holds a managed pointer, and a class with 64 or more fields sets
        // all 64 bits the ABI can carry (see `inject_class_registrations`). So an
        // all-ones mask means "every slot is managed", and the slots past 63 --
        // which the descriptor still allocates mask words for -- must be traced
        // too. Reading the mask as 64 literal bits would leave the tail of a
        // wide class (the self-hosted `Emitter` has 86 fields) untraced, and the
        // collector would free objects that are still reachable from a field.
        let managed = if mask == u64::MAX {
            true
        } else {
            i < 64 && mask & (1u64 << i) != 0
        };
        if managed {
            words[i / 32] |= 1 << (i % 32);
        }
    }
    // Leaked deliberately: descriptors outlive every collection and are never
    // freed; the volumes are tiny (one copy per user class).
    let name_copy = Box::leak(name);
    let mask_copy = Box::leak(words.into_boxed_slice());
    let (flags, finalizer): (u32, extern "C" fn(*mut PickleObject)) = if finalizer == 0 {
        (0, builtin_nop_finalizer)
    } else {
        // SAFETY: the compiler emits the address of an `extern "C"` function
        // with the `fn(*mut PickleObject)` shape; non-zero means a `deinit`
        // block exists for this class.
        let f: extern "C" fn(*mut PickleObject) = unsafe { core::mem::transmute(finalizer) };
        (PICKLE_CLASS_FLAG_FINALIZER, f)
    };
    let d = ClassDescriptor {
        name_ptr: name_copy.as_ptr(),
        name_len: name_len as u32,
        flags,
        slot_count: slot_count as u32,
        mask_words: mask_copy.len() as u32,
        managed_mask: mask_copy.as_ptr(),
        finalizer,
    };
    let g = crate::gc::gc_mut();
    let id = g.register_class(d);
    g.set_class_parent(id, parent);
    g.set_class_owned_mask(id, owned_mask);
    id
}

/// True when `obj` is an instance of `target` or of any class that inherits
/// (directly or transitively) from `target`. Uses the registry's parent links,
/// which the compiler emits at class registration.
pub fn class_is(obj: *const PickleObject, target: u32) -> bool {
    if obj.is_null() {
        return false;
    }
    // SAFETY: a live managed object always has a readable header.
    let mut cid = unsafe { (*obj).class_id };
    loop {
        if cid == target {
            return true;
        }
        let parent = crate::gc::gc_mut().class_parent(cid);
        if parent == 0 {
            return false;
        }
        cid = parent;
    }
}

/// Allocate an instance of user `class_id` with `slot_count` null field slots.
#[no_mangle]
pub extern "C" fn pickle_class_new(class_id: i64, slot_count: i64) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let n = if slot_count < 0 {
        0
    } else {
        slot_count as usize
    };
    let id = if class_id < PICKLE_CLASS_USER_BASE as i64 {
        PICKLE_CLASS_USER_BASE as u64
    } else {
        class_id as u64
    };
    class_new(id, n, gc)
}

/// A field slot (managed pointer); null when out of bounds.
#[no_mangle]
pub extern "C" fn pickle_obj_slot_get(obj: *const PickleObject, slot: i64) -> *mut PickleObject {
    class_slot(obj, slot)
}

/// A zero-argument Pickle closure body: the hoisted `fn` the compiler pointed a
/// closure object at, called with the closure object as its hidden environment
/// argument. This is the `CallInd` shape `fn_value_call` emits.
type Closure0Fn = unsafe extern "C" fn(*mut PickleObject);

/// Call a zero-argument closure object. Used by `task` bodies and by the `once`
/// initialiser.
///
/// This lives here rather than in `threads` because `threads` is gated out of
/// the freestanding build while `once` is not, and reading a closure's entry
/// point is a property of the object layout.
pub(crate) fn call_closure0(closure: *mut PickleObject) {
    if closure.is_null() {
        return;
    }
    // Slot 0 of a closure object is the boxed i64 address of its hoisted body
    // (see `closure_obj` in the emitter).
    let addr = crate::boxscalar::box_bits(pickle_obj_slot_get(closure, 0));
    let entry: Closure0Fn = unsafe { core::mem::transmute(addr as usize) };
    unsafe { entry(closure) };
}

/// Write a field slot.
#[no_mangle]
pub extern "C" fn pickle_obj_slot_set(obj: *mut PickleObject, slot: i64, value: *mut PickleObject) {
    class_set_slot(obj, slot, value)
}

/// Runtime type test for `x is T` across an inheritance hierarchy: true when
/// `obj` is an instance of user class `class_id` or a descendant of it.
#[no_mangle]
pub extern "C" fn pickle_class_is(obj: *const PickleObject, class_id: i64) -> bool {
    if class_id < 0 {
        return false;
    }
    class_is(obj, class_id as u32)
}

/// Runtime downcast for `x as T`: returns `obj` when it is an instance of
/// `class_id` (or a descendant), otherwise aborts with a panic.
#[no_mangle]
pub extern "C" fn pickle_class_cast(obj: *mut PickleObject, class_id: i64) -> *mut PickleObject {
    if class_id >= 0 && class_is(obj, class_id as u32) {
        return obj;
    }
    crate::panic::pickle_panic_cstr(b"pickle: invalid class cast\0".as_ptr())
}

/// True when `obj`'s class (or any ancestor) is registered as implementing the
/// interface `iface_id` — the runtime test for `x is I`.
pub fn class_implements(obj: *const PickleObject, iface_id: u32) -> bool {
    if obj.is_null() || iface_id == 0 {
        return false;
    }
    let mut cid = unsafe { (*obj).class_id };
    loop {
        let g = crate::gc::gc_mut();
        if g.class_iface_has(cid as usize, iface_id) {
            return true;
        }
        let parent = g.class_parent(cid);
        if parent == 0 {
            return false;
        }
        cid = parent;
    }
}

/// Implementation fn ptr of interface `iface_id`, method `method_index` for
/// `obj`'s class. Walks the superclass chain so inherited interfaces resolve
/// to their nearest implementation. Returns 0 when the class does not
/// implement the interface.
pub fn iface_method(obj: *const PickleObject, iface_id: u32, method_index: u32) -> usize {
    if obj.is_null() || iface_id == 0 {
        return 0;
    }
    let mut cid = unsafe { (*obj).class_id };
    loop {
        let g = crate::gc::gc_mut();
        if let Some(fp) = g.class_iface_method(cid as usize, iface_id, method_index) {
            return fp;
        }
        let parent = g.class_parent(cid);
        if parent == 0 {
            return 0;
        }
        cid = parent;
    }
}

/// Mark `class_id` as implementing the interface `iface_id`, so `is`/`as`
/// checks and dispatch tables see it (an interface with no methods still needs
/// the bucket so `x is I` holds). Emitted by the compiler for every declared
/// `implements`.
#[no_mangle]
pub extern "C" fn pickle_class_add_interface(class_id: i64, iface_id: i64) {
    if class_id < 0 || iface_id < 0 {
        return;
    }
    crate::gc::gc_mut().class_ensure_iface(class_id as usize, iface_id as u32);
}

/// Record that `class_id` implements method `method_index` (its position
/// among the interface's methods) of interface `iface_id` with the function at
/// `fn_ptr`. Emitted by the compiler for each interface method the class
/// implements.
#[no_mangle]
pub extern "C" fn pickle_class_add_iface_method(
    class_id: i64,
    iface_id: i64,
    method_index: i64,
    fn_ptr: usize,
) {
    if class_id < 0 || iface_id < 0 || method_index < 0 {
        return;
    }
    crate::gc::gc_mut().class_add_iface_method(
        class_id as usize,
        iface_id as u32,
        method_index as u32,
        fn_ptr,
    );
}

/// Runtime interface test for `x is I`: true when `obj` implements `iface_id`.
#[no_mangle]
pub extern "C" fn pickle_class_implements(obj: *const PickleObject, iface_id: i64) -> bool {
    if iface_id < 0 {
        return false;
    }
    class_implements(obj, iface_id as u32)
}

/// Resolve the implementation of interface `iface_id`, method `method_index`
/// for `obj`; 0 when the class does not implement the interface.
#[no_mangle]
pub extern "C" fn pickle_iface_method(
    obj: *const PickleObject,
    iface_id: i64,
    method_index: i64,
) -> usize {
    if iface_id < 0 || method_index < 0 {
        return 0;
    }
    iface_method(obj, iface_id as u32, method_index as u32)
}

/// Runtime downcast for `x as I`: returns `obj` when its class implements
/// interface `iface_id`, otherwise aborts with a panic.
#[no_mangle]
pub extern "C" fn pickle_iface_cast(obj: *mut PickleObject, iface_id: i64) -> *mut PickleObject {
    if iface_id >= 0 && class_implements(obj, iface_id as u32) {
        return obj;
    }
    crate::panic::pickle_panic_cstr(b"pickle: invalid interface cast\0".as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn new_zeroes_slots() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        unsafe {
            let o = class_new(7, 3, gc);
            for i in 0..3 {
                assert!(class_slot(o, i).is_null());
            }
            assert_eq!((*o).class_id, 7);
            assert_eq!((*o).size, (pickle_header_size() + 24) as u32);
        }
    }

    #[test]
    fn get_set_roundtrip() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let o = class_new(7, 2, gc);
        let a = crate::boxscalar::pickle_box_i64(10);
        let b = crate::boxscalar::pickle_box_i64(20);
        class_set_slot(o, 0, a);
        class_set_slot(o, 1, b);
        assert_eq!(crate::boxscalar::box_bits(class_slot(o, 0)), 10);
        assert_eq!(crate::boxscalar::box_bits(class_slot(o, 1)), 20);
        assert!(class_slot(o, 2).is_null(), "out-of-range reads null");
        assert!(class_slot(o, -1).is_null(), "negative indices read null");
        assert!(class_slot(o, 1_000_000).is_null());
    }

    #[test]
    fn register_copies_name_and_mask() {
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        let name = b"Hero".to_vec();
        let id = pickle_class_register(name.as_ptr(), name.len(), 4, 0b1111, 0, 0, 0);
        assert!(
            id >= PICKLE_CLASS_USER_BASE,
            "user classes come after the builtins"
        );
        let g = crate::gc::gc_mut();
        assert_eq!(g.class_name(id).map(|b| b.to_vec()), Some(b"Hero".to_vec()));
        let d = g.descriptors.get(id).unwrap();
        assert_eq!(d.slot_count, 4);
        assert_eq!(d.mask_words, 1);
        assert_eq!(unsafe { *d.managed_mask }, 0b1111);
    }

    #[test]
    fn gc_marks_through_field_slots() {
        // Every field slot is a managed pointer, so a descriptor with an
        // all-ones mask must keep reachable fields alive across a trace.
        let _guard = crate::gc::test_begin();
        use crate::heap::Heap;
        use crate::object::DescriptorTable;
        let heap = Heap::new();
        // Dummies for the reserved builtin range, then the class descriptor at
        // `PICKLE_CLASS_USER_BASE`, the id a class-new object carries.
        let mut desc = DescriptorTable::new();
        let dummy = ClassDescriptor {
            name_ptr: b"x\0".as_ptr(),
            name_len: 1,
            flags: 0,
            slot_count: 0,
            mask_words: 0,
            managed_mask: core::ptr::null(),
            finalizer: builtin_nop_finalizer,
        };
        for _ in 0..(PICKLE_CLASS_USER_BASE) {
            desc.register(dummy);
        }
        desc.register(ClassDescriptor {
            name_ptr: b"Hero\0".as_ptr(),
            name_len: 4,
            flags: 0,
            slot_count: 1,
            mask_words: 1,
            managed_mask: &[1u32] as *const u32,
            finalizer: builtin_nop_finalizer,
        });
        let gc = crate::gc::gc_mut();
        unsafe {
            let payload = crate::boxscalar::pickle_box_i64(99);
            let o = class_new(PICKLE_CLASS_USER_BASE.into(), 1, gc);
            class_set_slot(o, 0, payload);
            assert!(!(*o).is_marked());
            assert!(!(*payload).is_marked());
            let root: *mut PickleObject = o;
            let roots: Vec<*mut *mut PickleObject> =
                vec![(&root as *const *mut PickleObject) as *mut _];
            crate::trace::trace_from_roots(&desc, &roots, &[], &[]);
            assert!((*o).is_marked());
            assert!((*payload).is_marked(), "field slot must be traced");
        }
        let _ = heap;
        let _ = gc;
    }

    /// A class wider than the 64 bits of managed mask the ABI carries.
    ///
    /// The compiler emits a saturating mask -- all ones meaning "every slot" --
    /// and `class_register` has to spread that over the extra mask words, or the
    /// collector frees whatever is reachable only from a field past 63. The
    /// self-hosted `Emitter` is the class that actually has this shape, with 86
    /// fields, so the case is reached by ordinary compiler use rather than only
    /// by this test.
    #[test]
    fn gc_marks_through_slots_past_the_mask_width() {
        const SLOTS: usize = 86;
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        // Registered through the compiler's own entry point, so the mask words
        // are the ones `class_register` derives from the saturating mask.
        let id = pickle_class_register(b"Wide".as_ptr(), 4, SLOTS, u64::MAX, 0, 0, 0);
        let gc = crate::gc::gc_mut();
        let d = gc.descriptors.get(id).unwrap();
        assert_eq!(d.slot_count, SLOTS as u32);
        assert_eq!(d.mask_words, 3, "86 fields need three 32-bit mask words");
        {
            let words =
                unsafe { std::slice::from_raw_parts(d.managed_mask, d.mask_words as usize) };
            // Only the bits that exist: 64 in the first two words, and 86 - 64 in
            // the last.
            assert_eq!(words, &[u32::MAX, u32::MAX, (1u32 << (SLOTS - 64)) - 1]);
        }
        unsafe {
            let o = class_new(id.into(), SLOTS, gc);
            // A payload in the last slot, which is 22 fields past the 64 bits the
            // mask argument can express.
            let tail = crate::boxscalar::pickle_box_i64(86);
            class_set_slot(o, (SLOTS - 1) as i64, tail);
            assert!(!(*tail).is_marked());
            let root: *mut PickleObject = o;
            let roots: Vec<*mut *mut PickleObject> =
                vec![(&root as *const *mut PickleObject) as *mut _];
            crate::trace::trace_from_roots(&gc.descriptors, &roots, &[], &[]);
            assert!((*o).is_marked());
            assert!(
                (*tail).is_marked(),
                "a field past the mask width must still be traced"
            );
        }
    }

    #[test]
    fn class_is_walks_parent_chain() {
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        let animal = pickle_class_register(b"Animal".as_ptr(), 6, 1, 0b1, 0, 0, 0);
        let dog = pickle_class_register(b"Dog".as_ptr(), 3, 1, 0b1, 0, 0, animal);
        let gc = crate::gc::gc_mut();
        let d = class_new(dog as u64, 1, gc);
        let a = class_new(animal as u64, 1, gc);
        assert!(class_is(d, dog), "an instance is its own class");
        assert!(class_is(d, animal), "a subclass instance is-an ancestor");
        assert!(
            !class_is(a, dog),
            "a superclass instance is not the subclass"
        );
        assert!(!class_is(a, 0), "id 0 is never a user ancestor");
        assert!(pickle_class_is(d, dog as i64));
        assert!(!pickle_class_is(a, dog as i64));
    }

    #[test]
    fn interface_buckets_lookup_and_parent_walk() {
        let _guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        let animal = pickle_class_register(b"Animal".as_ptr(), 6, 1, 0b1, 0, 0, 0);
        let dog = pickle_class_register(b"Dog".as_ptr(), 3, 1, 0b1, 0, 0, animal);
        // `animal` implements iface 1 with two methods; `dog` adds one override.
        pickle_class_add_interface(animal as i64, 1);
        pickle_class_add_interface(animal as i64, 2);
        pickle_class_add_iface_method(animal as i64, 1, 0, 0x1111);
        pickle_class_add_iface_method(animal as i64, 1, 1, 0x2222);
        pickle_class_add_iface_method(dog as i64, 1, 0, 0x3333);
        let gc = crate::gc::gc_mut();
        let d = class_new(dog as u64, 1, gc);
        let a = class_new(animal as u64, 1, gc);
        // `is` checks: a dog is-an animal so it implements animal's interfaces.
        assert!(class_implements(d, 1));
        assert!(class_implements(d, 2));
        assert!(class_implements(a, 1));
        assert!(!class_implements(d, 3), "unregistered interface is absent");
        assert!(!class_implements(core::ptr::null(), 1));
        // Resolution walks up: dog overrides method 0, inherits method 1.
        assert_eq!(iface_method(d, 1, 0), 0x3333);
        assert_eq!(iface_method(d, 1, 1), 0x2222);
        assert_eq!(iface_method(a, 1, 0), 0x1111);
        assert_eq!(iface_method(a, 2, 0), 0, "empty interface has no methods");
        assert_eq!(iface_method(d, 3, 0), 0, "unknown interface resolves 0");
        // Exported wrappers.
        assert!(pickle_class_implements(d, 1));
        assert!(!pickle_class_implements(a, 4));
        assert_eq!(pickle_iface_method(d, 1, 1), 0x2222);
        assert_eq!(
            pickle_iface_method(d, 1, 9),
            0,
            "missing method index resolves 0"
        );
        assert_eq!(pickle_class_cast(d, animal as i64), d);
        assert_eq!(pickle_iface_cast(d, 2), d);
    }
}
