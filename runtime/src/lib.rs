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
#![cfg_attr(feature = "nostd", no_std)]

extern crate alloc;

#[cfg(feature = "nostd")]
#[macro_use]
extern crate alloc as alloc_macros;

#[cfg(feature = "nostd")]
#[macro_use]
mod nostd;

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
#[cfg(not(feature = "nostd"))]
mod process;
mod raw;
pub(crate) mod shadow;
mod statics;
mod stream;
pub(crate) mod strings;
mod test;
#[cfg(not(feature = "nostd"))]
mod threads;
mod trace;
mod r#tuple;
mod sync;
#[cfg(not(feature = "nostd"))]
pub mod world;

/// Descriptor for the builtin types; ids 0..=9 are reserved and `tuple` keeps a
/// (non-traced) placeholder so user classes, registered via
/// `pickle_runtime_register_class_table`/`pickle_class_register`, start at
/// `PICKLE_CLASS_USER_BASE` (10).
#[cfg(not(feature = "nostd"))]
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

/// The freestanding build has the `stream` module but no `process` support, so
/// `Proc` streams are absent. `stream_desc` still occupies class id 9 to keep
/// every builtin id identical to the `std` build, leaving `PICKLE_CLASS_USER_BASE`
/// at 10.
#[cfg(feature = "nostd")]
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
        managed_mask: core::ptr::null(),
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
///
/// A program's tasks outlive its `main`, and a task's heap references are only
/// meaningless once it has finished, so the pool is drained first: the thread
/// that tears the heap down must not be the one that is about to pull the rug
/// out from under a task. The world lock is handed back afterwards so a
/// shutdown from a task (or a test) cannot leave the process unable to run
/// anything else.
#[no_mangle]
pub extern "C" fn pickle_runtime_shutdown() {
    #[cfg(not(feature = "nostd"))]
    threads::drain();
    gc::shutdown();
    #[cfg(not(feature = "nostd"))]
    world::release();
}

/// Reset the runtime to a pristine state between `pickle test` files: drop
/// registered tests and hooks, static-field cells, compiled class descriptors
/// and all heap objects. Each file compiles its own class ids starting at
/// `PICKLE_CLASS_USER_BASE`, so the registry and statics must restart with it.
#[no_mangle]
pub extern "C" fn pickle_runtime_reset() {
    #[cfg(not(feature = "nostd"))]
    test::pickle_test_clear();
    #[cfg(not(feature = "nostd"))]
    statics::reset();
    gc::reset();
    #[cfg(not(feature = "nostd"))]
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

/// Declare the runtime's linkable ABI once, deriving every consumer-side
/// table from that single list: the `abi` re-export module for the sibling
/// `pickle-cli` crate, [`ALL_ABI_NAMES`] for the crash reporter's symbol
/// table, and [`runtime_addr`] for the linker's `pickle_*` resolution. A new
/// ABI symbol therefore needs exactly one edit to become linkable.
macro_rules! declare_abi {
    ($( $( #[$meta:meta] )? $name:ident => $path:path ),* $(,)?) => {
        /// Crate-public ABI surface for the sibling `pickle-cli` crate:
        /// re-exports the `pickle_*` symbols under a single stable path.
        #[doc(hidden)]
        pub mod abi {
            $( $( #[$meta] )? pub use $path; )*
            #[cfg(not(feature = "nostd"))]
            pub use crate::test::{PickleHook, PickleTest};
        }

        /// Every ABI symbol name, in declaration order.
        pub const ALL_ABI_NAMES: &[&str] = &[ $( stringify!($name) ),* ];

        /// The address of ABI symbol `name`, or `None` when it is not part of
        /// the runtime ABI. (Symbols supplied by the host rather than the
        /// runtime ÃƒÆ’Ã†â€™Ãƒâ€šÃ‚Â¢ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â€šÂ¬Ã…Â¡Ãƒâ€šÃ‚Â¬ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â‚¬Å¡Ã‚Â¬Ãƒâ€šÃ‚Â `pickle_fmod` ÃƒÆ’Ã†â€™Ãƒâ€šÃ‚Â¢ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â€šÂ¬Ã…Â¡Ãƒâ€šÃ‚Â¬ÃƒÆ’Ã‚Â¢ÃƒÂ¢Ã¢â‚¬Å¡Ã‚Â¬Ãƒâ€šÃ‚Â are not listed here.)
        pub fn runtime_addr(name: &str) -> Option<usize> {
            match name {
                $( $( #[$meta] )? stringify!($name) => Some($path as *const () as usize), )*
                _ => None,
            }
        }
    };
}

declare_abi! {
    pickle_runtime_init => crate::pickle_runtime_init,
    pickle_runtime_reset => crate::pickle_runtime_reset,
    pickle_runtime_shutdown => crate::pickle_runtime_shutdown,

    pickle_class_add_iface_method => crate::class::pickle_class_add_iface_method,
    pickle_class_add_interface => crate::class::pickle_class_add_interface,
    pickle_class_cast => crate::class::pickle_class_cast,
    pickle_class_implements => crate::class::pickle_class_implements,
    pickle_class_is => crate::class::pickle_class_is,
    pickle_class_new => crate::class::pickle_class_new,
    pickle_class_register => crate::class::pickle_class_register,
    pickle_iface_cast => crate::class::pickle_iface_cast,
    pickle_iface_method => crate::class::pickle_iface_method,
    pickle_obj_slot_get => crate::class::pickle_obj_slot_get,
    pickle_obj_slot_set => crate::class::pickle_obj_slot_set,

    pickle_static_get => crate::statics::pickle_static_get,
    pickle_static_set => crate::statics::pickle_static_set,

    pickle_print_bool => crate::console::pickle_print_bool,
    pickle_print_byte => crate::console::pickle_print_byte,
    pickle_print_bytes => crate::console::pickle_print_bytes,
    pickle_print_char => crate::console::pickle_print_char,
    pickle_print_cstr => crate::console::pickle_print_cstr,
    pickle_print_f64 => crate::console::pickle_print_f64,
    pickle_print_i64 => crate::console::pickle_print_i64,
    pickle_print_newline => crate::console::pickle_print_newline,
    pickle_print_obj => crate::console::pickle_print_obj,
    pickle_print_u64 => crate::console::pickle_print_u64,

    pickle_box_bool => crate::boxscalar::pickle_box_bool,
    pickle_box_char => crate::boxscalar::pickle_box_char,
    pickle_box_f64 => crate::boxscalar::pickle_box_f64,
    pickle_box_i64 => crate::boxscalar::pickle_box_i64,
    pickle_unbox_bool => crate::boxscalar::pickle_unbox_bool,
    pickle_unbox_char => crate::boxscalar::pickle_unbox_char,
    pickle_unbox_f64 => crate::boxscalar::pickle_unbox_f64,
    pickle_unbox_i64 => crate::boxscalar::pickle_unbox_i64,

    pickle_list_get => crate::list::pickle_list_get,
    pickle_list_insert => crate::list::pickle_list_insert,
    pickle_list_len => crate::list::pickle_list_len,
    pickle_list_new => crate::list::pickle_list_new,
    pickle_list_pop => crate::list::pickle_list_pop,
    pickle_list_push => crate::list::pickle_list_push,
    pickle_list_remove => crate::list::pickle_list_remove,
    pickle_list_set => crate::list::pickle_list_set,
    pickle_list_sort => crate::list::pickle_list_sort,
    pickle_range => crate::list::pickle_range,

    pickle_map_get => crate::map::pickle_map_get,
    pickle_map_get_boxed => crate::map::pickle_map_get_boxed,
    pickle_map_has => crate::map::pickle_map_has,
    pickle_map_keys => crate::map::pickle_map_keys,
    pickle_map_len => crate::map::pickle_map_len,
    pickle_map_new => crate::map::pickle_map_new,
    pickle_map_remove => crate::map::pickle_map_remove,
    pickle_map_set => crate::map::pickle_map_set,
    pickle_map_values => crate::map::pickle_map_values,

    pickle_enum_eq => crate::r#enum::pickle_enum_eq,
    pickle_enum_field => crate::r#enum::pickle_enum_field,
    pickle_enum_new => crate::r#enum::pickle_enum_new,
    pickle_enum_set_field => crate::r#enum::pickle_enum_set_field,
    pickle_enum_tag => crate::r#enum::pickle_enum_tag,

    pickle_tuple_field => crate::r#tuple::pickle_tuple_field,
    pickle_tuple_len => crate::r#tuple::pickle_tuple_len,
    pickle_tuple_new => crate::r#tuple::pickle_tuple_new,
    pickle_tuple_set_field => crate::r#tuple::pickle_tuple_set_field,

    pickle_delete => crate::fs::pickle_delete,
    pickle_file_exists => crate::fs::pickle_file_exists,
    pickle_list_dir => crate::fs::pickle_list_dir,
    pickle_mkdir => crate::fs::pickle_mkdir,
    pickle_read_file => crate::fs::pickle_read_file,
    pickle_write_file => crate::fs::pickle_write_file,

    pickle_args => crate::os::pickle_args,
    pickle_args_set => crate::os::pickle_args_set,
    pickle_exit => crate::os::pickle_exit,

    #[cfg(not(feature = "nostd"))]
    pickle_current_dir => crate::process::pickle_current_dir,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_code => crate::process::pickle_proc_code,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_err_read => crate::process::pickle_proc_err_read,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_in_close => crate::process::pickle_proc_in_close,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_in_write => crate::process::pickle_proc_in_write,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_kill => crate::process::pickle_proc_kill,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_out_read => crate::process::pickle_proc_out_read,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_running => crate::process::pickle_proc_running,
    #[cfg(not(feature = "nostd"))]
    pickle_proc_wait => crate::process::pickle_proc_wait,
    #[cfg(not(feature = "nostd"))]
    pickle_run_cmd => crate::process::pickle_run_cmd,
    #[cfg(not(feature = "nostd"))]
    pickle_run_spawn => crate::process::pickle_run_spawn,

    pickle_stderr_stream => crate::stream::pickle_stderr_stream,
    pickle_stdout_stream => crate::stream::pickle_stdout_stream,
    pickle_stream_close => crate::stream::pickle_stream_close,
    pickle_stream_flush => crate::stream::pickle_stream_flush,
    pickle_stream_open_append => crate::stream::pickle_stream_open_append,
    pickle_stream_open_read => crate::stream::pickle_stream_open_read,
    pickle_stream_open_write => crate::stream::pickle_stream_open_write,
    pickle_stream_read => crate::stream::pickle_stream_read,
    pickle_stream_write => crate::stream::pickle_stream_write,

    pickle_panic_arity_diff => crate::panic::pickle_panic_arity_diff,
    pickle_panic_bytes => crate::panic::pickle_panic_bytes,
    pickle_panic_cstr => crate::panic::pickle_panic_cstr,
    pickle_panic_no_iface_method => crate::panic::pickle_panic_no_iface_method,
    pickle_panic_no_match => crate::panic::pickle_panic_no_match,
    pickle_panic_none_unwrap => crate::panic::pickle_panic_none_unwrap,
    pickle_test_capture_begin => crate::panic::pickle_test_capture_begin,
    pickle_test_capture_take => crate::panic::pickle_test_capture_take,

    #[cfg(not(feature = "nostd"))]
    pickle_thread_count => crate::threads::pickle_thread_count,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_current => crate::threads::pickle_thread_current,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_finished => crate::threads::pickle_thread_finished,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_join => crate::threads::pickle_thread_join,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_panic_message => crate::threads::pickle_thread_panic_message,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_sleep_ms => crate::threads::pickle_thread_sleep_ms,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_spawn => crate::threads::pickle_thread_spawn,
    #[cfg(not(feature = "nostd"))]
    pickle_thread_yield => crate::threads::pickle_thread_yield,

    pickle_atomic_add => crate::sync::pickle_atomic_add,
    pickle_atomic_cas => crate::sync::pickle_atomic_cas,
    pickle_atomic_load => crate::sync::pickle_atomic_load,
    pickle_atomic_new => crate::sync::pickle_atomic_new,
    pickle_atomic_store => crate::sync::pickle_atomic_store,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_cap => crate::sync::pickle_channel_cap,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_close => crate::sync::pickle_channel_close,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_closed => crate::sync::pickle_channel_closed,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_len => crate::sync::pickle_channel_len,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_new => crate::sync::pickle_channel_new,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_recv => crate::sync::pickle_channel_recv,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_send => crate::sync::pickle_channel_send,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_try_recv => crate::sync::pickle_channel_try_recv,
    #[cfg(not(feature = "nostd"))]
    pickle_channel_try_send => crate::sync::pickle_channel_try_send,
    #[cfg(not(feature = "nostd"))]
    pickle_cond_broadcast => crate::sync::pickle_cond_broadcast,
    #[cfg(not(feature = "nostd"))]
    pickle_cond_new => crate::sync::pickle_cond_new,
    #[cfg(not(feature = "nostd"))]
    pickle_cond_signal => crate::sync::pickle_cond_signal,
    #[cfg(not(feature = "nostd"))]
    pickle_cond_wait => crate::sync::pickle_cond_wait,
    pickle_mutex_lock => crate::sync::pickle_mutex_lock,
    pickle_mutex_new => crate::sync::pickle_mutex_new,
    pickle_mutex_trylock => crate::sync::pickle_mutex_trylock,
    pickle_mutex_unlock => crate::sync::pickle_mutex_unlock,
    #[cfg(not(feature = "nostd"))]
    pickle_once_done => crate::sync::pickle_once_done,
    #[cfg(not(feature = "nostd"))]
    pickle_once_new => crate::sync::pickle_once_new,
    #[cfg(not(feature = "nostd"))]
    pickle_once_run => crate::sync::pickle_once_run,
    #[cfg(not(feature = "nostd"))]
    pickle_sem_acquire => crate::sync::pickle_sem_acquire,
    #[cfg(not(feature = "nostd"))]
    pickle_sem_new => crate::sync::pickle_sem_new,
    #[cfg(not(feature = "nostd"))]
    pickle_sem_release => crate::sync::pickle_sem_release,
    #[cfg(not(feature = "nostd"))]
    pickle_sem_try_acquire => crate::sync::pickle_sem_try_acquire,

    pickle_raw_alloc => crate::raw::pickle_raw_alloc,
    pickle_raw_free => crate::raw::pickle_raw_free,

    pickle_shadow_get => crate::shadow::pickle_shadow_get,
    pickle_shadow_pop => crate::shadow::pickle_shadow_pop,
    pickle_shadow_push => crate::shadow::pickle_shadow_push,
    pickle_shadow_set => crate::shadow::pickle_shadow_set,

    pickle_str_cmp => crate::strings::pickle_str_cmp,
    pickle_str_concat => crate::strings::pickle_str_concat,
    pickle_str_from_bool => crate::strings::pickle_str_from_bool,
    pickle_str_from_byte => crate::strings::pickle_str_from_byte,
    pickle_str_from_bytes => crate::strings::pickle_str_from_bytes,
    pickle_str_from_char => crate::strings::pickle_str_from_char,
    pickle_str_from_f64 => crate::strings::pickle_str_from_f64,
    pickle_str_from_i64 => crate::strings::pickle_str_from_i64,
    pickle_str_from_list => crate::strings::pickle_str_from_list,
    pickle_str_get => crate::strings::pickle_str_get,
    pickle_str_len => crate::strings::pickle_str_len,
    pickle_str_set => crate::strings::pickle_str_set,
    pickle_str_to_bytes => crate::strings::pickle_str_to_bytes,

    pickle_expect_display => crate::test::pickle_expect_display,
    pickle_expect_list_contains => crate::test::pickle_expect_list_contains,
    pickle_expect_obj_eq => crate::test::pickle_expect_obj_eq,
    pickle_expect_str_contains => crate::test::pickle_expect_str_contains,
    pickle_runtime_run_tests => crate::test::pickle_runtime_run_tests,
    pickle_test_clear => crate::test::pickle_test_clear,
    pickle_test_fail_obj => crate::test::pickle_test_fail_obj,
    pickle_test_register_hooks => crate::test::pickle_test_register_hooks,
    pickle_test_register_table => crate::test::pickle_test_register_table,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_abi_name_resolves() {
        for name in ALL_ABI_NAMES {
            assert!(runtime_addr(name).is_some(), "{name} must resolve");
        }
        assert!(runtime_addr("pickle_no_such_symbol").is_none());
        // The manifest is what the linker's `pickle_*` table and the crash
        // reporter read, so a symbol that is not listed is not linkable.
        assert_eq!(
            ALL_ABI_NAMES.len(),
            ALL_ABI_NAMES.iter().collect::<std::collections::BTreeSet<_>>().len(),
            "ABI names must be unique"
        );
    }

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
            managed_mask: core::ptr::null(),
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
            managed_mask: core::ptr::null(),
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
