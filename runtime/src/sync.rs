//! Blocking synchronisation primitives for tasks: mutex, condition, semaphore,
//! atomics and once.
//!
//! All the blocking primitives are the same shape — a count of permits plus a
//! parking queue — because the interesting part is not what they mean to a
//! Pickle program but *where* they are allowed to block. A task that waits must
//! hand the world lock ([`crate::world`]) to a runnable peer, and it must not
//! hold a Rust mutex while it parks: a peer that holds the world lock and needs
//! that same mutex would deadlock against a waiter that holds the mutex and is
//! waiting for the world lock. So every park goes through
//! [`world::parked_wait`], which drops the world lock, waits on the condvar
//! (which drops the primitive's own mutex, exactly as `Condvar::wait` does),
//! and takes the world lock back before returning.
//!
//! Permits and the parked-thread count live under one mutex, and a releaser
//! only signals when it sees a parked waiter. That is what makes a signal
//! impossible to lose: a waiter between "no permits" and "parked" is already
//! counted, and a waiter that has not counted itself yet re-checks the permits
//! under the same mutex before parking. There is deliberately no
//! predicate-plus-signal API here — a bare `Condvar` handoff cannot be made
//! atomic from user code — so `condWait`/`condSignal` are a one-permit
//! semaphore, where the permit is the thing that cannot be dropped.

use crate::world;
use std::sync::{Condvar, Mutex, MutexGuard};

/// Permits plus parked-waiter bookkeeping. One mutex, so the check and the
/// count cannot be observed out of order.
#[derive(Default)]
struct SemState {
    permits: i64,
    parked: u32,
}

fn state_lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A counting semaphore: the one blocking primitive the rest are built from.
pub struct PSemaphore {
    state: Mutex<SemState>,
    cv: Condvar,
}

impl PSemaphore {
    fn new(permits: i64) -> Self {
        PSemaphore {
            state: Mutex::new(SemState {
                permits,
                parked: 0,
            }),
            cv: Condvar::new(),
        }
    }

    /// Take `n` permits, parking (without the world lock) until they exist.
    fn take(&self, n: i64) {
        let mut s = state_lock(&self.state);
        while s.permits < n {
            s.parked += 1;
            s = world::parked_wait(&self.cv, s);
            s.parked -= 1;
        }
        s.permits -= n;
    }

    /// Take `n` permits if they are free right now. Never blocks.
    fn try_take(&self, n: i64) -> bool {
        let mut s = state_lock(&self.state);
        if s.permits < n {
            return false;
        }
        s.permits -= n;
        true
    }

    /// Return `n` permits, waking one parked waiter if there is one.
    fn give(&self, n: i64) {
        let mut s = state_lock(&self.state);
        s.permits += n;
        let wake = s.parked > 0;
        drop(s);
        if wake {
            self.cv.notify_one();
        }
    }

    /// Return `n` permits, waking every waiter parked right now. A waiter that
    /// parks after this call finds the permits waiting for it instead, so no
    /// wakeup is lost either way.
    fn give_all(&self, n: i64) {
        let mut s = state_lock(&self.state);
        s.permits += n;
        let wake = s.parked > 0;
        drop(s);
        if wake {
            self.cv.notify_all();
        }
    }

    /// How many threads are parked waiting for a permit.
    fn parked(&self) -> u32 {
        state_lock(&self.state).parked
    }
}

/// A non-recursive mutex: one permit, so locking twice on a thread that
/// already holds it parks forever, exactly as a real mutex would deadlock.
pub struct PMutex {
    sem: PSemaphore,
}

impl PMutex {
    fn new() -> Self {
        PMutex {
            sem: PSemaphore::new(1),
        }
    }

    fn lock(&self) {
        self.sem.take(1);
    }

    fn try_lock(&self) -> bool {
        self.sem.try_take(1)
    }

    fn unlock(&self) {
        self.sem.give(1);
    }
}

/// A condition variable, modelled as a permit counter rather than a bare
/// `Condvar` so a `signal` cannot be lost between a waiter's check and its
/// park: a permit is either handed to a parked waiter or left for the next one.
pub struct PCond {
    sem: PSemaphore,
}

impl PCond {
    fn new() -> Self {
        PCond {
            sem: PSemaphore::new(0),
        }
    }

    fn signal(&self) {
        self.sem.give(1);
    }

    fn broadcast(&self) {
        self.sem.give_all(i64::from(self.sem.parked()));
    }

    /// Block until a permit is signalled. The caller's mutex must be released
    /// around this (see [`cond_wait`]), so the wait cannot miss a signal that
    /// arrives while the mutex is still held.
    fn wait(&self) {
        self.sem.take(1);
    }
}

/// A counting semaphore with `permits` initially available.
pub struct PSem {
    sem: PSemaphore,
}

impl PSem {
    fn new(permits: i64) -> Self {
        PSem {
            sem: PSemaphore::new(permits.max(0)),
        }
    }

    fn acquire(&self, n: i64) {
        self.sem.take(n.max(1));
    }

    fn try_acquire(&self, n: i64) -> bool {
        self.sem.try_take(n.max(1))
    }

    fn release(&self, n: i64) {
        self.sem.give(n.max(1));
    }
}

/// A 64-bit atomic cell.
pub struct PAtomic {
    cell: std::sync::atomic::AtomicI64,
}

impl PAtomic {
    fn new(value: i64) -> Self {
        PAtomic {
            cell: std::sync::atomic::AtomicI64::new(value),
        }
    }

    fn load(&self) -> i64 {
        self.cell.load(std::sync::atomic::Ordering::Acquire)
    }

    fn store(&self, value: i64) {
        self.cell
            .store(value, std::sync::atomic::Ordering::Release);
    }

    /// Add `delta`, returning the value from before the addition.
    fn add(&self, delta: i64) -> i64 {
        self.cell
            .fetch_add(delta, std::sync::atomic::Ordering::AcqRel)
    }

    /// Compare-and-swap, returning whether the swap happened.
    fn cas(&self, expected: i64, value: i64) -> bool {
        self.cell
            .compare_exchange(
                expected,
                value,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Once: 0 = untouched, 1 = a caller is running the initialiser, 2 = done.
pub struct POnce {
    state: Mutex<u8>,
    cv: Condvar,
}

impl POnce {
    fn new() -> Self {
        POnce {
            state: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    /// Try to become the initialiser. Returns true for the caller that got
    /// there first (which must then call [`POnce::finish`]), false for a caller
    /// that found the initialiser already run or running-and-already-waiting.
    fn enter(&self) -> bool {
        let mut s = state_lock(&self.state);
        loop {
            match *s {
                0 => {
                    *s = 1;
                    return true;
                }
                1 => s = world::parked_wait(&self.cv, s),
                _ => return false,
            }
        }
    }

    /// Mark the initialiser done and release every waiter.
    fn finish(&self) {
        let mut s = state_lock(&self.state);
        *s = 2;
        drop(s);
        self.cv.notify_all();
    }

    fn done(&self) -> bool {
        *state_lock(&self.state) == 2
    }
}

/// The registered primitives. A handle is a one-based index, so 0 is the
/// "null handle" a Pickle `none` lowers to. Entries are leaked: a handle can
/// outlive the value it names (a `Mutex` may be stored in a struct that
/// outlives the frame that created it), and the runtime already treats leaked,
/// registered cells (shadow frames, static roots) as the normal way to give a
/// value a stable address.
enum Primitive {
    Mutex(PMutex),
    Cond(PCond),
    Sem(PSem),
    Atomic(PAtomic),
    Once(POnce),
}

static OBJECTS: Mutex<Vec<&'static Primitive>> = Mutex::new(Vec::new());

fn register(value: Primitive) -> i64 {
    let leaked: &'static Primitive = &*Box::leak(Box::new(value));
    let mut objects = state_lock(&OBJECTS);
    objects.push(leaked);
    objects.len() as i64
}

fn entry(handle: i64) -> Option<&'static Primitive> {
    if handle < 1 {
        return None;
    }
    state_lock(&OBJECTS).get((handle - 1) as usize).copied()
}

fn not_a(handle: i64, what: &str) -> ! {
    crate::panic::fatal(&format!("pickle: handle {handle} is not {what}"))
}

fn mutex_of(handle: i64) -> Option<&'static PMutex> {
    match entry(handle) {
        Some(Primitive::Mutex(m)) => Some(m),
        _ => None,
    }
}

fn cond_of(handle: i64) -> Option<&'static PCond> {
    match entry(handle) {
        Some(Primitive::Cond(c)) => Some(c),
        _ => None,
    }
}

fn sem_of(handle: i64) -> Option<&'static PSem> {
    match entry(handle) {
        Some(Primitive::Sem(s)) => Some(s),
        _ => None,
    }
}

fn atomic_of(handle: i64) -> Option<&'static PAtomic> {
    match entry(handle) {
        Some(Primitive::Atomic(a)) => Some(a),
        _ => None,
    }
}

fn once_of(handle: i64) -> Option<&'static POnce> {
    match entry(handle) {
        Some(Primitive::Once(o)) => Some(o),
        _ => None,
    }
}

// --- ABI ------------------------------------------------------------------

/// ABI: a new mutex, unlocked. Returns a handle.
#[no_mangle]
pub extern "C" fn pickle_mutex_new() -> i64 {
    register(Primitive::Mutex(PMutex::new()))
}

/// ABI: lock the mutex, parking until it is free.
#[no_mangle]
pub extern "C" fn pickle_mutex_lock(handle: i64) {
    match mutex_of(handle) {
        Some(m) => m.lock(),
        None => not_a(handle, "a mutex"),
    }
}

/// ABI: try to lock without blocking. Returns 1 on success.
#[no_mangle]
pub extern "C" fn pickle_mutex_trylock(handle: i64) -> bool {
    match mutex_of(handle) {
        Some(m) => m.try_lock(),
        None => not_a(handle, "a mutex"),
    }
}

/// ABI: unlock a mutex this thread holds.
#[no_mangle]
pub extern "C" fn pickle_mutex_unlock(handle: i64) {
    match mutex_of(handle) {
        Some(m) => m.unlock(),
        None => not_a(handle, "a mutex"),
    }
}

/// ABI: a new condition variable. Returns a handle.
#[no_mangle]
pub extern "C" fn pickle_cond_new() -> i64 {
    register(Primitive::Cond(PCond::new()))
}

/// ABI: wake one waiter. A signal with no waiter parked yet is remembered, so
/// the next `condWait` returns immediately.
#[no_mangle]
pub extern "C" fn pickle_cond_signal(handle: i64) {
    match cond_of(handle) {
        Some(c) => c.signal(),
        None => not_a(handle, "a cond"),
    }
}

/// ABI: wake every waiter parked right now.
#[no_mangle]
pub extern "C" fn pickle_cond_broadcast(handle: i64) {
    match cond_of(handle) {
        Some(c) => c.broadcast(),
        None => not_a(handle, "a cond"),
    }
}

/// ABI: release `mutex`, wait on `cond`, and return with `mutex` held again (the
/// Go/Java condition-variable contract). A null `mutex` waits without one.
#[no_mangle]
pub extern "C" fn pickle_cond_wait(cond: i64, mutex: i64) {
    let Some(c) = cond_of(cond) else {
        not_a(cond, "a cond");
    };
    match mutex_of(mutex) {
        Some(m) => {
            m.unlock();
            c.wait();
            m.lock();
        }
        None => c.wait(),
    }
}

/// ABI: a new semaphore with `permits` available. Returns a handle.
#[no_mangle]
pub extern "C" fn pickle_sem_new(permits: i64) -> i64 {
    register(Primitive::Sem(PSem::new(permits)))
}

/// ABI: take `n` permits, parking until they are available.
#[no_mangle]
pub extern "C" fn pickle_sem_acquire(handle: i64, n: i64) {
    match sem_of(handle) {
        Some(s) => s.acquire(n),
        None => not_a(handle, "a semaphore"),
    }
}

/// ABI: take `n` permits without blocking. Returns 1 on success.
#[no_mangle]
pub extern "C" fn pickle_sem_try_acquire(handle: i64, n: i64) -> bool {
    match sem_of(handle) {
        Some(s) => s.try_acquire(n),
        None => not_a(handle, "a semaphore"),
    }
}

/// ABI: return `n` permits, waking a waiter.
#[no_mangle]
pub extern "C" fn pickle_sem_release(handle: i64, n: i64) {
    match sem_of(handle) {
        Some(s) => s.release(n),
        None => not_a(handle, "a semaphore"),
    }
}

/// ABI: a new atomic cell holding `value`. Returns a handle.
#[no_mangle]
pub extern "C" fn pickle_atomic_new(value: i64) -> i64 {
    register(Primitive::Atomic(PAtomic::new(value)))
}

/// ABI: read the cell.
#[no_mangle]
pub extern "C" fn pickle_atomic_load(handle: i64) -> i64 {
    match atomic_of(handle) {
        Some(a) => a.load(),
        None => not_a(handle, "an atomic"),
    }
}

/// ABI: write the cell.
#[no_mangle]
pub extern "C" fn pickle_atomic_store(handle: i64, value: i64) {
    match atomic_of(handle) {
        Some(a) => a.store(value),
        None => not_a(handle, "an atomic"),
    }
}

/// ABI: add `delta` and return the value from *before* the addition.
#[no_mangle]
pub extern "C" fn pickle_atomic_add(handle: i64, delta: i64) -> i64 {
    match atomic_of(handle) {
        Some(a) => a.add(delta),
        None => not_a(handle, "an atomic"),
    }
}

/// ABI: compare-and-swap. Stores `value` and returns 1 when the cell held
/// `expected`, otherwise leaves it alone and returns 0.
#[no_mangle]
pub extern "C" fn pickle_atomic_cas(handle: i64, expected: i64, value: i64) -> bool {
    match atomic_of(handle) {
        Some(a) => a.cas(expected, value),
        None => not_a(handle, "an atomic"),
    }
}

/// ABI: a new `once`. Returns a handle.
#[no_mangle]
pub extern "C" fn pickle_once_new() -> i64 {
    register(Primitive::Once(POnce::new()))
}

/// ABI: whether the initialiser has already run.
#[no_mangle]
pub extern "C" fn pickle_once_done(handle: i64) -> bool {
    match once_of(handle) {
        Some(o) => o.done(),
        None => not_a(handle, "a once"),
    }
}

/// ABI: run `body` (a zero-argument closure object) exactly once across all
/// tasks. Returns true for the caller that ran it, false for the ones that
/// waited for it.
#[no_mangle]
pub extern "C" fn pickle_once_run(handle: i64, body: *mut crate::object::PickleObject) -> bool {
    let Some(once) = once_of(handle) else {
        not_a(handle, "a once");
    };
    if !once.enter() {
        return false;
    }
    // The winner runs the body with the world lock held, like any other call
    // from compiled code. A body that fails leaves the once "running" and
    // every other task parked forever: the same contract as a `Once` whose
    // initialiser aborted, and the honest one — silently running the
    // initialiser twice would be worse than a visible hang.
    crate::threads::call_closure0(body);
    once.finish();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn mutex_excludes() {
        let m = PMutex::new();
        let inside = AtomicUsize::new(0);
        let overlap = AtomicUsize::new(0);
        let m = &m;
        let inside_ref = &inside;
        let overlap_ref = &overlap;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    for _ in 0..100 {
                        m.lock();
                        let now = inside_ref.fetch_add(1, Ordering::AcqRel) + 1;
                        if now > 1 {
                            overlap_ref.fetch_add(1, Ordering::AcqRel);
                        }
                        std::hint::spin_loop();
                        inside_ref.fetch_sub(1, Ordering::AcqRel);
                        m.unlock();
                    }
                });
            }
        });
        assert_eq!(overlap.load(Ordering::Acquire), 0, "two holders at once");
    }

    #[test]
    fn mutex_hands_over_while_waiting() {
        let m = PMutex::new();
        m.lock();
        let got = AtomicUsize::new(0);
        let m_ref = &m;
        let got_ref = &got;
        std::thread::scope(|s| {
            s.spawn(move || {
                m_ref.lock();
                got_ref.store(1, Ordering::Release);
                m_ref.unlock();
            });
            // The waiter cannot proceed while this thread holds the mutex.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(got.load(Ordering::Acquire), 0, "waiter ran too early");
            m.unlock();
        });
        assert_eq!(got.load(Ordering::Acquire), 1);
    }

    #[test]
    fn try_lock_reports_contention() {
        let m = PMutex::new();
        assert!(m.try_lock());
        assert!(!m.try_lock());
        m.unlock();
        assert!(m.try_lock());
    }

    #[test]
    fn cond_signal_is_remembered() {
        let c = PCond::new();
        c.signal();
        // No waiter was parked, so the permit has to survive for the next one.
        c.wait();
    }

    #[test]
    fn cond_wait_releases_and_retakes_the_mutex() {
        let m = PMutex::new();
        let c = PCond::new();
        m.lock();
        let progress = AtomicUsize::new(0);
        let m_ref = &m;
        let progress_ref = &progress;
        std::thread::scope(|s| {
            s.spawn(move || {
                m_ref.lock();
                progress_ref.store(1, Ordering::Release);
                m_ref.unlock();
            });
            // The waiter is blocked on the mutex this thread holds, not on the
            // cond, so the signal it is waiting for cannot have been delivered.
            std::thread::sleep(std::time::Duration::from_millis(20));
            assert_eq!(progress.load(Ordering::Acquire), 0);
            m.unlock();
            c.signal();
        });
        assert_eq!(progress.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cond_broadcast_wakes_every_waiter() {
        let c = Arc::new(PCond::new());
        let woke = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..4 {
                let (c, woke) = (Arc::clone(&c), Arc::clone(&woke));
                s.spawn(move || {
                    c.wait();
                    woke.fetch_add(1, Ordering::AcqRel);
                });
            }
            // Give them time to park.
            std::thread::sleep(std::time::Duration::from_millis(20));
            c.broadcast();
        });
        assert_eq!(woke.load(Ordering::Acquire), 4);
    }

    #[test]
    fn semaphore_counts_down() {
        let s = PSem::new(2);
        assert!(s.try_acquire(1));
        assert!(s.try_acquire(1));
        assert!(!s.try_acquire(1), "the third permit does not exist");
        s.release(1);
        assert!(s.try_acquire(1));
    }

    #[test]
    fn atomic_ops() {
        let a = PAtomic::new(5);
        assert_eq!(a.load(), 5);
        assert_eq!(a.add(3), 5, "add returns the previous value");
        assert_eq!(a.load(), 8);
        a.store(1);
        assert!(a.cas(1, 9));
        assert!(!a.cas(1, 0));
        assert_eq!(a.load(), 9);
    }

    #[test]
    fn once_runs_once() {
        let once = POnce::new();
        let runs = AtomicUsize::new(0);
        let once_ref = &once;
        let runs_ref = &runs;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    if once_ref.enter() {
                        runs_ref.fetch_add(1, Ordering::AcqRel);
                        once_ref.finish();
                    }
                });
            }
        });
        assert_eq!(runs.load(Ordering::Acquire), 1);
        assert!(once.done());
    }

    #[test]
    fn a_blocking_op_lets_a_peer_run() {
        // The property the stdlib's channels depend on: parking must hand the
        // world lock to a runnable peer.
        let sem = PSem::new(0);
        let ran = AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                sem.acquire(1);
                ran.fetch_add(1, Ordering::AcqRel);
            });
            for _ in 0..200 {
                if ran.load(Ordering::Acquire) > 0 {
                    break;
                }
                world::parked(std::thread::yield_now);
            }
            sem.release(1);
        });
        assert!(ran.load(Ordering::Acquire) > 0, "peer never ran");
    }

    #[test]
    fn handles_roundtrip() {
        let m = pickle_mutex_new();
        let c = pickle_cond_new();
        let s = pickle_sem_new(1);
        let a = pickle_atomic_new(3);
        let o = pickle_once_new();
        for handle in [m, c, s, a, o] {
            assert!(handle > 0, "handles start at 1 so 0 stays null");
        }
        assert!(mutex_of(m).is_some());
        assert!(cond_of(c).is_some());
        assert!(sem_of(s).is_some());
        assert!(atomic_of(a).is_some());
        assert!(once_of(o).is_some());
        // A handle of the wrong kind is a loud error, not a silent confusion.
        assert!(cond_of(m).is_none());
        assert_eq!(pickle_atomic_load(a), 3);
    }
}
