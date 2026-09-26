//! Garbage collector orchestration: the process-wide heap + descriptor
//! registry + static-root table, and the exported `pickle_gc_*` ABI.
//!
//! v1 collector: non-moving, stop-the-world style mark-and-sweep. Runs on
//! demand (`pickle_gc_collect`) and automatically when allocation passes a
//! threshold.

use crate::heap::{raw_free, Heap};
use crate::layout::{list_data, map_entries};
use crate::object::{
    DescriptorTable, PickleObject, RootCell, PICKLE_CLASS_FLAG_FINALIZER, PICKLE_CLASS_LIST,
    PICKLE_CLASS_MAP, PICKLE_CLASS_STRING, PICKLE_FLAG_MANUAL,
};
use crate::shadow;
use crate::trace;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};

/// `PKL_NO_AUTO_GC=1` suppresses automatic collections. It is read once, not
/// per allocation: on Windows an environment probe costs ~190us, which would
/// dwarf the allocation itself on any box/string/list churn workload.
static NO_AUTO_GC: AtomicBool = AtomicBool::new(false);
static NO_AUTO_GC_INIT: Once = Once::new();

fn no_auto_gc_once() -> bool {
    NO_AUTO_GC_INIT.call_once(|| {
        NO_AUTO_GC.store(
            std::env::var_os("PKL_NO_AUTO_GC").is_some(),
            Ordering::Relaxed,
        );
    });
    NO_AUTO_GC.load(Ordering::Relaxed)
}

/// Static root cells (compiler-emitted global variables and class statics).
static STATIC_ROOTS: Mutex<Vec<&'static mut RootCell>> = Mutex::new(Vec::new());

/// Guards against a re-entrant collection on one thread (a finalizer or
/// allocation helper that runs inside a sweep). The stop-the-world guarantee
/// itself comes from the [`crate::world`] lock, which the collector holds for
/// the whole cycle.
static COLLECT_LOCK: Mutex<()> = Mutex::new(());

/// Set while a GC is in progress so a re-entrant allocation can panic.
static COLLECTING: AtomicBool = AtomicBool::new(false);

/// Allocation counter since the last collection.
static ALLOC_SINCE_GC: AtomicU32 = AtomicU32::new(0);

/// Number of object-bytes allocated since the last collection.
static BYTES_SINCE_GC: AtomicU32 = AtomicU32::new(0);

/// Default threshold in object-bytes that triggers a collection.
pub const DEFAULT_AUTO_COLLECT_THRESHOLD: u32 = 8 * 1024 * 1024;

/// Current auto-collect threshold, settable at runtime.
static AUTO_THRESHOLD: AtomicU32 = AtomicU32::new(DEFAULT_AUTO_COLLECT_THRESHOLD);

/// Total completed collection cycles (observability for tests).
static COLLECTIONS: AtomicU32 = AtomicU32::new(0);

/// The runtime heap and class registry.
pub struct Gc {
    pub heap: Heap,
    pub descriptors: DescriptorTable,
    /// Superclass of each registered class, indexed by class id (0 = none).
    /// Builtins are never a superclass of a user class, so 0 doubles as the
    /// "no parent" sentinel; the compiler passes the parent id on registration.
    class_parents: Vec<u32>,
    /// Total live object bytes (updated at sweep).
    pub live_bytes: AtomicU32,
    /// Objects owned by `#[manualAlloc]` bindings. Always traced as roots and
    /// never swept; released only by `pickle_manual_free`.
    manual_objects: Vec<*mut PickleObject>,
    /// Objects that a native runtime helper is still building. Traced as roots
    /// (so nested collections mid-helper cannot sweep them) until the helper
    /// registers the finished value somewhere visible via `temp_unroot`.
    temp_roots: Vec<*mut PickleObject>,
    /// Owned (`#[manualAlloc]`) field mask per registered class id: bit `i`
    /// marks payload slot `i` as an owned object freed recursively with its
    /// holder.
    class_owned_mask: Vec<u64>,
    /// Interface dispatch tables per registered class id: for each interface
    /// id the class implements, the method implementations `(method_index,
    /// fn_ptr)`. Lookup walks the superclass chain, so a subclass inherits its
    /// ancestors' interfaces (and their implementations) without copying.
    #[allow(clippy::type_complexity)]
    class_iface_buckets: Vec<Vec<(u32, Vec<(u32, usize)>)>>,
}

impl Gc {
    pub fn new() -> Gc {
        Gc {
            heap: Heap::new(),
            descriptors: DescriptorTable::new(),
            class_parents: Vec::new(),
            live_bytes: AtomicU32::new(0),
            manual_objects: Vec::new(),
            temp_roots: Vec::new(),
            class_owned_mask: Vec::new(),
            class_iface_buckets: Vec::new(),
        }
    }

    /// Allocate an object of `size` bytes (>= header, multiple of 8),
    /// initialising its header. Increments the allocation counters.
    pub fn alloc(&mut self, size: u32, class_id: u32) -> *mut PickleObject {
        let size = crate::heap::align8(size as usize)
            .max(crate::heap::MIN_OBJECT)
            .min(u32::MAX as usize) as u32;
        let obj = self.heap.alloc_object(size as usize);
        unsafe {
            (*obj).init(class_id, size);
        }
        let _ = ALLOC_SINCE_GC.fetch_add(1, Ordering::Relaxed);
        let _ = BYTES_SINCE_GC.fetch_add(size, Ordering::Relaxed);
        // The threshold is checked *after* the object exists, so pin it across
        // its own collection: it is not reachable from any root yet, and an
        // unpinned sweep would free (and, for a class with a `deinit`, run the
        // finalizer on) the object before its constructor can even store it.
        // Sweep clears the mark of every surviving object, and we clear it
        // again here in case no collection ran.
        unsafe {
            (*obj).set_marked();
        }
        self.maybe_collect();
        unsafe {
            (*obj).clear_marked();
        }
        obj
    }

    /// Class name of `class_id` (for printing/reflection), if registered.
    pub fn class_name(&self, class_id: u32) -> Option<&[u8]> {
        let d = self.descriptors.get(class_id)?;
        unsafe { Some(std::slice::from_raw_parts(d.name_ptr, d.name_len as usize)) }
    }

    /// Register a class descriptor; returns its class id (index).
    pub fn register_class(&mut self, d: crate::object::ClassDescriptor) -> u32 {
        let id = self.descriptors.register(d);
        self.class_parents.push(0);
        self.class_owned_mask.push(0);
        self.class_iface_buckets.push(Vec::new());
        id
    }

    /// Record which payload slots of `class_id` are owned (`#[manualAlloc]`)
    /// fields, freed recursively with their holder.
    pub fn set_class_owned_mask(&mut self, class_id: u32, mask: u64) {
        let idx = class_id as usize;
        if self.class_owned_mask.len() <= idx {
            self.class_owned_mask.resize(idx + 1, 0);
        }
        self.class_owned_mask[idx] = mask;
    }

    /// Owned-field mask of `class_id` (0 for builtins / no owned fields).
    pub fn class_owned_mask(&self, class_id: u32) -> u64 {
        self.class_owned_mask
            .get(class_id as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Non-null owned children of `obj`, in slot order.
    unsafe fn owned_children(&self, obj: *mut PickleObject) -> Vec<*mut PickleObject> {
        let mut out = Vec::new();
        let mask = self.class_owned_mask((*obj).class_id);
        if mask == 0 {
            return out;
        }
        let slot_ptr = (*obj).payload_mut() as *mut *mut PickleObject;
        let mut bit = 0;
        while bit < 64 {
            if mask & (1u64 << bit) != 0 {
                let child = slot_ptr.add(bit).read();
                if !child.is_null() {
                    out.push(child);
                }
            }
            bit += 1;
        }
        out
    }

    /// Recursively release every owned (`#[manualAlloc]`) field of `obj`.
    /// Children already released (or involved in an ownership cycle) are
    /// skipped: `manual_free` deregisters a block before recursing, so a back
    /// edge can never free the same block twice or loop forever.
    pub fn free_owned_fields(&mut self, obj: *mut PickleObject) {
        if obj.is_null() {
            return;
        }
        let children = unsafe { self.owned_children(obj) };
        for child in children {
            if self.manual_objects.contains(&child) {
                self.manual_free(child);
            }
        }
    }

    /// Record the superclass id of a registered class.
    pub fn set_class_parent(&mut self, class_id: u32, parent: u32) {
        let idx = class_id as usize;
        if self.class_parents.len() <= idx {
            self.class_parents.resize(idx + 1, 0);
        }
        self.class_parents[idx] = parent;
    }

    /// Superclass id of `class_id` (0 = none / builtin).
    pub fn class_parent(&self, class_id: u32) -> u32 {
        self.class_parents
            .get(class_id as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Ensure `class_id`'s interface bucket records interface `iface_id` (with
    /// an empty method table when not present yet).
    pub fn class_ensure_iface(&mut self, class_id: usize, iface_id: u32) {
        if iface_id == 0 {
            return;
        }
        if self.class_iface_buckets.len() <= class_id {
            self.class_iface_buckets.resize(class_id + 1, Vec::new());
        }
        let bucket = &mut self.class_iface_buckets[class_id];
        if !bucket.iter().any(|(i, _)| *i == iface_id) {
            bucket.push((iface_id, Vec::new()));
        }
    }

    /// Record that `class_id` implements interface `iface_id`'s method
    /// `method_index` with the function at `fn_ptr`.
    pub fn class_add_iface_method(
        &mut self,
        class_id: usize,
        iface_id: u32,
        method_index: u32,
        fn_ptr: usize,
    ) {
        self.class_ensure_iface(class_id, iface_id);
        let bucket = &mut self.class_iface_buckets[class_id];
        let methods = &mut bucket.iter_mut().find(|(i, _)| *i == iface_id).unwrap().1;
        if methods.iter().all(|(m, _)| *m != method_index) {
            methods.push((method_index, fn_ptr));
        }
    }

    /// True when `class_id` (or, per callers walking the chain, an ancestor)
    /// implements interface `iface_id`.
    pub fn class_iface_has(&self, class_id: usize, iface_id: u32) -> bool {
        if iface_id == 0 {
            return false;
        }
        self.class_iface_buckets
            .get(class_id)
            .is_some_and(|b| b.iter().any(|(i, _)| *i == iface_id))
    }

    /// Implementation fn ptr of interface `iface_id`'s method `method_index`
    /// on `class_id`, when recorded.
    pub fn class_iface_method(
        &self,
        class_id: usize,
        iface_id: u32,
        method_index: u32,
    ) -> Option<usize> {
        self.class_iface_buckets.get(class_id).and_then(|b| {
            b.iter().find(|(i, _)| *i == iface_id).and_then(|(_, m)| {
                m.iter()
                    .find(|(mi, _)| *mi == method_index)
                    .map(|(_, fp)| *fp)
            })
        })
    }

    /// Run a full mark-and-sweep cycle.
    ///
    /// The caller already owns the world lock (every path in here reaches the
    /// heap through [`gc_mut`]), and it holds it for the whole cycle: the walk
    /// reads peer threads' shadow stacks, so no other thread may be mutating
    /// while it runs. [`COLLECT_LOCK`] therefore only guards against a
    /// *re-entrant* collection on this same thread — a finalizer or a
    /// `temp_unroot` helper that allocates from inside a sweep.
    pub fn collect(&mut self) {
        // Only one collector at a time.
        let _guard = match COLLECT_LOCK.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if COLLECTING.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = COLLECTIONS.fetch_add(1, Ordering::Relaxed);

        // Snapshot static roots.
        let roots: Vec<*mut *mut crate::object::PickleObject> = {
            let cells = STATIC_ROOTS.lock().unwrap();
            cells.iter().map(|cell| cell.cell).collect()
        };
        let descriptors = &self.descriptors;
        trace::trace_from_roots(descriptors, &roots, &self.manual_objects, &self.temp_roots);

        // Sweep: release dead list/map raw buffers and pick out the objects
        // whose finalizers must run. Finalizers are *deferred*: the heap borrow
        // ends before they run so a finalizer that touches the heap cannot
        // alias the collector's `&mut Heap`.
        let heap = &mut self.heap;
        let descriptors_sweep = &self.descriptors;
        let owned_masks = &self.class_owned_mask;
        let mut live: u32 = 0;
        let deferred = heap.sweep(
            |obj| unsafe {
                let class_id = (*obj).class_id;
                match class_id {
                    PICKLE_CLASS_LIST => {
                        let data = list_data(obj);
                        if !data.is_null() {
                            raw_free(data as *mut u8);
                        }
                    }
                    PICKLE_CLASS_MAP => {
                        let entries = map_entries(obj);
                        if !entries.is_null() {
                            raw_free(entries as *mut u8);
                        }
                    }
                    _ => {}
                }
                live += (*obj).size;
            },
            |obj| unsafe {
                let class_id = (*obj).class_id;
                if matches!(
                    class_id,
                    PICKLE_CLASS_LIST | PICKLE_CLASS_MAP | PICKLE_CLASS_STRING
                ) {
                    return false;
                }
                descriptors_sweep
                    .get(class_id)
                    .is_some_and(|d| d.flags & PICKLE_CLASS_FLAG_FINALIZER != 0)
                    // Objects that own `#[manualAlloc]` fields must be deferred
                    // so those fields can be released after the heap borrow.
                    || owned_masks.get(class_id as usize).copied().unwrap_or(0) != 0
            },
        );
        self.live_bytes.store(live, Ordering::Relaxed);

        // Run finalizers now that the heap borrow has ended, then return the
        // deferred blocks to the free list. `COLLECTING` is still set, so a
        // finalizer that allocates cannot start a nested collection.
        for obj in &deferred {
            unsafe {
                let class_id = (**obj).class_id;
                if let Some(d) = self.descriptors.get(class_id) {
                    if d.flags & PICKLE_CLASS_FLAG_FINALIZER != 0 {
                        (d.finalizer)(*obj);
                    }
                }
            }
            self.free_owned_fields(*obj);
        }
        for obj in deferred {
            self.heap.free_object(obj);
        }

        ALLOC_SINCE_GC.store(0, Ordering::Relaxed);
        BYTES_SINCE_GC.store(0, Ordering::Relaxed);
        COLLECTING.store(false, Ordering::Release);
        // Adaptive auto-collect: after each cycle, raise the threshold towards
        // the surviving heap so large-live-set workloads stop thrashing. A
        // stable multi-hundred-MB working set (e.g. the self-hosted compiler)
        // were sweeping the whole arena after every 8 MiB of allocation; with
        // the threshold pinned to a multiple of the live set, collections
        // amortize instead of taking over the run time.
        let next = DEFAULT_AUTO_COLLECT_THRESHOLD.max(
            self.live_bytes
                .load(Ordering::Relaxed)
                .saturating_mul(2),
        );
        AUTO_THRESHOLD.store(next.min(1 << 30), Ordering::Relaxed);
    }

    /// Maybe collect if the allocation threshold has been crossed.
    pub fn maybe_collect(&mut self) {
        if !no_auto_gc_once()
            && BYTES_SINCE_GC.load(Ordering::Relaxed) >= AUTO_THRESHOLD.load(Ordering::Relaxed)
        {
            self.collect();
        }
    }

    /// Adopt `obj` as a programmer-owned (`#[manualAlloc]`) allocation. The
    /// object is flagged and registered so the collector treats it as a root;
    /// it is released only by `manual_free`. Returns `obj` for chaining.
    pub fn manual_adopt(&mut self, obj: *mut PickleObject) -> *mut PickleObject {
        if obj.is_null() {
            return obj;
        }
        unsafe {
            (*obj).flags |= PICKLE_FLAG_MANUAL;
        }
        if !self.manual_objects.contains(&obj) {
            self.manual_objects.push(obj);
        }
        obj
    }

    /// Temporarily root `obj` while a native helper is still building it.
    /// Unlike `manual_adopt` the flag is untouched: this is purely a liveness
    /// lease so a nested collection inside the helper cannot sweep the
    /// not-yet-published value. The helper must call `temp_unroot` before
    /// returning a value that has been published to a slot/frame/root.
    pub fn temp_root(&mut self, obj: *mut PickleObject) {
        if !obj.is_null() && !self.temp_roots.contains(&obj) {
            self.temp_roots.push(obj);
        }
    }

    /// Release the liveness lease on `obj` taken by `temp_root`.
    pub fn temp_unroot(&mut self, obj: *mut PickleObject) {
        self.temp_roots.retain(|&o| o != obj);
    }

    /// RAII lease keeping `obj` live across any nested collections triggered
    /// inside a native helper. See [`TempRootLease`].
    pub fn temp_lease(&mut self, obj: *mut PickleObject) -> TempRootLease {
        self.temp_root(obj);
        TempRootLease(obj)
    }

    /// Explicitly release a `#[manualAlloc]` object: run its finalizer, free
    /// any raw side buffers, and return the block to the heap. Panics if the
    /// object was not adopted (double free / free of a managed object).
    pub fn manual_free(&mut self, obj: *mut PickleObject) {
        assert!(!obj.is_null(), "pickle: `free()` on a null object");
        let (class_id, flags) = unsafe { ((*obj).class_id, (*obj).flags) };
        assert!(
            flags & PICKLE_FLAG_MANUAL != 0,
            "pickle: `free()` on an object not allocated with `#[manualAlloc]`"
        );
        // Deregister first so a finalizer that allocates can never see a
        // half-freed manual object.
        self.manual_objects.retain(|&o| o != obj);
        if let Some(d) = self.descriptors.get(class_id) {
            if d.flags & PICKLE_CLASS_FLAG_FINALIZER != 0 {
                (d.finalizer)(obj);
            }
        }
        // Owned fields follow their holder: release them recursively.
        self.free_owned_fields(obj);
        unsafe {
            match class_id {
                PICKLE_CLASS_LIST => {
                    let data = list_data(obj);
                    if !data.is_null() {
                        raw_free(data as *mut u8);
                    }
                }
                PICKLE_CLASS_MAP => {
                    let entries = map_entries(obj);
                    if !entries.is_null() {
                        raw_free(entries as *mut u8);
                    }
                }
                _ => {}
            }
        }
        self.heap.free_object(obj);
    }
}

impl Default for Gc {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard that keeps `obj` alive across nested collections until dropped.
/// A native helper that builds a fresh aggregate (list/map/string) and then
/// keeps allocating must hold one of these over the whole build, or a nested
/// collection will sweep the half-built value (see the `str_to_bytes` crash
/// this fixed). Mirrors the compiler side: the JIT roots values via shadow
/// frames; helpers root their in-flight temporaries via this lease.
#[must_use]
pub struct TempRootLease(*mut PickleObject);

impl TempRootLease {
    pub fn release(mut self) {
        self.release_inner();
    }
    fn release_inner(&mut self) {
        if !self.0.is_null() {
            gc_mut().temp_unroot(self.0);
            self.0 = std::ptr::null_mut();
        }
    }
}

impl Drop for TempRootLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// The process-global GC. Initialised by `pickle_runtime_init`.
static mut GLOBAL_GC: *mut Gc = std::ptr::null_mut();

/// Whether the runtime has been initialised.
pub fn is_initialized() -> bool {
    unsafe { !GLOBAL_GC.is_null() }
}

/// The heap, reachable only while its thread owns the world lock. Every access
/// goes through [`gc_mut`], which takes the lock if the caller does not hold
/// it, so the collector's single-mutator assumption holds no matter which OS
/// thread is running compiled code.
pub(crate) fn gc_mut() -> &'static mut Gc {
    crate::world::acquire();
    unsafe {
        assert!(!GLOBAL_GC.is_null(), "pickle runtime not initialised");
        &mut *GLOBAL_GC
    }
}

/// Initialise the collector. Idempotent enough for tests.
pub(crate) fn init() {
    unsafe {
        if GLOBAL_GC.is_null() {
            GLOBAL_GC = Box::into_raw(Box::new(Gc::new()));
        }
    }
    // Ensure the calling thread is registered so its roots are seen.
    shadow::register_thread();
}

/// Shut the collector down (tests / process exit).
pub(crate) fn shutdown() {
    unsafe {
        if !GLOBAL_GC.is_null() {
            let mut gc = Box::from_raw(GLOBAL_GC);
            gc.heap.destroy();
            GLOBAL_GC = std::ptr::null_mut();
        }
    }
}

/// Allocate a managed object of `size` bytes for class `class_id`.
#[no_mangle]
pub extern "C" fn pickle_gc_alloc(size: u32, class_id: u32) -> *mut PickleObject {
    let gc = gc_mut();
    gc.alloc(size, class_id)
}

/// Adopt an object into the manual (`#[manualAlloc]`) registry. Returns the
/// object pointer so it can wrap a constructor expression.
#[no_mangle]
pub extern "C" fn pickle_manual_adopt(obj: *mut PickleObject) -> *mut PickleObject {
    gc_mut().manual_adopt(obj)
}

/// Release an object adopted with `pickle_manual_adopt`.
#[no_mangle]
pub extern "C" fn pickle_manual_free(obj: *mut PickleObject) {
    gc_mut().manual_free(obj);
}

/// Register a static root cell (`*mut *mut PickleObject`). The cell's pointee
/// is traced at every collection. Returns a handle for `pickle_gc_root_drop`.
#[no_mangle]
pub extern "C" fn pickle_gc_root_add(cell: *mut *mut PickleObject) -> *mut RootCell {
    let node = Box::into_raw(Box::new(RootCell {
        cell,
        next: std::ptr::null_mut(),
    }));
    STATIC_ROOTS.lock().unwrap().push(unsafe { &mut *node });
    node
}

/// Remove a static root cell previously registered with `pickle_gc_root_add`.
#[no_mangle]
pub extern "C" fn pickle_gc_root_drop(handle: *mut RootCell) {
    if handle.is_null() {
        return;
    }
    let mut list = STATIC_ROOTS.lock().unwrap();
    list.retain(|c| {
        let ptr = &**c as *const RootCell;
        !std::ptr::eq(ptr, handle)
    });
    // SAFETY: handle came from `pickle_gc_root_add`.
    unsafe {
        drop(Box::from_raw(handle));
    }
}

/// Collect garbage now. `force` is accepted for ABI compatibility; v1 always
/// collects (same as `force = true`).
#[no_mangle]
pub extern "C" fn pickle_gc_collect(_force: bool) {
    let gc = gc_mut();
    gc.collect();
}

/// Number of object allocations since the last collection (debug/stats).
#[no_mangle]
pub extern "C" fn pickle_gc_allocations_since_gc() -> u32 {
    ALLOC_SINCE_GC.load(Ordering::Relaxed)
}

/// Number of completed collection cycles (debug/stats).
#[no_mangle]
pub extern "C" fn pickle_gc_collection_count() -> u32 {
    COLLECTIONS.load(Ordering::Relaxed)
}

/// Set the auto-collect threshold in object bytes.
#[no_mangle]
pub extern "C" fn pickle_gc_set_threshold(bytes: u32) {
    AUTO_THRESHOLD.store(bytes, Ordering::Relaxed);
}

/// Reset the collector: counters, dropped root cells, heap teardown and a
/// fresh `Gc`. Used between `pickle test` files so a new module's compiled
/// class ids (restarting at `PICKLE_CLASS_USER_BASE`) line up with an empty
/// registry, and so objects from the previous module can never be traced or
/// swept by this one.
pub(crate) fn reset() {
    ALLOC_SINCE_GC.store(0, Ordering::Relaxed);
    BYTES_SINCE_GC.store(0, Ordering::Relaxed);
    AUTO_THRESHOLD.store(DEFAULT_AUTO_COLLECT_THRESHOLD, Ordering::Relaxed);
    COLLECTIONS.store(0, Ordering::Relaxed);
    // Drop roots left behind by the previous module (e.g. leaked static-field
    // cells) so they can never trace objects from a destroyed heap.
    STATIC_ROOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    unsafe {
        if !GLOBAL_GC.is_null() {
            let mut gc = Box::from_raw(GLOBAL_GC);
            gc.heap.destroy();
        }
        GLOBAL_GC = Box::into_raw(Box::new(Gc::new()));
        shadow::register_thread();
    }
}

/// Serialises tests that touch the shared global `Gc`. Each test fresh-starts
/// the collector under the lock so parallel tests cannot sweep each other's
/// allocations. Returns the guard; hold it for the test's duration.
#[cfg(test)]
pub(crate) fn test_begin() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    let guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate a fresh user-class-like object descriptor with zero managed
    /// slots (leaf).
    fn leaf_class(gc: &mut Gc) -> u32 {
        gc.register_class(crate::object::ClassDescriptor {
            name_ptr: b"leaf\0".as_ptr(),
            name_len: 4,
            flags: 0,
            slot_count: 0,
            mask_words: 0,
            managed_mask: std::ptr::null(),
            finalizer: crate::object::builtin_nop_finalizer,
        })
    }

    /// Counts finalizer runs for the tests below. Reset in each test.
    static FINALIZED: AtomicU32 = AtomicU32::new(0);

    extern "C" fn count_finalizer(_obj: *mut PickleObject) {
        let _ = FINALIZED.fetch_add(1, Ordering::Relaxed);
    }

    /// A user-class-like descriptor carrying `count_finalizer`.
    fn finalizable_class(gc: &mut Gc) -> u32 {
        gc.register_class(crate::object::ClassDescriptor {
            name_ptr: b"fin\0".as_ptr(),
            name_len: 3,
            flags: PICKLE_CLASS_FLAG_FINALIZER,
            slot_count: 0,
            mask_words: 0,
            managed_mask: std::ptr::null(),
            finalizer: count_finalizer,
        })
    }

    #[test]
    fn finalizer_runs_once_for_a_dead_object_and_block_is_reused() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        FINALIZED.store(0, Ordering::Relaxed);
        let gc = gc_mut();
        let cls = finalizable_class(gc);
        let orphan = gc.alloc(64, cls);
        let addr_before = orphan as usize;
        gc.collect();
        assert_eq!(
            FINALIZED.load(Ordering::Relaxed),
            1,
            "the dead object must be finalized exactly once"
        );
        // The finalizer runs before the block is recycled.
        let recycled = gc.alloc(64, cls);
        assert_eq!(recycled as usize, addr_before);
    }

    #[test]
    fn rooted_finalizable_object_is_not_finalized() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        FINALIZED.store(0, Ordering::Relaxed);
        let gc = gc_mut();
        let cls = finalizable_class(gc);
        let obj = gc.alloc(64, cls);
        let root: *mut *mut PickleObject = Box::into_raw(Box::new(obj));
        let handle = pickle_gc_root_add(root);
        gc.collect();
        assert_eq!(
            FINALIZED.load(Ordering::Relaxed),
            0,
            "a rooted object must survive and not be finalized"
        );
        assert_eq!(unsafe { *root }, obj);
        pickle_gc_root_drop(handle);
        unsafe { drop(Box::from_raw(root)) };
    }

    #[test]
    fn rooted_object_survives_collect() {
        let _guard = test_begin();
        // Register the builtin descriptors first so user classes occupy the
        // reserved id range (>= PICKLE_CLASS_USER_BASE); otherwise `leaf_class`
        // above lands on id 0 and is mistraced as the `string` builtin.
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls = leaf_class(gc);
        let obj = gc.alloc(48, cls);
        let root: *mut *mut PickleObject = Box::into_raw(Box::new(obj));
        let handle = pickle_gc_root_add(root);
        gc.collect();
        unsafe {
            assert!(!(*obj).is_marked(), "marks cleared after sweep");
        }
        assert_eq!(unsafe { *root }, obj, "rooted object must survive");
        pickle_gc_root_drop(handle);
        unsafe { drop(Box::from_raw(root)) };
    }

    #[test]
    fn unreachable_object_is_collected_and_reused() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls = leaf_class(gc);
        let orphan = gc.alloc(64, cls);
        let addr_before = orphan as usize;
        gc.collect();
        let recycled = gc.alloc(64, cls);
        assert_eq!(
            recycled as usize, addr_before,
            "expected the swept block to be reused"
        );
    }

    #[test]
    fn auto_collect_runs_at_threshold_and_reclaims() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls = leaf_class(gc);
        pickle_gc_set_threshold(256);
        assert_eq!(pickle_gc_collection_count(), 0);
        let before = pickle_gc_collection_count();
        // This allocation crosses the 256-byte threshold and collects. The
        // object itself is pinned across that collection (it is not reachable
        // yet), so it survives.
        let orphan = gc.alloc(256, cls);
        let after = pickle_gc_collection_count();
        assert!(after > before, "auto-collect must run at the threshold");
        assert_eq!(
            unsafe { (*orphan).class_id },
            cls,
            "the allocation that crossed the threshold must survive its own collect"
        );
        // The orphan is unrooted, so the next collection sweeps it and the
        // allocation after that reuses its block.
        let _ = gc.alloc(256, cls);
        let recycled = gc.alloc(256, cls);
        assert_eq!(recycled as usize, orphan as usize);
        pickle_gc_set_threshold(DEFAULT_AUTO_COLLECT_THRESHOLD);
    }

    #[test]
    fn crossing_allocation_is_not_finalized_by_its_own_collect() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        FINALIZED.store(0, Ordering::Relaxed);
        let gc = gc_mut();
        let cls = finalizable_class(gc);
        pickle_gc_set_threshold(256);
        // Allocating right at the threshold triggers a collection; the object
        // under construction must not be treated as dead (which would run its
        // finalizer and hand the constructor a freed block).
        let obj = gc.alloc(256, cls);
        assert_eq!(
            FINALIZED.load(Ordering::Relaxed),
            0,
            "the object being allocated must not be finalized by its own collection"
        );
        assert_eq!(unsafe { (*obj).class_id }, cls);
        // Once unrooted, a later collection reclaims it and finalizes once.
        let _ = gc.alloc(256, cls);
        assert_eq!(FINALIZED.load(Ordering::Relaxed), 1);
        pickle_gc_set_threshold(DEFAULT_AUTO_COLLECT_THRESHOLD);
    }

    #[test]
    fn collect_clears_marks_and_keeps_live() {
        let _guard = test_begin();
        // Register the builtin descriptors first: a bare table has `cls_parent`
        // land on id 1 == PICKLE_CLASS_LIST, so `pair` gets mistraced as a list
        // and the collector walks uninitialised payload slots for `list_len`
        // bytes (a misaligned-deref on Linux, where a fresh arena can be dirty).
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls_leaf = leaf_class(gc);
        let cls_parent = gc.register_class(crate::object::ClassDescriptor {
            name_ptr: b"pair\0".as_ptr(),
            name_len: 4,
            flags: 0,
            slot_count: 1,
            mask_words: 1,
            managed_mask: &[1u32] as *const u32,
            finalizer: crate::object::builtin_nop_finalizer,
        });
        let pair = gc.alloc(64, cls_parent);
        let child = gc.alloc(48, cls_leaf);
        unsafe {
            (*pair).set_slot(0, child);
        }
        let root: *mut *mut PickleObject = Box::into_raw(Box::new(pair));
        let handle = pickle_gc_root_add(root);
        gc.collect();
        unsafe {
            assert!(!(*pair).is_marked());
            assert!(!(*child).is_marked());
            assert_eq!((*pair).slot(0), child);
        }
        pickle_gc_root_drop(handle);
        unsafe { drop(Box::from_raw(root)) };
    }

    #[test]
    fn manual_object_survives_collect_and_finalizes_on_free() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        FINALIZED.store(0, Ordering::Relaxed);
        let gc = gc_mut();
        let cls = finalizable_class(gc);
        let obj = gc.alloc(64, cls);
        let addr = obj as usize;
        assert_eq!(gc.manual_adopt(obj), obj, "adopt returns the object");
        assert_ne!(
            unsafe { (*obj).flags } & crate::object::PICKLE_FLAG_MANUAL,
            0,
            "adopted objects carry the manual flag"
        );
        // A collection must neither sweep nor finalize a manual object.
        gc.collect();
        assert_eq!(
            FINALIZED.load(Ordering::Relaxed),
            0,
            "a manual object must not be finalized by the collector"
        );
        assert_eq!(
            unsafe { (*obj).class_id },
            cls,
            "manual object must survive the sweep"
        );
        assert_eq!(gc.manual_objects.len(), 1);
        // Explicit free finalizes exactly once, deregisters, and recycles.
        gc.manual_free(obj);
        assert_eq!(
            FINALIZED.load(Ordering::Relaxed),
            1,
            "`free` must run the finalizer exactly once"
        );
        assert!(
            gc.manual_objects.is_empty(),
            "`free` must deregister the object"
        );
        let recycled = gc.alloc(64, cls);
        assert_eq!(
            recycled as usize, addr,
            "the freed manual block must be reusable"
        );
    }

    #[test]
    fn manual_adopt_is_idempotent() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls = leaf_class(gc);
        let obj = gc.alloc(48, cls);
        gc.manual_adopt(obj);
        gc.manual_adopt(obj);
        assert_eq!(
            gc.manual_objects.len(),
            1,
            "adopting twice must register once"
        );
        gc.manual_free(obj);
        assert!(gc.manual_objects.is_empty());
    }

    #[test]
    fn manual_object_keeps_managed_field_alive() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let holder_cls = gc.register_class(crate::object::ClassDescriptor {
            name_ptr: b"holder\0".as_ptr(),
            name_len: 6,
            flags: 0,
            slot_count: 1,
            mask_words: 1,
            managed_mask: &[1u32] as *const u32,
            finalizer: crate::object::builtin_nop_finalizer,
        });
        let leaf = leaf_class(gc);
        let holder = gc.alloc(48, holder_cls);
        let child = gc.alloc(48, leaf);
        unsafe {
            (*holder).set_slot(0, child);
        }
        gc.manual_adopt(holder);
        gc.collect();
        assert_eq!(
            unsafe { (*holder).slot(0) },
            child,
            "a managed field of a manual object must be traced and survive"
        );
        gc.manual_free(holder);
    }

    #[test]
    #[should_panic(expected = "not allocated with")]
    fn manual_free_rejects_managed_object() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let cls = leaf_class(gc);
        let obj = gc.alloc(48, cls);
        gc.manual_free(obj);
    }

    /// Registers an unreachable `holder` class with payload slot 0 owned.
    fn owned_holder_class(gc: &mut Gc) -> u32 {
        let id = gc.register_class(crate::object::ClassDescriptor {
            name_ptr: b"holder\0".as_ptr(),
            name_len: 6,
            flags: 0,
            slot_count: 1,
            mask_words: 0,
            managed_mask: std::ptr::null(),
            finalizer: crate::object::builtin_nop_finalizer,
        });
        gc.set_class_owned_mask(id, 0b1);
        id
    }

    #[test]
    fn manual_free_releases_owned_fields_recursively() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let holder_cls = owned_holder_class(gc);
        let leaf = leaf_class(gc);
        let holder = gc.alloc(48, holder_cls);
        let child = gc.alloc(48, leaf);
        unsafe {
            (*holder).set_slot(0, child);
        }
        gc.manual_adopt(holder);
        gc.manual_adopt(child);
        assert_eq!(gc.manual_objects.len(), 2);

        gc.manual_free(holder);
        assert!(
            gc.manual_objects.is_empty(),
            "freeing a holder must release its owned child"
        );
    }

    #[test]
    fn collector_releases_owned_fields_of_dead_holder() {
        let _guard = test_begin();
        crate::pickle_runtime_init();
        let gc = gc_mut();
        let holder_cls = owned_holder_class(gc);
        let leaf = leaf_class(gc);
        let child = gc.alloc(48, leaf);
        gc.manual_adopt(child);
        let holder = gc.alloc(48, holder_cls);
        unsafe {
            (*holder).set_slot(0, child);
        }
        // `holder` is not rooted anywhere: the collector must sweep it and, on
        // the way out, release the manual child it owns.
        gc.collect();
        assert!(
            gc.manual_objects.is_empty(),
            "a dead holder must release its owned child"
        );
    }
}
