//! The world lock.
//!
//! The heap, the class registry and the static-root table are process globals
//! reached through `gc_mut()`. With several OS threads running compiled code
//! (see [`crate::threads`]) that is a mutable global shared between
//! unsynchronised threads, so exactly one thread may be inside the runtime's
//! mutator at a time. That single writer is the world lock, and it is what
//! makes the v1 collector safe without per-object synchronisation: marking and
//! sweeping only ever observe one thread's shadow stack plus parked peers.
//!
//! The lock has *session* semantics rather than bracket semantics. A thread
//! acquires it once ([`acquire`], called by `gc_mut()` on every heap touch and
//! by [`crate::threads`] when a task starts) and keeps it — including across
//! safepoint allocations — until it reaches a point where it must block
//! ([`release`]) or its thread exits. Blocking operations in
//! [`crate::threads`] therefore *park*: they hand the lock back, wait, and take
//! it again on the way out, which is what lets a peer make progress while one
//! thread sleeps, joins a task or waits on a condition variable.
//!
//! The model is deliberately a lock rather than a signal-based safepoint: the
//! compiler needs no safepoint table and no handshake in the middle of a
//! compiled function, which keeps the "no unwind info, no re-entrancy"
//! invariant the rest of the runtime relies on. The cost is that a thread only
//! loses the lock where it explicitly says it might block, so the
//! [`parked`](parked) helper is the API for that and hand-written blocking
//! helpers must use it.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

/// Owner sentinel for "the world lock is free". Thread ids start at 1 so the
/// sentinel is never a live owner.
const FREE: u32 = 0;

static NEXT_TID: AtomicU32 = AtomicU32::new(1);

/// The lock itself: the owner thread id, or [`FREE`]. Guarded by [`GATE`],
/// which also provides the parking queue.
static GATE: Mutex<u32> = Mutex::new(FREE);
static GATE_CV: Condvar = Condvar::new();

/// Owner mirror of `GATE`'s contents, so the uncontended acquire/release path
/// never takes the mutex.
static OWNER: AtomicU32 = AtomicU32::new(FREE);

/// Set once a thread has had to wait for the world lock.
static CONTENTION: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// This thread's id, or [`FREE`] before first use.
    static MY_TID: Cell<u32> = const { Cell::new(FREE) };
    /// Whether this thread currently owns the world lock. Kept in TLS rather
    /// than read back from [`OWNER`] so a stale owner (a thread that died
    /// without running TLS destructors) is still detected by a re-acquire.
    static HOLDS: Cell<bool> = const { Cell::new(false) };
    /// Releases the lock when its thread exits without blocking again, so a
    /// finished test or task cannot wedge every other thread.
    static EXIT_GUARD: ExitGuard = const { ExitGuard };
}

struct ExitGuard;

impl Drop for ExitGuard {
    fn drop(&mut self) {
        release();
    }
}

fn tid() -> u32 {
    MY_TID.with(|t| {
        let id = t.get();
        if id == FREE {
            let id = NEXT_TID.fetch_add(1, Ordering::Relaxed);
            t.set(id);
            id
        } else {
            id
        }
    })
}

fn gate() -> MutexGuard<'static, u32> {
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Take the world lock for this thread, waiting for the current owner to
/// release it. Re-entrant: a thread that already owns the lock returns
/// immediately, which is what makes it safe to call from `gc_mut()` on every
/// heap touch.
pub fn acquire() {
    // Installs the exit guard on first use, and reclaims a lock left behind by
    // a thread that vanished without unwinding.
    EXIT_GUARD.with(|_| ());
    if HOLDS.with(Cell::get) {
        return;
    }
    let me = tid();
    if OWNER.load(Ordering::Acquire) == me {
        HOLDS.with(|h| h.set(true));
        return;
    }
    let mut owner = gate();
    while *owner != FREE && *owner != me {
        CONTENTION.store(true, Ordering::Release);
        owner = GATE_CV
            .wait(owner)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    *owner = me;
    OWNER.store(me, Ordering::Release);
    drop(owner);
    HOLDS.with(|h| h.set(true));
}

/// Give the world lock up. Only meaningful at a point where this thread is
/// about to block (see [`parked`]); a thread that keeps running must keep the
/// lock, because it may hold pointers into the heap.
///
/// Releasing a lock this thread does not own is a no-op, so a `release` on a
/// path that never acquired (or after a panic unwound past the matching
/// `acquire`) cannot corrupt the owner.
pub fn release() {
    if !HOLDS.with(Cell::get) {
        return;
    }
    HOLDS.with(|h| h.set(false));
    let me = tid();
    let mut owner = gate();
    if *owner != me {
        return;
    }
    *owner = FREE;
    OWNER.store(FREE, Ordering::Release);
    drop(owner);
    GATE_CV.notify_all();
}

/// Run a blocking `op` without the world lock, taking it back afterwards.
/// `op` must not touch the heap.
pub fn parked<F: FnOnce()>(op: F) {
    let was_owner = held_by_current_thread();
    release();
    op();
    if was_owner {
        acquire();
    }
}

/// Block on `cv` (dropping `guard` for the wait, as `Condvar::wait` does)
/// without the world lock, then take the world lock back before returning.
///
/// This is the one way a blocking primitive may wait. Releasing the world lock
/// first is what lets a runnable peer make progress; releasing the primitive's
/// own mutex at the same time is what stops that peer from deadlocking against
/// a waiter that is holding it.
pub fn parked_wait<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    let was_owner = held_by_current_thread();
    release();
    let guard = cv.wait(guard).unwrap_or_else(|p| p.into_inner());
    if was_owner {
        acquire();
    }
    guard
}

/// Whether this thread owns the world lock. A heap touch from a thread that
/// does not hold it is a bug in the calling helper (see [`acquire`]).
#[inline]
pub fn held_by_current_thread() -> bool {
    HOLDS.with(Cell::get)
}

/// The id of the thread that owns the world lock, or [`FREE`] when it is free.
/// Diagnostics only (tests, backtraces).
pub fn owner() -> u32 {
    OWNER.load(Ordering::Acquire)
}

/// Whether some thread has ever had to wait for the world lock. Diagnostics
/// only (tests): it distinguishes "the runtime handed the lock around" from
/// "these threads never overlapped".
pub fn contended() -> bool {
    CONTENTION.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    #[test]
    fn same_thread_reacquire_is_cheap_and_idempotent() {
        acquire();
        assert!(held_by_current_thread());
        acquire();
        assert!(held_by_current_thread());
        release();
    }

    #[test]
    fn only_one_thread_holds_at_a_time() {
        // No `test_begin()` here: these tests never touch the heap, and a guard
        // would hold the world lock for the whole test — including across the
        // joins below, which is exactly the deadlock the lock is there to
        // prevent.
        let inside = AtomicUsize::new(0);
        let overlap = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let arrived = AtomicUsize::new(0);
        let blocked = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let inside_ref = &inside;
        let overlap_ref = &overlap;
        let peak_ref = &peak;
        let arrived_ref = &arrived;
        let done_ref = &done;
        // Hold the world lock, then spawn the workers: every one of them has to
        // park on `acquire` until this thread hands the lock back. That makes
        // the contention this test exists to observe deterministic instead of
        // depending on how the OS happens to schedule four short critical
        // sections.
        acquire();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    // Peer has reached the lock; it cannot be inside the
                    // critical section until this thread releases.
                    arrived_ref.fetch_add(1, Ordering::AcqRel);
                    acquire();
                    let now = inside_ref.fetch_add(1, Ordering::AcqRel) + 1;
                    peak_ref.fetch_max(now, Ordering::AcqRel);
                    if now > 1 {
                        overlap_ref.fetch_add(1, Ordering::AcqRel);
                    }
                    assert!(held_by_current_thread());
                    for _ in 0..50 {
                        core::hint::spin_loop();
                    }
                    inside_ref.fetch_sub(1, Ordering::AcqRel);
                    release();
                    done_ref.fetch_add(1, Ordering::AcqRel);
                });
            }
            // Every worker is queued behind the lock this thread is holding.
            for _ in 0..5000 {
                if arrived.load(Ordering::Acquire) == 4 {
                    break;
                }
                std::thread::yield_now();
            }
            assert_eq!(arrived.load(Ordering::Acquire), 4, "all workers reached the lock");
            // None of them may be inside the critical section: the lock this
            // thread holds must have excluded them.
            assert_eq!(
                inside.load(Ordering::Acquire),
                0,
                "a worker ran while the world lock was held elsewhere"
            );
            blocked.fetch_add(1, Ordering::AcqRel);
            release();
        });
        assert_eq!(done.load(Ordering::Acquire), 4);
        assert_eq!(overlap.load(Ordering::Acquire), 0, "two owners at once");
        assert_eq!(peak.load(Ordering::Acquire), 1);
        assert!(blocked.load(Ordering::Acquire) > 0, "the workers had to wait");
        // A session that ends at thread exit (no `release`) must not wedge the
        // next thread either.
        std::thread::spawn(|| {
            acquire();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn parking_hands_the_lock_over() {
        let ran = AtomicUsize::new(0);
        acquire();
        assert!(held_by_current_thread());
        {
            let ran = &ran;
            std::thread::scope(|s| {
                s.spawn(move || {
                    // A peer that can only run once this thread parks.
                    acquire();
                    ran.fetch_add(1, Ordering::AcqRel);
                    release();
                });
                for _ in 0..2_000 {
                    if ran.load(Ordering::Acquire) > 0 {
                        break;
                    }
                    parked(std::thread::yield_now);
                }
                // Hand the lock over for good: the scoped peer still has to
                // finish before the scope closes, and this thread must not hold
                // it while waiting for that.
                release();
            });
        }
        assert!(ran.load(Ordering::Acquire) > 0, "peer never got the lock");
        acquire();
        assert!(held_by_current_thread(), "lock is takeable again");
        release();
    }

    #[test]
    fn parked_wait_hands_the_lock_over_and_back() {
        let slots = std::sync::Mutex::new(0_u32);
        let cv = Condvar::new();
        {
            let slots = &slots;
            let cv = &cv;
            std::thread::scope(|s| {
                s.spawn(move || {
                    // The waiter holds `slots` and is parked; this peer must be
                    // able to run and to touch the very mutex the waiter is
                    // parked on. It could not if the waiter kept either lock.
                    acquire();
                    let mut v = slots.lock().unwrap_or_else(|p| p.into_inner());
                    *v += 1;
                    drop(v);
                    cv.notify_all();
                    release();
                });
                acquire();
                let mut v = slots.lock().unwrap_or_else(|p| p.into_inner());
                *v += 1;
                // Park until the peer has been through, then confirm both sides
                // agree and the world lock came back with us.
                while *v < 2 {
                    v = parked_wait(cv, v);
                }
                assert!(held_by_current_thread(), "lock came back with the waiter");
                drop(v);
                release();
            });
        }
        assert_eq!(*slots.lock().unwrap_or_else(|p| p.into_inner()), 2);
    }
}
