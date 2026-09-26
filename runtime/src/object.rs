//! Object model: `PickleObjectHeader`, class descriptor tables, and the
//! object/pointer accessors the collector and codegen rely on.
//!
//! Layout follows `docs/05-memory.md`:
//!
//! ```text
//! struct PickleObjectHeader {
//!     next:      *mut PickleObject,  // GC free-list/live-list link
//!     class_id:  u32,
//!     flags:     u32,                // mark, etc.
//!     size:      u32,                // total object bytes
//!     _pad:      u32,
//! }   // 24 bytes
//! ```
//!
//! The payload begins immediately after the header at a stable position that
//! is 8-byte aligned (24 = 3 * 8).

use alloc::vec::Vec;
use core::ffi::c_void;

/// Class ids for builtin types. User classes emitted by the compiler start at
/// `PICKLE_CLASS_USER_BASE`.
pub const PICKLE_CLASS_STRING: u32 = 0;
pub const PICKLE_CLASS_LIST: u32 = 1;
pub const PICKLE_CLASS_MAP: u32 = 2;
/// Boxed scalars: single 8-byte payload objects so `int`/`float`/`bool`/
/// `char` values can live in managed collections, fields, and optionals.
pub const PICKLE_CLASS_BOX_INT: u32 = 3;
pub const PICKLE_CLASS_BOX_FLOAT: u32 = 4;
pub const PICKLE_CLASS_BOX_BOOL: u32 = 5;
pub const PICKLE_CLASS_BOX_CHAR: u32 = 6;
/// `PEnum { header, tag: i64, fields: [*mut PickleObject] }` — a variant tag
/// followed by its boxed payload fields (see `layout::ENUM_*`).
pub const PICKLE_CLASS_ENUM: u32 = 7;
/// `PTuple { header, fields: [*mut PickleObject] }` — a boxed tuple value;
/// every payload slot is a managed pointer to a boxed scalar or object (see
/// `layout::TUPLE_*`).
pub const PICKLE_CLASS_TUPLE: u32 = 8;
/// `PStream { header, inner: *mut StreamInner }` — a buffered file/console
/// stream whose single payload slot is a *raw* pointer to a Rust-heap
/// `stream::StreamInner` (never traced; the class descriptor has no managed
/// slots and a finalizer that drops the inner state, see `layout::STREAM_*`).
pub const PICKLE_CLASS_STREAM: u32 = 9;
pub const PICKLE_CLASS_USER_BASE: u32 = 10;

/// Object flags.
pub const PICKLE_FLAG_MARKED: u32 = 1 << 0;
/// Installed a finalizer (class descriptors set this in `flags` instead).
pub const PICKLE_FLAG_FINALIZABLE: u32 = 1 << 1;
/// Programmer-owned allocation (`#[manualAlloc]`); never swept automatically,
/// released only by `pickle_manual_free`.
pub const PICKLE_FLAG_MANUAL: u32 = 1 << 2;

/// The first word of every managed object.
#[repr(C)]
pub struct PickleObjectHeader {
    /// Free-list / live-list link used by the collector.
    pub next: *mut PickleObject,
    pub class_id: u32,
    pub flags: u32,
    /// Total byte size of the object, including the header (multiple of 8).
    pub size: u32,
    pub _pad: u32,
}

/// Type alias so codegen and the runtime can name header pointers.
pub type PickleObject = PickleObjectHeader;

// The collector mutates headers through raw pointers from multiple contexts;
// Send/Sync are our invariants, not the compiler's.
unsafe impl Send for PickleObjectHeader {}
unsafe impl Sync for PickleObjectHeader {}

const HEADER_BYTES: usize = 24;

/// Tombstone sentinel used for removed map keys. Never heap-allocated and
/// never swept; its address is a stable "removed" marker.
pub static NIL_SENTINEL: PickleObjectHeader = PickleObjectHeader {
    next: core::ptr::null_mut(),
    class_id: PICKLE_CLASS_STRING,
    flags: 0,
    size: 0,
    _pad: 0,
};

/// Address of the tombstone sentinel.
pub const fn nil_sentinel() -> *const PickleObjectHeader {
    &NIL_SENTINEL
}

/// Canonical `none` map key. An `Option<T>` key's `none` value is the null
/// pointer at the runtime boundary, but null is the map table's empty-slot
/// marker, so a `none` key cannot be stored as-is. Map operations therefore
/// normalize a null key to this sentinel before hashing/comparing/storing
/// (and hash/equality treat the sentinel exactly like null). Never
/// heap-allocated and never swept.
pub static NONE_KEY: PickleObjectHeader = PickleObjectHeader {
    next: core::ptr::null_mut(),
    class_id: PICKLE_CLASS_STRING,
    flags: 0,
    size: 0,
    _pad: 0,
};

/// Address of the canonical `none`-key sentinel.
pub const fn none_key() -> *const PickleObjectHeader {
    &NONE_KEY
}

/// Byte offset of the payload within an object.
pub const fn pickle_header_size() -> usize {
    HEADER_BYTES
}

impl PickleObjectHeader {
    /// Create a fresh header; `size` must be >= header and a multiple of 8.
    pub fn init(&mut self, class_id: u32, size: u32) {
        self.next = core::ptr::null_mut();
        self.class_id = class_id;
        self.flags = 0;
        self.size = size;
        self._pad = 0;
    }

    pub fn is_marked(&self) -> bool {
        self.flags & PICKLE_FLAG_MARKED != 0
    }

    pub fn set_marked(&mut self) {
        self.flags |= PICKLE_FLAG_MARKED;
    }

    pub fn clear_marked(&mut self) {
        self.flags &= !PICKLE_FLAG_MARKED;
    }

    /// Raw payload pointer (immutable).
    pub fn payload(&self) -> *const c_void {
        unsafe { (self as *const PickleObject as *const u8).add(HEADER_BYTES) as *const c_void }
    }

    /// Raw mutable payload pointer.
    pub fn payload_mut(&mut self) -> *mut c_void {
        unsafe { (self as *mut PickleObject as *mut u8).add(HEADER_BYTES) as *mut c_void }
    }

    /// Convenience: read a payload slot as a managed pointer.
    pub fn slot(&self, index: usize) -> *mut PickleObject {
        unsafe {
            let base = self.payload() as *const *mut PickleObject;
            base.add(index).read()
        }
    }

    /// Write a payload slot as a managed pointer.
    pub fn set_slot(&mut self, index: usize, value: *mut PickleObject) {
        unsafe {
            let base = self.payload_mut() as *mut *mut PickleObject;
            base.add(index).write(value);
        }
    }
}

/// Root/static cell registration: a stable `*mut *mut PickleObject` that the
/// collector marks from. Used for global variables the compiler emits.
#[repr(C)]
pub struct RootCell {
    pub cell: *mut *mut PickleObject,
    pub next: *mut RootCell,
}

// Root cells are traced under the static-root mutex; Sync is our invariant.
unsafe impl Send for RootCell {}
unsafe impl Sync for RootCell {}

/// A class descriptor as emitted by the compiler and registered at startup.
///
/// `managed_mask` is an array of 32-bit words: bit `i` in word `i / 32`
/// (counting from bit 0 of word 0) marks payload *slot* `i` (8-byte units) as
/// holding a managed pointer for the collector. Words beyond the payload are
/// ignored.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ClassDescriptor {
    pub name_ptr: *const u8,
    pub name_len: u32,
    /// `PICKLE_CLASS_FLAG_*` bits.
    pub flags: u32,
    /// Number of payload slots (8-byte units) the collector considers.
    pub slot_count: u32,
    /// Number of u32 words in `managed_mask`.
    pub mask_words: u32,
    pub managed_mask: *const u32,
    /// Finalizer, if `flags & PICKLE_CLASS_FLAG_FINALIZER != 0`.
    pub finalizer: extern "C" fn(*mut PickleObject),
}

pub const PICKLE_CLASS_FLAG_FINALIZER: u32 = 1 << 0;

/// A registry of class descriptors. The compiler emits a contiguous table and
/// calls `pickle_descriptor_register` for each, or registers the whole table
/// in one call. `class_id` is the index into `descriptors`.
pub struct DescriptorTable {
    pub descriptors: Vec<ClassDescriptor>,
}

impl DescriptorTable {
    pub fn new() -> DescriptorTable {
        DescriptorTable {
            descriptors: Vec::new(),
        }
    }

    pub fn register(&mut self, d: ClassDescriptor) -> u32 {
        let id = self.descriptors.len() as u32;
        self.descriptors.push(d);
        id
    }

    pub fn get(&self, class_id: u32) -> Option<&ClassDescriptor> {
        if (class_id as usize) < self.descriptors.len() {
            Some(&self.descriptors[class_id as usize])
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }
}

impl Default for DescriptorTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A class descriptor for the builtin string type. Never stored in the
/// compiler-emitted table; builtins have reserved ids.
pub const STRING_DESCRIPTOR: ClassDescriptor = ClassDescriptor {
    name_ptr: b"string\0".as_ptr(),
    name_len: 6,
    flags: 0,
    slot_count: 0,
    mask_words: 0,
    managed_mask: core::ptr::null(),
    finalizer: builtin_nop_finalizer,
};

pub(crate) extern "C" fn builtin_nop_finalizer(_obj: *mut PickleObject) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_24_bytes() {
        assert_eq!(core::mem::size_of::<PickleObjectHeader>(), 24);
    }

    #[test]
    fn payload_offset_is_24() {
        let mut obj: PickleObjectHeader = PickleObjectHeader {
            next: core::ptr::null_mut(),
            class_id: 0,
            flags: 0,
            size: 32,
            _pad: 0,
        };
        let base = &mut obj as *mut PickleObjectHeader as usize;
        assert_eq!(base + 24, obj.payload_mut() as usize);
    }

    #[test]
    fn marker_roundtrip() {
        let mut obj: PickleObjectHeader = PickleObjectHeader {
            next: core::ptr::null_mut(),
            class_id: 0,
            flags: 0,
            size: 32,
            _pad: 0,
        };
        assert!(!obj.is_marked());
        obj.set_marked();
        assert!(obj.is_marked());
        obj.clear_marked();
        assert!(!obj.is_marked());
    }
}
