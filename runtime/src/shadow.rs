//! Shadow-stack protocol.
//!
//! Compiled functions own a fixed-size slot frame (see `docs/05-memory.md`);
//! the prologue calls `pickle_shadow_push`, spills every live managed pointer
//! into its slot around each safepoint, and `pickle_shadow_pop` in the
//! epilogue. The collector treats every registered thread's frame slots as
//! roots.

use crate::object::PickleObject;
use alloc::boxed::Box;

/// Layout of a shadow frame. Slots follow the header inline:
///
/// ```text
/// #[repr(C)]
/// struct ShadowFrame {
///     prev: *mut ShadowFrame,      // 8
///     slot_count: u32,             // 4
///     _pad: u32,                   // 4
///     // slots: [*mut PickleObject; slot_count]
/// }
/// ```
///
/// Total header = 16 bytes; slot `i` lives at byte offset `16 + i * 8`.
#[repr(C)]
pub struct ShadowFrame {
    pub prev: *mut ShadowFrame,
    pub slot_count: u32,
    pub _pad: u32,
}

const FRAME_HEADER_BYTES: usize = 16;

/// Per-OS-thread GC state, kept in a process-wide registry so any thread can
/// collect from every thread's shadow stack.
#[repr(C)]
pub struct GcThread {
    /// Head of this thread's shadow-frame list (LIFO).
    pub frames: *mut ShadowFrame,
    pub id: u32,
    /// Registry link.
    pub next: *mut GcThread,
}

#[cfg(feature = "nostd")]
use crate::nostd::sync::{Mutex, MutexGuard};
/// Serialises mutations of and walks over the thread registry (`THREAD_HEAD`)
/// and each node's frame list. The collector takes it for the whole walk so a
/// concurrent `register_thread`/`unregister_thread`/`pickle_shadow_push` cannot
/// free or mutate a node while it is being traced.
#[cfg(not(feature = "nostd"))]
use std::sync::{Mutex, MutexGuard};

/// Serialises mutations of and walks over the thread registry (`THREAD_HEAD`)
/// and each node's frame list. The collector takes it for the whole walk so a
/// concurrent `register_thread`/`unregister_thread`/`pickle_shadow_push` cannot
/// free or mutate a node while it is being traced.
static REGISTRY: Mutex<()> = Mutex::new(());

/// Flat registry of threads; also the intrusive list of `GcThread` nodes.
static mut THREAD_HEAD: *mut GcThread = core::ptr::null_mut();
/// Id handed to the next thread that registers. Ids start at 1 so that 0 can
/// mean "no thread" to callers of `pickle_gc_register_thread`/`pickle_thread_current`,
/// which report 0 for an unregistered thread.
static THREAD_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// Lock the thread registry for mutation or traversal. Returns the guard;
/// hold it for the duration of any operation that touches `THREAD_HEAD` or a
/// node's `frames` list.
pub(crate) fn lock_registry() -> MutexGuard<'static, ()> {
    REGISTRY.lock().unwrap_or_else(|p| p.into_inner())
}

thread_local! {
    /// Thread-local handle into the registry (null until registered).
    static CURRENT_THREAD: core::cell::Cell<*mut GcThread> = const { core::cell::Cell::new(core::ptr::null_mut()) };
}

fn assert_ptr_empty(p: *mut u8, len: usize) {
    for i in 0..len {
        assert_eq!(
            unsafe { *p.add(i) },
            0,
            "frame slot memory not zeroed at offset {i}"
        );
    }
}

impl ShadowFrame {
    /// Pointer to the first slot.
    pub fn slots_ptr(&self) -> *mut *mut PickleObject {
        unsafe {
            (self as *const ShadowFrame as *mut u8).add(FRAME_HEADER_BYTES)
                as *mut *mut PickleObject
        }
    }

    /// Direct access to a slot.
    pub fn slot(&self, index: u32) -> *mut PickleObject {
        assert!(index < self.slot_count);
        unsafe { self.slots_ptr().add(index as usize).read() }
    }

    pub fn set_slot(&mut self, index: u32, value: *mut PickleObject) {
        assert!(index < self.slot_count);
        unsafe { self.slots_ptr().add(index as usize).write(value) }
    }

    /// Debug: require the whole slot area to be zeroed before first use.
    pub fn debug_check_zeroed(&self) {
        let len = (self.slot_count as usize) * 8;
        assert_ptr_empty(self.slots_ptr() as *mut u8, len);
    }
}

/// Returns this thread's registered `GcThread`, or null if not registered.
#[inline]
pub fn current_thread() -> *mut GcThread {
    CURRENT_THREAD.with(|c| c.get())
}

/// Allocate a `GcThread` node off the registry (leaked intentionally; freed
/// only at process exit). Sets the thread-local handle.
///
/// Returns the new node. Safe to call once per OS thread.
pub fn register_thread() -> *mut GcThread {
    let _guard = lock_registry();
    ensure_registered_locked(&_guard)
}

/// Insert `node` into the registry under the already-held registry guard.
/// Does not touch the thread-local handle (that is the caller's job).
fn insert_thread_locked(_guard: &MutexGuard<'static, ()>, node: *mut GcThread) {
    unsafe {
        (*node).next = THREAD_HEAD;
        THREAD_HEAD = node;
    }
}

/// Ensure the current thread is registered, assuming the registry guard is held.
/// Returns the thread's `GcThread` node (thread-local, so no lock needed to read).
fn ensure_registered_locked(guard: &MutexGuard<'static, ()>) -> *mut GcThread {
    let existing = current_thread();
    if !existing.is_null() {
        return existing;
    }
    let node = Box::into_raw(Box::new(GcThread {
        frames: core::ptr::null_mut(),
        id: THREAD_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst),
        next: core::ptr::null_mut(),
    }));
    insert_thread_locked(guard, node);
    CURRENT_THREAD.with(|c| c.set(node));
    node
}

/// Remove `node` (and any frames it still has) from the registry and detach.
pub fn unregister_thread(node: *mut GcThread) {
    let _guard = lock_registry();
    unsafe {
        let mut cur = THREAD_HEAD;
        let mut prev: *mut GcThread = core::ptr::null_mut();
        while !cur.is_null() {
            if cur == node {
                if prev.is_null() {
                    THREAD_HEAD = (*cur).next;
                } else {
                    (*prev).next = (*cur).next;
                }
                break;
            }
            prev = cur;
            cur = (*cur).next;
        }
        // Detach local TLS, then drop the node.
        CURRENT_THREAD.with(|c| c.set(core::ptr::null_mut()));
        drop(Box::from_raw(node));
    }
}

fn push_frame(frame: *mut ShadowFrame) {
    assert!(!frame.is_null());
    let _guard = lock_registry();
    unsafe {
        let thread = ensure_registered_locked(&_guard);
        (*frame).prev = (*thread).frames;
        (*thread).frames = frame;
    }
}

/// Push `frame` onto the current thread's stack. Must be LIFO-matched with
/// `pop_frame`.
#[no_mangle]
pub extern "C" fn pickle_shadow_push(frame: *mut ShadowFrame) {
    push_frame(frame);
}

fn pop_frame(frame: *mut ShadowFrame) {
    let _guard = lock_registry();
    unsafe {
        let thread = ensure_registered_locked(&_guard);
        assert_eq!((*thread).frames, frame, "shadow stack pop out of order");
        (*thread).frames = (*frame).prev;
        (*frame).prev = core::ptr::null_mut();
    }
}

/// Pop `frame`, which must be the current top of this thread's stack.
#[no_mangle]
pub extern "C" fn pickle_shadow_pop(frame: *mut ShadowFrame) {
    pop_frame(frame);
}

fn set_frame_slot(frame: *mut ShadowFrame, index: u32, value: *mut PickleObject) {
    assert!(!frame.is_null());
    unsafe { (*frame).set_slot(index, value) }
}

/// Set slot `index` of `frame` to a managed pointer.
#[no_mangle]
pub extern "C" fn pickle_shadow_set(frame: *mut ShadowFrame, index: u32, value: *mut PickleObject) {
    set_frame_slot(frame, index, value);
}

fn get_frame_slot(frame: *mut ShadowFrame, index: u32) -> *mut PickleObject {
    unsafe { (*frame).slot(index) }
}

/// Read slot `index` of `frame` as a managed pointer.
#[no_mangle]
pub extern "C" fn pickle_shadow_get(frame: *mut ShadowFrame, index: u32) -> *mut PickleObject {
    get_frame_slot(frame, index)
}

/// Register the calling OS thread with the collector. Returns the thread's id.
#[no_mangle]
pub extern "C" fn pickle_gc_register_thread() -> u32 {
    let node = register_thread();
    unsafe { (*node).id }
}

/// Unregister the calling OS thread from the collector.
#[no_mangle]
pub extern "C" fn pickle_gc_unregister_thread() {
    let node = current_thread();
    if !node.is_null() {
        unregister_thread(node);
    }
}

/// Process-wide list of registered thread nodes (for the collector).
pub fn all_threads() -> *mut GcThread {
    unsafe { THREAD_HEAD }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_16_byte_header() {
        assert_eq!(core::mem::size_of::<ShadowFrame>(), 16);
    }

    #[test]
    fn slots_roundtrip() {
        // Must not run concurrently with a collector: the trace walks every
        // registered thread's frame slots, and this test stuffs them with
        // deliberately-garbage fake pointers (0x11/0x22) which would segfault
        // the walker. `test_begin` serialises it with all GC/registry tests.
        let _guard = crate::gc::test_begin();
        register_thread();
        let mut frame = ShadowFrame {
            prev: core::ptr::null_mut(),
            slot_count: 2,
            _pad: 0,
        };
        pickle_shadow_push(&mut frame as *mut ShadowFrame);
        let obj_a = 0x11 as *mut PickleObject;
        let obj_b = 0x22 as *mut PickleObject;
        pickle_shadow_set(&mut frame as *mut ShadowFrame, 0, obj_a);
        pickle_shadow_set(&mut frame as *mut ShadowFrame, 1, obj_b);
        assert_eq!(pickle_shadow_get(&mut frame as *mut ShadowFrame, 0), obj_a);
        assert_eq!(pickle_shadow_get(&mut frame as *mut ShadowFrame, 1), obj_b);
        pickle_shadow_pop(&mut frame as *mut ShadowFrame);

        let node = current_thread();
        assert!(!node.is_null());
        unregister_thread(node);
    }
}
