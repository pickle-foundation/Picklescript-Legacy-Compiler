//! `std.stream` support: buffered, open-handle file streaming plus
//! stdout/stderr writers.
//!
//! A `Stream` value is a managed `PStream` object whose single payload slot
//! holds a *raw* pointer to this module's `StreamInner`: a boxed buffered
//! reader/writer or a console handle. The collector never chases the slot —
//! the class descriptor has zero managed masks — and the inner state is
//! process-lifetime data. The descriptor's finalizer drops the box, so an
//! unclosed `Stream` closes its file handle when the object is swept; a
//! script-level `close()` drops the handle immediately.

#[cfg(not(feature = "nostd"))]
use crate::fs::path_of;
use crate::layout::{list_data, list_len, STREAM_INNER_OFF, STREAM_OBJECT_SIZE};
#[cfg(feature = "nostd")]
use crate::nostd::BufferedFile;
use crate::object::{PickleObject, PICKLE_CLASS_STREAM};
#[cfg(feature = "nostd")]
use alloc::{boxed::Box, vec::Vec};
#[cfg(not(feature = "nostd"))]
use std::fs::File;
#[cfg(not(feature = "nostd"))]
use std::io::{BufReader, BufWriter, Read, Write};

/// A buffered stream target held by a `Stream` value.
///
/// `Reader`/`Writer` wrap a file handle, so dropping a target closes the
/// underlying handle. In the `std` build that is a `File` behind a
/// `BufReader`/`BufWriter`, and `Proc` overlays a running subprocess handle
/// (see `process::ProcInner`), whose `Drop` kills and reaps an un-waited child.
/// A `nostd` build has no `process` support, so it has no `Proc` variant and
/// uses the Win32-backed [`BufferedFile`] in place of the `std` buffer types.
#[cfg(not(feature = "nostd"))]
pub(crate) enum StreamTarget {
    Reader(BufReader<File>),
    Writer(BufWriter<File>),
    Stdout,
    Stderr,
    Proc(Box<crate::process::ProcInner>),
}

#[cfg(feature = "nostd")]
pub(crate) enum StreamTarget {
    Reader(BufferedFile),
    Writer(BufferedFile),
    Stdout,
    Stderr,
}

/// The boxed state behind a `PStream` object.
///
/// `target` is `Option` so `close()` can take — and thus drop — the
/// underlying handle; a closed stream holds nothing.
pub(crate) struct StreamInner {
    pub(crate) closed: bool,
    pub(crate) target: Option<StreamTarget>,
}

/// The managed `PStream` object owning `inner`.
pub(crate) fn stream_new(inner: StreamInner) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    let obj = gc.alloc(STREAM_OBJECT_SIZE as u32, PICKLE_CLASS_STREAM);
    let ptr = Box::into_raw(Box::new(inner));
    unsafe {
        let slot = (obj as *mut u8).add(STREAM_INNER_OFF) as *mut *mut PickleObject;
        slot.write(ptr as *mut PickleObject);
    }
    obj
}

/// The boxed `StreamInner` behind `stream`. Only valid when `stream` is a
/// live, non-null `PStream` object.
#[inline]
fn inner_of(stream: *mut PickleObject) -> *mut StreamInner {
    unsafe { *((stream as *mut u8).add(STREAM_INNER_OFF) as *mut *mut StreamInner) }
}

/// Open `path` for reading, or `none` when the path cannot be opened.
///
/// The object marshalling is shared; only the handle acquisition differs, so
/// each build's `open` lives in a same-signature helper rather than repeating
/// the wrapper (and risking a second `#[no_mangle]` that the linker ignores).
#[cfg(not(feature = "nostd"))]
fn open_reader(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Reader(BufReader::new(
        File::open(path_of(path)).ok()?,
    )))
}

#[cfg(feature = "nostd")]
fn open_reader(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Reader(BufferedFile::open_read(
        crate::fs::path_bytes(path),
    )?))
}

#[no_mangle]
pub extern "C" fn pickle_stream_open_read(path: *const PickleObject) -> *mut PickleObject {
    match open_reader(path) {
        Some(target) => stream_new(StreamInner {
            closed: false,
            target: Some(target),
        }),
        None => core::ptr::null_mut(),
    }
}

/// Open `path` for writing, truncating any existing content, or `none` when
/// the path cannot be opened.
#[cfg(not(feature = "nostd"))]
fn open_writer_create(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Writer(BufWriter::new(
        File::create(path_of(path)).ok()?,
    )))
}

#[cfg(feature = "nostd")]
fn open_writer_create(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Writer(BufferedFile::open_write(
        crate::fs::path_bytes(path),
    )?))
}

#[no_mangle]
pub extern "C" fn pickle_stream_open_write(path: *const PickleObject) -> *mut PickleObject {
    match open_writer_create(path) {
        Some(target) => stream_new(StreamInner {
            closed: false,
            target: Some(target),
        }),
        None => core::ptr::null_mut(),
    }
}

/// Open `path` for appending, creating it if absent, or `none` when the path
/// cannot be opened.
#[cfg(not(feature = "nostd"))]
fn open_writer_append(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Writer(BufWriter::new(
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path_of(path))
            .ok()?,
    )))
}

#[cfg(feature = "nostd")]
fn open_writer_append(path: *const PickleObject) -> Option<StreamTarget> {
    Some(StreamTarget::Writer(BufferedFile::open_append(
        crate::fs::path_bytes(path),
    )?))
}

#[no_mangle]
pub extern "C" fn pickle_stream_open_append(path: *const PickleObject) -> *mut PickleObject {
    match open_writer_append(path) {
        Some(target) => stream_new(StreamInner {
            closed: false,
            target: Some(target),
        }),
        None => core::ptr::null_mut(),
    }
}

/// A `Stream` bound to the process's stdout.
#[no_mangle]
pub extern "C" fn pickle_stdout_stream() -> *mut PickleObject {
    stream_new(StreamInner {
        closed: false,
        target: Some(StreamTarget::Stdout),
    })
}

/// A `Stream` bound to the process's stderr.
#[no_mangle]
pub extern "C" fn pickle_stderr_stream() -> *mut PickleObject {
    stream_new(StreamInner {
        closed: false,
        target: Some(StreamTarget::Stderr),
    })
}

/// Read up to `n` bytes from `stream`. Returns a fresh `List<byte>` of exactly
/// the bytes read (a short list means the stream was exhausted partway), or
/// `none` at end of stream / when the stream is closed, is not readable (a
/// writer or console), or the underlying read fails.
#[no_mangle]
pub extern "C" fn pickle_stream_read(stream: *mut PickleObject, n: i64) -> *mut PickleObject {
    if stream.is_null() {
        return core::ptr::null_mut();
    }
    let inner = unsafe { &mut *inner_of(stream) };
    if inner.closed {
        return core::ptr::null_mut();
    }
    let cap = usize::try_from(n).unwrap_or(0);
    let mut buf = vec![0u8; cap];
    let got = match &mut inner.target {
        Some(StreamTarget::Reader(r)) => read_into(r, &mut buf),
        _ => return core::ptr::null_mut(),
    };
    if got == 0 {
        return core::ptr::null_mut(); // end of stream
    }
    let gc = crate::gc::gc_mut();
    let l = crate::list::list_new(got, gc);
    let _lease = gc.temp_lease(l);
    for &byte in &buf[..got] {
        crate::list::list_push(l, crate::boxscalar::pickle_box_i64(byte as i64));
    }
    l
}

/// Fill `buf` from a reader target, returning how many bytes were read.
///
/// The two builds read through different types (`BufReader<File>` in `std`,
/// `BufferedFile` in `nostd`); this keeps that difference in one place so
/// `pickle_stream_read` stays build-agnostic.
#[cfg(not(feature = "nostd"))]
fn read_into(r: &mut BufReader<File>, buf: &mut [u8]) -> usize {
    r.read(buf).unwrap_or(0)
}

#[cfg(feature = "nostd")]
fn read_into(r: &mut BufferedFile, buf: &mut [u8]) -> usize {
    r.read_some(buf)
}

/// Append the raw payloads of the slots in `bytes` (a `List<byte>`) through
/// `put`, stopping cleanly at the first slot that fails or is null. `put`
/// reports whether it accepted the byte. Returns the number of bytes written.
fn write_slots(bytes: *mut PickleObject, mut put: impl FnMut(&[u8]) -> bool) -> i64 {
    let n = list_len(bytes);
    let data = list_data(bytes);
    let mut written = 0i64;
    for i in 0..n {
        let e = unsafe { data.add(i).read() };
        if e.is_null() {
            break;
        }
        let byte = [crate::boxscalar::box_bits(e) as u8];
        if put(&byte) {
            written += 1;
        } else {
            break;
        }
    }
    written
}

/// Write `bytes` (a `List<byte>`) to `stream`, returning the number of bytes
/// written — `0` when the stream is closed, is not writable (a reader), or the
/// write immediately fails; less than `bytes.len()` when it fails partway.
#[no_mangle]
pub extern "C" fn pickle_stream_write(stream: *mut PickleObject, bytes: *mut PickleObject) -> i64 {
    if stream.is_null() {
        return 0;
    }
    let inner = unsafe { &mut *inner_of(stream) };
    if inner.closed {
        return 0;
    }
    match &mut inner.target {
        Some(StreamTarget::Writer(w)) => write_slots(bytes, |b| writer_write(w, b)),
        Some(StreamTarget::Stdout) => {
            // Route through the console bridge so `stdout_stream().write(...)`
            // is captured by the test harness exactly like `print`.
            let mut buf = Vec::new();
            let n = write_slots(bytes, |b| {
                buf.extend_from_slice(b);
                true
            });
            if n > 0 {
                crate::console::write_to_con(&buf);
            }
            n
        }
        Some(StreamTarget::Stderr) => stderr_write(bytes),
        _ => 0,
    }
}

/// Write `bytes` to the process's stderr.
///
/// Both builds now send these bytes to the real error handle, so a
/// self-hosted PE's `stderr_stream()` is indistinguishable from a JIT program's.
///
/// This previously folded `nostd` stderr into the console bridge, the same one
/// [`StreamTarget::Stdout`] uses. That made `stderr_stream().write(...)` and
/// `stdout_stream().write(...)` indistinguishable on `nostd`, which is only
/// correct for an in-process harness capturing the bridge and wrong for a
/// standalone executable: the ported runtime was observed writing to stdout
/// where the `std` build writes to stderr. It also contradicted the runtime's
/// own [`crate::nostd::console_error_write`], which the uncaught-throw report
/// uses and which already targets `STD_ERROR_HANDLE` — so on `nostd` the
/// uncaught message reached the real stderr while `stderr_stream()` did not.
///
/// [`StreamTarget::Stdout`]: enum.StreamTarget
#[cfg(not(feature = "nostd"))]
fn stderr_write(bytes: *mut PickleObject) -> i64 {
    write_slots(bytes, |b| std::io::stderr().write_all(b).is_ok())
}

#[cfg(feature = "nostd")]
fn stderr_write(bytes: *mut PickleObject) -> i64 {
    let mut buf = Vec::new();
    let n = write_slots(bytes, |b| {
        buf.extend_from_slice(b);
        true
    });
    if n > 0 {
        crate::nostd::console_error_write(&buf);
    }
    n
}

/// Push buffered writer bytes (or console bytes) out to the host. `true` when
/// the flush succeeded; `false` for a closed stream; `true` for a reader
/// (nothing buffered).
#[no_mangle]
pub extern "C" fn pickle_stream_flush(stream: *mut PickleObject) -> bool {
    if stream.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner_of(stream) };
    if inner.closed {
        return false;
    }
    match &mut inner.target {
        Some(StreamTarget::Writer(w)) => writer_flush(w),
        Some(StreamTarget::Stdout) => {
            // Pushes the console bridge (which flushes real stdout) then
            // reports success; nothing to flush in test capture mode.
            crate::console::write_to_con(&[]);
            true
        }
        Some(StreamTarget::Stderr) => stderr_flush(),
        Some(StreamTarget::Reader(_)) => true,
        #[cfg(not(feature = "nostd"))]
        Some(StreamTarget::Proc(_)) => true,
        None => false,
    }
}

/// `std::io::stderr()` is unbuffered, so there is nothing to push; the
/// `nostd` build has no stderr handle and shares the console bridge.
#[cfg(not(feature = "nostd"))]
fn stderr_flush() -> bool {
    std::io::stderr().flush().is_ok()
}

#[cfg(feature = "nostd")]
    fn stderr_flush() -> bool {
        // `console_error_write` writes through `WriteFile`, which is unbuffered,
        // so there is nothing left to push. Reporting success matches the `std`
        // arm and keeps `stderr_stream().flush()` from looking like a failure.
        true
    }

/// Push a file writer's buffered bytes out. `BufWriter::flush` returns
/// `io::Result`; `BufferedFile::flush` already reports success as a `bool`.
#[cfg(not(feature = "nostd"))]
fn writer_flush<W: Write>(w: &mut W) -> bool {
    w.flush().is_ok()
}

#[cfg(feature = "nostd")]
fn writer_flush(w: &mut BufferedFile) -> bool {
    w.flush()
}

/// Append `b` to a file writer. `Write::write_all` returns `io::Result`, while
/// `BufferedFile::write_all` already reports success as a `bool`.
#[cfg(not(feature = "nostd"))]
fn writer_write<W: Write>(w: &mut W, b: &[u8]) -> bool {
    w.write_all(b).is_ok()
}

#[cfg(feature = "nostd")]
fn writer_write(w: &mut BufferedFile, b: &[u8]) -> bool {
    w.write_all(b)
}

/// Close `stream`, flushing any buffered output and releasing the handle.
/// Idempotent: `true` on the first close, `false` for a closed or foreign
/// stream. Closing a process handle kills and reaps its un-exited child
/// (`ProcInner`'s `Drop`), so no zombie outlives a dropped handle.
#[no_mangle]
pub extern "C" fn pickle_stream_close(stream: *mut PickleObject) -> bool {
    if stream.is_null() {
        return false;
    }
    let inner = unsafe { &mut *inner_of(stream) };
    if inner.closed {
        return false; // idempotent: a second close reports failure
    }
    inner.closed = true;
    match inner.target.take() {
        Some(StreamTarget::Writer(mut w)) => writer_flush(&mut w),
        Some(StreamTarget::Stdout) => {
            crate::console::write_to_con(&[]);
            true
        }
        Some(StreamTarget::Stderr) => stderr_flush(),
        Some(StreamTarget::Reader(_)) => true,
        #[cfg(not(feature = "nostd"))]
        Some(StreamTarget::Proc(_)) => true,
        None => false,
    }
}

/// The `process::ProcInner` behind a process-handle `Stream`, or dangling
/// when `stream` is not a live process handle.
///
/// Only the `std` build has process handles; `process` is host-only, so a
/// `nostd` build has no `Proc` target to hand back.
#[cfg(not(feature = "nostd"))]
pub(crate) fn proc_inner_mut(stream: *mut PickleObject) -> *mut crate::process::ProcInner {
    if stream.is_null() {
        return core::ptr::null_mut();
    }
    let inner = unsafe { &mut *inner_of(stream) };
    if inner.closed {
        return core::ptr::null_mut();
    }
    match &mut inner.target {
        Some(StreamTarget::Proc(p)) => p.as_mut(),
        _ => core::ptr::null_mut(),
    }
}

/// Drop the boxed `StreamInner`. The collector calls this when a `Stream`
/// object is swept, releasing the file handle of a stream that was never
/// closed explicitly.
pub(crate) extern "C" fn stream_finalizer(obj: *mut PickleObject) {
    if obj.is_null() {
        return;
    }
    let ptr = inner_of(obj);
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(ptr));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::test_begin;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pickle_stream_{name}"))
    }

    /// A managed `string` from an owned byte slice (short borrow of the GC).
    fn s(bytes: &[u8]) -> *mut PickleObject {
        crate::strings::string_from_bytes(bytes.as_ptr(), bytes.len(), crate::gc::gc_mut())
    }

    /// A managed `List<byte>` of `bytes` (short borrow of the GC).
    fn list_of(bytes: &[u8]) -> *mut PickleObject {
        let gc = &mut crate::gc::gc_mut();
        let l = crate::list::list_new(bytes.len(), gc);
        let _lease = gc.temp_lease(l);
        for b in bytes {
            crate::list::list_push(l, crate::boxscalar::pickle_box_i64(*b as i64));
        }
        l
    }

    /// Read `stream` to end of stream, returning the raw byte payload.
    fn read_all(stream: *mut PickleObject) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let chunk = pickle_stream_read(stream, 3);
            if chunk.is_null() {
                break;
            }
            let n = list_len(chunk);
            for i in 0..n {
                let e = crate::list::pickle_list_get(chunk, i);
                out.push(crate::boxscalar::box_bits(e) as u8);
            }
        }
        out
    }

    #[test]
    fn roundtrip_via_write_then_read() {
        let _guard = test_begin();
        let path = tmp_path("roundtrip.txt");
        let _ = std::fs::remove_file(&path);

        let w = pickle_stream_open_write(s(path.to_str().unwrap().as_bytes()));
        assert!(!w.is_null());
        assert_eq!(pickle_stream_write(w, list_of(b"hello")), 5);
        assert!(pickle_stream_flush(w));
        assert!(pickle_stream_close(w));
        assert!(!pickle_stream_close(w)); // second close fails
        assert_eq!(pickle_stream_write(w, list_of(b"x")), 0); // closed writes 0

        let r = pickle_stream_open_read(s(path.to_str().unwrap().as_bytes()));
        assert!(!r.is_null());
        assert_eq!(read_all(r), b"hello");
        // Past EOF: reads report none.
        assert!(pickle_stream_read(r, 3).is_null());
        assert!(pickle_stream_close(r));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_path_is_none() {
        let _guard = test_begin();
        // A missing file is only none for reading (`open_write` creates it).
        let path = tmp_path("does_not_exist.txt");
        let _ = std::fs::remove_file(&path);
        let p = s(path.to_str().unwrap().as_bytes());
        assert!(pickle_stream_open_read(p).is_null());
        // A path whose parent directory does not exist opens for nothing in
        // every mode (including create-on-append).
        let nested = tmp_path("no_such_dir").join("file.txt");
        let q = s(nested.to_str().unwrap().as_bytes());
        assert!(pickle_stream_open_read(q).is_null());
        assert!(pickle_stream_open_write(q).is_null());
        assert!(pickle_stream_open_append(q).is_null());
    }

    #[test]
    fn append_extends_existing() {
        let _guard = test_begin();
        let path = tmp_path("append.txt");
        std::fs::write(&path, b"ab").unwrap();
        let w = pickle_stream_open_append(s(path.to_str().unwrap().as_bytes()));
        assert!(!w.is_null());
        assert_eq!(pickle_stream_write(w, list_of(b"c")), 1);
        assert!(pickle_stream_close(w));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn console_streams_are_writable() {
        let _guard = test_begin();
        let out = pickle_stdout_stream();
        let err = pickle_stderr_stream();
        assert!(!out.is_null());
        assert!(!err.is_null());
        // A write to a console stream counts its bytes and never panics.
        assert_eq!(pickle_stream_write(out, list_of(b"")), 0);
        assert!(pickle_stream_flush(out));
        // Leave the console streams open: closing/printing them would touch
        // the real console.
    }

    #[test]
    fn closed_stream_rejects_io() {
        let _guard = test_begin();
        let out = pickle_stdout_stream();
        assert!(pickle_stream_close(out));
        assert_eq!(pickle_stream_write(out, list_of(b"")), 0);
        assert!(!pickle_stream_flush(out));
        assert!(pickle_stream_read(out, 4).is_null());
    }
}
