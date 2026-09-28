//! no_std support for `pickle-runtime`.
//!
//! Only compiled when the `nostd` cargo feature is on. Provides the pieces a
//! freestanding build needs so the resulting `rlib` has no `std`-crate
//! dependencies and can be linked directly by the pickle-written linker
//! (`compiler-selfhost/linker.pkl`) without rustc.
//!
//! - a Windows `HeapAlloc`-backed global allocator (so `alloc` works)
//! - a `#[panic_handler]` (leaf, so the compiled crate never reaches `std`)
//! - `extern "system"` WinAPI entry points used by converted runtime code

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;

pub type WinHandle = *mut c_void;

#[link(name = "kernel32")]
extern "system" {
    pub fn GetProcessHeap() -> WinHandle;
    pub fn HeapAlloc(heap: WinHandle, flags: u32, size: usize) -> *mut c_void;
    pub fn HeapFree(heap: WinHandle, flags: u32, mem: *mut c_void) -> i32;
    pub fn GetStdHandle(device: u32) -> WinHandle;
    pub fn WriteFile(
        file: WinHandle,
        buf: *const u8,
        count: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    pub fn ExitProcess(code: u32) -> !;

    // ---- timing, used by the `nostd` `thread_sleep_ms` backing
    pub fn Sleep(ms: u32);

    // ---- file / directory access, used by the `nostd` `fs` + `stream` backends
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        creation: u32,
        flags: u32,
        template: WinHandle,
    ) -> WinHandle;
    pub fn ReadFile(
        file: WinHandle,
        buf: *mut u8,
        count: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    pub fn CloseHandle(object: WinHandle) -> i32;
    pub fn DeleteFileW(name: *const u16) -> i32;
    pub fn GetFileAttributesW(name: *const u16) -> u32;
    pub fn CreateDirectoryW(name: *const u16, security: *mut c_void) -> i32;
    pub fn RemoveDirectoryW(name: *const u16) -> i32;
    pub fn FindFirstFileW(pattern: *const u16, data: *mut Win32FindDataW) -> WinHandle;
    pub fn FindNextFileW(find: WinHandle, data: *mut Win32FindDataW) -> i32;
    pub fn FindClose(find: WinHandle) -> i32;
}

/// `FILETIME`. Modelled as two `u32`s rather than a `u64` so the struct keeps
/// `FILETIME`'s real 4-byte alignment: representing it as a `u64` would insert
/// padding after `dwFileAttributes` and shift everything after it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileTime {
    pub dw_low_date_time: u32,
    pub dw_high_date_time: u32,
}

/// `WIN32_FIND_DATAW`. `cFileName` is a fixed `WCHAR[MAX_PATH]` at byte 44
/// (the real struct is 592 bytes on disk, but its natural alignment is 4, so
/// the in-memory size is 564 and `offset_of` is what callers must trust).
#[repr(C)]
pub struct Win32FindDataW {
    pub dw_file_attributes: u32,
    pub ft_creation_time: FileTime,
    pub ft_last_access_time: FileTime,
    pub ft_last_write_time: FileTime,
    pub n_file_size_high: u32,
    pub n_file_size_low: u32,
    pub dw_reserved0: u32,
    pub dw_reserved1: u32,
    pub c_file_name: [u16; 260],
}

// `FindFirstFileW`/`FindNextFileW` hand back this struct by pointer, so a
// layout mistake silently misreads `cFileName`. Pin the layout at compile time
// rather than trusting the comment above.
const _: () = {
    assert!(core::mem::size_of::<Win32FindDataW>() == 564);
    assert!(core::mem::align_of::<Win32FindDataW>() == 4);
    assert!(core::mem::offset_of!(Win32FindDataW, c_file_name) == 44);
};

pub const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
pub const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
pub const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_SHARE_READ: u32 = 0x0000_0001;
pub const FILE_SHARE_WRITE: u32 = 0x0000_0002;
pub const FILE_SHARE_DELETE: u32 = 0x0000_0004;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const OPEN_ALWAYS: u32 = 4;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
pub const FIND_DATA_SIZE: usize = core::mem::size_of::<Win32FindDataW>();
/// `INVALID_HANDLE_VALUE`, the `(-1)` HANDLE sentinel CreateFileW returns.
#[inline]
pub const fn invalid_handle() -> WinHandle {
    (-1isize) as WinHandle
}

/// `FILE_APPEND_DATA`: the write-only access right that forces every write to
/// land at the end of the file, without an explicit seek.
pub const FILE_APPEND_DATA: u32 = 0x0000_0004;

/// Pause the current thread for at least `ms` milliseconds.
///
/// `Sleep` is a kernel call with no CRT involvement, so it works in a
/// CRT-less image; the caller clamps the value, since `Sleep` takes an unsigned
/// count and a negative request would otherwise read as an enormous one.
pub fn sleep_ms(ms: u32) {
    unsafe { Sleep(ms) }
}

/// Buffer size for [`BufferedFile`]. Matches the `BufReader`/`BufWriter`
/// capacity the `std` build of `stream` uses, so both builds issue the same
/// number of `ReadFile`/`WriteFile` calls for the same program.
const STREAM_BUFFER: usize = 8 * 1024;

/// A buffered reader/writer over a Win32 file handle.
///
/// This is the freestanding stand-in for `BufReader<File>`/`BufWriter<File>`:
/// same observable behaviour (reads are served from a refill buffer, writes
/// accumulate until `flush`), but backed by the raw `ReadFile`/`WriteFile`
/// entry points so a `nostd` build needs no `std::io`. Dropping it closes the
/// handle, which is what makes `StreamInner`'s finalizer release the file.
pub struct BufferedFile {
    handle: WinHandle,
    /// Unread bytes of the last refill: `rbuf[rpos..rend]`.
    rbuf: alloc::vec::Vec<u8>,
    rpos: usize,
    rend: usize,
    /// Bytes written but not yet handed to the OS.
    wbuf: alloc::vec::Vec<u8>,
}

impl BufferedFile {
    fn from_handle(handle: WinHandle) -> Option<Self> {
        if handle == invalid_handle() || handle.is_null() {
            return None;
        }
        Some(Self {
            handle,
            rbuf: alloc::vec::Vec::new(),
            rpos: 0,
            rend: 0,
            wbuf: alloc::vec::Vec::new(),
        })
    }

    fn open(path: &[u8], access: u32, disposition: u32) -> Option<Self> {
        let mut w: alloc::vec::Vec<u16> =
            core::str::from_utf8(path).unwrap_or("").encode_utf16().collect();
        w.push(0);
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                share,
                core::ptr::null_mut(),
                disposition,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        Self::from_handle(h)
    }

    /// Open an existing file for reading. `None` when it cannot be opened.
    pub fn open_read(path: &[u8]) -> Option<Self> {
        Self::open(path, GENERIC_READ, OPEN_EXISTING)
    }

    /// Open for writing, truncating any existing content.
    pub fn open_write(path: &[u8]) -> Option<Self> {
        Self::open(path, GENERIC_WRITE, CREATE_ALWAYS)
    }

    /// Open for writing, keeping existing content and appending after it.
    pub fn open_append(path: &[u8]) -> Option<Self> {
        Self::open(path, FILE_APPEND_DATA, OPEN_ALWAYS)
    }

    /// Copy up to `out.len()` buffered bytes into `out`, refilling from the OS
    /// as needed. Returns the count copied; `0` means end of input.
    pub fn read_some(&mut self, out: &mut [u8]) -> usize {
        if out.is_empty() {
            return 0;
        }
        if self.rpos == self.rend && !self.refill() {
            return 0;
        }
        let avail = self.rend - self.rpos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        n
    }

    /// Refill `rbuf` from the OS. `false` at end of input or on error.
    ///
    /// The buffer is only zero-filled the first time; a refilled tail is
    /// overwritten by `ReadFile` and only `rbuf[..rend]` is ever read, so
    /// re-zeroing `STREAM_BUFFER` bytes per call would be wasted work.
    fn refill(&mut self) -> bool {
        if self.rbuf.len() < STREAM_BUFFER {
            self.rbuf.resize(STREAM_BUFFER, 0);
        }
        let mut n: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle,
                self.rbuf.as_mut_ptr(),
                STREAM_BUFFER as u32,
                &mut n,
                core::ptr::null_mut(),
            ) != 0
        };
        self.rpos = 0;
        self.rend = n as usize;
        ok && self.rend > 0
    }

    /// Append `bytes` to the write buffer, flushing whenever it outgrows
    /// `STREAM_BUFFER` so a long write does not grow without bound. `false`
    /// when the OS rejected the bytes.
    pub fn write_all(&mut self, bytes: &[u8]) -> bool {
        self.wbuf.extend_from_slice(bytes);
        if self.wbuf.len() < STREAM_BUFFER {
            return true;
        }
        self.flush()
    }

    /// Hand every buffered byte to the OS. `false` on a short or failed write.
    pub fn flush(&mut self) -> bool {
        if self.wbuf.is_empty() {
            return true;
        }
        let total = self.wbuf.len();
        let mut done = 0usize;
        while done < total {
            let mut n: u32 = 0;
            let left = total - done;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    self.wbuf[done..].as_ptr(),
                    left as u32,
                    &mut n,
                    core::ptr::null_mut(),
                ) != 0
            };
            if !ok || n == 0 {
                break;
            }
            done += n as usize;
        }
        self.wbuf.clear();
        done == total
    }
}

impl Drop for BufferedFile {
    fn drop(&mut self) {
        // Best effort: a failed flush must not stop the handle from closing.
        self.flush();
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Writes bytes to the process's standard output handle. `false` when the
/// write call fails. Never panics.
pub fn console_write(buf: &[u8]) -> bool {
    let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if h.is_null() || core::ptr::eq(h, core::ptr::null_mut::<c_void>()) {
        return false;
    }
    if !buf.is_empty() {
        let mut written: u32 = 0;
        let ok = unsafe { WriteFile(h, buf.as_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut written, core::ptr::null_mut()) };
        ok != 0
    } else {
        true
    }
}

/// Writes bytes to the process's standard error handle. `false` when the write
/// call fails. Never panics.
///
/// Not the general-purpose stderr for a freestanding build: the [`eprintln!`]
/// shim below sends to stdout on purpose, because a program with no separate
/// error channel still has to have its diagnostics *appear* somewhere and stdout
/// is the handle a console will show. This is for the one case where the
/// channel is part of the contract rather than the delivery -- an uncaught
/// `throw` report, which a host reads from stderr to find out that a run failed.
/// Redirecting stdout and discarding it must not be able to hide a failure.
///
/// [`eprintln!`]: macro@crate
pub fn console_error_write(buf: &[u8]) -> bool {
    let h = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if h.is_null() || core::ptr::eq(h, core::ptr::null_mut::<c_void>()) {
        return false;
    }
    if !buf.is_empty() {
        let mut written: u32 = 0;
        let ok = unsafe { WriteFile(h, buf.as_ptr(), buf.len().min(u32::MAX as usize) as u32, &mut written, core::ptr::null_mut()) };
        ok != 0
    } else {
        true
    }
}

unsafe impl GlobalAlloc for PfHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HeapAlloc(GetProcessHeap(), 8, layout.size()) as *mut u8
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        HeapFree(GetProcessHeap(), 0, ptr as *mut c_void);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let h = GetProcessHeap();
        let newp = HeapAlloc(h, 8, new_size) as *mut u8;
        if !newp.is_null() {
            let copy_n = core::cmp::min(layout.size(), new_size);
            core::ptr::copy(ptr, newp, copy_n);
            HeapFree(h, 0, ptr as *mut c_void);
        }
        newp
    }
}

struct PfHeap;

#[global_allocator]
static GLOBAL: PfHeap = PfHeap;

/// Rust's "allocator shim" symbols. When rustc drives the final binary link it
/// synthesizes these itself; the pickle-written linker
/// (`compiler-selfhost/linker.pkl`) does not, so an rlib that declares a
/// `#[global_allocator]` must spell them out for the flat link:
///
/// - `___rust_no_alloc_shim_is_unstable_v2`: a marker `alloc` calls (via a
///   plain function `call`) so that the absence of an allocator is a link
///   error. The body is never inspected; it just has to be executable code.
/// - `___rust_alloc_error_handler` / `___rdl_alloc_error_handler`: the OOM
///   entry point (`handle_alloc_error` routes here). Freestanding builds just
///   terminate.
///
/// The mangled names embed the `___rustc` shim-crate disambiguator, which is
/// stable for a given toolchain; the relevant objects are all produced by the
/// same `stable-x86_64-pc-windows-msvc` rustc this crate is built with.
///
/// Marker that proves an allocator shim is linked. `alloc` emits a `call` to
/// it (rust-lang/rust#141061 turned the marker static into a function), so it
/// must be real code, not a data static.
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_RNvCs1njKG4L9aB3_7___rustc35___rust_no_alloc_shim_is_unstable_v2")]
pub fn __rust_no_alloc_shim_is_unstable_v2() {}

/// OOM entry point (allocator-shim route). Never returns.
#[unsafe(export_name = "_RNvCs1njKG4L9aB3_7___rustc26___rust_alloc_error_handler")]
fn rust_alloc_error_handler(_size: usize, _align: usize) -> ! {
    unsafe {
        ExitProcess(75);
    }
}

/// Default OOM entry point (what the allocator-shim route calls into).
#[unsafe(export_name = "_RNvCs1njKG4L9aB3_7___rustc25___rdl_alloc_error_handler")]
fn rdl_alloc_error_handler(_size: usize, _align: usize) -> ! {
    unsafe {
        ExitProcess(75);
    }
}

// A test target links `std`, which brings its own `#[panic_handler]`; two of
// those in one crate is a duplicate lang item. Ours is for the freestanding
// image only, where nothing else provides one.
#[cfg(not(test))]
#[panic_handler]
fn on_panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe {
        ExitProcess(70);
    }
}

// `__chkstk`: MSVC's stack-probe helper.
//
// rustc emits a call to this whenever a function reserves more than one page
// of stack, and it expects the callee to touch every intervening page so the
// guard page is expanded before the writes land. The MSVC CRT normally supplies
// it, but the pickle-written linker links without a CRT, and `core`/`alloc`
// objects pulled in from the sysroot rlibs reference it. The algorithm is the
// standard MSVC x64 sequence: probe downward from the incoming stack pointer in
// `PAGE_SIZE` steps, stopping at the page the frame already occupies.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".globl __chkstk",
    ".def __chkstk; .scl 2; .type 32; .endef",
    "__chkstk:",
    "push    rbx",
    "mov     r11, rsp",
    "add     r11, 8",
    "mov     rbx, rsp",
    "and     rbx, -4096",
    "cmp     r11, rbx",
    "jbe     2f",
    "1:",
    "test    byte ptr [r11 - 4096], 0",
    "sub     r11, 4096",
    "cmp     r11, rbx",
    "ja      1b",
    "2:",
    "add     r11, 8",
    "mov     rsp, r11",
    "pop     rbx",
    "ret",
);

/// Fallback `println!` for code paths used on a freestanding build.
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        let s = alloc::format!($($arg)*);
        $crate::nostd::console_write(s.as_bytes());
        $crate::nostd::console_write(b"\n");
    }};
}

/// Fallback `eprintln!` (no separate stderr handle: writes to stdout).
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let s = alloc::format!($($arg)*);
        $crate::nostd::console_write(s.as_bytes());
        $crate::nostd::console_write(b"\n");
    }};
}

/// Single-threaded substitute for `std::thread_local!`: the process has one
/// OS thread in a freestanding build, so "thread local" storage is a plain
/// static. The `.with(...)` call sites keep their exact shape (`c` is a
/// `&RefCell<T>` just like the std expansion passes).
pub struct Tls<T>(core::cell::RefCell<T>);

impl<T> Tls<T> {
    pub const fn new(v: T) -> Self {
        Tls(core::cell::RefCell::new(v))
    }
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.borrow())
    }
}

/// The process has exactly one OS thread in a freestanding build, so a
/// "thread local" static is only ever touched from that thread: sharing the
/// `RefCell` through `&` is sound.
unsafe impl<T> Sync for Tls<T> {}

/// `std::thread_local!` replacement. The supported shapes mirror the ones the
/// runtime actually uses; anything else fails to compile loudly.
#[macro_export]
macro_rules! thread_local {
    ($( $(#[$attr:meta])* $vis:vis static $name:ident : RefCell<$ty:ty> = const { $e:expr };)+) => {
        $( $(#[$attr])* $vis static $name: $crate::nostd::Tls<core::cell::RefCell<$ty>> = $crate::nostd::Tls::new($e);)+
    };
    ($( $(#[$attr:meta])* $vis:vis static $name:ident : $ty:ty = const { $e:expr };)+) => {
        $( $(#[$attr])* $vis static $name: $crate::nostd::Tls<$ty> = $crate::nostd::Tls::new($e);)+
    };
    ($( $(#[$attr:meta])* $vis:vis static $name:ident : RefCell<$ty:ty> = $e:expr ;)+) => {
        $( $(#[$attr])* $vis static $name: $crate::nostd::Tls<core::cell::RefCell<$ty>> = $crate::nostd::Tls::new($e);)+
    };
    ($( $(#[$attr:meta])* $vis:vis static $name:ident : $ty:ty = $e:expr ;)+) => {
        $( $(#[$attr])* $vis static $name: $crate::nostd::Tls<$ty> = $crate::nostd::Tls::new($e);)+
    };
}

/// Mutex/Once lookalikes used by the freestanding build. Single-threaded: a
/// `lock()` only conflicts with a *re-entrant* borrow on the same thread, and
/// `try_lock` reports that so re-entrant collectors can bail like the real
/// (std) version does. The API mirrors `std::sync` closely enough that the
/// `.lock().unwrap()` / `.unwrap_or_else(|p| p.into_inner())` call sites
/// compile unchanged.
pub mod sync {
    use core::cell::{RefCell, RefMut};
    use core::ops::{Deref, DerefMut};

    pub struct Mutex<T> {
        data: RefCell<T>,
    }

    pub struct PoisonError<T0 = ()>(core::marker::PhantomData<T0>);

    impl<T0> PoisonError<T0> {
        pub fn into_inner<T1>(self) -> T1 {
            core::unreachable!("re-entrant lock under nostd")
        }
    }

    impl<T0> core::fmt::Debug for PoisonError<T0> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("PoisonError { mutex poisoned by re-entrant lock }")
        }
    }

    pub struct MutexGuard<'a, T>(RefMut<'a, T>);

    impl<T> Mutex<T> {
        pub const fn new(v: T) -> Self {
            Mutex {
                data: RefCell::new(v),
            }
        }
        pub fn lock(&self) -> Result<MutexGuard<'_, T>, PoisonError> {
            self.data
                .try_borrow_mut()
                .map(MutexGuard)
                .map_err(|_| PoisonError(core::marker::PhantomData))
        }
        pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, PoisonError> {
            self.lock()
        }
    }

    /// One OS thread in a freestanding build ⇒ a `Mutex` is never contended
    /// across threads, so exposing the inner `RefCell` through `&` is sound.
    unsafe impl<T> Sync for Mutex<T> {}

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }
    impl<T> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }

    pub struct Once {
        done: core::sync::atomic::AtomicBool,
    }

    impl Once {
        pub const fn new() -> Self {
            Once {
                done: core::sync::atomic::AtomicBool::new(false),
            }
        }
        pub fn call_once<F>(&self, f: F)
        where
            F: FnOnce(),
        {
            if !self
                .done
                .swap(true, core::sync::atomic::Ordering::AcqRel)
            {
                f();
            }
        }
    }
}

/// Terminate the process with `code` (never returns).
pub fn exit(code: i32) -> ! {
    unsafe {
        ExitProcess(code as u32);
    }
}

/// Environment probe used by the converted runtime. Freestanding builds read
/// no environment, so this always reports "absent".
pub fn getenv_present(_name: &str) -> bool {
    false
}

/// C builtins the pickle-written linker (`compiler-selfhost/linker.pkl`) must
/// be able to resolve when it links this crate against `core`/`alloc` without
/// the compiler_builtins crate: byte moves, compares, and the soft 128-bit and
/// IEEE-half conversions that are pulled into kept `core`/`alloc` code.
///
/// Defined here (rather than pulling the gigantic `compiler_builtins` rlib
/// into the flat link) because the full set the objects actually reference is
/// exactly these seven names plus `memcpy`/`memmove`/`memset`.
///
/// MSVC uses this sentinel to know that floating point code is referenced;
/// the object files reference it symbolically, so any definition satisfies it.
#[no_mangle]
pub static _fltused: i32 = 0x0000_128A;

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    core::ptr::copy(src as *const u8, dst as *mut u8, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut c_void, val: i32, n: usize) -> *mut c_void {
    core::ptr::write_bytes(dst as *mut u8, val as u8, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let ap = a as *const u8;
    let bp = b as *const u8;
    let mut i = 0;
    while i < n {
        let x = *ap.add(i);
        let y = *bp.add(i);
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const i8) -> usize {
    let mut i = 0;
    while *s.add(i) != 0 {
        i += 1;
    }
    i
}

/// 128x128 unsigned division. Implemented as binary long division by
/// subtraction so the body never lowers to another `__udivti3` call.
#[no_mangle]
pub unsafe extern "C" fn __udivti3(a: u128, b: u128) -> u128 {
    if b == 0 || a < b {
        return 0;
    }
    let mut q: u128 = 0;
    let mut r: u128 = 0;
    let mut i = 128;
    while i > 0 {
        i -= 1;
        let bit = (a >> i) & 1;
        r = (r << 1) | bit;
        if r >= b {
            r -= b;
            q |= 1 << i;
        }
    }
    q
}

/// IEEE-754 binary16 -> binary32.
#[no_mangle]
pub unsafe extern "C" fn __extendhfsf2(h: u16) -> f32 {
    let sign = ((h as u32) & 0x8000) << 16;
    let exp = ((h as u32) >> 10) & 0x1f;
    let mant = (h as u32) & 0x3ff;
    if exp == 0 {
        if mant == 0 {
            return f32::from_bits(sign);
        }
        let mut m = mant;
        let mut e: i32 = 113;
        while (m & 0x400) == 0 {
            m <<= 1;
            e -= 1;
        }
        f32::from_bits(sign | ((e as u32) << 23) | ((m & 0x3ff) << 13))
    } else if exp == 0x1f {
        if mant == 0 {
            f32::from_bits(sign | 0x7f80_0000)
        } else {
            f32::from_bits(sign | 0x7f80_0000 | (mant << 13))
        }
    } else {
        let e = exp - 15 + 127;
        f32::from_bits(sign | (e << 23) | (mant << 13))
    }
}

/// IEEE-754 binary32 -> binary16, round-to-nearest-even.
#[no_mangle]
pub unsafe extern "C" fn __truncsfhf2(f: f32) -> u16 {
    let bits = f.to_bits();
    let mut out: u32 = (bits >> 16) & 0x8000;
    let exp = (bits >> 23) & 0xff;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        if mant == 0 {
            out |= 0x7c00;
        } else {
            out |= 0x7c00 | ((mant >> 13) | 0x200);
        }
        return out as u16;
    }
    let e: i32 = (exp as i32) - 127 + 15;
    if e >= 31 {
        return (out | 0x7c00) as u16;
    }
    if e <= 0 {
        let m = mant | 0x80_0000;
        if e <= -11 {
            return out as u16;
        }
        let sh: u32 = (1 - e) as u32;
        let h0 = m >> sh;
        let half = 1u32 << (sh - 1);
        let rem = m & ((1u32 << sh) - 1);
        let mut h = h0;
        if (rem > half) || (rem == half && (h0 & 1) != 0) {
            h = h0 + 1;
        }
        return (out | h) as u16;
    }
    let m10 = mant >> 13;
    let rem = mant & 0x1fff;
    let mut m10 = m10;
    if (rem > 0x1000) || (rem == 0x1000 && (m10 & 1) != 0) {
        m10 += 1;
    }
    if m10 == 0x400 {
        let e2 = e + 1;
        if e2 >= 31 {
            return (out | 0x7c00) as u16;
        }
        out |= ((e2 as u32) << 10) & 0x7c00;
        return out as u16;
    }
    out = out | (((e as u32) << 10) & 0x7c00) | m10;
    out as u16
}