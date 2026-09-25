//! `pickle-runtime`
//!
//! Static library implementing the garbage collector, object model, builtin
//! types, console bridge, and process entry point for compiled PickleScript
//! programs. See `docs/07-runtime.md` and `docs/05-memory.md`.
//!
//! The ABI is a fixed set of `pickle_*` `extern "C"` symbols plus the
//! compiler-emitted class descriptor table; the runtime provides `main` and
//! calls the compiler's `pickle_main`.

#![allow(dead_code)]
#![allow(clippy::manual_c_str_literals)]

mod boxscalar;
mod class;
pub(crate) mod console;
mod r#enum;
mod fs;
mod gc;
mod heap;
mod layout;
mod list;
mod map;
mod object;
mod os;
mod panic;
mod process;
mod raw;
pub(crate) mod shadow;
mod statics;
mod stream;
pub(crate) mod strings;
mod test;
mod trace;
mod r#tuple;

/// Descriptor for the builtin types; ids 0..=9 are reserved and `tuple` keeps a
/// (non-traced) placeholder so user classes, registered via
/// `pickle_runtime_register_class_table`/`pickle_class_register`, start at
/// `PICKLE_CLASS_USER_BASE` (10).
const BUILTIN_DESCRIPTORS: &[object::ClassDescriptor] = &[
    string_desc(),
    list_desc(),
    map_desc(),
    box_int_desc(),
    box_float_desc(),
    box_bool_desc(),
    box_char_desc(),
    enum_desc(),
    tuple_desc(),
    stream_desc(),
];

const fn string_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"string\0".as_ptr(),
        name_len: 6,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn list_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"List\0".as_ptr(),
        name_len: 4,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn map_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"Map\0".as_ptr(),
        name_len: 3,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn box_int_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"int\0".as_ptr(),
        name_len: 3,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn box_float_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"float\0".as_ptr(),
        name_len: 5,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn box_bool_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"bool\0".as_ptr(),
        name_len: 4,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

const fn box_char_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"char\0".as_ptr(),
        name_len: 4,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

// The enum layout (tag slot + fields) is traced by a hardcoded path, so this
// placeholder descriptor only reserves class id 7 for `PEnum`.
const fn enum_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"enum\0".as_ptr(),
        name_len: 4,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

// The tuple layout (len word + fields) is traced by a hardcoded path, so this
// placeholder descriptor only reserves class id 8 for `PTuple`.
const fn tuple_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"tuple\0".as_ptr(),
        name_len: 5,
        flags: 0,
        slot_count: 0,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: object::builtin_nop_finalizer,
    }
}

// The stream slot 0 holds a raw `*mut StreamInner` that the collector must
// never chase, so this descriptor only reserves class id 9 for `PStream`; the
// finalizer drops the boxed inner state when the object is swept.
const fn stream_desc() -> object::ClassDescriptor {
    object::ClassDescriptor {
        name_ptr: b"stream\0".as_ptr(),
        name_len: 6,
        flags: object::PICKLE_CLASS_FLAG_FINALIZER,
        slot_count: 1,
        mask_words: 0,
        managed_mask: std::ptr::null(),
        finalizer: stream::stream_finalizer,
    }
}

/// One-time runtime bring-up: install the panic guard, initialise the
/// collector, register the builtin descriptors, and register this thread.
/// Idempotent; safe to call more than once.
#[no_mangle]
pub extern "C" fn pickle_runtime_init() -> u32 {
    panic::install_panic_hook();
    gc::init();
    let g = gc::gc_mut();
    if g.descriptors.len() < BUILTIN_DESCRIPTORS.len() {
        for d in BUILTIN_DESCRIPTORS {
            g.register_class(*d);
        }
    }
    0
}

/// Register `count` compiler-emitted class descriptors (a `[ClassDescriptor;
/// count]` table in the generated object). Returns the next free class id.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_runtime_register_class_table(
    table: *const object::ClassDescriptor,
    count: u32,
) -> u32 {
    if table.is_null() || count == 0 {
        return gc::gc_mut().descriptors.len() as u32;
    }
    unsafe {
        let g = gc::gc_mut();
        if g.descriptors.len() < BUILTIN_DESCRIPTORS.len() {
            for d in BUILTIN_DESCRIPTORS {
                g.register_class(*d);
            }
        }
        for i in 0..count as usize {
            g.register_class(table.add(i).read());
        }
        g.descriptors.len() as u32
    }
}

/// Tear down the collector. Usually not needed at exit, but available.
#[no_mangle]
pub extern "C" fn pickle_runtime_shutdown() {
    gc::shutdown();
}

/// Reset the runtime to a pristine state between `pickle test` files: drop
/// registered tests and hooks, static-field cells, compiled class descriptors
/// and all heap objects. Each file compiles its own class ids starting at
/// `PICKLE_CLASS_USER_BASE`, so the registry and statics must restart with it.
#[no_mangle]
pub extern "C" fn pickle_runtime_reset() {
    test::pickle_test_clear();
    statics::reset();
    gc::reset();
    os::reset();
    let g = gc::gc_mut();
    if g.descriptors.len() < BUILTIN_DESCRIPTORS.len() {
        for d in BUILTIN_DESCRIPTORS {
            g.register_class(*d);
        }
    }
}

// The compiler-emitted program entry. Resolved at link time.
#[cfg(all(feature = "entry", not(test)))]
unsafe extern "C" {
    fn pickle_main();
}

// Process entry point: bootstrap the runtime, run the program, flush output.
#[cfg(all(feature = "entry", not(test)))]
#[no_mangle]
/// # Safety
/// `argc`/`argv` are the process entry vector (must be valid for the whole
/// duration of `main`), forwarded to `os::pickle_args_set`, which copies them.
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8) -> i32 {
    unsafe {
        os::pickle_args_set(argc as usize, argv);
        pickle_runtime_init();
        pickle_main();
        console::pickle_flush();
        pickle_runtime_shutdown();
    }
    0
}

/// Crate-public ABI surface for the sibling `pickle-cli` crate: re-exports the
/// `pickle_*` symbols under a single stable path.
#[doc(hidden)]
pub mod abi {
    pub use crate::pickle_runtime_init;
    pub use crate::pickle_runtime_reset;
    pub use crate::pickle_runtime_shutdown;

    pub use crate::class::pickle_class_add_iface_method;
    pub use crate::class::pickle_class_add_interface;
    pub use crate::class::pickle_class_cast;
    pub use crate::class::pickle_class_implements;
    pub use crate::class::pickle_class_is;
    pub use crate::class::pickle_class_new;
    pub use crate::class::pickle_class_register;
    pub use crate::class::pickle_iface_cast;
    pub use crate::class::pickle_iface_method;
    pub use crate::class::pickle_obj_slot_get;
    pub use crate::class::pickle_obj_slot_set;

    pub use crate::gc::pickle_manual_adopt;
    pub use crate::gc::pickle_manual_free;

    pub use crate::statics::pickle_static_get;
    pub use crate::statics::pickle_static_set;

    pub use crate::console::pickle_print_bool;
    pub use crate::console::pickle_print_byte;
    pub use crate::console::pickle_print_bytes;
    pub use crate::console::pickle_print_char;
    pub use crate::console::pickle_print_cstr;
    pub use crate::console::pickle_print_f64;
    pub use crate::console::pickle_print_i64;
    pub use crate::console::pickle_print_newline;
    pub use crate::console::pickle_print_obj;
    pub use crate::console::pickle_print_u64;

    pub use crate::boxscalar::pickle_box_bool;
    pub use crate::boxscalar::pickle_box_char;
    pub use crate::boxscalar::pickle_box_f64;
    pub use crate::boxscalar::pickle_box_i64;
    pub use crate::boxscalar::pickle_unbox_bool;
    pub use crate::boxscalar::pickle_unbox_char;
    pub use crate::boxscalar::pickle_unbox_f64;
    pub use crate::boxscalar::pickle_unbox_i64;

    pub use crate::list::pickle_list_get;
    pub use crate::list::pickle_list_insert;
    pub use crate::list::pickle_list_len;
    pub use crate::list::pickle_list_new;
    pub use crate::list::pickle_list_pop;
    pub use crate::list::pickle_list_push;
    pub use crate::list::pickle_list_remove;
    pub use crate::list::pickle_list_set;
    pub use crate::list::pickle_list_sort;
    pub use crate::list::pickle_range;

    pub use crate::map::pickle_map_get;
    pub use crate::map::pickle_map_get_boxed;
    pub use crate::map::pickle_map_has;
    pub use crate::map::pickle_map_keys;
    pub use crate::map::pickle_map_len;
    pub use crate::map::pickle_map_new;
    pub use crate::map::pickle_map_remove;
    pub use crate::map::pickle_map_set;
    pub use crate::map::pickle_map_values;

    pub use crate::r#enum::pickle_enum_eq;
    pub use crate::r#enum::pickle_enum_field;
    pub use crate::r#enum::pickle_enum_new;
    pub use crate::r#enum::pickle_enum_set_field;
    pub use crate::r#enum::pickle_enum_tag;

    pub use crate::r#tuple::pickle_tuple_field;
    pub use crate::r#tuple::pickle_tuple_len;
    pub use crate::r#tuple::pickle_tuple_new;
    pub use crate::r#tuple::pickle_tuple_set_field;

    pub use crate::fs::pickle_delete;
    pub use crate::fs::pickle_file_exists;
    pub use crate::fs::pickle_list_dir;
    pub use crate::fs::pickle_mkdir;
    pub use crate::fs::pickle_read_file;
    pub use crate::fs::pickle_write_file;

    pub use crate::os::pickle_args;
    pub use crate::os::pickle_args_set;
    pub use crate::os::pickle_exit;

    pub use crate::process::pickle_current_dir;
    pub use crate::process::pickle_proc_code;
    pub use crate::process::pickle_proc_err_read;
    pub use crate::process::pickle_proc_in_close;
    pub use crate::process::pickle_proc_in_write;
    pub use crate::process::pickle_proc_kill;
    pub use crate::process::pickle_proc_out_read;
    pub use crate::process::pickle_proc_running;
    pub use crate::process::pickle_proc_wait;
    pub use crate::process::pickle_run_cmd;
    pub use crate::process::pickle_run_spawn;

    pub use crate::stream::pickle_stderr_stream;
    pub use crate::stream::pickle_stdout_stream;
    pub use crate::stream::pickle_stream_close;
    pub use crate::stream::pickle_stream_flush;
    pub use crate::stream::pickle_stream_open_append;
    pub use crate::stream::pickle_stream_open_read;
    pub use crate::stream::pickle_stream_open_write;
    pub use crate::stream::pickle_stream_read;
    pub use crate::stream::pickle_stream_write;

    pub use crate::panic::pickle_panic_arity_diff;
    pub use crate::panic::pickle_panic_no_iface_method;
    pub use crate::panic::pickle_panic_no_match;
    pub use crate::panic::pickle_panic_none_unwrap;
    pub use crate::panic::pickle_test_capture_begin;
    pub use crate::panic::pickle_test_capture_take;

    pub use crate::raw::pickle_raw_alloc;
    pub use crate::raw::pickle_raw_free;

    pub use crate::shadow::pickle_shadow_get;
    pub use crate::shadow::pickle_shadow_pop;
    pub use crate::shadow::pickle_shadow_push;
    pub use crate::shadow::pickle_shadow_set;

    pub use crate::strings::pickle_str_cmp;
    pub use crate::strings::pickle_str_concat;
    pub use crate::strings::pickle_str_from_bool;
    pub use crate::strings::pickle_str_from_byte;
    pub use crate::strings::pickle_str_from_bytes;
    pub use crate::strings::pickle_str_from_char;
    pub use crate::strings::pickle_str_from_f64;
    pub use crate::strings::pickle_str_from_i64;
    pub use crate::strings::pickle_str_from_list;
    pub use crate::strings::pickle_str_get;
    pub use crate::strings::pickle_str_len;
    pub use crate::strings::pickle_str_set;
    pub use crate::strings::pickle_str_to_bytes;

    pub use crate::test::pickle_expect_display;
    pub use crate::test::pickle_expect_list_contains;
    pub use crate::test::pickle_expect_obj_eq;
    pub use crate::test::pickle_expect_str_contains;
    pub use crate::test::pickle_runtime_run_tests;
    pub use crate::test::pickle_test_clear;
    pub use crate::test::pickle_test_fail_obj;
    pub use crate::test::pickle_test_register_hooks;
    pub use crate::test::pickle_test_register_table;
    pub use crate::test::PickleHook;
    pub use crate::test::PickleTest;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_cycle_alloc_mark_sweep() {
        let _guard = crate::gc::test_begin();
        pickle_runtime_init();

        let cls = {
            let g = gc::gc_mut();
            g.register_class(object::ClassDescriptor {
                name_ptr: b"Hero\0".as_ptr(),
                name_len: 4,
                flags: 0,
                slot_count: 1,
                mask_words: 1,
                managed_mask: &[1u32] as *const u32,
                finalizer: object::builtin_nop_finalizer,
            })
        };

        // Rooted: a Hero pointing at a string.
        let hero = gc::pickle_gc_alloc(48, cls);
        let name = {
            let s = strings::string_from_bytes(b"Alice".as_ptr(), 5, gc::gc_mut());
            unsafe { (*hero).set_slot(0, s) };
            s
        };

        // Unrooted: garbage to sweep.
        let _trash = strings::string_from_bytes(b"junk".as_ptr(), 4, gc::gc_mut());

        let root: *mut *mut object::PickleObject = Box::into_raw(Box::new(hero));
        let handle = gc::pickle_gc_root_add(root);
        gc::pickle_gc_collect(true);

        unsafe {
            assert!(!(*hero).is_marked());
            assert_eq!((*hero).slot(0), name, "rooted string must survive");
        }

        // The list + map builtins survive a full cycle too.
        let list = list::list_new(0, gc::gc_mut());
        list::list_push(list, name);
        let map = map::map_new(0, gc::gc_mut());
        let key = strings::string_from_bytes(b"class".as_ptr(), 5, gc::gc_mut());
        let val = strings::string_from_bytes(b"warrior".as_ptr(), 7, gc::gc_mut());
        map::map_set(map, key, val);
        assert_eq!(map::map_get(map, key), val);

        let hero_root: *mut *mut object::PickleObject = Box::into_raw(Box::new(hero));
        let list_root: *mut *mut object::PickleObject = Box::into_raw(Box::new(list));
        let map_root: *mut *mut object::PickleObject = Box::into_raw(Box::new(map));
        let rh = gc::pickle_gc_root_add(hero_root);
        let rl = gc::pickle_gc_root_add(list_root);
        let rm = gc::pickle_gc_root_add(map_root);
        gc::pickle_gc_collect(true);

        assert_eq!(list::list_get(list, 0), name);
        assert_eq!(map::map_get(map, key), val);

        gc::pickle_gc_root_drop(handle);
        gc::pickle_gc_root_drop(rh);
        gc::pickle_gc_root_drop(rl);
        gc::pickle_gc_root_drop(rm);
        unsafe {
            drop(Box::from_raw(root));
            drop(Box::from_raw(hero_root));
            drop(Box::from_raw(list_root));
            drop(Box::from_raw(map_root));
        }
    }

    #[test]
    fn register_class_table_assigns_ids_after_builtins() {
        let _guard = crate::gc::test_begin();
        pickle_runtime_init();
        const D: object::ClassDescriptor = object::ClassDescriptor {
            name_ptr: b"State\0".as_ptr(),
            name_len: 5,
            flags: 0,
            slot_count: 0,
            mask_words: 0,
            managed_mask: std::ptr::null(),
            finalizer: object::builtin_nop_finalizer,
        };
        let start = pickle_runtime_register_class_table(&D as *const _, 1);
        assert!(start >= 3, "user classes come after the builtins");
        let g = gc::gc_mut();
        assert_eq!(
            g.class_name(0).map(|b| b.to_vec()),
            Some(b"string".to_vec())
        );
        assert_eq!(
            g.class_name(start - 1).map(|b| b.to_vec()),
            Some(b"State".to_vec())
        );
    }

    #[test]
    fn runtime_reset_restores_pristine_state() {
        let _guard = crate::gc::test_begin();
        pickle_runtime_init();

        const D: object::ClassDescriptor = object::ClassDescriptor {
            name_ptr: b"Reset\0".as_ptr(),
            name_len: 5,
            flags: 0,
            slot_count: 0,
            mask_words: 0,
            managed_mask: std::ptr::null(),
            finalizer: object::builtin_nop_finalizer,
        };

        // Simulate one compiled file: register a class, touch a static, and
        // register a test.
        let first_id = pickle_runtime_register_class_table(&D as *const _, 1);
        let boxed = boxscalar::pickle_box_i64(99);
        statics::pickle_static_set(first_id - 1, 0, boxed);
        extern "C" fn body() {}
        let t = test::PickleTest {
            name: b"reset > survives".as_ptr(),
            name_len: "reset > survives".len() as u32,
            body,
        };
        let tests = [t];
        test::pickle_test_register_table(tests.as_ptr() as *const _, 1);

        // The next file may compile even more classes before its own registry
        // assignments; a second registration must get a fresh id sequence.
        let second_id = pickle_runtime_register_class_table(&D as *const _, 1);
        assert!(
            second_id > first_id,
            "ids accumulate across files pre-reset"
        );

        // Reset exactly like `pickle test` does between files.
        pickle_runtime_reset();

        // Registry is back to builtins + the newly compiled file's classes.
        let g = gc::gc_mut();
        assert_eq!(g.class_name(first_id).map(|b| b.to_vec()), None);
        assert_eq!(
            g.class_name(0).map(|b| b.to_vec()),
            Some(b"string".to_vec()),
            "builtins must be re-registered after reset"
        );
        // The old static cell must read as unset, not the stale 99.
        assert!(statics::pickle_static_get(first_id - 1, 0).is_null());
        // A fresh compilation gets the same ids as the first file did.
        let fresh_id = pickle_runtime_register_class_table(&D as *const _, 1);
        assert_eq!(fresh_id, first_id, "ids must restart per file");
    }
}
