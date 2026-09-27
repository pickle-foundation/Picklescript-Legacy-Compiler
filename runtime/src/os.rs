//! `std.os` intrinsics: the program's command-line arguments (`args()`) and
//! in-language process exit (`exit(int)`).
//!
//! The argument vector is captured once at startup: the AOT entry
//! (`runtime::main`) forwards the real `_argc/_argv`, and the JIT driver
//! forwards the CLI's trailing `--` arguments. `args()` snapshots it as a
//! fresh `List<string>` (program name excluded, matching `argv[1..]`).

use crate::object::PickleObject;
use crate::strings::string_from_bytes;
use alloc::vec::Vec;
#[cfg(not(feature = "nostd"))]
use std::sync::Mutex;
#[cfg(feature = "nostd")]
use crate::nostd::sync::Mutex;

/// Owned copy of the program's arguments, excluding the program name.
static PROGRAM_ARGS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Install the stored argument vector from an `argc`/`argv` pair (the process
/// entry forwards the real one; the JIT driver forwards its trailing `--` args).
/// The runtime copies the bytes, so the caller's pointers may be freed after
/// this call returns. Dereferences and reads `argv`.
///
/// # Safety
/// `argv` must point to a valid array of `argc` pointers, each of which must
/// point to a NUL-terminated byte string that is valid for reads for `argc`
/// steps. `argv[0]` (the program name) is skipped; `args()` returns `argv[1..]`.
/// In practice the process entry passes the real C `argv` (which obeys the
/// usual process-entry validity rules), and the JIT driver passes a `Vec` of
/// `CString` that outlives this call. The runtime only needs the strings valid
/// for the duration of this call, since it copies them.
#[no_mangle]
pub unsafe extern "C" fn pickle_args_set(argc: usize, argv: *const *const u8) {
    let mut stored = PROGRAM_ARGS.lock().unwrap();
    stored.clear();
    if argc == 0 || argv.is_null() {
        return;
    }
    // Skip argv[0] (the program name); `args()` returns argv[1..].
    for i in 1..argc {
        let p = unsafe { *argv.add(i) };
        if p.is_null() {
            continue;
        }
        let mut len = 0usize;
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
        }
        let bytes = unsafe { core::slice::from_raw_parts(p, len) };
        stored.push(bytes.to_vec());
    }
}

/// Reset the stored argument vector (used by the runtime reset between
/// test-module runs so an earlier module's `--` args do not leak through).
pub(crate) fn reset() {
    PROGRAM_ARGS.lock().unwrap().clear();
}

/// `args() -> List<string>`: the program's command-line arguments, excluding
/// the program name. Returns a fresh list on every call.
#[no_mangle]
pub extern "C" fn pickle_args() -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let l = crate::list::list_new(0, gc);
    let _lease = gc.temp_lease(l);
    let stored = PROGRAM_ARGS.lock().unwrap();
    for arg in stored.iter() {
        let s = string_from_bytes(arg.as_ptr(), arg.len(), gc);
        crate::list::list_push(l, s);
    }
    l
}

/// `exit(code)`: flush buffered output and terminate the process with
/// `code`. Guaranteed not to return.
#[no_mangle]
pub extern "C" fn pickle_exit(code: i64) -> ! {
    crate::console::pickle_flush();
    #[cfg(not(feature = "nostd"))]
    std::process::exit(code.clamp(i32::MIN as i64, i32::MAX as i64) as i32);
    #[cfg(feature = "nostd")]
    unsafe { crate::nostd::ExitProcess(code.clamp(0, u32::MAX as i64) as u32) }
}
