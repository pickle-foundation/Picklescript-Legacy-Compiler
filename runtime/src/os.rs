//! `std.os` intrinsics: the program's command-line arguments (`args()`) and
//! in-language process exit (`exit(int)`).
//!
//! The argument vector is captured once at startup: the AOT entry
//! (`runtime::main`) forwards the real `_argc/_argv`, and the JIT driver
//! forwards the CLI's trailing `--` arguments. `args()` snapshots it as a
//! fresh `List<string>` (program name excluded, matching `argv[1..]`).

#[cfg(feature = "nostd")]
use crate::nostd::sync::Mutex;
use crate::object::PickleObject;
use crate::strings::string_from_bytes;
use alloc::vec::Vec;
#[cfg(not(feature = "nostd"))]
use std::sync::Mutex;

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

/// `nostd` build on Windows only: capture the real process command line into
/// [`PROGRAM_ARGS`]. The freestanding Windows exe has no CRT to forward
/// `_argc/_argv` through `pickle_args_set`, so the boot stub asks the OS
/// directly: get the wide command line, split it the way the CRT would, and
/// drop `argv[0]` (the program name) so `args()` stays consistent with the
/// `entry`-build contract.
///
/// Everywhere else the vector arrives the usual way: the `entry` build's
/// `main(argc, argv)` (and the JIT driver's trailing `--` forwarding) call
/// `pickle_args_set`, so no OS query is needed on POSIX/macOS or their wider
/// CLI paths.
///
/// Safe to call even with zero arguments; `PROGRAM_ARGS` is cleared first so a
/// repeated init cannot leak a previous program's vector.
#[cfg(all(feature = "nostd", target_os = "windows"))]
pub(crate) fn capture_from_windows() {
    unsafe {
        let cmd = crate::nostd::GetCommandLineW();
        if cmd.is_null() {
            return;
        }
        let mut argc: i32 = 0;
        let argv = crate::nostd::CommandLineToArgvW(cmd, &mut argc);
        if argv.is_null() {
            return;
        }
        let mut stored = PROGRAM_ARGS.lock().unwrap();
        stored.clear();
        if argc > 1 {
            for i in 1..argc {
                let p = *argv.add(i as usize);
                if p.is_null() {
                    continue;
                }
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let units = core::slice::from_raw_parts(p, len);
                let s = alloc::string::String::from_utf16_lossy(units);
                stored.push(s.into_bytes());
            }
        }
        crate::nostd::LocalFree(argv as *mut core::ffi::c_void);
    }
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
    unsafe {
    crate::nostd::ExitProcess(code.clamp(0, u32::MAX as i64) as u32)
    }
    }

/// `current_dir()`: the process working directory, or `none`.
///
/// The body lives in `process` for `std` builds and in `nostd` for freestanding
/// ones. `declare_abi!` can only name each ABI symbol once -- its name list
/// ignores the per-entry `#[cfg]`, so listing the same name twice would trip the
/// uniqueness check -- so the table points here and this module picks the arm.
#[cfg(not(feature = "nostd"))]
pub use crate::process::pickle_current_dir;
#[cfg(feature = "nostd")]
pub use crate::nostd::pickle_current_dir;
