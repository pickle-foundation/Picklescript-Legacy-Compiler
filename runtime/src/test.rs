//! Test harness registry (see `docs/07-runtime.md`, "Testing hooks").
//!
//! `test fn` bodies are compiled to `pickle_test_<name>` zero-argument
//! functions. The JIT registers a table of `PickleTest` descriptors
//! (`pickle_test_register_table`) and runs them on demand
//! (`pickle_runtime_run_tests`): a body that calls `pickle_test_fail_obj`
//! (the lowering of `assert`) records a captured message and the test is
//! reported as failed, followed by a summary and a nonzero exit code when any
//! test failed — the behaviour `pickle test` links against.
//!
//! Bodies must never unwind a Rust panic across the JIT frame (the compiled
//! functions carry no unwind info), so `assert` records failure instead of
//! panicking, and the runner does not use `catch_unwind`.

/// A single test descriptor. `#[repr(C)]` so the host toolchain can lay it
/// out next to compiled code.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PickleTest {
    pub name: *const u8,
    pub name_len: u32,
    pub body: extern "C" fn(),
}

/// A hook descriptor attached to a group. `group` is the group path string
/// (`""` for the root group), `kind` is one of `HOOK_*`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PickleHook {
    pub group: *const u8,
    pub group_len: u32,
    pub kind: u8,
    pub body: extern "C" fn(),
}

pub const HOOK_BEFORE_ALL: u8 = 0;
pub const HOOK_BEFORE_EACH: u8 = 1;
pub const HOOK_AFTER_EACH: u8 = 2;
pub const HOOK_AFTER_ALL: u8 = 3;

// Descriptors are inert data (`name` points into static bytes); the Vec copy
// is only ever read back on the running thread.
unsafe impl Send for PickleTest {}
unsafe impl Send for PickleHook {}

/// Registered tests, in registration order.
static TESTS: std::sync::Mutex<Vec<PickleTest>> = std::sync::Mutex::new(Vec::new());

/// Registered hooks, in registration order.
static HOOKS: std::sync::Mutex<Vec<RegisteredHook>> = std::sync::Mutex::new(Vec::new());

/// An owned hook copy (group bytes are copied out of the caller's buffer).
#[derive(Clone)]
struct RegisteredHook {
    group: String,
    kind: u8,
    body: extern "C" fn(),
}

/// ABI: register `count` test descriptors. Returns the total number of
/// registered tests.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_test_register_table(table: *const PickleTest, count: u32) -> u32 {
    if table.is_null() || count == 0 {
        return 0;
    }
    let mut tests = TESTS.lock().unwrap_or_else(|p| p.into_inner());
    unsafe {
        for i in 0..count as usize {
            tests.push(table.add(i).read());
        }
    }
    tests.len() as u32
}

/// ABI: register `count` hook descriptors. Returns the total number of
/// registered hooks.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_test_register_hooks(table: *const PickleHook, count: u32) -> u32 {
    if table.is_null() || count == 0 {
        return 0;
    }
    let mut hooks = HOOKS.lock().unwrap_or_else(|p| p.into_inner());
    unsafe {
        for i in 0..count as usize {
            let h = table.add(i).read();
            let group = if h.group.is_null() {
                String::new()
            } else {
                let bytes = std::slice::from_raw_parts(h.group, h.group_len as usize);
                String::from_utf8_lossy(bytes).into_owned()
            };
            hooks.push(RegisteredHook {
                group,
                kind: h.kind,
                body: h.body,
            });
        }
    }
    hooks.len() as u32
}

/// ABI: drop every registered test and hook. `pickle test` calls this between
/// files so each module's tests run in isolation.
#[no_mangle]
pub extern "C" fn pickle_test_clear() {
    TESTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    HOOKS.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// ABI: a failed `assert(cond, msg?)`. `obj` is a managed `string` object, or
/// null when the assertion carried no message. Inside a test (capture mode) or
/// a task body the message is recorded and the caller continues; elsewhere the
/// failure is fatal, like `panic`.
///
/// Continuing is the point: `assert` is lowered to a void call on a path whose
/// control flow stays valid, so the body can keep going and a task can finish
/// (and be reported as failed) rather than dying mid-frame.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_test_fail_obj(obj: *const crate::object::PickleObject) {
    let msg = if obj.is_null() {
        "assertion failed".to_string()
    } else {
        let bytes = crate::strings::string_bytes_ptr(obj);
        let len = crate::strings::string_bytes_len(obj);
        unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(bytes, len)).into_owned() }
    };
    if crate::panic::record_recoverable(msg.clone()) {
        return;
    }
    crate::panic::fatal(&msg);
}

/// ABI: render a managed value as a text `string` object (used by `expect` to
/// build `Expected:/Received:` lines). `none` renders as "none", strings as
/// their contents, lists element-wise as `[..]`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_expect_display(
    obj: *const crate::object::PickleObject,
) -> *mut crate::object::PickleObject {
    let mut buf = Vec::new();
    crate::console::fmt_obj_to(&mut buf, obj as *mut crate::object::PickleObject);
    crate::strings::string_from_bytes(buf.as_ptr(), buf.len(), crate::gc::gc_mut())
}

/// ABI: structural equality for managed values. `none == none`, strings
/// compare by content, lists recursively (elements are boxed scalars or
/// nested lists); any other pair is equal only when it is the same pointer.
/// Returns 1 when equal, 0 otherwise.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_expect_obj_eq(
    a: *const crate::object::PickleObject,
    b: *const crate::object::PickleObject,
) -> i32 {
    use crate::object::{
        PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_FLOAT, PICKLE_CLASS_BOX_INT,
        PICKLE_CLASS_LIST, PICKLE_CLASS_STRING,
    };
    match (a.is_null(), b.is_null()) {
        (true, true) => return 1,
        (true, false) | (false, true) => return 0,
        _ => {}
    }
    if a == b {
        return 1;
    }
    let ca = unsafe { (*a).class_id };
    let cb = unsafe { (*b).class_id };
    match (ca, cb) {
        (PICKLE_CLASS_STRING, PICKLE_CLASS_STRING) => {
            let (pa, la) = (
                crate::strings::string_bytes_ptr(a),
                crate::strings::string_bytes_len(a),
            );
            let (pb, lb) = (
                crate::strings::string_bytes_ptr(b),
                crate::strings::string_bytes_len(b),
            );
            // SAFETY: both slices come straight from managed string payloads.
            let eq =
                unsafe { std::slice::from_raw_parts(pa, la) == std::slice::from_raw_parts(pb, lb) };
            eq as i32
        }
        (PICKLE_CLASS_LIST, PICKLE_CLASS_LIST) => {
            let (la, lb) = (crate::layout::list_len(a), crate::layout::list_len(b));
            if la != lb {
                return 0;
            }
            for i in 0..la {
                let ea = crate::list::pickle_list_get(a, i);
                let eb = crate::list::pickle_list_get(b, i);
                if pickle_expect_obj_eq(ea, eb) == 0 {
                    return 0;
                }
            }
            1
        }
        (PICKLE_CLASS_BOX_INT, PICKLE_CLASS_BOX_INT)
        | (PICKLE_CLASS_BOX_FLOAT, PICKLE_CLASS_BOX_FLOAT)
        | (PICKLE_CLASS_BOX_BOOL, PICKLE_CLASS_BOX_BOOL)
        | (PICKLE_CLASS_BOX_CHAR, PICKLE_CLASS_BOX_CHAR) => {
            (crate::boxscalar::box_bits(a) == crate::boxscalar::box_bits(b)) as i32
        }
        _ => 0,
    }
}

/// ABI: 1 when `hay` (a managed string) contains `needle` as a substring.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_expect_str_contains(
    hay: *const crate::object::PickleObject,
    needle: *const crate::object::PickleObject,
) -> i32 {
    let (ph, lh) = (
        crate::strings::string_bytes_ptr(hay),
        crate::strings::string_bytes_len(hay),
    );
    let (pn, ln) = (
        crate::strings::string_bytes_ptr(needle),
        crate::strings::string_bytes_len(needle),
    );
    if ln > lh {
        return 0;
    }
    let h = unsafe { std::slice::from_raw_parts(ph, lh) };
    let n = unsafe { std::slice::from_raw_parts(pn, ln) };
    if n.is_empty() {
        return 1;
    }
    h.windows(n.len()).any(|w| w == n) as i32
}

/// ABI: 1 when `list` contains an element equal to `needle` (deep equality
/// per `pickle_expect_obj_eq`).
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn pickle_expect_list_contains(
    list: *const crate::object::PickleObject,
    needle: *const crate::object::PickleObject,
) -> i32 {
    use crate::object::PICKLE_CLASS_LIST;
    if list.is_null() || unsafe { (*list).class_id } != PICKLE_CLASS_LIST {
        return 0;
    }
    let len = crate::layout::list_len(list);
    for i in 0..len {
        let el = crate::list::pickle_list_get(list, i);
        if pickle_expect_obj_eq(el, needle) != 0 {
            return 1;
        }
    }
    0
}

/// Run every registered test (group-aware: emits `describe` headers, runs the
/// group's `beforeAll`/`beforeEach`/`afterEach`/`afterAll` hooks around each
/// test, and reports one line per test plus a summary). Returns 0 when all
/// tests pass, 1 when any test recorded a failure.
#[no_mangle]
pub extern "C" fn pickle_runtime_run_tests() -> i32 {
    crate::panic::set_capture_mode(true);
    let tests = TESTS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let hooks = HOOKS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut open: Vec<String> = Vec::new(); // active group path, beforeAll done
                                            // The root group has no path segment, so it never appears in a test's
                                            // `prefixes`; its hooks run explicitly around the whole suite.
    if !tests.is_empty() {
        run_hooks(&hooks, "", HOOK_BEFORE_ALL);
    }
    for t in &tests {
        let (group, desc) = split_test_name(t);
        let prefixes = group_prefixes(&group);
        // Close groups this test is not nested in, running `afterAll` on exit.
        let mut common = 0usize;
        while common < open.len() && common < prefixes.len() && open[common] == prefixes[common] {
            common += 1;
        }
        while open.len() > common {
            let g = open.pop().unwrap();
            run_hooks(&hooks, &g, HOOK_AFTER_ALL);
        }
        // Enter groups along the path, running `beforeAll` and printing a
        // header for the newly opened group.
        while open.len() < prefixes.len() {
            let g = prefixes[open.len()].clone();
            run_hooks(&hooks, &g, HOOK_BEFORE_ALL);
            group_header(&g, open.len());
            open.push(g);
        }
        // before/afterEach run around each body: beforeAll-group set
        // root-first, afterEach leaf-first.
        run_hooks(&hooks, "", HOOK_BEFORE_EACH);
        for p in &prefixes {
            run_hooks(&hooks, p, HOOK_BEFORE_EACH);
        }
        let detail = {
            (t.body)();
            crate::panic::take_captured_panic()
        };
        let ok = detail.is_none();
        if ok {
            passed += 1;
        } else {
            failed += 1;
        }
        for p in prefixes.iter().rev() {
            run_hooks(&hooks, p, HOOK_AFTER_EACH);
        }
        run_hooks(&hooks, "", HOOK_AFTER_EACH);
        report(&desc, open.len(), ok, detail.as_deref());
    }
    while let Some(g) = open.pop() {
        run_hooks(&hooks, &g, HOOK_AFTER_ALL);
    }
    run_hooks(&hooks, "", HOOK_AFTER_ALL);
    crate::panic::set_capture_mode(false);
    report_summary(passed, failed);
    if failed > 0 {
        1
    } else {
        0
    }
}

fn test_name(t: &PickleTest) -> String {
    if t.name.is_null() {
        "<unnamed>".to_string()
    } else {
        unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(t.name, t.name_len as usize))
                .into_owned()
        }
    }
}

/// Split a compiled test name `"g1 > g2 > desc"` into its (group path, test
/// description). A name with no `" > "` lives in the root group.
fn split_test_name(t: &PickleTest) -> (String, String) {
    let name = test_name(t);
    let mut parts: Vec<&str> = name.split(" > ").collect();
    match parts.pop() {
        Some(desc) if !parts.is_empty() => (parts.join(" > "), desc.to_string()),
        Some(desc) => (String::new(), desc.to_string()),
        None => (String::new(), name),
    }
}

/// Every proper ancestor group string of `group` (the prefixes of its path).
/// `""` (root) yields `[]`; `"a>b"` yields `["a", "a>b"]` after joining.
fn group_prefixes(group: &str) -> Vec<String> {
    if group.is_empty() {
        return Vec::new();
    }
    let segs: Vec<&str> = group.split(" > ").collect();
    let mut out = Vec::with_capacity(segs.len());
    for i in 0..segs.len() {
        out.push(segs[..=i].join(" > "));
    }
    out
}

/// Run every registered hook of `kind` whose group is exactly `group`.
fn run_hooks(hooks: &[RegisteredHook], group: &str, kind: u8) {
    for h in hooks.iter().filter(|h| h.kind == kind && h.group == group) {
        (h.body)();
    }
}

/// `describe <last segment>` at the group's indentation depth.
fn group_header(group: &str, depth: usize) {
    let name = group.split(" > ").last().unwrap_or(group);
    let mut line = format!("{}describe {name}\n", "  ".repeat(depth));
    crate::console::write_to_con(line.as_bytes());
    let _ = &mut line;
}

fn report(desc: &str, depth: usize, ok: bool, detail: Option<&str>) {
    let colored = crate::console::terminal_color_enabled();
    let mut line = format!("{}test {desc} ... ", "  ".repeat(depth));
    if ok {
        if colored {
            line.push_str("\x1b[32m");
        }
        line.push_str("ok");
    } else {
        if colored {
            line.push_str("\x1b[1;31m");
        }
        line.push_str("FAILED");
        if let Some(d) = detail {
            line.push_str(" (");
            line.push_str(d);
            line.push(')');
        }
    }
    if colored {
        line.push_str("\x1b[0m");
    }
    line.push('\n');
    crate::console::write_to_con(line.as_bytes());
}

fn report_summary(passed: usize, failed: usize) {
    let colored = crate::console::terminal_color_enabled();
    let mut line = String::new();
    if colored {
        line.push_str(if failed > 0 { "\x1b[1;31m" } else { "\x1b[32m" });
    }
    line.push_str(&format!("test result: {passed} passed; {failed} failed"));
    if colored {
        line.push_str("\x1b[0m");
    }
    line.push('\n');
    crate::console::write_to_con(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn pass() {}

    extern "C" fn fail_assert() {
        pickle_test_fail_obj(std::ptr::null());
    }

    static MSG: std::sync::atomic::AtomicPtr<crate::object::PickleObject> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    extern "C" fn fail_msg() {
        let obj = MSG.load(std::sync::atomic::Ordering::Relaxed);
        pickle_test_fail_obj(obj);
    }

    fn reg(tests: &[PickleTest]) -> Vec<PickleTest> {
        TESTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
        pickle_test_register_table(tests.as_ptr(), tests.len() as u32);
        tests.to_vec()
    }

    fn capture_start() {
        crate::console::testio::HAS_CAPTURE.with(|c| c.set(true));
        crate::console::testio::CAPTURE.with(|b| b.borrow_mut().clear());
    }

    fn capture_stop() -> String {
        let out = crate::console::testio::CAPTURE.with(|b| b.borrow().clone());
        crate::console::testio::HAS_CAPTURE.with(|c| c.set(false));
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn all_pass_returns_zero() {
        let _guard = crate::gc::test_begin();
        capture_start();
        let table = [
            PickleTest {
                name: b"pass_a\0".as_ptr(),
                name_len: 6,
                body: pass,
            },
            PickleTest {
                name: b"pass_b\0".as_ptr(),
                name_len: 6,
                body: pass,
            },
        ];
        let regs = reg(&table);
        assert_eq!(regs.len(), 2);
        assert_eq!(pickle_runtime_run_tests(), 0);
        assert_eq!(
            capture_stop(),
            "test pass_a ... ok\ntest pass_b ... ok\ntest result: 2 passed; 0 failed\n"
        );
        TESTS.lock().unwrap().clear();
    }

    #[test]
    fn failing_body_records_and_reports() {
        let _guard = crate::gc::test_begin();
        capture_start();
        let s = crate::strings::string_from_bytes(b"nope".as_ptr(), 4, crate::gc::gc_mut());
        MSG.store(s, std::sync::atomic::Ordering::Relaxed);
        let table = [
            PickleTest {
                name: b"ok\0".as_ptr(),
                name_len: 2,
                body: pass,
            },
            PickleTest {
                name: b"bad\0".as_ptr(),
                name_len: 3,
                body: fail_msg,
            },
            PickleTest {
                name: b"boom\0".as_ptr(),
                name_len: 4,
                body: fail_assert,
            },
        ];
        reg(&table);
        assert_eq!(pickle_runtime_run_tests(), 1);
        assert_eq!(
            capture_stop(),
            "test ok ... ok\ntest bad ... FAILED (nope)\ntest boom ... FAILED (assertion failed)\ntest result: 1 passed; 2 failed\n"
        );
        TESTS.lock().unwrap().clear();
    }
}
