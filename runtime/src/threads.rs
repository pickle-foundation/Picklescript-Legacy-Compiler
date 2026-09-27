//! Tasks: OS threads running compiled code, plus the blocking points that let
//! them wait for each other.
//!
//! `task { ... }` from PickleScript lowers to `pickle_thread_spawn` on a
//! zero-argument closure object, and the body runs on a fresh OS thread that
//! joins the collector's thread registry with its own shadow stack (see
//! [`crate::shadow`]). Each of those threads takes the [`crate::world`] lock
//! for the whole of its session and hands it back at the blocking points
//! implemented here — `join`, `sleep`, `yield` — which is what keeps the
//! single-mutator collector sound while tasks overlap.
//!
//! Two rules make that work with a v1 collector, and both are load-bearing:
//!
//! 1. A task thread roots what it holds across a blocking call in its own shadow
//!    frame, and the closure's captures are rooted by the spawning thread's
//!    frame until the task has taken its own session, so a body can never run
//!    with an unrooted environment.
//! 2. A failing `assert` or `panic` in a task body is *recorded*, not fatal and
//!    not silently dropped: the thread-local task sink in [`crate::panic`]
//!    takes it and `join` reports it. Unwinding is not available here —
//!    compiled frames carry no unwind info, so a Rust panic crossing one
//!    aborts the process.

use crate::object::PickleObject;
use crate::shadow::{self, ShadowFrame};
use crate::world;
use std::sync::{Condvar, Mutex};

/// A task's terminal state. `finished` is the join predicate; `panicked` is set
/// when the body recorded a failure, so `join` can report it and the drain at
/// exit can surface it rather than running to completion as if nothing had
/// happened.
#[derive(Default)]
struct Outcome {
    finished: bool,
    panicked: bool,
    /// Whether a `join` has already reported this failure. An unjoined failure
    /// is the runtime's to report at exit; a joined one is the program's, and
    /// reporting it twice would be noise.
    reported: bool,
    message: Option<String>,
    /// Tasks currently parked in [`TaskState::wait`], counted under this same
    /// mutex so a wakeup cannot be lost between a waiter's check and its park.
    parked: u32,
}

/// Per-task state: the join predicate and the parking queue behind it. Handles
/// are indexes into [`TASKS`], so a task handle is a plain `int` in compiled
/// code with no GC representation.
struct TaskState {
    outcome: Mutex<Outcome>,
    /// Signalled when `finished` goes true, and only when a joiner is parked.
    done_cv: Condvar,
}

/// Every task spawned in this process, in handle order. Entries are never
/// removed: a `Task` value can outlive the task it names, and handles stay valid
/// for the life of the process.
static TASKS: Mutex<Vec<std::sync::Arc<TaskState>>> = Mutex::new(Vec::new());

/// A managed pointer that is safe to hand to a fresh OS thread. The pointee is
/// protected by the world lock, not by this wrapper, and the spawning thread
/// holds that lock until the task takes its own session.
struct SendPtr(*mut PickleObject);
unsafe impl Send for SendPtr {}

/// A zero-argument Pickle closure body: the hoisted `fn` the compiler pointed a
/// closure object at, called with the closure object as its hidden environment
/// argument. This is the `CallInd` shape `fn_value_call` emits.
type TaskFn = unsafe extern "C" fn(*mut PickleObject);

/// The task's own shadow frame: a `ShadowFrame` header with one inline slot, so
/// the frame the collector walks and the slot that roots the closure live in
/// one object.
#[repr(C)]
struct TaskFrame {
    header: ShadowFrame,
    slots: [*mut PickleObject; 1],
}

impl TaskFrame {
    fn new(env: *mut PickleObject) -> Self {
        TaskFrame {
            header: ShadowFrame {
                prev: core::ptr::null_mut(),
                slot_count: 1,
                _pad: 0,
            },
            slots: [env],
        }
    }

    fn frame(&mut self) -> *mut ShadowFrame {
        &mut self.header as *mut ShadowFrame
    }
}

/// Pops a shadow frame however the body ends.
struct FramePop(*mut ShadowFrame);

impl Drop for FramePop {
    fn drop(&mut self) {
        shadow::pickle_shadow_pop(self.0);
    }
}

/// ABI: run `body` (a zero-argument closure object) on a new OS thread and
/// return its task handle. Called from compiled code with the world lock held;
/// the task takes the lock for its own session once it gets there.
#[no_mangle]
pub extern "C" fn pickle_thread_spawn(body: *mut PickleObject) -> i64 {
    let state = std::sync::Arc::new(TaskState {
        outcome: Mutex::new(Outcome::default()),
        done_cv: Condvar::new(),
    });
    let handle = {
        let mut tasks = TASKS.lock().unwrap_or_else(|p| p.into_inner());
        tasks.push(std::sync::Arc::clone(&state));
        (tasks.len() - 1) as i64
    };
    let addr = crate::boxscalar::box_bits(crate::class::pickle_obj_slot_get(body, 0));
    let entry: TaskFn = unsafe { core::mem::transmute(addr as usize) };
    let worker = TaskRunner {
        state,
        entry,
        env: SendPtr(body),
    };
    // A task that cannot get a thread is a hard error, not a silent no-op: the
    // handle would then never finish and every `join` on it would hang.
    if std::thread::Builder::new()
        .name(format!("pickle-task-{handle}"))
        .spawn(move || worker.run())
        .is_err()
    {
        crate::panic::fatal("pickle: could not start a task thread");
    }
    handle
}

/// What the spawned OS thread needs: the shared state to report into, the
/// body's code address (read while the world lock is still the caller's) and
/// the closure object itself.
struct TaskRunner {
    state: std::sync::Arc<TaskState>,
    entry: TaskFn,
    env: SendPtr,
}

impl TaskRunner {
    /// The body proper: on a fresh OS thread, registered with the collector,
    /// holding the world lock for its session.
    fn run(self) {
        // Join the collector's registry so a collection — from this thread or
        // any other — walks this thread's shadow stack too.
        shadow::pickle_gc_register_thread();
        // A recoverable failure in the body belongs to this task, not to the
        // test that spawned it.
        crate::panic::set_task_mode(true);
        // Take the world lock for this task's session. The spawning thread has
        // held it since `pickle_thread_spawn`, so this is where the two hand it
        // over; from here the task runs compiled code and keeps the lock until
        // it blocks or finishes.
        world::acquire();
        let mut frame = TaskFrame::new(self.env.0);
        unsafe {
            shadow::pickle_shadow_push(frame.frame());
            // Pops the frame whether the body returns or a host unwind gets
            // here; a `panic` in a task is fatal to the process, so the body
            // only ever leaves by returning.
            let _pop = FramePop(frame.frame());
            (self.entry)(self.env.0);
        }
        // Whatever the body recorded on the way out (a failed `assert`) is the
        // task's result: `join` reports it, and nobody else will.
        let message = crate::panic::take_task_panic();
        crate::panic::set_task_mode(false);
        // Order matters: hand the world lock back and leave the registry before
        // the joiner is told, so a collection on another thread never walks a
        // frame that is being torn down, and so a thread that failed while
        // holding the lock cannot wedge its peers.
        world::release();
        shadow::pickle_gc_unregister_thread();
        self.state.finish(message.is_some(), message);
    }
}

impl TaskState {
    fn finish(&self, panicked: bool, message: Option<String>) {
        let mut outcome = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        outcome.finished = true;
        if panicked {
            outcome.panicked = true;
            outcome.message = message;
        }
        let wake = outcome.parked > 0;
        drop(outcome);
        if wake {
            self.done_cv.notify_all();
        }
    }

    /// Wait for the task to finish, parking without the world lock, and return
    /// whether it failed plus its message. This joiner owns the report: a later
    /// drain sees the failure as already reported.
    fn join(&self) -> (bool, Option<String>) {
        let mut outcome = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        while !outcome.finished {
            outcome.parked += 1;
            outcome = world::parked_wait(&self.done_cv, outcome);
            outcome.parked -= 1;
        }
        let failed = outcome.panicked;
        outcome.reported = true;
        (failed, outcome.message.clone())
    }

    /// Wait for the task to finish without claiming the report. Used by the
    /// drain at exit, which reports failures nobody joined.
    fn drain(&self) {
        let mut outcome = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        while !outcome.finished {
            outcome.parked += 1;
            outcome = world::parked_wait(&self.done_cv, outcome);
            outcome.parked -= 1;
        }
        if outcome.panicked && !outcome.reported {
            let message = outcome.message.clone();
            drop(outcome);
            crate::panic::fatal(&format!(
                "pickle: task failed: {}",
                message.unwrap_or_else(|| "no message".to_string())
            ));
        }
    }

    /// Snapshot of the outcome, without waiting.
    fn peek(&self) -> (bool, bool, Option<String>) {
        let outcome = self.outcome.lock().unwrap_or_else(|p| p.into_inner());
        (outcome.finished, outcome.panicked, outcome.message.clone())
    }
}

/// ABI: wait for task `handle` to finish. Returns 1 when the body ran without
/// recording a failure, 0 when it panicked (the message is then available from
/// `pickle_thread_panic_message`). An unknown handle reports success.
#[no_mangle]
pub extern "C" fn pickle_thread_join(handle: i64) -> bool {
    let Some(state) = task_state(handle) else {
        return true;
    };
    if state.peek().0 {
        return true;
    }
    let (failed, _message) = state.join();
    !failed
}

/// ABI: whether task `handle` has finished. Never blocks.
#[no_mangle]
pub extern "C" fn pickle_thread_finished(handle: i64) -> bool {
    task_state(handle).is_some_and(|s| s.peek().0)
}

/// ABI: the failure message task `handle` recorded, as a `string` object, or
/// null when the task is still running or finished cleanly.
#[no_mangle]
pub extern "C" fn pickle_thread_panic_message(handle: i64) -> *mut PickleObject {
    let Some(state) = task_state(handle) else {
        return core::ptr::null_mut();
    };
    let (finished, panicked, message) = state.peek();
    if !finished || !panicked {
        return core::ptr::null_mut();
    }
    let Some(message) = message else {
        return core::ptr::null_mut();
    };
    crate::strings::string_from_bytes(message.as_bytes().as_ptr(), message.len(), crate::gc::gc_mut())
}

/// ABI: this thread's id in the collector's registry: stable for the life of
/// the thread, 0 before registration. Distinct from a task handle, which names
/// a spawned body rather than a thread.
#[no_mangle]
pub extern "C" fn pickle_thread_current() -> i64 {
    let node = shadow::current_thread();
    if node.is_null() {
        0
    } else {
        unsafe { (*node).id as i64 }
    }
}

/// ABI: how many tasks have been spawned in this process.
#[no_mangle]
pub extern "C" fn pickle_thread_count() -> i64 {
    TASKS.lock().unwrap_or_else(|p| p.into_inner()).len() as i64
}

/// ABI: pause this task for `ms` milliseconds, handing the world lock to a
/// runnable peer while it sleeps.
#[no_mangle]
pub extern "C" fn pickle_thread_sleep_ms(ms: i64) {
    let ms = ms.clamp(0, 60_000) as u64;
    world::parked(|| std::thread::sleep(std::time::Duration::from_millis(ms)));
}

/// ABI: hand the world lock to a runnable peer, then take it back. A bare
/// `yield_now` is a scheduler hint rather than a hand-off, so this also parks
/// briefly: a peer that has not been scheduled yet would otherwise find the
/// lock free and take it straight back.
#[no_mangle]
pub extern "C" fn pickle_thread_yield() {
    world::parked(|| {
        std::thread::yield_now();
        std::thread::sleep(std::time::Duration::from_micros(50));
    });
}

fn task_state(handle: i64) -> Option<std::sync::Arc<TaskState>> {
    if handle < 0 {
        return None;
    }
    TASKS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(handle as usize)
        .cloned()
}

/// Wait for every spawned task to finish, parking between waits so the tasks
/// can run. Called from `pickle_runtime_shutdown`: a program's tasks outlive
/// its `main`, and the heap they touch is about to be torn down. A task that
/// failed is a loud error here — nobody joined it, so nothing reported it.
pub(crate) fn drain() {
    loop {
        let pending: Vec<std::sync::Arc<TaskState>> = {
            let tasks = TASKS.lock().unwrap_or_else(|p| p.into_inner());
            let pending: Vec<std::sync::Arc<TaskState>> = tasks
                .iter()
                .filter(|state| !state.peek().0)
                .cloned()
                .collect();
            if pending.is_empty() {
                return;
            }
            pending
        };
        for state in pending {
            state.drain();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc;
    use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    /// A closure object exactly as the emitter builds one: a one-slot class
    /// instance whose slot 0 is the boxed address of the body. Enough to drive
    /// the task machinery from a unit test without the compiler.
    fn closure(body: extern "C" fn(*mut PickleObject)) -> *mut PickleObject {
        let cid = {
            let g = gc::gc_mut();
            g.register_class(crate::object::ClassDescriptor {
                name_ptr: b"testclosure\0".as_ptr(),
                name_len: 11,
                flags: 0,
                slot_count: 1,
                mask_words: 0,
                managed_mask: core::ptr::null(),
                finalizer: crate::object::builtin_nop_finalizer,
            })
        };
        let obj = crate::class::pickle_class_new(cid as i64, 1);
        let addr = crate::boxscalar::pickle_box_i64(body as usize as i64);
        crate::class::pickle_obj_slot_set(obj, 0, addr);
        obj
    }

    /// Run `f` with the world lock free, the way compiled code does at a
    /// blocking point, and hand the lock back afterwards. A test that has
    /// touched the heap holds the lock for its whole session, and a task cannot
    /// start until it is handed over.
    fn with_world_free<T>(f: impl FnOnce() -> T) -> T {
        let mut out = None;
        world::parked(|| out = Some(f()));
        world::acquire();
        out.expect("parked ran the closure")
    }

    static RAN: AtomicUsize = AtomicUsize::new(0);
    static DID_WORK: AtomicI64 = AtomicI64::new(0);

    extern "C" fn body(env: *mut PickleObject) {
        RAN.fetch_add(1, Ordering::AcqRel);
        // A task body is compiled code: it uses the heap, which is what makes
        // the world lock load-bearing rather than decorative.
        let g = gc::gc_mut();
        DID_WORK.store(g.alloc(64, 0) as i64, Ordering::Release);
        let _ = env;
    }

    #[test]
    fn spawn_runs_the_body_and_join_reports_success() {
        let _guard = gc::test_begin();
        RAN.store(0, Ordering::Release);
        DID_WORK.store(0, Ordering::Release);
        with_world_free(|| {
            let handle = pickle_thread_spawn(closure(body));
            assert!(handle >= 0);
            assert!(pickle_thread_join(handle), "join reports success");
            assert!(pickle_thread_finished(handle));
        });
        assert_eq!(RAN.load(Ordering::Acquire), 1);
        assert_ne!(DID_WORK.load(Ordering::Acquire), 0, "the body allocated");
        // A task that has finished stays joinable, and re-joining keeps saying
        // the same thing.
        let second = pickle_thread_spawn(closure(body));
        with_world_free(|| {
            assert!(pickle_thread_join(second));
        });
    }

    #[test]
    fn finished_is_false_until_the_body_is_done() {
        let _guard = gc::test_begin();
        // A body that parks gives the test a window in which the task is
        // genuinely still running.
        static SLEPT: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn slow(_env: *mut PickleObject) {
            SLEPT.store(1, Ordering::Release);
            pickle_thread_sleep_ms(30);
            SLEPT.store(2, Ordering::Release);
        }
        SLEPT.store(0, Ordering::Release);
        let handle = with_world_free(|| pickle_thread_spawn(closure(slow)));
        // Wait for the body to actually be in flight, then check the predicate
        // reports "not finished" while it sleeps.
        for _ in 0..2_000 {
            if SLEPT.load(Ordering::Acquire) == 1 {
                break;
            }
            world::parked(std::thread::yield_now);
        }
        assert!(!pickle_thread_finished(handle), "still sleeping");
        with_world_free(|| {
            assert!(pickle_thread_join(handle));
        });
        assert!(pickle_thread_finished(handle));
        assert_eq!(SLEPT.load(Ordering::Acquire), 2);
    }

    #[test]
    fn a_recorded_assertion_fails_the_task_without_killing_it() {
        let _guard = gc::test_begin();
        static REACHED_END: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn failing(_env: *mut PickleObject) {
            // `assert(false, ...)` in a task: recorded, the body carries on, and
            // `join` reports the failure. The continuation is the point — a
            // task that died mid-body could not be joined at all.
            crate::test::pickle_test_fail_obj(core::ptr::null());
            REACHED_END.store(1, Ordering::Release);
        }

        REACHED_END.store(0, Ordering::Release);
        let handle = with_world_free(|| pickle_thread_spawn(closure(failing)));
        with_world_free(|| {
            assert!(!pickle_thread_join(handle), "join reports the failure");
        });
        assert_eq!(REACHED_END.load(Ordering::Acquire), 1, "body finished");
        let message = pickle_thread_panic_message(handle);
        assert!(!message.is_null(), "the failure message survives");
        let bytes = crate::strings::string_bytes_ptr(message);
        let len = crate::strings::string_bytes_len(message);
        let text = unsafe { String::from_utf8_lossy(core::slice::from_raw_parts(bytes, len)) };
        assert!(text.contains("assertion failed"), "got {text:?}");
    }

    #[test]
    fn a_task_does_not_capture_into_the_spawning_test() {
        // The bug this rules out: a task spawned by a test inherits the
        // harness' global capture mode, and its failure would be reported as
        // *that test's* failure.
        let _guard = gc::test_begin();
        crate::panic::set_capture_mode(true);
        static FAILED: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn failing(_env: *mut PickleObject) {
            crate::test::pickle_test_fail_obj(core::ptr::null());
            FAILED.store(1, Ordering::Release);
        }
        FAILED.store(0, Ordering::Release);
        let handle = with_world_free(|| pickle_thread_spawn(closure(failing)));
        with_world_free(|| {
            assert!(!pickle_thread_join(handle), "the task owns the failure");
        });
        assert_eq!(FAILED.load(Ordering::Acquire), 1);
        // The spawning test saw nothing.
        assert_eq!(crate::panic::take_captured_panic(), None);
        crate::panic::set_capture_mode(false);
    }

    #[test]
    fn current_thread_and_count() {
        let _guard = gc::test_begin();
        assert!(pickle_thread_current() > 0, "the test thread is registered");
        let before = pickle_thread_count();
        let handle = with_world_free(|| pickle_thread_spawn(closure(body)));
        assert_eq!(pickle_thread_count(), before + 1);
        with_world_free(|| {
            assert!(pickle_thread_join(handle));
        });
    }

    #[test]
    fn drain_waits_for_unjoined_tasks() {
        let _guard = gc::test_begin();
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn counter(_env: *mut PickleObject) {
            for _ in 0..3 {
                // Never joined: the drain at shutdown is the only thing that
                // waits for these, and it must not return early.
                pickle_thread_sleep_ms(1);
                COUNT.fetch_add(1, Ordering::AcqRel);
            }
        }
        COUNT.store(0, Ordering::Release);
        with_world_free(|| {
            for _ in 0..3 {
                pickle_thread_spawn(closure(counter));
            }
            // Hand the lock over and wait for the pool to finish.
            super::drain();
        });
        assert_eq!(COUNT.load(Ordering::Acquire), 9, "every task ran to the end");
    }

    #[test]
    fn sleep_and_yield_hand_the_world_lock_over() {
        let _guard = gc::test_begin();
        static PEER: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn peer(_env: *mut PickleObject) {
            PEER.store(1, Ordering::Release);
        }
        PEER.store(0, Ordering::Release);
        let handle = with_world_free(|| pickle_thread_spawn(closure(peer)));
        // This thread holds the world lock; the peer cannot finish until one of
        // these blocking points hands it over.
        pickle_thread_sleep_ms(5);
        pickle_thread_yield();
        with_world_free(|| {
            assert!(pickle_thread_join(handle));
        });
        assert_eq!(PEER.load(Ordering::Acquire), 1, "the peer ran while parked");
    }
}
