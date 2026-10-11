//! Non-moving heap: bump-allocation in arena segments, a free-list triaged by
//! size class, and a sweep that coalesces adjacent dead objects.
//!
//! See `docs/05-memory.md` — v1 collector is non-moving, stop-the-world
//! mark-and-sweep. The *mark* phase lives in `trace.rs`; this module owns
//! memory layout, allocation, and sweeping.

use crate::object::PickleObject;
use alloc::alloc::{dealloc, Layout};
use alloc::vec::Vec;

/// Minimum object size (header + one aligned slot), also the smallest bin.
pub const MIN_OBJECT: usize = 32;
/// Free-bin count: covers objects up to 2^31; bigger sizes go into the last bin.
pub const NBINS: usize = 28;

pub(crate) const FREE_SENTINEL: u32 = 0xDEAD;

fn gc_trace_env() -> bool {
    #[cfg(feature = "nostd")]
    {
        crate::nostd::getenv_present("PKL_GC_TRACE")
    }
    #[cfg(not(feature = "nostd"))]
    {
        std::env::var_os("PKL_GC_TRACE").is_some()
    }
}

#[repr(C)]
struct Segment {
    next: *mut Segment,
    /// Total byte capacity of the `data` area.
    cap: usize,
    /// Bump cursor: bytes of `data` already handed out.
    used: usize,
    /// `data` is the flexible array immediately after `data_offset`.
    data_offset: usize,
}

const DATA_OFFSET: usize = core::mem::size_of::<Segment>();

impl Segment {
    fn alloc(cap: usize) -> *mut Segment {
        let total = DATA_OFFSET + cap;
        unsafe {
            let layout = Layout::from_size_align(total, 16).unwrap();
            let mem = alloc::alloc::alloc(layout);
            assert!(!mem.is_null(), "pickle: out of virtual memory");
            let seg = mem as *mut Segment;
            (*seg).next = core::ptr::null_mut();
            (*seg).cap = cap;
            (*seg).used = 0;
            (*seg).data_offset = DATA_OFFSET;
            seg
        }
    }

    fn data_base(&self) -> *mut u8 {
        unsafe { (self as *const Segment as *mut u8).add(DATA_OFFSET) }
    }

    unsafe fn dealloc(seg: *mut Segment) {
        let cap = (*seg).cap;
        let layout = Layout::from_size_align(DATA_OFFSET + cap, 16).unwrap();
        dealloc(seg as *mut u8, layout);
    }
}

/// The heap. Global; accessed from codegen via `pickle_gc_*`.
pub struct Heap {
    segments: *mut Segment,
    /// Free-lists by size class; intrusive via `header.next`.
    free_heads: Vec<*mut PickleObject>,
    /// Cumulative bytes handed out by the bump allocator. Non-moving arenas
    /// never return pages, so this is a monotone proxy for real footprint.
    issue: u64,
    /// Hard ceiling on `issue`; 0 = unlimited. Read from `PICKLE_HEAP_CAP_MB`
    /// so pathological driver/compiler runs fail fast (seconds, ~MiB scale)
    /// instead of ballooning to gigabytes of committed memory.
    cap: u64,
}

fn heap_cap_bytes() -> u64 {
    #[cfg(feature = "nostd")]
    {
        0
    }
    #[cfg(not(feature = "nostd"))]
    {
        let mib = std::env::var_os("PICKLE_HEAP_CAP_MB")
            .and_then(|v| v.into_string().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        mib.saturating_mul(1 << 20)
    }
}

/// Bin index for a block of `size` bytes.
fn bin_of(size: usize) -> usize {
    let s = size.max(MIN_OBJECT);
    let mut b = 0usize;
    let mut pow = MIN_OBJECT;
    while pow < s && b + 1 < NBINS {
        pow <<= 1;
        b += 1;
    }
    b
}

impl Heap {
    pub fn new() -> Heap {
        let first = Segment::alloc(1 << 20);
        Heap {
            segments: first,
            free_heads: vec![core::ptr::null_mut(); NBINS],
            issue: 0,
            cap: heap_cap_bytes(),
        }
    }

    fn push_free(&mut self, block: *mut PickleObject, size: usize) {
        unsafe {
            (*block).class_id = FREE_SENTINEL;
            (*block).flags = 0;
            (*block).size = size as u32;
            let idx = bin_of(size);
            (*block).next = self.free_heads[idx];
            self.free_heads[idx] = block;
        }
    }

    fn alloc_from_free(&mut self, size: usize) -> Option<*mut PickleObject> {
        // Bound the free-bin walk: after a few non-fitting nodes (blocks whose
        // span cannot be split into a valid leftover), bail to the bump
        // allocator. An unbounded walk turns 32-byte-box churn into O(n^2) once
        // dead list/string objects fill the shared size bins. capping the scan
        // keeps every allocation O(1) regardless of chain composition.
        const MAX_SCAN: usize = 8;
        let mut prev: *mut PickleObject = core::ptr::null_mut();
        let mut scanned: usize = 0;
        for idx in bin_of(size)..NBINS {
            let mut cur = self.free_heads[idx];
            while !cur.is_null() {
                if scanned >= MAX_SCAN {
                    return None;
                }
                scanned += 1;
                unsafe {
                    let full = (*cur).size as usize;
                    // Only reuse a block whose span can exactly match the
                    // request: either a perfect fit, or a split whose leftover
                    // is itself a valid free object (>= MIN_OBJECT). Splitting
                    // into a sub-MIN_OBJECT stub is unsafe: such stubs would
                    // poison the shared bins for every later allocation.
                    if full == size || full >= size + MIN_OBJECT {
                        let next = (*cur).next;
                        if prev.is_null() {
                            self.free_heads[idx] = next;
                        } else {
                            (*prev).next = next;
                        }
                        if full > size {
                            let tail = (cur as *mut u8).add(size) as *mut PickleObject;
                            self.push_free(tail, full - size);
                            (*cur).size = size as u32;
                        }
                        return Some(cur);
                    }
                    prev = cur;
                    cur = (*cur).next;
                }
            }
            prev = core::ptr::null_mut();
        }
        None
    }

    fn bump(&mut self, size: usize) -> *mut u8 {
        unsafe {
            let seg = self.segments;
            let room = (*seg).cap - (*seg).used;
            if size > room {
                // Grow the arena in modest, bounded chunks. Unbounded doubling
                // lets a churn-heavy workload with a modest live set grow a
                // single segment to gigabytes before a failure to commit
                // virtual memory; a cap keeps every segment allocation cheap
                // and survivable. A giant single request still gets an exact
                // (unbounded) segment.
                const SEG_CAP_MAX: usize = 1 << 26; // 64 MiB
                let grown = ((*seg).cap * 2).max(1 << 20).max(size);
                let new_cap = if size >= (*seg).cap * 4 {
                    size
                } else {
                    grown.min(SEG_CAP_MAX)
                };
                let fresh = Segment::alloc(new_cap);
                (*fresh).next = seg;
                self.segments = fresh;
            }
            let seg = self.segments;
            let base = (*seg).data_base();
            let p = base.add((*seg).used);
            (*seg).used += size;
            self.issue = self.issue.saturating_add(size as u64);
            let cap = self.cap;
            if cap > 0 && self.issue > cap {
                panic!(
                    "internal error: pickle: heap cap {} MiB exhausted ({} MiB issued); \
                     set PICKLE_HEAP_CAP_MB=0 to disable",
                    cap >> 20,
                    self.issue >> 20
                );
            }
            p
        }
    }

    /// Debug: is `p` the start of a live heap block? Used by the UAF probe to
    /// tell "managed argument that the collector already freed" apart from
    /// "pointer that was never a heap object".
    pub fn debug_live_block(&self, p: *const PickleObject) -> bool {
        unsafe {
            let mut seg = self.segments;
            while !seg.is_null() {
                let start = (*seg).data_base() as usize;
                let end = start + (*seg).used;
                let addr = p as usize;
                if addr >= start && addr < end {
                    let mut cur = self.segments;
                    while !cur.is_null() {
                        let base = (*cur).data_base() as usize;
                        let top = base + (*cur).used;
                        let mut c = base;
                        while c < top {
                            let obj = c as *mut PickleObject;
                            let size = (*obj).size as usize;
                            if size == 0 {
                                return false;
                            }
                            if c == addr {
                                return (*obj).class_id != FREE_SENTINEL;
                            }
                            c += size;
                        }
                        cur = (*cur).next;
                    }
                    return false;
                }
                seg = (*seg).next;
            }
            false
        }
    }

    /// Allocate an object of exactly `size` (>= header, multiple of 8) bytes.
    pub fn alloc_object(&mut self, size: usize) -> *mut PickleObject {
        let size = align8(size).max(MIN_OBJECT);
        if let Some(block) = self.alloc_from_free(size) {
            return block;
        }
        self.bump(size) as *mut PickleObject
    }

    /// Allocate `n` raw (non-object) bytes, aligned 8. The caller owns the
    /// block until it is handed back to `raw_free`. A hidden size prefix is
    /// stored before the returned pointer.
    pub fn raw_alloc(&mut self, n: usize) -> *mut u8 {
        let n = align8(n);
        let total = 8 + n + 8;
        unsafe {
            let layout = Layout::from_size_align(total, 8).unwrap();
            let mem = alloc::alloc::alloc(layout);
            assert!(!mem.is_null(), "pickle: out of memory");
            (mem as *mut usize).write(n);
            mem.add(8)
        }
    }

    /// Reallocate a raw block, preserving `preserve` leading bytes.
    pub fn raw_realloc(&mut self, ptr: *mut u8, new_n: usize, preserve: usize) -> *mut u8 {
        let fresh = self.raw_alloc(new_n);
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, fresh, preserve.min(new_n));
            raw_free(ptr);
        }
        fresh
    }

    /// The live (bump) walk: for each object in every segment, the cumulative
    /// size. Used by the sweeper.
    fn for_each_object_slot<F: FnMut(*mut u8, usize, *mut Segment)>(&self, mut f: F) {
        unsafe {
            let mut seg = self.segments;
            while !seg.is_null() {
                let start = (*seg).data_base();
                let top = start.add((*seg).used);
                let mut cur = start;
                while cur < top {
                    let obj = cur as *mut PickleObject;
                    let size = (*obj).size as usize;
                    debug_assert!(size >= MIN_OBJECT, "corrupt object size");
                    f(cur, size, seg);
                    cur = cur.add(size);
                }
                seg = (*seg).next;
            }
        }
    }

    /// Sweep every segment: clear marks on live objects and release dead
    /// objects back to the free list, coalescing contiguous dead runs within
    /// a segment. Also frees raw buffers owned by dead lists/maps.
    ///
    /// `release_raw` is the raw-block deallocator for `PList`/`PMap`.
    ///
    /// `should_defer` marks a dead object whose finalizer must run before its
    /// memory is reused. Such objects are *not* coalesced into a dead run and
    /// are returned from this call; the caller runs their finalizers and then
    /// hands each back via [`Heap::free_object`]. This keeps finalizers off the
    /// sweep path so a finalizer that touches the heap cannot alias the
    /// collector's `&mut Heap` borrow.
    pub fn sweep<F, G>(&mut self, mut release_raw: F, mut should_defer: G) -> Vec<*mut PickleObject>
    where
        F: FnMut(*mut PickleObject),
        G: FnMut(*mut PickleObject) -> bool,
    {
        let mut deferred: Vec<*mut PickleObject> = Vec::new();
        unsafe {
            let mut seg = self.segments;
            while !seg.is_null() {
                let start = (*seg).data_base();
                let top = start.add((*seg).used);
                let mut cur = start;
                let mut dead_start: *mut u8 = core::ptr::null_mut();
                let mut dead_len: usize = 0;

                let flush = |heap: &mut Heap, dead_start: *mut u8, dead_len: usize| {
                    if !dead_start.is_null() {
                        heap.push_free(dead_start as *mut PickleObject, dead_len);
                    }
                };
                let flush_dead =
                    |heap: &mut Heap, dead_start: &mut *mut u8, dead_len: &mut usize| {
                        flush(heap, *dead_start, *dead_len);
                        *dead_start = core::ptr::null_mut();
                        *dead_len = 0;
                    };

                while cur < top {
                    let obj = cur as *mut PickleObject;
                    let size = (*obj).size as usize;
                    debug_assert!(size.is_multiple_of(8), "sweep walk misaligned");
                    // Blocks already returned to the free list are skipped, not re-freed.
                    if (*obj).class_id == FREE_SENTINEL {
                        flush_dead(self, &mut dead_start, &mut dead_len);
                    } else if (*obj).is_marked() {
                        (*obj).clear_marked();
                        flush_dead(self, &mut dead_start, &mut dead_len);
                    } else {
                        // Dead: hand any owned raw buffers to the callback.
                        release_raw(obj);
                        if should_defer(obj) {
                            // A deferred block cannot be coalesced (its header
                            // stays valid until its finalizer has run).
                            flush_dead(self, &mut dead_start, &mut dead_len);
                            deferred.push(obj);
                        } else {
                            if gc_trace_env() {
                                let cid = (*obj).class_id;
                                eprintln!("GCSWEEP - {:p} class={} size={}", obj, cid, size);
                            }
                            if gc_trace_env()
                                && (*obj).class_id == crate::object::PICKLE_CLASS_BOX_INT
                            {
                                eprintln!("GCBOX - {:p} swept", obj);
                            }
                            if gc_trace_env() {
                                let cid = (*obj).class_id;
                                if cid == crate::object::PICKLE_CLASS_LIST
                                    || cid == crate::object::PICKLE_CLASS_MAP
                                {
                                    eprintln!("GCRAW - {:p} class={} swept", obj, cid);
                                }
                            }
                            if dead_start.is_null() {
                                dead_start = cur;
                            }
                            dead_len += size;
                        }
                    }
                    cur = cur.add(size);
                }
                flush_dead(self, &mut dead_start, &mut dead_len);
                seg = (*seg).next;
            }
        }
        deferred
    }

    /// Return a single dead object to its size class. Used for blocks whose
    /// finalizer ran after a sweep; no coalescing is attempted.
    pub fn free_object(&mut self, obj: *mut PickleObject) {
        let size = unsafe { (*obj).size as usize };
        self.push_free(obj, size);
    }

    /// Free all segments and bins (used in tests / shutdown).
    pub fn destroy(&mut self) {
        unsafe {
            let mut seg = self.segments;
            while !seg.is_null() {
                let next = (*seg).next;
                Segment::dealloc(seg);
                seg = next;
            }
            self.segments = core::ptr::null_mut();
            self.free_heads = vec![core::ptr::null_mut(); NBINS];
        }
    }
}

pub fn align8(n: usize) -> usize {
    (n + 7) & !7usize
}

/// Free a raw block returned by `Heap::raw_alloc` / `raw_realloc`.
///
/// This is an associated free function so the sweeper (which already holds
/// `&mut Heap`) can free raw buffers without a second borrow.
pub unsafe fn raw_free(ptr: *mut u8) {
    debug_assert!(!ptr.is_null());
    let n = (ptr as *const usize).sub(1).read();
    let layout = Layout::from_size_align(8 + n + 8, 8).unwrap();
    dealloc(ptr.sub(8), layout);
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

/// Instantiate a fresh header at `obj` (debug helper; real codegen writes the
/// fields directly).
#[cfg(test)]
fn init_header(obj: *mut PickleObject, class_id: u32, size: u32) {
    unsafe {
        (*obj).init(class_id, size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_mapping() {
        assert_eq!(bin_of(32), 0);
        assert_eq!(bin_of(33), 1);
        assert_eq!(bin_of(64), 1);
        assert_eq!(bin_of(4096), 7);
    }

    #[test]
    fn alloc_and_payload() {
        let mut heap = Heap::new();
        let o = heap.alloc_object(48);
        assert!((o as usize).is_multiple_of(8));
        unsafe {
            (*o).init(7, 48);
            assert_eq!((*o).class_id, 7);
            assert_eq!((*o).size, 48);
        }
        heap.destroy();
    }

    #[test]
    fn raw_alloc_roundtrip() {
        let mut heap = Heap::new();
        let p = heap.raw_alloc(100);
        assert!((p as usize).is_multiple_of(8));
        unsafe {
            for i in 0..100 {
                *p.add(i) = (i % 251) as u8;
            }
            let p2 = heap.raw_realloc(p, 300, 100);
            for i in 0..100 {
                assert_eq!(*p2.add(i), (i % 251) as u8);
            }
            raw_free(p2);
        }
        heap.destroy();
    }

    #[test]
    fn free_reuse() {
        let mut heap = Heap::new();
        let a = heap.alloc_object(64);
        let b = heap.alloc_object(64);
        unsafe {
            init_header(a, 1, 64);
            init_header(b, 1, 64);
            // Mark only `a`; `b` dies.
            (*a).set_marked();
        }
        // Mark only `a`; b dies.
        heap.sweep(
            |_obj| {
                // no raw buffers
            },
            |_obj| false,
        );
        // b should be on the free list and a unmarked.
        unsafe {
            assert!(!(*a).is_marked());
        }
        let c = heap.alloc_object(64);
        assert_eq!(c as usize, b as usize, "expected reuse of freed block");
        heap.destroy();
    }
}
