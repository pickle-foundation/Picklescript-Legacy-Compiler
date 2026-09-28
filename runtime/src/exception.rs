//! `throw` / `try` / `catch`: a cooperative, GC-sound propagation channel.
//!
//! Compiled frames carry no unwind information (see [`crate::panic`]), so a
//! `throw` cannot skip a frame: doing so would leave the collector' shadow
//! stack pointing at reclaimed machine stack ([`crate::shadow`]) and would have
//! to abandon a `Rust` frame mid-call. Instead propagation *cooperates*:
//!
//! 1. `throw v` stores `v` in a thread-local and raises a per-thread pending
//!    flag. It does not return a value and does not branch.
//! 2. The emitter checks the flag after every call. When it is set the
//!    enclosing function returns immediately, so each frame between the throw
//!    and the handler exits *normally* and pops its own shadow frame.
//! 3. The `catch` that matches the flag consumes it and binds the payload.
//!
//! Every frame therefore leaves in order, which keeps the shadow stack balanced
//! and makes an in-flight payload the only object a GC would otherwise miss.

use crate::object::PickleObject;
use alloc::string::{String, ToString};
use core::cell::Cell;
#[cfg(not(feature = "nostd"))]
use std::io::Write;

thread_local! {
    /// Whether a throw is in flight on this thread. Read after every call, so
    /// it is a single relaxed cell read on the propagation path.
    static PENDING: Cell<bool> = const { Cell::new(false) };
    /// The payload of the in-flight throw. Rooted through
    /// [`crate::gc::Gc::temp_root`] while pending: the frames that referenced it
    /// have already returned, so nothing else traces it.
    static PAYLOAD: Cell<*mut PickleObject> =
        const { Cell::new(core::ptr::null_mut()) };
}

/// Whether a throw is currently propagating on this thread.
pub fn is_pending() -> bool {
    PENDING.with(|f| f.get())
}

/// ABI: `throw v`. Publishes `v` as the in-flight payload and raises this
/// thread's pending flag. Returns normally: the emitter places it on a path
/// that branches away, and a caller checks [`pickle_exc_pending`] after each
/// call, so the function that threw is already unwinding by the time this
/// returns.
#[no_mangle]
pub extern "C" fn pickle_exc_throw(payload: *mut PickleObject) {
    if !payload.is_null() {
        crate::gc::gc_mut().temp_root(payload);
    }
    let old = PAYLOAD.with(|p| p.replace(payload));
    if !old.is_null() && old != payload {
        crate::gc::gc_mut().temp_unroot(old);
    }
    PENDING.with(|f| f.set(true));
}

/// ABI: is a throw in flight on this thread? Emitted after every call so an
/// enclosing function can return before it starts using a failed result.
#[no_mangle]
pub extern "C" fn pickle_exc_pending() -> bool {
    PENDING.with(|f| f.get())
}

/// ABI: consume the in-flight throw, returning its payload (null if none) and
/// clearing the flag. Emitted by `catch`, which has already stored the payload
/// where the handler can read it.
#[no_mangle]
pub extern "C" fn pickle_exc_take() -> *mut PickleObject {
    let payload = PAYLOAD.with(|p| p.replace(core::ptr::null_mut()));
    PENDING.with(|f| f.set(false));
    if !payload.is_null() {
        crate::gc::gc_mut().temp_unroot(payload);
    }
    payload
}

/// ABI: drop an in-flight throw without handling it, returning whether one was
/// there. The emitter uses this on the path out of a `try` that has no `catch`
/// and did not match, so the flag does not leak into an unrelated later call.
#[no_mangle]
pub extern "C" fn pickle_exc_clear() -> bool {
    if !PENDING.with(|f| f.get()) {
        return false;
    }
    let _ = pickle_exc_take();
    true
}

/// ABI: report a throw that escaped the program and exit 1, or return normally
/// when nothing escaped.
///
/// A module entry point calls this exactly once, after `main` returns. The
/// native [`crate::Program::run`] has a host to ask whether a throw came out the
/// top, which is how `cli` reports one. A self-hosted compiler generates its own
/// `pickle_main` and has no such host, so the generated code calls this instead
/// and the decision lives here: only the runtime can consume the flag and render
/// the payload.
///
/// Returns normally when the pending flag is clear, so a program that threw
/// nothing pays a single cell read. Exits 1 after reporting, matching how the
/// native entry points report an escaped payload.
#[no_mangle]
pub extern "C" fn pickle_exc_uncaught() {
    let Some(msg) = take_escaped() else {
        return;
    };
    report_escaped(&msg);
}

/// Write `uncaught exception: <msg>` and exit 1.
///
/// Deliberately not [`crate::panic::fatal`]: that prefixes `fatal: `, and the
/// native entry points report an escaped throw without a banner, so routing this
/// through `fatal` would make a self-hosted binary's stderr differ from the
/// native one for the same program.
fn report_escaped(msg: &str) -> ! {
    #[cfg(not(feature = "nostd"))]
    {
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = lock.write_all(b"uncaught exception: ");
        let _ = lock.write_all(msg.as_bytes());
        let _ = lock.write_all(b"\n");
        let _ = lock.flush();
        std::process::exit(1);
    }
    #[cfg(feature = "nostd")]
    {
        // Not `eprintln!`: on a freestanding build that shim writes to stdout,
        // and the channel is part of the contract here. A host that runs a
        // self-host binary and discards stdout would otherwise see a failed run
        // reported as a clean exit, so this writes to the error handle directly.
        crate::nostd::console_error_write(b"uncaught exception: ");
        crate::nostd::console_error_write(msg.as_bytes());
        crate::nostd::console_error_write(b"\n");
        crate::nostd::exit(1);
    }
}

/// Drop any in-flight throw, returning a message if one escaped to the top of a
/// module. Called by the program entry points, where a payload nobody caught is
/// a failure to report rather than a flag to clear.
pub fn take_escaped() -> Option<String> {
    if !PENDING.with(|f| f.get()) {
        return None;
    }
    let payload = pickle_exc_take();
    if payload.is_null() {
        return Some("uncaught exception".to_string());
    }
    Some(describe(payload))
}

/// Render a payload as text for an uncaught-exception report.
fn describe(obj: *mut PickleObject) -> String {
    let bytes = crate::strings::string_bytes_ptr(obj);
    let len = crate::strings::string_bytes_len(obj);
    if bytes.is_null() {
        return "uncaught exception".to_string();
    }
    unsafe { String::from_utf8_lossy(core::slice::from_raw_parts(bytes, len)).into_owned() }
}

/// Forget any in-flight throw, so a previous module's leftover cannot be
/// mistaken for this module's.
///
/// Deliberately does not touch the GC: this runs while the heap is being torn
/// down, when the payload's root is about to disappear with it.
pub fn reset() {
    PAYLOAD.with(|p| p.set(core::ptr::null_mut()));
    PENDING.with(|f| f.set(false));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Initialise a heap for the test and assert the in-flight state is empty
    /// on both entry and exit, so a failure cannot leak into the next test.
    fn begin() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::gc::test_begin();
        crate::pickle_runtime_init();
        reset();
        guard
    }

    fn message(text: &[u8]) -> *mut PickleObject {
        crate::strings::string_from_bytes(text.as_ptr(), text.len(), crate::gc::gc_mut())
    }

    #[test]
    fn throw_then_take_round_trips() {
        let _guard = begin();
        let msg = message(b"boom");
        pickle_exc_throw(msg);
        assert!(is_pending());
        assert!(pickle_exc_pending());
        assert_eq!(pickle_exc_take(), msg);
        assert!(!is_pending(), "take clears the flag");
        assert!(pickle_exc_take().is_null(), "payload consumed once");
    }

    #[test]
    fn a_thrown_payload_is_rooted_until_it_is_taken() {
        let _guard = begin();
        let msg = message(b"rooted");
        pickle_exc_throw(msg);
        assert!(
            crate::gc::gc_mut().temp_root_contains_for_test(msg),
            "an in-flight payload has no frame left to trace it"
        );
        pickle_exc_take();
        assert!(!crate::gc::gc_mut().temp_root_contains_for_test(msg));
    }

    #[test]
    fn clear_reports_whether_a_throw_was_in_flight() {
        let _guard = begin();
        let msg = message(b"x");
        assert!(!pickle_exc_clear());
        pickle_exc_throw(msg);
        assert!(pickle_exc_clear());
        assert!(!is_pending());
        assert!(pickle_exc_take().is_null());
    }

    #[test]
    fn uncaught_check_is_a_no_op_when_nothing_escaped() {
        let _guard = begin();
        // Only the quiet half of `pickle_exc_uncaught` is unit testable: the
        // reporting half exits the process, which would take the test runner
        // with it. That half is covered end to end instead, by linking a
        // self-host module and running it. What matters here is that an entry
        // point calling this on a program that threw nothing returns, and
        // leaves the state untouched so a later throw is not mistaken for an
        // escape.
        pickle_exc_uncaught();
        assert!(!is_pending());
        assert!(pickle_exc_take().is_null());
    }

    #[test]
    fn a_null_payload_is_still_a_throw() {
        let _guard = begin();
        pickle_exc_throw(core::ptr::null_mut());
        assert!(is_pending());
        assert!(pickle_exc_take().is_null());
        assert!(!is_pending());
    }

    #[test]
    fn throwing_twice_releases_the_first_payload() {
        let _guard = begin();
        let first = message(b"a");
        let second = message(b"b");
        pickle_exc_throw(first);
        pickle_exc_throw(second);
        assert_eq!(pickle_exc_take(), second, "last throw wins");
        assert!(!crate::gc::gc_mut().temp_root_contains_for_test(first));
        assert!(!crate::gc::gc_mut().temp_root_contains_for_test(second));
    }

    #[test]
    fn escaped_throw_is_reported_once() {
        let _guard = begin();
        assert!(take_escaped().is_none());
        pickle_exc_throw(message(b"gone"));
        assert_eq!(take_escaped().as_deref(), Some("gone"));
        assert!(take_escaped().is_none(), "reported once");
    }
}
