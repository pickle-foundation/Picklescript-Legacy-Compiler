//! `std.process` intrinsics: subprocess spawning, capture, and handles.
//!
//! Two entry points:
//! * `pickle_run_cmd(...)` runs a program to completion and snapshots its
//!   exit code (an `int` box, `none` on signal death), stdout, stderr, and a
//!   timeout flag as a 4-field `(int?, string, string, bool)` tuple — or
//!   `none` when the program could not be spawned at all.
//! * `pickle_run_spawn(...)` overlays a `Stream` value onto a living child:
//!   the handle reads drained output asynchronously (`proc_out_read` /
//!   `proc_err_read` poll the buffers without blocking, `none` at EOF), writes
//!   to the child's stdin (`proc_in_write` / `proc_in_close`), and queries
//!   `proc_running` / `proc_wait` / `proc_kill` / `proc_code`.
//!
//! The child's stdout/stderr are always drained by reader threads into
//! `Arc<Mutex<Vec<u8>>>` buffers (bounded by `max_out`: overflow is read and
//! discarded, never blocking the child), so a capture may never deadlock on a
//! full pipe. Strings are opaque UTF-8 exactly like `read_file`; byte lists
//! round-trip binary.
//!
//! Processes are never automatically reaped while the handle is alive; the
//! `Stream` finalizer runs `ProcInner`'s `Drop`, which kills and reaps an
//! un-waited child so a dropped handle cannot leave a zombie behind, while
//! `wait()` reaps cleanly (the drop then becomes a no-op).

use crate::boxscalar::box_bits;
use crate::layout::{list_data, list_len, map_cap, map_entries, MapEntry};
use crate::object::{nil_sentinel, none_key, PickleObject};
use crate::strings::{string_bytes_len, string_bytes_ptr, string_from_bytes};
use core::sync::atomic::{AtomicBool, Ordering};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Default capture bound for stdout and stderr (16 MiB each). Callers may
/// pass a tighter bound; higher data is drained and discarded.
const DEFAULT_MAX_OUT: usize = 16 * 1024 * 1024;

/// The boxed state behind a process-handle `Stream`.
pub(crate) struct ProcInner {
    /// The child, held until `wait()` reaps it (or the box is dropped).
    child: Option<Child>,
    /// Exit code recorded by `wait()`; `None` for signal deaths.
    code: Option<i64>,
    /// Whether the child has been reaped by `wait()` (or reported finished).
    reaped: bool,
    /// Drained child stdout, capped at `cap` bytes.
    out: Arc<Mutex<Vec<u8>>>,
    /// Drained child stderr, capped at `cap` bytes.
    err: Arc<Mutex<Vec<u8>>>,
    /// EOF/poll flags for the drain contexts.
    out_eof: Arc<AtomicBool>,
    err_eof: Arc<AtomicBool>,
    /// The child's stdin, taken by `proc_in_close`.
    stdin: Option<std::process::ChildStdin>,
    /// Per-pipe capture bound in bytes.
    cap: usize,
    /// Drainer threads; `wait()` joins them (EOF == child exit).
    out_thread: Option<JoinHandle<()>>,
    err_thread: Option<JoinHandle<()>>,
}

/// Dropping a live child kills and reaps it, so a swept/closed handle cannot
/// leak a zombie. After a clean `wait()` the child is already taken and this
/// is a no-op; reader threads hold their own `Arc` clones of the buffers.
impl Drop for ProcInner {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// The managed string payload of an object (must be a `string` object).
fn os_arg(obj: *const PickleObject) -> String {
    let bytes =
        unsafe { core::slice::from_raw_parts(string_bytes_ptr(obj), string_bytes_len(obj)) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Snapshot a `List<string>` argv.
fn argv_of(argv: *mut PickleObject) -> Option<Vec<String>> {
    if argv.is_null() {
        return None;
    }
    let n = list_len(argv);
    let data = list_data(argv);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let e = unsafe { data.add(i).read() };
        if e.is_null() {
            continue;
        }
        out.push(os_arg(e));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Apply `env_removes` (applied first) then `envs` onto a fresh child env.
fn apply_env(cmd: &mut Command, envs: *mut PickleObject, env_removes: *mut PickleObject) {
    if !env_removes.is_null() {
        let n = list_len(env_removes);
        let data = list_data(env_removes);
        for i in 0..n {
            let e = unsafe { data.add(i).read() };
            if e.is_null() {
                continue;
            }
            cmd.env_remove(os_arg(e));
        }
    }
    if !envs.is_null() {
        // Open addressing: live entries are the non-tombstone slots.
        let cap = map_cap(envs);
        let entries = map_entries(envs);
        for i in 0..cap {
            let e: MapEntry = unsafe { *entries.add(i) };
            if e.key.is_null()
                || core::ptr::eq(e.key, nil_sentinel())
                || core::ptr::eq(e.key, none_key())
            {
                continue;
            }
            let k = os_arg(e.key);
            let v = os_arg(e.value);
            cmd.env(k, v);
        }
    }
}

/// Snapshot a `List<byte>`.
fn byte_vec_of(l: *mut PickleObject) -> Vec<u8> {
    if l.is_null() {
        return Vec::new();
    }
    let n = list_len(l);
    let data = list_data(l);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let e = unsafe { data.add(i).read() };
        if e.is_null() {
            continue;
        }
        out.push(box_bits(e) as u8);
    }
    out
}

/// Wait for `child`, killing it once `ms` (if any) has elapsed. Returns the
/// exit code (`none` on signal death) and whether the timeout fired.
fn wait_with_timeout(child: &mut Child, timeout_ms: Option<i64>) -> (Option<i32>, bool) {
    let Some(ms) = timeout_ms else {
        return match child.wait() {
            Ok(status) => (status.code(), false),
            Err(_) => (None, false),
        };
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms.max(0) as u64);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code(), false),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return match child.wait() {
                        Ok(status) => (status.code(), true),
                        Err(_) => (None, true),
                    };
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(_) => return (None, false),
        }
    }
}

/// Reader thread: drain `r` into `buf`, capping at `cap`, and flag EOF when
/// the pipe closes (which follows child exit).
fn drainer<W>(
    mut r: W,
    buf: Arc<Mutex<Vec<u8>>>,
    eof: Arc<AtomicBool>,
    cap: usize,
) -> JoinHandle<()>
where
    W: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut tmp = [0u8; 8192];
        while let Ok(n) = r.read(&mut tmp) {
            if n == 0 {
                break;
            }
            let mut g = buf.lock().unwrap();
            let room = cap.saturating_sub(g.len());
            if room > 0 {
                let take = n.min(room);
                g.extend_from_slice(&tmp[..take]);
            }
        }
        eof.store(true, Ordering::SeqCst);
    })
}

/// Build the `(code:int?, out:string, err:string, timed:bool)` capture tuple.
fn cmd_result(code: Option<i64>, out: &[u8], err: &[u8], timed: bool) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let t = crate::tuple::tuple_new(4, gc);
    let _lease = gc.temp_lease(t);
    let code_slot = match code {
        Some(c) => crate::boxscalar::pickle_box_i64(c),
        None => core::ptr::null_mut(),
    };
    crate::tuple::tuple_set_field(t, 0, code_slot);
    let out_s = string_from_bytes(out.as_ptr(), out.len(), gc);
    crate::tuple::tuple_set_field(t, 1, out_s);
    let err_s = string_from_bytes(err.as_ptr(), err.len(), gc);
    crate::tuple::tuple_set_field(t, 2, err_s);
    let timed_s = crate::boxscalar::pickle_box_bool(timed);
    crate::tuple::tuple_set_field(t, 3, timed_s);
    t
}

/// A shared `Command` with the common program/env/cwd applied.
fn base_command(
    argv: *mut PickleObject,
    cwd: *const PickleObject,
    envs: *mut PickleObject,
    env_removes: *mut PickleObject,
) -> Option<Command> {
    let args = argv_of(argv)?;
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    if !cwd.is_null() {
        cmd.current_dir(PathBuf::from(os_arg(cwd)));
    }
    apply_env(&mut cmd, envs, env_removes);
    Some(cmd)
}

/// Unwrap an optional `int` argument (`none`/boxed), defaulting when absent.
fn opt_int(v: *const PickleObject) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    Some(box_bits(v))
}

/// `run_cmd(argv, cwd, envs, env_removes, stdin_bytes, max_out, timeout_ms,
/// inherit_io)`: run to completion.
///
/// Returns `(int?, string, string, bool)` with the exit code (`none` on a
/// signal death), stdout, stderr, and whether a timeout fired — or `none`
/// when the program could not be spawned.
///
/// `inherit_io` ignores `stdin_bytes` and shares the parent's stdin/stdout/
/// stderr (out and err then come back empty).
#[no_mangle]
pub extern "C" fn pickle_run_cmd(
    argv: *mut PickleObject,
    cwd: *const PickleObject,
    envs: *mut PickleObject,
    env_removes: *mut PickleObject,
    stdin_bytes: *mut PickleObject,
    max_out: *const PickleObject,
    timeout_ms: *const PickleObject,
    inherit_io: bool,
) -> *mut PickleObject {
    let Some(mut cmd) = base_command(argv, cwd, envs, env_removes) else {
        return core::ptr::null_mut();
    };
    let cap = opt_int(max_out)
        .map(|v| v.max(0) as usize)
        .unwrap_or(DEFAULT_MAX_OUT);
    let timeout = opt_int(timeout_ms);

    if inherit_io {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return core::ptr::null_mut(),
        };
        let (code, timed) = wait_with_timeout(&mut child, timeout);
        return cmd_result(code.map(|c| c as i64), &[], &[], timed);
    }

    cmd.stdin(if stdin_bytes.is_null() {
        Stdio::null()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return core::ptr::null_mut(),
    };

    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let out_eof = Arc::new(AtomicBool::new(false));
    let err_eof = Arc::new(AtomicBool::new(false));
    let out_t = drainer(
        child.stdout.take().unwrap(),
        out.clone(),
        out_eof.clone(),
        cap,
    );
    let err_t = drainer(
        child.stderr.take().unwrap(),
        err.clone(),
        err_eof.clone(),
        cap,
    );
    // The caller's stdin bytes are written on a helper thread so a child that
    // never reads cannot deadlock the capture.
    let stdin_t = if stdin_bytes.is_null() {
        None
    } else {
        let bytes = byte_vec_of(stdin_bytes);
        let mut si = child.stdin.take().unwrap();
        Some(std::thread::spawn(move || {
            let _ = si.write_all(&bytes);
        }))
    };

    let (code, timed) = wait_with_timeout(&mut child, timeout);
    // The child has exited, so both pipes are at EOF; join the drainers.
    let _ = out_t.join();
    let _ = err_t.join();
    if let Some(t) = stdin_t {
        let _ = t.join();
    }
    let out_vec = out.lock().unwrap().clone();
    let err_vec = err.lock().unwrap().clone();
    cmd_result(code.map(|c| c as i64), &out_vec, &err_vec, timed)
}

/// `run_spawn(argv, cwd, envs, env_removes, max_out)`: start a child and
/// return a `Stream` handle to it — `none` when it cannot be spawned. Always
/// pipes; the caller writes stdin and reads output via `proc_*`.
#[no_mangle]
pub extern "C" fn pickle_run_spawn(
    argv: *mut PickleObject,
    cwd: *const PickleObject,
    envs: *mut PickleObject,
    env_removes: *mut PickleObject,
    max_out: *const PickleObject,
) -> *mut PickleObject {
    let Some(mut cmd) = base_command(argv, cwd, envs, env_removes) else {
        return core::ptr::null_mut();
    };
    let cap = opt_int(max_out)
        .map(|v| v.max(0) as usize)
        .unwrap_or(DEFAULT_MAX_OUT);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return core::ptr::null_mut(),
    };
    let child_out = child.stdout.take().unwrap();
    let child_err = child.stderr.take().unwrap();
    let stdin = child.stdin.take();

    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let out_eof = Arc::new(AtomicBool::new(false));
    let err_eof = Arc::new(AtomicBool::new(false));
    let mut inner = ProcInner {
        child: Some(child),
        code: None,
        reaped: false,
        out,
        err,
        out_eof,
        err_eof,
        stdin,
        cap,
        out_thread: None,
        err_thread: None,
    };
    inner.out_thread = Some(drainer(
        child_out,
        inner.out.clone(),
        inner.out_eof.clone(),
        cap,
    ));
    inner.err_thread = Some(drainer(
        child_err,
        inner.err.clone(),
        inner.err_eof.clone(),
        cap,
    ));

    crate::stream::stream_new(crate::stream::StreamInner {
        closed: false,
        target: Some(crate::stream::StreamTarget::Proc(Box::new(inner))),
    })
}

/// The `ProcInner` behind a `Stream` handle, or dangling when `h` is not a
/// live process handle.
fn proc_mut(h: *mut PickleObject) -> *mut ProcInner {
    crate::stream::proc_inner_mut(h)
}

/// Read up to `n` drained bytes, removing them from the front of `buf`.
/// A short/empty read is a poll result; `none` only once EOF is reached.
fn read_available(n: usize, buf: &Arc<Mutex<Vec<u8>>>, eof: &Arc<AtomicBool>) -> *mut PickleObject {
    let got: Vec<u8> = {
        let mut g = buf.lock().unwrap();
        let take = g.len().min(n);
        g.drain(..take).collect()
    };
    if got.is_empty() && eof.load(Ordering::SeqCst) {
        return core::ptr::null_mut();
    }
    let gc = crate::gc::gc_mut();
    let l = crate::list::list_new(got.len(), gc);
    let _lease = gc.temp_lease(l);
    for b in &got {
        crate::list::list_push(l, crate::boxscalar::pickle_box_i64(*b as i64));
    }
    l
}

/// Write the raw payloads of `bytes` (a `List<byte>`) to `w`, stopping at the
/// first slot that fails or is null; returns the bytes written.
fn write_slots<W: Write>(w: &mut W, bytes: *mut PickleObject) -> i64 {
    let n = list_len(bytes);
    let data = list_data(bytes);
    let mut written = 0i64;
    for i in 0..n {
        let e = unsafe { data.add(i).read() };
        if e.is_null() {
            break;
        }
        let byte = [box_bits(e) as u8];
        if w.write_all(&byte).is_ok() {
            written += 1;
        } else {
            break;
        }
    }
    written
}

/// `proc_wait(h)`: block until the child exits and reap it. `true`.
#[no_mangle]
pub extern "C" fn pickle_proc_wait(h: *mut PickleObject) -> bool {
    let inner = proc_mut(h);
    if inner.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner };
    if inner.reaped {
        return true;
    }
    // Joining the drainers waits for both pipes to hit EOF, which the child's
    // exit guarantees; the child itself is reaped after (no zombie).
    if let Some(t) = inner.out_thread.take() {
        let _ = t.join();
    }
    if let Some(t) = inner.err_thread.take() {
        let _ = t.join();
    }
    inner.code = if let Some(mut c) = inner.child.take() {
        match c.wait() {
            Ok(status) => status.code().map(|x| x as i64),
            Err(_) => None,
        }
    } else {
        None
    };
    inner.reaped = true;
    true
}

/// `proc_running(h)`: whether the child is still running. `false` for a
/// finished-but-not-yet-reaped child (wait first to reap and read its code).
#[no_mangle]
pub extern "C" fn pickle_proc_running(h: *mut PickleObject) -> bool {
    let inner = proc_mut(h);
    if inner.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner };
    if inner.reaped || inner.child.is_none() {
        return false;
    }
    match inner.child.as_mut().unwrap().try_wait() {
        Ok(Some(_)) => false,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// `proc_code(h)`: the exit code as `int?` — meaningful only after `wait()`;
/// `none` before a reap and on signal deaths.
#[no_mangle]
pub extern "C" fn pickle_proc_code(h: *mut PickleObject) -> *mut PickleObject {
    let inner = proc_mut(h);
    if inner.is_null() {
        return core::ptr::null_mut();
    }
    let inner = unsafe { &*inner };
    match inner.code {
        Some(c) => crate::boxscalar::pickle_box_i64(c),
        None => core::ptr::null_mut(),
    }
}

/// `proc_kill(h)`: terminate the child (best effort). `bool` success.
#[no_mangle]
pub extern "C" fn pickle_proc_kill(h: *mut PickleObject) -> bool {
    let inner = proc_mut(h);
    if inner.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner };
    match inner.child.as_mut() {
        Some(c) => c.kill().is_ok(),
        None => false,
    }
}

/// `proc_out_read(h, n)`: poll up to `n` drained stdout bytes as a
/// `List<byte>`, or `none` once the pipe is at EOF (and drained).
#[no_mangle]
pub extern "C" fn pickle_proc_out_read(h: *mut PickleObject, n: i64) -> *mut PickleObject {
    let inner = proc_mut(h);
    if inner.is_null() {
        return core::ptr::null_mut();
    }
    let inner = unsafe { &*inner };
    read_available(usize::try_from(n).unwrap_or(0), &inner.out, &inner.out_eof)
}

/// `proc_err_read(h, n)`: poll up to `n` drained stderr bytes as a
/// `List<byte>`, or `none` once the pipe is at EOF (and drained).
#[no_mangle]
pub extern "C" fn pickle_proc_err_read(h: *mut PickleObject, n: i64) -> *mut PickleObject {
    let inner = proc_mut(h);
    if inner.is_null() {
        return core::ptr::null_mut();
    }
    let inner = unsafe { &*inner };
    read_available(usize::try_from(n).unwrap_or(0), &inner.err, &inner.err_eof)
}

/// `proc_in_write(h, b)`: write `b` to the child's stdin; bytes written.
#[no_mangle]
pub extern "C" fn pickle_proc_in_write(h: *mut PickleObject, bytes: *mut PickleObject) -> i64 {
    let inner = proc_mut(h);
    if inner.is_null() {
        return 0;
    }
    let inner = unsafe { &mut *inner };
    match inner.stdin.as_mut() {
        Some(si) => write_slots(si, bytes),
        None => 0,
    }
}

/// `proc_in_close(h)`: close the child's stdin. `bool` success.
#[no_mangle]
pub extern "C" fn pickle_proc_in_close(h: *mut PickleObject) -> bool {
    let inner = proc_mut(h);
    if inner.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner };
    inner.stdin.take();
    true
}

/// `current_dir()`: the process's working directory as `string?`, or `none`
/// when it cannot be determined.
#[no_mangle]
pub extern "C" fn pickle_current_dir() -> *mut PickleObject {
    match std::env::current_dir() {
        Ok(p) => {
            let bytes = p.as_os_str().to_string_lossy().into_owned().into_bytes();
            let gc = crate::gc::gc_mut();
            string_from_bytes(bytes.as_ptr(), bytes.len(), gc)
        }
        Err(_) => core::ptr::null_mut(),
    }
}
