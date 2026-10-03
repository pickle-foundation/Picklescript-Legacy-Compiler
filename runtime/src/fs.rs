//! `std.fs` intrinsics: read, write, and test files on the host filesystem.
//!
//! These are the thin runtime layer for the stdlib's file module. Read
//! failures are surfaced as `none` (`null` object pointer) — errors are
//! values — and writes report success as a `bool`.
//!
//! The object marshalling is shared; only the syscalls differ. The `std` build
//! goes through `std::fs`, while the `nostd` build talks to the wide Win32
//! entry points declared in [`crate::nostd`], so the freestanding runtime can
//! serve the same `fs` ABI without linking `std`. Both are handed the raw path
//! bytes and must accept invalid UTF-8 as an empty path.

use crate::object::PickleObject;
use crate::strings::{string_bytes_len, string_bytes_ptr, string_from_bytes};

pub(crate) fn path_bytes(path: *const PickleObject) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(string_bytes_ptr(path), string_bytes_len(path)) }
}

#[cfg(not(feature = "nostd"))]
pub(crate) fn path_of(path: *const PickleObject) -> &'static std::path::Path {
    std::path::Path::new(core::str::from_utf8(path_bytes(path)).unwrap_or(""))
}

/// The path as a NUL-terminated UTF-16 buffer, which is what the wide Win32
/// entry points take. Invalid UTF-8 decodes to an empty path, matching
/// `path_of`'s `unwrap_or("")`.
#[cfg(feature = "nostd")]
pub(crate) fn path_w(path: &[u8]) -> alloc::vec::Vec<u16> {
    let s = core::str::from_utf8(path).unwrap_or("");
    let mut w: alloc::vec::Vec<u16> = s.encode_utf16().collect();
    w.push(0);
    w
}

/// `true` when `path` ends in a separator, so a child name can be appended
/// without doubling it up (this is what `Path::join` does).
#[cfg(feature = "nostd")]
fn ends_with_sep(path: &[u8]) -> bool {
    matches!(path.last(), Some(b'\\') | Some(b'/'))
}

/// The syscall layer. Each function takes the raw path bytes and mirrors the
/// `std::fs` behaviour documented on the `pickle_*` wrapper below.
mod backend {
    #[cfg(not(feature = "nostd"))]
    mod imp {
        use alloc::vec::Vec;

        fn path_of_bytes(path: &[u8]) -> &std::path::Path {
            std::path::Path::new(core::str::from_utf8(path).unwrap_or(""))
        }

        pub fn read(path: &[u8]) -> Option<Vec<u8>> {
            std::fs::read(path_of_bytes(path)).ok()
        }

        pub fn write(path: &[u8], data: &[u8]) -> bool {
            std::fs::write(path_of_bytes(path), data).is_ok()
        }

        pub fn exists(path: &[u8]) -> bool {
            std::fs::metadata(path_of_bytes(path)).is_ok()
        }

        pub fn delete(path: &[u8]) -> bool {
            let p = path_of_bytes(path);
            std::fs::remove_file(p).is_ok() || std::fs::remove_dir(p).is_ok()
        }

        pub fn mkdir(path: &[u8]) -> bool {
            std::fs::create_dir(path_of_bytes(path)).is_ok()
        }

        /// Full child paths in host order, as raw bytes.
        pub fn list(path: &[u8]) -> Option<Vec<Vec<u8>>> {
            let read = std::fs::read_dir(path_of_bytes(path)).ok()?;
            let mut out = Vec::new();
            for entry in read.flatten() {
                let child = entry.path();
                let rendered = child.to_string_lossy();
                out.push(rendered.as_bytes().to_vec());
            }
            Some(out)
        }
    }

    #[cfg(feature = "nostd")]
    mod imp {
        use alloc::vec::Vec;
        use core::ffi::c_void;

        use crate::nostd::{
            invalid_handle, CloseHandle, CreateDirectoryW, CreateFileW, DeleteFileW, FindClose,
            FindFirstFileW, FindNextFileW, GetFileAttributesW, ReadFile, RemoveDirectoryW,
            Win32FindDataW, WriteFile, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE,
            INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
        };

        use super::super::{ends_with_sep, path_w};

        pub fn read(path: &[u8]) -> Option<Vec<u8>> {
            unsafe {
                let w = path_w(path);
                let h = CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    core::ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    core::ptr::null_mut(),
                );
                if h == invalid_handle() || h.is_null() {
                    return None;
                }
                // Read in chunks until the file reports end of input rather
                // than sizing the buffer up front: that keeps one code path for
                // regular files, pipes, and anything else the handle yields,
                // and the chunk lives on the heap so the frame stays small.
                let mut data: Vec<u8> = Vec::new();
                let mut chunk: Vec<u8> = alloc::vec![0u8; 16 * 1024];
                let mut read_any = false;
                loop {
                    let mut n: u32 = 0;
                    if ReadFile(
                        h,
                        chunk.as_mut_ptr(),
                        chunk.len() as u32,
                        &mut n,
                        core::ptr::null_mut(),
                    ) == 0
                    {
                        break;
                    }
                    read_any = true;
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..n as usize]);
                }
                CloseHandle(h);
                if read_any {
                    Some(data)
                } else {
                    None
                }
            }
        }

        pub fn write(path: &[u8], data: &[u8]) -> bool {
            unsafe {
                let w = path_w(path);
                let h = CreateFileW(
                    w.as_ptr(),
                    GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    core::ptr::null_mut(),
                    CREATE_ALWAYS,
                    FILE_ATTRIBUTE_NORMAL,
                    core::ptr::null_mut(),
                );
                if h == invalid_handle() || h.is_null() {
                    return false;
                }
                let mut written: u32 = 0;
                let ok = if data.is_empty() {
                    true
                } else {
                    WriteFile(
                        h,
                        data.as_ptr(),
                        data.len() as u32,
                        &mut written,
                        core::ptr::null_mut(),
                    ) != 0
                        && written as usize == data.len()
                };
                CloseHandle(h);
                ok
            }
        }

        pub fn exists(path: &[u8]) -> bool {
            unsafe { GetFileAttributesW(path_w(path).as_ptr()) != INVALID_FILE_ATTRIBUTES }
        }

        pub fn delete(path: &[u8]) -> bool {
            unsafe {
                let w = path_w(path);
                DeleteFileW(w.as_ptr()) != 0 || RemoveDirectoryW(w.as_ptr()) != 0
            }
        }

        pub fn mkdir(path: &[u8]) -> bool {
            unsafe { CreateDirectoryW(path_w(path).as_ptr(), core::ptr::null_mut::<c_void>()) != 0 }
        }

        /// Full child paths in host order, as raw bytes. Skips the `.` and `..`
        /// entries, matching `std::fs::read_dir`.
        pub fn list(path: &[u8]) -> Option<Vec<Vec<u8>>> {
            unsafe {
                let w = path_w(path);
                // Pattern: the directory plus a separator (unless it already ends
                // in one) and `*`.
                let mut pattern: Vec<u16> = w[..w.len() - 1].to_vec(); // drop the NUL
                if !ends_with_sep(path) {
                    pattern.push('\\' as u16);
                }
                pattern.push('*' as u16);
                pattern.push(0);

                let mut data = core::mem::MaybeUninit::<Win32FindDataW>::zeroed();
                let find = FindFirstFileW(pattern.as_ptr(), data.as_mut_ptr());
                if find == invalid_handle() || find.is_null() {
                    return None;
                }
                let mut out: Vec<Vec<u8>> = Vec::new();
                loop {
                    let d = data.assume_init_ref();
                    let name = &d.c_file_name;
                    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    let is_dot = len == 1 && name[0] == '.' as u16;
                    let is_dotdot = len == 2 && name[0] == '.' as u16 && name[1] == '.' as u16;
                    if !is_dot && !is_dotdot {
                        let mut child: Vec<u16> = w[..w.len() - 1].to_vec();
                        if !ends_with_sep(path) {
                            child.push('\\' as u16);
                        }
                        child.extend_from_slice(&name[..len]);
                        out.push(alloc::string::String::from_utf16_lossy(&child).into_bytes());
                    }
                    if FindNextFileW(find, data.as_mut_ptr()) == 0 {
                        break;
                    }
                }
                FindClose(find);
                Some(out)
            }
        }
    }

    pub use imp::*;
}

/// Read a whole file as a `string`. Strings are opaque UTF-8 byte arrays, so
/// binary or non-UTF-8 content still round-trips byte-exact; a path that
/// cannot be read (missing, is a directory, no permission) is `none`.
#[no_mangle]
pub extern "C" fn pickle_read_file(path: *const PickleObject) -> *mut PickleObject {
    match backend::read(path_bytes(path)) {
        Some(data) => {
            let gc = crate::gc::gc_mut();
            string_from_bytes(data.as_ptr(), data.len(), gc)
        }
        None => core::ptr::null_mut(),
    }
}

/// Overwrite `path` with `text`'s bytes; returns whether the write succeeded.
#[no_mangle]
pub extern "C" fn pickle_write_file(path: *const PickleObject, text: *const PickleObject) -> bool {
    backend::write(path_bytes(path), path_bytes(text))
}

/// Whether `path` exists on the filesystem (any kind: file, directory, ...).
#[no_mangle]
pub extern "C" fn pickle_file_exists(path: *const PickleObject) -> bool {
    backend::exists(path_bytes(path))
}

/// Remove a file or empty directory at `path`; `false` when it cannot be
/// removed or does not exist.
#[no_mangle]
pub extern "C" fn pickle_delete(path: *const PickleObject) -> bool {
    backend::delete(path_bytes(path))
}

/// Create a directory at `path`; `false` when it already exists or cannot be
/// created. Parents are not created implicitly.
#[no_mangle]
pub extern "C" fn pickle_mkdir(path: *const PickleObject) -> bool {
    backend::mkdir(path_bytes(path))
}

/// List the directory at `path`. Returns a fresh `List<string>` of full
/// child paths (files and subdirectories) in host order, or `null` (`none`)
/// when the path is missing or is not a readable directory.
#[no_mangle]
pub extern "C" fn pickle_list_dir(path: *const PickleObject) -> *mut PickleObject {
    let entries = match backend::list(path_bytes(path)) {
        Some(e) => e,
        None => return core::ptr::null_mut(),
    };
    let gc = crate::gc::gc_mut();
    let l = crate::list::list_new(0, gc);
    let _lease = gc.temp_lease(l);
    for rendered in entries {
        let s = crate::strings::string_from_bytes(rendered.as_ptr(), rendered.len(), gc);
        crate::list::list_push(l, s);
    }
    l
}
