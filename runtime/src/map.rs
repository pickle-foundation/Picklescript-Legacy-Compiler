//! Builtin `Map<K, V>` support.
//!
//! Keys are managed objects: a `string` object (class `PICKLE_CLASS_STRING`),
//! a boxed scalar (one of the `PICKLE_CLASS_BOX_*` classes), or a composite
//! object (list, map, enum, class/struct instance). Hashing and equality
//! dispatch on the key's class id: strings hash over their bytes, boxes over
//! `(class_id, payload)`, and composite keys hash/compare *structurally*
//! (deep), so `Map<int, V>`, `Map<List<int>, V>`, `Map<MyClass, V>`, and
//! `Map<string, V>` all share this table. Cyclic structures fall back to
//! root-identity semantics.
//!
//! Layout per `docs/05-memory.md`: `{ header, len, cap, entries: *mut Entry }`
//! with `Entry { key, value }` pairs; open addressing with linear probing and
//! a power-of-two table. The collector scans live entries as managed refs.

use alloc::vec::Vec;

use crate::boxscalar::box_bits;
use crate::gc::Gc;
use crate::layout::{
    enum_field_count, enum_tag, list_data, list_len, map_cap, map_entries, map_len, map_set_cap,
    map_set_entries, map_set_len, tuple_field_count, MapEntry, MAP_OBJECT_SIZE,
};
use crate::object::{
    nil_sentinel, none_key, PickleObject, PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR,
    PICKLE_CLASS_BOX_FLOAT, PICKLE_CLASS_BOX_INT, PICKLE_CLASS_ENUM, PICKLE_CLASS_LIST,
    PICKLE_CLASS_MAP, PICKLE_CLASS_STRING, PICKLE_CLASS_TUPLE,
};
use crate::strings::{string_bytes_len, string_bytes_ptr, string_cmp};

const INITIAL_CAP: usize = 16;
const MAX_LOAD: f64 = 0.7;

/// FNV-1a over a string's bytes.
/// Debug probe (`PKL_GC_CHECK`): a managed container argument must still be a
/// live heap block *of the expected builtin class*. Enabled builds turn a
/// premature free — or a pointer of the wrong shape, which reads as garbage
/// header fields — into a named panic at the argument site instead of a
/// mistyped pointer read further down.
#[inline]
fn gc_check_obj(p: *const PickleObject, what: &str) {
    if !crate::gc::gc_check_enabled() || p.is_null() {
        return;
    }
    if core::ptr::eq(p, nil_sentinel()) || core::ptr::eq(p, none_key()) {
        return;
    }
    let gc = crate::gc::gc_mut();
    if !gc.heap.debug_live_block(p) {
        crate::gc::report_call_site(what);
        panic!("pickle: UAF: {what} argument {p:?} is not a live heap block");
    }
    let class_id = unsafe { (*p).class_id };
    if !matches!(
        class_id,
        crate::object::PICKLE_CLASS_STRING
            | crate::object::PICKLE_CLASS_LIST
            | crate::object::PICKLE_CLASS_MAP
            | crate::object::PICKLE_CLASS_TUPLE
            | crate::object::PICKLE_CLASS_ENUM
            | crate::object::PICKLE_CLASS_BOX_INT
            | crate::object::PICKLE_CLASS_BOX_FLOAT
            | crate::object::PICKLE_CLASS_BOX_BOOL
            | crate::object::PICKLE_CLASS_BOX_CHAR
            | crate::object::PICKLE_CLASS_STREAM
    ) {
        crate::gc::report_call_site(what);
        panic!("pickle: bad argument: {what} argument {p:?} has class {class_id}");
    }
}

/// Debug probe for a container argument whose class the caller requires.
#[inline]
fn gc_check_map(p: *const PickleObject, what: &str) {
    gc_check_obj(p, what);
    if crate::gc::gc_check_enabled() && !p.is_null() {
        let class_id = unsafe { (*p).class_id };
        if class_id != crate::object::PICKLE_CLASS_MAP {
            crate::gc::report_call_site(what);
            panic!("pickle: bad argument: {what} argument {p:?} is class {class_id}, not a map");
        }
    }
}

fn fnv1a(bytes: *const u8, len: usize) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    unsafe {
        for i in 0..len {
            h ^= *bytes.add(i) as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// Hash a map key object: strings hash over their bytes; boxed scalars hash
/// over `(class_id, payload)`; and composite keys — lists, maps, enums, and
/// user-class/struct instances — hash structurally (deep). `class_id` is
/// mixed in everywhere so equal payloads never collide across key types.
///
/// A key that contains a cycle (e.g. a list that refers back to itself) falls
/// back to *root-identity* hashing: identity is the one strength-safe
/// semantics for cyclic structures.
fn key_hash(key: *const PickleObject) -> u64 {
    gc_check_obj(key, "map key_hash");

    unsafe {
        // A `none` key (null to the outside, the canonical sentinel to the
        // table) is one distinguished identity: its hash matches the fixed
        // constant `deep_hash` already uses for a null slot, so none-as-key
        // and none-nested are interchangeable.
        if key.is_null() || core::ptr::eq(key, none_key()) {
            return 0x9e37_79b9_7f4a_7c15;
        }
        let class_id = (*key).class_id;
        match class_id {
            PICKLE_CLASS_STRING => fnv1a(string_bytes_ptr(key), string_bytes_len(key)),
            PICKLE_CLASS_BOX_INT
            | PICKLE_CLASS_BOX_FLOAT
            | PICKLE_CLASS_BOX_BOOL
            | PICKLE_CLASS_BOX_CHAR => {
                let bits = box_bits(key);
                let mut h: u64 = 0xcbf29ce484222325;
                h ^= class_id as u64;
                h = h.wrapping_mul(0x100000001b3);
                h ^= bits as u64;
                h = h.wrapping_mul(0x100000001b3);
                h
            }
            _ => {
                let mut path: Vec<*const PickleObject> = Vec::new();
                let (h, cyclic) = deep_hash(key, &mut path);
                if cyclic {
                    let mut h: u64 = 0xcbf29ce484222325;
                    h ^= key as usize as u64;
                    h = h.wrapping_mul(0x100000001b3);
                    h ^= class_id as u64;
                    h = h.wrapping_mul(0x100000001b3);
                    h
                } else {
                    h
                }
            }
        }
    }
}

/// Keys equal when their class matches (string vs string, int box vs int box,
/// ...) and their bytes/payload match. A string never equals a box. Composite
/// keys compare structurally (deep); cyclic keys fall back to *root identity*
/// — the same strength-safe semantics used by `key_hash` above.
fn key_eq(a: *const PickleObject, b: *const PickleObject) -> bool {
    if core::ptr::eq(a, b) {
        return true;
    }
    // A `none` key equals only itself (its identity is null/sentinel).
    if a.is_null() || core::ptr::eq(a, none_key()) || b.is_null() || core::ptr::eq(b, none_key()) {
        return false;
    }
    let mut path: Vec<(*const PickleObject, *const PickleObject)> = Vec::new();
    match deep_eq(a, b, &mut path) {
        Cmp::Equal => true,
        Cmp::Diff => false,
        Cmp::Cyclic => false,
    }
}

/// Deep structural hash. `path` holds the ancestor chain so cycles are
/// detected (a revisit reports `cyclic = true` instead of descending); equal
/// acyclic structures always produce the same hash.
fn deep_hash(obj: *const PickleObject, path: &mut Vec<*const PickleObject>) -> (u64, bool) {
    unsafe {
        if obj.is_null() || core::ptr::eq(obj, none_key()) {
            return (0x9e37_79b9_7f4a_7c15, false);
        }
        if path.contains(&obj) {
            return (0x2f02_9c2d_f8c0_0001, true);
        }
        path.push(obj);
        let class_id = (*obj).class_id;
        // `FREE_SENTINEL` blocks can reach `deep_hash` only through a stale
        // key (a live value freed by a collection then reused as a map key,
        // observed in the selfhost build). Hash by identity instead of
        // trampling a free-list block; lookups then miss rather than corrupt.
        if class_id == crate::heap::FREE_SENTINEL {
            let eh = 0xcbf29ce484222325u64
                ^ (class_id as u64).wrapping_mul(0x9e3779b97f4a7c15)
                ^ (obj as u64);
            return (eh.wrapping_mul(0x100000001b3), false);
        }
        let mut h: u64 = 0xcbf29ce484222325;
        let mut cyclic = false;
        let mix = |h: &mut u64, x: u64| {
            *h ^= x;
            *h = h.wrapping_mul(0x100000001b3);
        };
        match class_id {
            PICKLE_CLASS_STRING => {
                h = fnv1a(string_bytes_ptr(obj), string_bytes_len(obj));
            }
            PICKLE_CLASS_BOX_INT
            | PICKLE_CLASS_BOX_FLOAT
            | PICKLE_CLASS_BOX_BOOL
            | PICKLE_CLASS_BOX_CHAR => {
                mix(&mut h, class_id as u64);
                mix(&mut h, box_bits(obj) as u64);
            }
            PICKLE_CLASS_LIST => {
                // Ordered: element order participates in the hash.
                mix(&mut h, class_id as u64);
                let len = list_len(obj);
                let data = list_data(obj);
                let mut i = 0usize;
                while i < len {
                    let (eh, ec) = deep_hash(data.add(i).read(), path);
                    mix(&mut h, eh);
                    cyclic |= ec;
                    i += 1;
                }
            }
            PICKLE_CLASS_MAP => {
                // Order-insensitive: entries combine by addition, so two maps
                // holding the same key/value pairs hash alike regardless of
                // insertion order.
                mix(&mut h, class_id as u64);
                let cap = map_cap(obj);
                let entries = map_entries(obj);
                let mut sum: u64 = 0;
                let mut i = 0usize;
                while i < cap {
                    let e = entries.add(i).read();
                    let k = e.key;
                    if k.is_null() || core::ptr::eq(k, crate::object::nil_sentinel()) {
                        i += 1;
                        continue;
                    }
                    let (kh, kc) = deep_hash(k, path);
                    let (vh, vc) = deep_hash(e.value, path);
                    cyclic |= kc | vc;
                    sum = sum
                        .wrapping_add(kh)
                        .wrapping_add(vh.wrapping_mul(0x9e3779b97f4a7c15));
                    i += 1;
                }
                mix(&mut h, sum);
            }
            PICKLE_CLASS_ENUM => {
                mix(&mut h, class_id as u64);
                mix(&mut h, enum_tag(obj) as u64);
                let n = enum_field_count(obj);
                let mut i = 0usize;
                while i < n {
                    let (eh, ec) = deep_hash((*obj).slot(1 + i), path);
                    mix(&mut h, eh);
                    cyclic |= ec;
                    i += 1;
                }
            }
            PICKLE_CLASS_TUPLE => {
                // Ordered: arity and element order participate.
                mix(&mut h, class_id as u64);
                let n = tuple_field_count(obj);
                mix(&mut h, n as u64);
                let mut i = 0usize;
                while i < n {
                    let (eh, ec) = deep_hash((*obj).slot(1 + i), path);
                    mix(&mut h, eh);
                    cyclic |= ec;
                    i += 1;
                }
            }
            _ => {
                // User class/struct/interface instance: class identity + slots.
                mix(&mut h, class_id as u64);
                let size = (*obj).size as usize;
                let header = crate::object::pickle_header_size();
                // A block header reporting an absurd size is a stale/reused key
                // (freed at a collection, reallocated since). Hashing its raw
                // slots would walk arbitrary memory (previously a misaligned
                // deref at 0x3d); hash by identity instead so lookups miss.
                if size < header || size >= (1 << 26) {
                    let eh = 0xcbf29ce484222325u64
                        ^ (class_id as u64).wrapping_mul(0x9e3779b97f4a7c15)
                        ^ (obj as u64);
                    return (eh.wrapping_mul(0x100000001b3), false);
                }
                let slots = size.saturating_sub(crate::object::pickle_header_size()) / 8;
                let mut i = 0usize;
                while i < slots {
                    let (sh, sc) = deep_hash((*obj).slot(i), path);
                    mix(&mut h, sh);
                    cyclic |= sc;
                    i += 1;
                }
            }
        }
        path.pop();
        (h, cyclic)
    }
}

/// Result of a deep-equality probe.
#[derive(Clone, Copy, PartialEq)]
enum Cmp {
    /// Structurally equal.
    Equal,
    /// Structurally different.
    Diff,
    /// The probe revisited an ancestor pair: the structure contains a cycle,
    /// so the caller falls back to root-identity comparison.
    Cyclic,
}

/// Deep structural equality. `path` holds the ancestor `(a, b)` pairs so
/// cycles are detected rather than recursed forever (see `Cmp::Cyclic`).
fn deep_eq(
    a: *const PickleObject,
    b: *const PickleObject,
    path: &mut Vec<(*const PickleObject, *const PickleObject)>,
) -> Cmp {
    unsafe {
        if core::ptr::eq(a, b) {
            return Cmp::Equal;
        }
        // A `none` (null or sentinel) equals only itself.
        if a.is_null() || core::ptr::eq(a, none_key()) {
            return if b.is_null() || core::ptr::eq(b, none_key()) {
                Cmp::Equal
            } else {
                Cmp::Diff
            };
        }
        if b.is_null() || core::ptr::eq(b, none_key()) {
            return Cmp::Diff;
        }
        let pair = (a, b);
        if path.contains(&pair) {
            return Cmp::Cyclic;
        }
        path.push(pair);
        let r = if (*a).class_id != (*b).class_id {
            Cmp::Diff
        } else {
            match (*a).class_id {
                PICKLE_CLASS_STRING => {
                    if string_cmp(a, b) == 0 {
                        Cmp::Equal
                    } else {
                        Cmp::Diff
                    }
                }
                PICKLE_CLASS_BOX_INT
                | PICKLE_CLASS_BOX_FLOAT
                | PICKLE_CLASS_BOX_BOOL
                | PICKLE_CLASS_BOX_CHAR => {
                    if box_bits(a) == box_bits(b) {
                        Cmp::Equal
                    } else {
                        Cmp::Diff
                    }
                }
                PICKLE_CLASS_LIST => {
                    let la = list_len(a);
                    let lb = list_len(b);
                    if la != lb {
                        Cmp::Diff
                    } else {
                        let da = list_data(a);
                        let db = list_data(b);
                        let mut r = Cmp::Equal;
                        let mut i = 0usize;
                        while i < la {
                            r = deep_eq(da.add(i).read(), db.add(i).read(), path);
                            if r != Cmp::Equal {
                                break;
                            }
                            i += 1;
                        }
                        r
                    }
                }
                PICKLE_CLASS_MAP => {
                    // Order-insensitive: every live entry of `a` must match a
                    // distinct live entry of `b`.
                    let la = map_len(a);
                    let lb = map_len(b);
                    if la != lb {
                        Cmp::Diff
                    } else {
                        let capa = map_cap(a);
                        let ea = map_entries(a);
                        let capb = map_cap(b);
                        let eb = map_entries(b);
                        let mut used: Vec<bool> = vec![false; capb];
                        let mut r = Cmp::Equal;
                        let mut i = 0usize;
                        while i < capa && r == Cmp::Equal {
                            let ka = ea.add(i).read().key;
                            let va = ea.add(i).read().value;
                            if ka.is_null() || core::ptr::eq(ka, crate::object::nil_sentinel()) {
                                i += 1;
                                continue;
                            }
                            let mut found = false;
                            let mut j = 0usize;
                            while j < capb {
                                if !used[j] {
                                    let kb = eb.add(j).read().key;
                                    let vb = eb.add(j).read().value;
                                    if !kb.is_null()
                                        && !core::ptr::eq(kb, crate::object::nil_sentinel())
                                    {
                                        match deep_eq(ka, kb, path) {
                                            Cmp::Equal => match deep_eq(va, vb, path) {
                                                Cmp::Equal => {
                                                    used[j] = true;
                                                    found = true;
                                                    j = capb;
                                                }
                                                Cmp::Diff => {}
                                                c => {
                                                    r = c;
                                                    j = capb;
                                                }
                                            },
                                            Cmp::Diff => {}
                                            c => {
                                                r = c;
                                                j = capb;
                                            }
                                        }
                                    }
                                }
                                j += 1;
                            }
                            if !found && r == Cmp::Equal {
                                r = Cmp::Diff;
                            }
                            i += 1;
                        }
                        r
                    }
                }
                PICKLE_CLASS_ENUM => {
                    if enum_tag(a) != enum_tag(b) {
                        Cmp::Diff
                    } else {
                        let na = enum_field_count(a);
                        let nb = enum_field_count(b);
                        if na != nb {
                            Cmp::Diff
                        } else {
                            let mut r = Cmp::Equal;
                            let mut i = 0usize;
                            while i < na {
                                r = deep_eq((*a).slot(1 + i), (*b).slot(1 + i), path);
                                if r != Cmp::Equal {
                                    break;
                                }
                                i += 1;
                            }
                            r
                        }
                    }
                }
                PICKLE_CLASS_TUPLE => {
                    let na = tuple_field_count(a);
                    let nb = tuple_field_count(b);
                    if na != nb {
                        Cmp::Diff
                    } else {
                        let mut r = Cmp::Equal;
                        let mut i = 0usize;
                        while i < na {
                            r = deep_eq((*a).slot(1 + i), (*b).slot(1 + i), path);
                            if r != Cmp::Equal {
                                break;
                            }
                            i += 1;
                        }
                        r
                    }
                }
                _ => {
                    // User class/struct/interface instance.
                    let sa = (*a).size as usize;
                    let sb = (*b).size as usize;
                    let na = sa.saturating_sub(crate::object::pickle_header_size()) / 8;
                    let nb = sb.saturating_sub(crate::object::pickle_header_size()) / 8;
                    if na != nb {
                        Cmp::Diff
                    } else {
                        let mut r = Cmp::Equal;
                        let mut i = 0usize;
                        while i < na {
                            r = deep_eq((*a).slot(i), (*b).slot(i), path);
                            if r != Cmp::Equal {
                                break;
                            }
                            i += 1;
                        }
                        r
                    }
                }
            }
        };
        path.pop();
        r
    }
}

fn find_slot(
    entries: *mut MapEntry,
    cap: usize,
    key: *const PickleObject,
    key_hash: u64,
) -> (*mut MapEntry, bool) {
    // Returns (empty-or-match slot, found)
    unsafe {
        let mask = cap - 1;
        let mut idx = (key_hash as usize) & mask;
        let mut first_tomb: *mut MapEntry = core::ptr::null_mut();
        let key_str = |e: *mut MapEntry| (*e).key;
        loop {
            let slot = entries.add(idx);
            let k = key_str(slot);
            if k.is_null() {
                // Empty: probe `first_tomb` if we passed any.
                let target = if first_tomb.is_null() {
                    slot
                } else {
                    first_tomb
                };
                return (target, false);
            }
            // Tombstone check (key == the unique tombstone sentinel).
            if core::ptr::eq(k, crate::object::nil_sentinel()) {
                if first_tomb.is_null() {
                    first_tomb = slot;
                }
            } else if key_eq(k, key) {
                return (slot, true);
            }
            idx = (idx + 1) & mask;
        }
    }
}

fn needs_grow(entries: *mut MapEntry, cap: usize, _len: usize) -> bool {
    let occupied = occupied_count(entries, cap);
    (occupied as f64 + 1.0) >= (cap as f64 * MAX_LOAD)
}

/// Slots that hold *anything* - a live entry or a tombstone. Growth must key off
/// this, not the live count: a tombstone still occupies a probe slot, so a table
/// whose live count is low but whose tombstones fill the array has no empty slot
/// left and `find_slot` would probe forever. Counting tombstones here forces a
/// rehash (which compacts them away) before that can happen.
fn occupied_count(entries: *mut MapEntry, cap: usize) -> usize {
    unsafe {
        let mut n = 0usize;
        for i in 0..cap {
            let k = entries.add(i).read().key;
            if !k.is_null() {
                n += 1;
            }
        }
        n
    }
}

fn rehash(
    old_entries: *mut MapEntry,
    old_cap: usize,
    new_cap: usize,
    gc: &mut Gc,
) -> *mut MapEntry {
    unsafe {
        let fresh = gc
            .heap
            .raw_alloc(new_cap * core::mem::size_of::<MapEntry>());
        core::ptr::write_bytes(fresh, 0, new_cap * core::mem::size_of::<MapEntry>());
        let fresh = fresh as *mut MapEntry;
        for i in 0..old_cap {
            let mut e = old_entries.add(i).read();
            if !e.key.is_null() && !core::ptr::eq(e.key, crate::object::nil_sentinel()) {
                let key_hash = key_hash(e.key);
                let (slot, _) = find_slot(fresh, new_cap, e.key, key_hash);
                core::ptr::swap(slot, &mut e);
            }
        }
        crate::heap::raw_free(old_entries as *mut u8);
        fresh
    }
}

/// Create an empty map.
pub fn map_new(cap: usize, gc: &mut Gc) -> *mut PickleObject {
    let cap = cap.max(INITIAL_CAP).next_power_of_two();
    let obj = gc.alloc(MAP_OBJECT_SIZE as u32, PICKLE_CLASS_MAP);
    unsafe {
        map_set_len(obj, 0);
        map_set_cap(obj, cap);
        let entries = gc.heap.raw_alloc(cap * core::mem::size_of::<MapEntry>());
        core::ptr::write_bytes(entries, 0, cap * core::mem::size_of::<MapEntry>());
        map_set_entries(obj, entries as *mut MapEntry);
    }
    obj
}

/// Insert `key -> value`. Returns the previous value, if any.
pub fn map_set(
    map: *mut PickleObject,
    key: *const PickleObject,
    value: *mut PickleObject,
) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    unsafe {
        // `none` is null at the boundary; the table needs a non-null marker.
        let key = if key.is_null() { none_key() } else { key };
        let key_hash = key_hash(key);
        if needs_grow(map_entries(map), map_cap(map), map_len(map)) {
            let new_cap = map_cap(map) * 2;
            let fresh = rehash(map_entries(map), map_cap(map), new_cap, gc);
            map_set_entries(map, fresh);
            map_set_cap(map, new_cap);
        }
        let (slot, found) = find_slot(map_entries(map), map_cap(map), key, key_hash);
        let old = (*slot).value;
        (*slot).key = key as *mut PickleObject;
        (*slot).value = value;
        if !found {
            map_set_len(map, map_len(map) + 1);
        }
        old
    }
}

/// Look up `key`; returns null when absent.
pub fn map_get(map: *const PickleObject, key: *const PickleObject) -> *mut PickleObject {
    gc_check_map(map, "map_get");
    gc_check_obj(key, "map_get key");

    unsafe {
        let key = if key.is_null() { none_key() } else { key };
        let key_hash = key_hash(key);
        // Probe with a small guard: always within cap steps.
        let cap = map_cap(map);
        let mask = cap - 1;
        let mut idx = (key_hash as usize) & mask;
        for _ in 0..cap {
            let slot = map_entries(map).add(idx);
            let k = (*slot).key;
            if k.is_null() {
                return core::ptr::null_mut();
            }
            if !core::ptr::eq(k, crate::object::nil_sentinel()) && key_eq(k, key) {
                return (*slot).value;
            }
            idx = (idx + 1) & mask;
        }
        core::ptr::null_mut()
    }
}

/// Remove `key`. Returns the value if present, else null.
pub fn map_remove(map: *mut PickleObject, key: *const PickleObject) -> *mut PickleObject {
    unsafe {
        let key = if key.is_null() { none_key() } else { key };
        let key_hash = key_hash(key);
        let cap = map_cap(map);
        let mask = cap - 1;
        let mut idx = (key_hash as usize) & mask;
        for _ in 0..cap {
            let slot = map_entries(map).add(idx);
            let k = (*slot).key;
            if k.is_null() {
                return core::ptr::null_mut();
            }
            if !core::ptr::eq(k, crate::object::nil_sentinel()) && key_eq(k, key) {
                let v = (*slot).value;
                (*slot).key = crate::object::nil_sentinel() as *mut PickleObject;
                (*slot).value = core::ptr::null_mut();
                map_set_len(map, map_len(map) - 1);
                return v;
            }
            idx = (idx + 1) & mask;
        }
        core::ptr::null_mut()
    }
}

/// Number of live entries.
pub fn map_len_of(map: *const PickleObject) -> usize {
    gc_check_map(map, "map_len_of");

    map_len(map)
}

/// Materialize the live keys (or values) as a fresh `List`.
fn map_snapshot(map: *const PickleObject, want_keys: bool) -> *mut PickleObject {
    gc_check_map(map, "map_snapshot");
    let gc = crate::gc::gc_mut();
    let l = crate::list::list_new(0, gc);
    gc_check_map(map, "map_snapshot/after list_new");
    let cap = map_cap(map);
    let entries = map_entries(map);
    unsafe {
        for i in 0..cap {
            let e = &*entries.add(i);
            if e.key.is_null() || core::ptr::eq(e.key, nil_sentinel()) {
                continue;
            }
            // A stored `none` key is the sentinel; surface it as null, the
            // runtime representation the compiler expects for `T?`.
            let val = if want_keys && core::ptr::eq(e.key, none_key()) {
                core::ptr::null_mut()
            } else if want_keys {
                e.key
            } else {
                e.value
            };
            crate::list::list_push(l, val);
        }
    }
    l
}

// ---- Exported ABI ----------------------------------------------------------
// Thin `extern "C"` wrappers over the internal Rust APIs. Keys are managed
// objects — a string object or a boxed scalar (`boxscalar::pickle_box_*`) —
// and are hashed/compared by `key_hash`/`key_eq`. Scalar values must be boxed
// first (`boxscalar::pickle_box_*`) before being stored, and unboxed after
// reading.

/// Create an empty map; `cap` is a requested capacity hint (a power of two).
#[no_mangle]
pub extern "C" fn pickle_map_new(cap: usize) -> *mut PickleObject {
    let gc = crate::gc::gc_mut();
    map_new(cap, gc)
}

/// Insert `key -> value`. The previous value (if any) is dropped.
#[no_mangle]
pub extern "C" fn pickle_map_set(
    map: *mut PickleObject,
    key: *const PickleObject,
    value: *mut PickleObject,
) {
    let _ = map_set(map, key, value);
}

/// Look up `key`; when absent, returns `default` instead of null so compiled
/// code never unboxes a null pointer.
#[no_mangle]
pub extern "C" fn pickle_map_get_boxed(
    map: *const PickleObject,
    key: *const PickleObject,
    default: *mut PickleObject,
) -> *mut PickleObject {
    let got = map_get(map, key);
    if got.is_null() {
        default
    } else {
        got
    }
}

/// Look up `key`; returns the stored value (a boxed scalar or managed
/// pointer) when present, else null — exactly the `T?` option representation,
/// so compiled code can hand the result straight to an option-typed slot.
#[no_mangle]
pub extern "C" fn pickle_map_get(
    map: *const PickleObject,
    key: *const PickleObject,
) -> *mut PickleObject {
    map_get(map, key)
}

/// Whether `key` is present.
#[no_mangle]
pub extern "C" fn pickle_map_has(map: *const PickleObject, key: *const PickleObject) -> bool {
    !map_get(map, key).is_null()
}

/// Remove `key`; returns the removed value (a boxed scalar or managed
/// pointer) when present, else null — exactly the `T?` option representation,
/// so compiled code can hand the result straight to an option-typed slot.
#[no_mangle]
pub extern "C" fn pickle_map_remove(
    map: *mut PickleObject,
    key: *const PickleObject,
) -> *mut PickleObject {
    map_remove(map, key)
}

/// Number of live entries.
#[no_mangle]
pub extern "C" fn pickle_map_len(map: *const PickleObject) -> usize {
    map_len_of(map)
}

/// All live keys as a fresh `List` (string pointers).
#[no_mangle]
pub extern "C" fn pickle_map_keys(map: *const PickleObject) -> *mut PickleObject {
    map_snapshot(map, true)
}

/// All live values as a fresh `List` (boxed/pointer values).
#[no_mangle]
pub extern "C" fn pickle_map_values(map: *const PickleObject) -> *mut PickleObject {
    map_snapshot(map, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::gc::test_begin()
    }

    #[test]
    fn insert_lookup_remove() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let k1 = crate::strings::string_from_bytes(b"alpha".as_ptr(), 5, gc);
        let k2 = crate::strings::string_from_bytes(b"beta".as_ptr(), 4, gc);
        let v1 = crate::strings::int64_to_string(11);
        let v2 = crate::strings::int64_to_string(22);
        assert!(map_get(m, k1).is_null());
        assert!(map_set(m, k1, v1).is_null());
        assert_eq!(map_get(m, k1), v1);
        assert!(map_get(m, k2).is_null());
        assert!(map_set(m, k2, v2).is_null());
        assert_eq!(map_get(m, k2), v2);
        assert_eq!(map_len_of(m), 2);
        // Overwrite returns old.
        let v3 = crate::strings::int64_to_string(33);
        assert_eq!(map_set(m, k1, v3), v1);
        assert_eq!(map_get(m, k1), v3);
        // Remove.
        assert_eq!(map_remove(m, k2), v2);
        assert!(map_get(m, k2).is_null());
        assert_eq!(map_len_of(m), 1);
    }

    #[test]
    fn many_keys_grow() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let mut expected: Vec<*mut PickleObject> = Vec::new();
        for i in 0..500usize {
            let k = crate::strings::int64_to_string(i as i64);
            let v = crate::strings::int64_to_string((i * 7) as i64);
            expected.push(v);
            map_set(m, k, v);
        }
        for (i, v) in expected.iter().enumerate() {
            let k = crate::strings::int64_to_string(i as i64);
            assert_eq!(map_get(m, k), *v, "key {i}");
        }
        assert_eq!(map_len_of(m), 500);
    }

    #[test]
    fn tombstones_count_toward_growth() {
        // Regression: `needs_grow` once counted LIVE entries only, so the tombstones
        // left by `map_remove` could fill every probe slot while the live count stayed
        // low. The next insert then called `find_slot` on a table with no empty slot to
        // stop on and probed forever. This is the smallest form of the real sequence
        // (grow / churn / grow / churn / grow); it hung before `occupied_count` was
        // folded into `needs_grow`.
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let ins = |m: *mut PickleObject, a: usize, b: usize| {
            for i in a..b {
                let k = crate::strings::int64_to_string(i as i64);
                let v = crate::strings::int64_to_string((i * 7) as i64);
                map_set(m, k, v);
            }
        };
        let del = |m: *mut PickleObject, a: usize, b: usize| {
            for i in a..b {
                let k = crate::strings::int64_to_string(i as i64);
                map_remove(m, k);
            }
        };
        ins(m, 0, 61);
        del(m, 15, 45);
        ins(m, 61, 91);
        del(m, 45, 75);
        ins(m, 91, 121);
        del(m, 75, 105);
        ins(m, 121, 129);
        assert_eq!(map_len_of(m), 39);
        // Survivors reachable, removed keys gone.
        let k5 = crate::strings::int64_to_string(5);
        assert!(!map_get(m, k5).is_null());
        let k20 = crate::strings::int64_to_string(20);
        assert!(map_get(m, k20).is_null());
        let k128 = crate::strings::int64_to_string(128);
        assert!(!map_get(m, k128).is_null());
    }

    #[test]
    fn abi_roundtrip_boxed_ints() {
        let _guard = setup();
        crate::pickle_runtime_init();
        let m = crate::map::pickle_map_new(0);
        let k1 = crate::strings::string_from_bytes(b"alpha".as_ptr(), 5, crate::gc::gc_mut());
        let k2 = crate::strings::string_from_bytes(b"beta".as_ptr(), 4, crate::gc::gc_mut());
        crate::map::pickle_map_set(m, k1, crate::boxscalar::pickle_box_i64(7));
        crate::map::pickle_map_set(m, k2, crate::boxscalar::pickle_box_i64(-3));
        assert!(crate::map::pickle_map_has(m, k1));
        let missing = crate::strings::string_from_bytes(b"gamma".as_ptr(), 5, crate::gc::gc_mut());
        assert!(!crate::map::pickle_map_has(m, missing));
        assert_eq!(crate::map::pickle_map_len(m), 2);
        let default = crate::boxscalar::pickle_box_i64(0);
        let v1 = crate::map::pickle_map_get_boxed(m, k1, default);
        assert_eq!(crate::boxscalar::pickle_unbox_i64(v1), 7);
        let missing = crate::strings::string_from_bytes(b"gamma".as_ptr(), 5, crate::gc::gc_mut());
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(crate::map::pickle_map_get_boxed(
                m, missing, default
            )),
            0
        );
        let keys = crate::map::pickle_map_keys(m);
        assert_eq!(crate::list::pickle_list_len(keys), 2);
        let vals = crate::map::pickle_map_values(m);
        assert_eq!(crate::list::pickle_list_len(vals), 2);
    }

    #[test]
    fn boxed_int_keys() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let k1 = crate::boxscalar::pickle_box_i64(100);
        let k2 = crate::boxscalar::pickle_box_i64(200);
        let k3 = crate::boxscalar::pickle_box_i64(-5);
        let v1 = crate::boxscalar::pickle_box_i64(11);
        let v2 = crate::boxscalar::pickle_box_i64(22);
        let v3 = crate::boxscalar::pickle_box_i64(33);
        assert!(map_get(m, k1).is_null());
        map_set(m, k1, v1);
        map_set(m, k2, v2);
        map_set(m, k3, v3);
        assert_eq!(map_get(m, k1), v1);
        assert_eq!(map_get(m, k2), v2);
        assert_eq!(map_get(m, k3), v3);
        assert_eq!(map_len_of(m), 3);
        // Overwrite; the int identity (payload + class) must still match.
        let v2b = crate::boxscalar::pickle_box_i64(99);
        map_set(m, k2, v2b);
        assert_eq!(map_get(m, k2), v2b);
        assert_eq!(map_len_of(m), 3);
        // A second box with the same payload is the same key.
        let k2_again = crate::boxscalar::pickle_box_i64(200);
        assert_eq!(map_get(m, k2_again), v2b);
        // Float box with equal payload is a *different* key (class mismatch).
        let k2_float = crate::boxscalar::pickle_box_f64(200.0);
        assert!(map_get(m, k2_float).is_null());
        // A string is never equal to a box key.
        let s200 = crate::strings::int64_to_string(200);
        assert!(map_get(m, s200).is_null());
        // Remove by payload identity.
        assert_eq!(map_remove(m, crate::boxscalar::pickle_box_i64(100)), v1);
        assert!(map_get(m, k1).is_null());
        assert_eq!(map_len_of(m), 2);
    }

    #[test]
    fn boxed_int_keys_grow() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        for i in 0..800i64 {
            let k = crate::boxscalar::pickle_box_i64(i);
            let v = crate::boxscalar::pickle_box_i64(i * 13);
            map_set(m, k, v);
        }
        for i in 0..800i64 {
            let k = crate::boxscalar::pickle_box_i64(i);
            let got = map_get(m, k);
            assert!(!got.is_null(), "key {i} missing after grow");
            assert_eq!(crate::boxscalar::pickle_unbox_i64(got), i * 13, "key {i}");
        }
        assert_eq!(map_len_of(m), 800);
    }

    #[test]
    fn bool_char_float_keys() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let kt = crate::boxscalar::pickle_box_bool(true);
        let kf = crate::boxscalar::pickle_box_bool(false);
        let ka = crate::boxscalar::pickle_box_char(b'a' as i32);
        let kz = crate::boxscalar::pickle_box_char(b'z' as i32);
        let kf1 = crate::boxscalar::pickle_box_f64(1.5);
        let kf2 = crate::boxscalar::pickle_box_f64(2.5);
        map_set(m, kt, crate::boxscalar::pickle_box_i64(1));
        map_set(m, kf, crate::boxscalar::pickle_box_i64(0));
        map_set(m, ka, crate::boxscalar::pickle_box_i64(65));
        map_set(m, kz, crate::boxscalar::pickle_box_i64(90));
        map_set(m, kf1, crate::boxscalar::pickle_box_i64(100));
        map_set(m, kf2, crate::boxscalar::pickle_box_i64(200));
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(m, crate::boxscalar::pickle_box_bool(true))),
            1
        );
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(
                m,
                crate::boxscalar::pickle_box_bool(false)
            )),
            0
        );
        // true != 1: bool box is a distinct class from int box.
        assert!(map_get(m, crate::boxscalar::pickle_box_i64(1)).is_null());
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(
                m,
                crate::boxscalar::pickle_box_char(b'a' as i32)
            )),
            65
        );
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(
                m,
                crate::boxscalar::pickle_box_char(b'z' as i32)
            )),
            90
        );
        // 'a' box != int 65.
        assert!(map_get(m, crate::boxscalar::pickle_box_i64(65)).is_null());
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(m, crate::boxscalar::pickle_box_f64(1.5))),
            100
        );
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_get(m, crate::boxscalar::pickle_box_f64(2.5))),
            200
        );
        assert_eq!(map_len_of(m), 6);
    }

    /// Make a class instance of class id `cid` with boxed int fields.
    fn make_obj(cid: u64, fields: &[i64], gc: &mut crate::gc::Gc) -> *mut PickleObject {
        let o = crate::class::class_new(cid, fields.len(), gc);
        for (i, f) in fields.iter().enumerate() {
            crate::class::class_set_slot(o, i as i64, crate::boxscalar::pickle_box_i64(*f));
        }
        o
    }

    fn list_of(vals: &[i64], gc: &mut crate::gc::Gc) -> *mut PickleObject {
        let l = crate::list::list_new(0, gc);
        for v in vals {
            crate::list::list_push(l, crate::boxscalar::pickle_box_i64(*v));
        }
        l
    }

    #[test]
    fn list_keys_structural_lookup() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        let k1 = list_of(&[1, 2, 3], gc);
        let k2 = list_of(&[4, 5], gc);
        let v1 = crate::boxscalar::pickle_box_i64(100);
        let v2 = crate::boxscalar::pickle_box_i64(200);
        map_set(m, k1, v1);
        map_set(m, k2, v2);
        // A *distinct* list with equal contents is the same key (structural).
        let k1b = list_of(&[1, 2, 3], gc);
        assert_eq!(map_get(m, k1b), v1);
        assert_eq!(map_get(m, list_of(&[4, 5], gc)), v2);
        assert_eq!(map_len_of(m), 2);
        // Different contents are different keys.
        assert!(map_get(m, list_of(&[1, 2], gc)).is_null());
        assert!(map_get(m, list_of(&[1, 2, 4], gc)).is_null());
        // Nesting: a list of lists.
        let inner = list_of(&[7], gc);
        let outer = crate::list::list_new(0, gc);
        crate::list::list_push(outer, inner);
        let outer_b = crate::list::list_new(0, gc);
        crate::list::list_push(outer_b, list_of(&[7], gc));
        map_set(m, outer, crate::boxscalar::pickle_box_i64(99));
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_get(m, outer_b)), 99);
    }

    #[test]
    fn class_and_enum_keys() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        // Two class instances with equal fields are the same key.
        let a = make_obj(crate::object::PICKLE_CLASS_USER_BASE as u64, &[1, 2], gc);
        let b = make_obj(crate::object::PICKLE_CLASS_USER_BASE as u64, &[1, 2], gc);
        map_set(m, a, crate::boxscalar::pickle_box_i64(7));
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_get(m, b)), 7);
        // Same class, different fields -> different key.
        let c = make_obj(crate::object::PICKLE_CLASS_USER_BASE as u64, &[1, 3], gc);
        assert!(map_get(m, c).is_null());
        // A different class id never matches.
        let d = make_obj(
            (crate::object::PICKLE_CLASS_USER_BASE + 1) as u64,
            &[1, 2],
            gc,
        );
        assert!(map_get(m, d).is_null());
        // Enum keys: tag + payload fields.
        let e1 = crate::r#enum::enum_new(0, 2, gc);
        crate::class::class_set_slot(e1, 1, crate::boxscalar::pickle_box_i64(9));
        crate::class::class_set_slot(e1, 2, crate::boxscalar::pickle_box_i64(10));
        let e1b = crate::r#enum::enum_new(0, 2, gc);
        crate::class::class_set_slot(e1b, 1, crate::boxscalar::pickle_box_i64(9));
        crate::class::class_set_slot(e1b, 2, crate::boxscalar::pickle_box_i64(10));
        map_set(m, e1, crate::boxscalar::pickle_box_i64(55));
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_get(m, e1b)), 55);
        // Different tag or payload is a different key.
        let e2 = crate::r#enum::enum_new(1, 2, gc);
        crate::class::class_set_slot(e2, 1, crate::boxscalar::pickle_box_i64(9));
        crate::class::class_set_slot(e2, 2, crate::boxscalar::pickle_box_i64(10));
        assert!(map_get(m, e2).is_null());
        assert!(map_get(m, crate::r#enum::enum_new(0, 1, gc)).is_null());
        assert_eq!(map_len_of(m), 2);
    }

    #[test]
    fn map_keys_and_rehash_with_composites() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        // Insert {a:[i, j]} -> v for many a to force a rehash (>= 16 entries).
        for a in 0..40i64 {
            let key = list_of(&[a, a * 2], gc);
            map_set(m, key, crate::boxscalar::pickle_box_i64(a * 100));
        }
        for a in 0..40i64 {
            let key = list_of(&[a, a * 2], gc);
            let got = map_get(m, key);
            assert!(!got.is_null(), "key {a} missing after grow");
            assert_eq!(crate::boxscalar::pickle_unbox_i64(got), a * 100, "key {a}");
        }
        assert_eq!(map_len_of(m), 40);
        // Remove by a structurally-equal key.
        assert_eq!(
            crate::boxscalar::pickle_unbox_i64(map_remove(m, list_of(&[7, 14], gc))),
            700
        );
        assert!(map_get(m, list_of(&[7, 14], gc)).is_null());
        assert_eq!(map_len_of(m), 39);
        // A map used as a key: equal contents match regardless of insertion
        // order inside the inner map.
        let inner = crate::map::map_new(0, gc);
        crate::map::map_set(
            inner,
            crate::strings::int64_to_string(1),
            crate::boxscalar::pickle_box_i64(1),
        );
        crate::map::map_set(
            inner,
            crate::strings::int64_to_string(2),
            crate::boxscalar::pickle_box_i64(2),
        );
        let inner_b = crate::map::map_new(0, gc);
        crate::map::map_set(
            inner_b,
            crate::strings::int64_to_string(2),
            crate::boxscalar::pickle_box_i64(2),
        );
        crate::map::map_set(
            inner_b,
            crate::strings::int64_to_string(1),
            crate::boxscalar::pickle_box_i64(1),
        );
        map_set(m, inner, crate::boxscalar::pickle_box_i64(321));
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_get(m, inner_b)), 321);
    }

    #[test]
    fn cyclic_keys_are_identity_based_and_never_loop() {
        let _guard = setup();
        let gc = crate::gc::gc_mut();
        let m = map_new(0, gc);
        // A self-referential list: l = [l] stored under itself.
        let l = crate::list::list_new(0, gc);
        crate::list::list_push(l, crate::boxscalar::pickle_box_i64(5)); // avoid empty-cycle shape
        crate::list::list_push(l, l);
        map_set(m, l, crate::boxscalar::pickle_box_i64(42));
        // Same root pointer found by identity.
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_get(m, l)), 42);
        // A second self-referential list is a *different* key (identity fallback),
        // and hashing it must terminate.
        let l2 = crate::list::list_new(0, gc);
        crate::list::list_push(l2, crate::boxscalar::pickle_box_i64(5));
        crate::list::list_push(l2, l2);
        assert!(map_get(m, l2).is_null());
        assert_eq!(map_len_of(m), 1);
        // remove by the same root works; by the other root is a miss.
        assert_eq!(crate::boxscalar::pickle_unbox_i64(map_remove(m, l)), 42);
        assert!(map_get(m, l).is_null());
    }
}
