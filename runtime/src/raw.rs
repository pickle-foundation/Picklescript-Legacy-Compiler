//! Raw manual memory for `unsafe` programs: `pickle_raw_alloc` / `pickle_raw_free`.
//!
//! `alloc(int, n)` hands back a plain heap block that the GC never traces;
//! the program owns it and must `free` it exactly once. The `LIVE` registry
//! exists only to catch the obvious double-free / foreign-free mistakes; it is
//! not a safety mechanism (a dangling pointer used before `free` is unchecked,
//! exactly like C).

use alloc::alloc::{alloc, dealloc, Layout};
use core::cell::RefCell;
use alloc::collections::BTreeMap;

/// Alignment for every raw block. Covers all scalar sizes (1..=8) and any
/// future pointer payloads, and keeps every element naturally aligned.
const ALIGN: usize = 8;

// ptr -> allocation size, so `free` can rebuild the exact `Layout`.
thread_local! {
    static LIVE: RefCell<BTreeMap<usize, usize>> = const { RefCell::new(BTreeMap::new()) };
}

/// Allocate `size` raw bytes. Returns the block address (a `*T` in Pk code).
#[no_mangle]
pub extern "C" fn pickle_raw_alloc(size: i64) -> usize {
    let size = size.max(1) as usize;
    let layout = Layout::from_size_align(size, ALIGN).expect("raw alloc: invalid size");
    let ptr = unsafe { alloc(layout) } as usize;
    LIVE.with(|l| l.borrow_mut().insert(ptr, size));
    ptr
}

/// Release a block previously returned by `pickle_raw_alloc`. Returns `true`
/// when the pointer was live and released, `false` when it was not (double
/// free, foreign pointer, already freed) — the caller reports that loudly.
fn raw_try_free(ptr: usize) -> bool {
    let size = LIVE.with(|l| l.borrow_mut().remove(&ptr));
    let Some(size) = size else {
        return false;
    };
    let layout = Layout::from_size_align(size, ALIGN).expect("raw free: invalid size");
    unsafe {
        dealloc(ptr as *mut u8, layout);
    }
    true
}

/// ABI for `free(p)`. A non-live pointer goes through the runtime panic path
/// instead of touching unknown memory.
#[no_mangle]
pub extern "C" fn pickle_raw_free(ptr: usize) {
    if !raw_try_free(ptr) {
        crate::panic::pickle_panic_cstr(
            b"pickle: raw free of a pointer that was not allocated (double free?)\0".as_ptr(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_alloc_free_roundtrip() {
        let p = pickle_raw_alloc(64);
        assert_ne!(p, 0);
        unsafe {
            (p as *mut i64).write(1234);
        }
        assert_eq!(unsafe { (p as *mut i64).read() }, 1234);
        assert!(raw_try_free(p));
    }

    #[test]
    fn raw_alloc_sizes_are_distinct() {
        let a = pickle_raw_alloc(8);
        let b = pickle_raw_alloc(8);
        assert_ne!(a, b);
        assert!(raw_try_free(a));
        assert!(raw_try_free(b));
    }

    #[test]
    fn raw_double_free_is_rejected() {
        let p = pickle_raw_alloc(16);
        assert!(raw_try_free(p));
        assert!(
            !raw_try_free(p),
            "the live registry must reject a second free"
        );
        assert!(
            !raw_try_free(0xdeadbeef),
            "foreign pointers must be rejected"
        );
    }
}
