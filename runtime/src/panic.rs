//! Panic path: language-level `panic(msg)` and a guard for Rust panics
//! escaping `extern "C"` boundaries.
//!
//! In capture mode (the test harness) panics become recorded failures so a
//! failing `test fn` body can be reported without aborting the process.
//!
//! Compiled frames carry no unwind info, so nothing on this path may unwind a
//! Rust panic through one (see [`crate::test`]). That splits failures in two:
//!
//! - *Recoverable*: `assert` (and the test hook) records a message and the
//!   compiled body carries on, which is already how [`crate::test`] works. A
//!   task body is in this class too â€” a task's failure is recorded on the
//!   thread ([`take_task_panic`]) so `join` can report it, and the task is
//!   checked for that first, before the test harness' global capture mode.
//! - *Fatal*: `panic`, a `none` unwrap, a non-exhaustive `match`, a bad spread
//!   arity. The emitter models these as a void call on a path that cannot
//!   continue, so they must genuinely diverge: they print and exit, exactly as
//!   a panic in a Go goroutine that never recovers does.

use alloc::string::String;
#[cfg(not(feature = "nostd"))]
use std::io::Write;

/// Fatal message prefix written to stderr.
const FATAL: &[u8] = b"fatal: ";

/// Capture mode: turn fatal panics into recoverable, recorded failures.
static CAPTURE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

thread_local! {
    static LAST_PANIC: core::cell::RefCell<Option<String>> =
        const { core::cell::RefCell::new(None) };
    /// Whether this thread is a task body ([`crate::threads`]). Checked before
    /// [`CAPTURE`]: a task spawned from a running test inherits the harness'
    /// capture mode, but its failure belongs to the task, not to that test.
    static TASK_MODE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The message a task body recorded, waiting for its `join`.
    static TASK_PANIC: core::cell::RefCell<Option<String>> =
        const { core::cell::RefCell::new(None) };
}

/// Whether panics are being captured for the test harness.
pub fn is_capturing() -> bool {
    CAPTURE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Enter/leave capture mode. Leaving also clears any captured message.
pub fn set_capture_mode(capture: bool) {
    CAPTURE.store(capture, core::sync::atomic::Ordering::Relaxed);
    if !capture {
        LAST_PANIC.with(|c| *c.borrow_mut() = None);
    }
}

/// The message of the most recent captured panic, if any.
pub fn take_captured_panic() -> Option<String> {
    LAST_PANIC.with(|c| c.borrow_mut().take())
}

/// ABI: `captureBegin()` from the ported test harness. Enters capture mode so
/// a failing test body records a message instead of killing the process. Does
/// not clear a pending message: a failure recorded before the begin (e.g. by
/// a hook) must survive until the harness' `captureTake()`.
#[no_mangle]
pub extern "C" fn pickle_test_capture_begin() {
    set_capture_mode(true);
}

/// ABI: `captureTake()` from the ported test harness. Exits nothing and
/// returns the message of the most recent captured failure as a `string?`
/// object (null when none), or null if capture is off.
#[no_mangle]
pub extern "C" fn pickle_test_capture_take() -> *mut crate::object::PickleObject {
    if is_capturing() {
        if let Some(msg) = take_captured_panic() {
            return crate::strings::string_from_bytes(
                msg.as_bytes().as_ptr(),
                msg.len(),
                crate::gc::gc_mut(),
            );
        }
    }
    core::ptr::null_mut()
}

fn record_panic(msg: String) {
    LAST_PANIC.with(|c| *c.borrow_mut() = Some(msg));
}

/// Record a captured failure message (used by the test harness' `assert`
/// lowering without unwinding through the JIT frame).
pub(crate) fn record_panic_msg(msg: impl Into<String>) {
    record_panic(msg.into());
}

/// Whether this thread is running a task body, so a recoverable failure is
/// recorded for that task's `join` instead of the test harness.
pub fn in_task_mode() -> bool {
    TASK_MODE.with(|m| m.get())
}

/// Mark this thread as a task body, or leave task mode. Entering clears any
/// message left over; leaving hands the message back, so a task that records a
/// failure and is then joined twice still reports it once.
pub fn set_task_mode(task: bool) {
    TASK_MODE.with(|m| m.set(task));
    if task {
        TASK_PANIC.with(|p| *p.borrow_mut() = None);
    }
}

/// The message this task body recorded, if any. Consumed by
/// [`crate::threads`] when the task finishes.
pub(crate) fn take_task_panic() -> Option<String> {
    TASK_PANIC.with(|p| p.borrow_mut().take())
}

/// Record a *recoverable* failure â€” one whose compiled call site is allowed to
/// return and carry on (`assert`, the test hook). Returns whether it was
/// recorded, i.e. whether the caller should return instead of failing hard.
///
/// A task records to its own slot first: a task spawned by a test inherits
/// capture mode, but the failure is the task's and belongs to its `join`.
pub fn record_recoverable(msg: impl Into<String>) -> bool {
    let msg = msg.into();
    if in_task_mode() {
        TASK_PANIC.with(|p| *p.borrow_mut() = Some(msg));
        return true;
    }
    if is_capturing() {
        record_panic(msg);
        return true;
    }
    false
}

/// Write the fatal banner plus `msg` and exit 1. Used outside capture mode
/// by `panic` and failing `assert`s in non-test code.
pub(crate) fn fatal(msg: &str) -> ! {
    #[cfg(not(feature = "nostd"))]
    {
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = lock.write_all(FATAL);
        let _ = lock.write_all(msg.as_bytes());
        let _ = lock.write_all(b"\n");
        let _ = lock.flush();
        std::process::exit(1);
    }
    #[cfg(feature = "nostd")]
    {
        crate::nostd::console_write(FATAL);
        crate::nostd::console_write(msg.as_bytes());
        crate::nostd::console_write(b"\n");
        crate::nostd::exit(1);
    }
}

#[cfg(not(feature = "nostd"))]
fn panic_text(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{info}")
    }
}

/// ABI: `panic("...")` from PickleScript. Always fatal: the emitter puts this
/// call on a path that cannot continue, so returning would hand compiled code an
/// uninitialised value, and unwinding is not available through a JIT frame.
/// A test that wants a recoverable failure uses `assert`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_panic_bytes(ptr: *const u8, len: usize) -> ! {
    let msg = if ptr.is_null() || len == 0 {
        String::new()
    } else {
        unsafe { String::from_utf8_lossy(core::slice::from_raw_parts(ptr, len)).into_owned() }
    };
    if in_task_mode() {
        // Name the task in the message: the failing body is otherwise invisible
        // in a program with many of them.
        fatal(&format!("pickle: task failed: {msg}"));
    }
    fatal(&msg);
}

/// ABI: `panic("...")` from a NUL-terminated C string.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_panic_cstr(ptr: *const u8) -> ! {
    let mut len = 0usize;
    unsafe {
        while !ptr.is_null() && *ptr.add(len) != 0 {
            len += 1;
        }
    }
    pickle_panic_bytes(ptr, len)
}

/// ABI: a `match` whose scrutinee matched no arm. Reuses the panic path so
/// compiled programs fail loudly instead of reading uninitialised state.
#[no_mangle]
pub extern "C" fn pickle_panic_no_match() -> ! {
    pickle_panic_cstr(b"pickle: match is not exhaustive\0".as_ptr())
}

/// ABI: a `!` unwrap applied to a `none` option. Reuses the panic path so
/// compiled programs fail loudly instead of reading null as a value.
#[no_mangle]
pub extern "C" fn pickle_panic_none_unwrap() -> ! {
    pickle_panic_cstr(b"pickle: unwrapped none value\0".as_ptr())
}

/// ABI: an interface-method dispatch whose receiver implements no such
/// method at runtime (an unregistered/foreign class path). Same panic
/// contract: fail loudly in the dispatch path.
#[no_mangle]
pub extern "C" fn pickle_panic_no_iface_method() -> ! {
    pickle_panic_cstr(b"pickle: interface method dispatch found no implementation\0".as_ptr())
}

/// ABI: a call built from a spread `...args` list whose runtime element count
/// does not match the number of parameters it must fill. The compiler emits a
/// static arity check before the call; this fires only when a spread list's
/// length disagrees at runtime.
#[no_mangle]
pub extern "C" fn pickle_panic_arity_diff() -> ! {
    pickle_panic_cstr(
        b"pickle: spread list does not match the expected number of arguments\0".as_ptr(),
    )
}

/// Replace the Rust panic hook so an internal panic cannot unwind across the
/// `extern "C"` ABI. It prints `internal error: <msg>` and exits 1.
///
/// In capture mode a panic is recorded instead (for the test harness), so
/// `pickle test` can report a failing test without aborting the process.
/// Under `cfg(test)` this is a no-op: the Rust test harness needs panics to
/// unwind to report failures itself.
#[cfg(all(not(test), not(feature = "nostd")))]
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = panic_text(info);
        if is_capturing() {
            record_panic(msg);
            return;
        }
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = lock.write_all(b"internal error: ");
        let _ = lock.write_all(msg.as_bytes());
        let location = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let _ = lock.write_all(location.as_bytes());
        let _ = lock.write_all(b"\n");
        let _ = lock.flush();
        std::process::exit(1);
    }));
}

/// No-op on a freestanding build: the `#[panic_handler]` in [`crate::nostd`]
/// is the only panic path there.
#[cfg(feature = "nostd")]
pub fn install_panic_hook() {}

#[cfg(all(test, not(feature = "nostd")))]
pub fn install_panic_hook() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_installs() {
        install_panic_hook();
        // The hook is active; calling the old default would print. Just make
        // sure re-installation doesn't blow up.
        install_panic_hook();
    }

    #[test]
    fn capture_records_and_clears_message() {
        assert!(!is_capturing());
        set_capture_mode(true);
        let result = std::panic::catch_unwind(|| {
            record_panic("boom".to_string());
        });
        assert!(result.is_ok());
        assert_eq!(take_captured_panic().as_deref(), Some("boom"));
        assert_eq!(take_captured_panic(), None, "message consumed once");
        set_capture_mode(false);
        assert!(!is_capturing());
    }
}
