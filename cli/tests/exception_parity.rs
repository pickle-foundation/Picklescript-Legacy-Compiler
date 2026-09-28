//! Gates for exception behaviour that must not drift between backends.
//!
//! Two things are checked, because they are the two ways a cooperative
//! `try`/`throw` can silently go wrong:
//!
//! 1. **Parity.** The JIT interprets IR directly while `pickle build` emits C
//!    and links the runtime, so a throw has to survive two very different
//!    lowering paths. These tests run the same module both ways and require
//!    byte-identical output.
//! 2. **Zero overhead.** A module with no `throw` must not reference any
//!    `pickle_exc_*` symbol at all, or every call in it pays for a check that
//!    can never fire.
//!
//! `build` shells out to the real `pickle-cli` binary rather than calling the
//! emitter directly, because the thing under test is the shipped executable,
//! including the C emission and the runtime link.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A throw inside a `try` with a `catch`, plus one with a `finally` only, plus
/// a bare uncaught throw. The uncaught case prints to stderr and exits
/// non-zero, so it is checked separately from the captured cases.
const PARITY_SOURCE: &str = r#"
module parity

fn boom(msg: string) {
    throw msg
}

fn caught() -> string {
    try {
        boom("caught-path")
        "unreachable"
    } catch (e) {
        "caught " + e
    }
}

fn rethrown() -> string {
    try {
        try {
            boom("inner")
        } finally {
            println("finally ran")
        }
    } catch (e) {
        "outer " + e
    }
}

fn main() {
    println(caught())
    println(rethrown())
}
"#;

/// No `throw` anywhere, so nothing here should reference the runtime's
/// exception ABI.
const NO_THROW_SOURCE: &str = r#"
module nothrow

fn add(a: int, b: int) -> int {
    a + b
}

fn main() {
    println(add(20, 22))
}
"#;

fn cli() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps/.
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("pickle-cli.exe")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("pickle-exc-gates");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir.join(name)
}

fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(cli())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", cli().display()));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Compile `source` to a native executable and run it, returning its output.
fn run_aot(source: &str, name: &str) -> (String, bool) {
    let src = scratch(&format!("{name}.pkl"));
    let exe = scratch(&format!("{name}.exe"));
    std::fs::write(&src, source).expect("write source");

    let (out, err, ok) = run(&["build", "-o", exe.to_str().unwrap(), src.to_str().unwrap()]);
    assert!(ok, "AOT build of {name} failed:\n{out}{err}");
    assert!(Path::new(&exe).exists(), "AOT build produced no executable for {name}");

    let out = Command::new(&exe)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", exe.display()));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

#[test]
fn jit_and_aot_agree_on_caught_throws() {
    let src = scratch("parity.pkl");
    std::fs::write(&src, PARITY_SOURCE).expect("write source");

    let (jit_out, jit_err, jit_ok) = run(&["run", src.to_str().unwrap()]);
    assert!(jit_ok, "JIT run failed:\n{jit_out}{jit_err}");

    let (aot_out, aot_ok) = run_aot(PARITY_SOURCE, "parity");

    assert!(aot_ok, "AOT run exited non-zero:\n{aot_out}");
    assert_eq!(
        jit_out, aot_out,
        "JIT and AOT disagree on exception behaviour"
    );
    // Guards against the comparison passing because both sides are empty.
    assert!(
        jit_out.contains("caught caught-path") && jit_out.contains("finally ran"),
        "unexpected output: {jit_out}"
    );
}

#[test]
fn an_uncaught_throw_fails_in_both_backends() {
    let src = scratch("uncaught.pkl");
    let source = r#"
module uncaught
fn boom() {
    throw "escaped"
}
fn main() {
    println("before")
    boom()
    println("after")
}
"#;
    std::fs::write(&src, source).expect("write source");

    let (jit_out, jit_err, jit_ok) = run(&["run", src.to_str().unwrap()]);
    assert!(!jit_ok, "JIT should exit non-zero on an uncaught throw");
    assert!(
        jit_out.contains("before") && !jit_out.contains("after"),
        "statements after a throwing call must not run: {jit_out}"
    );
    assert!(
        jit_err.contains("escaped"),
        "JIT should report the payload on stderr: {jit_err}"
    );

    let (aot_out, aot_ok) = run_aot(source, "uncaught");
    assert!(!aot_ok, "AOT should exit non-zero on an uncaught throw");
    assert!(
        aot_out.contains("before") && !aot_out.contains("after"),
        "AOT must also stop at the throw: {aot_out}"
    );
}

#[test]
fn a_module_with_no_throw_references_no_exception_abi() {
    let src = scratch("nothrow.pkl");
    std::fs::write(&src, NO_THROW_SOURCE).expect("write source");

    let (irx_out, irx_err, irx_ok) = run(&["irx", src.to_str().unwrap()]);
    assert!(irx_ok, "irx failed:\n{irx_out}{irx_err}");

    // IRX is the lossless form, so an absent symbol here means the symbol was
    // never emitted rather than merely unreferenced by the text printer.
    //
    // Extern names are hex-encoded, one per `extern N` line, so match the
    // encoded form rather than the readable one.
    for sym in [
        "pickle_exc_throw",
        "pickle_exc_pending",
        "pickle_exc_take",
        "pickle_exc_clear",
    ] {
        let encoded = hex(sym);
        assert!(
            !extern_names(&irx_out).contains(&encoded),
            "a throw-free module references {sym}, so it pays for checks that cannot fire"
        );
    }
}

/// The lowercased hex of `s`, as IRX writes extern names.
fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Every extern name declared in IRX text, decoded back to a readable string.
fn extern_names(irx: &str) -> Vec<String> {
    irx.lines()
        .filter_map(|l| l.strip_prefix("extern "))
        .filter_map(|rest| rest.split_whitespace().nth(1))
        .filter(|h| h.len() % 2 == 0)
        .map(|h| {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as char)
                .collect::<String>()
        })
        .collect()
}

#[test]
fn a_throwing_module_does_reference_the_exception_abi() {
    // The mirror of the test above, so the previous test cannot pass merely
    // because IRX stopped serialising externs altogether.
    let src = scratch("dothrow.pkl");
    std::fs::write(&src, PARITY_SOURCE).expect("write source");

    let (irx_out, irx_err, ok) = run(&["irx", src.to_str().unwrap()]);
    assert!(ok, "irx failed:\n{irx_out}{irx_err}");
    let names = extern_names(&irx_out);
    for sym in ["pickle_exc_throw", "pickle_exc_take"] {
        assert!(
            names.iter().any(|n| n == sym),
            "a throwing module should reference {sym}, got {names:?}"
        );
    }
}
