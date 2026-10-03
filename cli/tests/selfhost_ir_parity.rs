//! Enforces that the self-hosted compiler lowers a module to the same IR as the
//! native compiler.
//!
//! The two lowerers are independent implementations of one specification, so
//! nothing in the build stops them from drifting: a change to the self-host
//! emitter that changes an instruction still produces a valid module, and the
//! only symptom is a runtime misbehaviour in a program nobody was looking at.
//! These oracles exist because they encode exception lowering -- the throw
//! channel, the propagation check after every call, and the exit rewrites that
//! run owed `finally` blocks -- which is where a divergence would be least
//! visible and most damaging.
//!
//! `cli/tests/exception_parity.rs` covers the other axis of the same risk, a
//! throw surviving the native JIT and the native AOT path. This file covers the
//! axis that only a self-host build can catch.
//!
//! All three fixtures run in one test function on purpose. The self-host side is
//! driven through `pickle/cli/ir_one.pkl`, which is stateless and reads its
//! target from argv, so this is about keeping one failure message that names
//! every module that drifted, rather than a race on a shared scratch file.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One module per entry: the file to lower, and why it is an oracle.
const FIXTURES: &[(&str, &str)] = &[
    (
        "exception_gc_test.pkl",
        "a throw that crosses a call while a shadow frame is live",
    ),
    (
        "exception_paths_test.pkl",
        "the propagation check after every call shape that can throw",
    ),
    (
        "import_exception_test.pkl",
        "imported names inside try/catch/finally and inside a throw operand",
    ),
];

fn cli() -> PathBuf {
    // Cargo points an integration test at the binary target it belongs to, and
    // that path is rebuilt whenever the source it is compared against changes.
    // Deriving the path from `current_exe()` instead is the obvious way to do
    // this and it is wrong: it names a file next to the test binary that nothing
    // in the test's own build graph depends on, so `cargo test` will happily
    // leave last week's compiler there. That failure is indistinguishable from
    // a real parity regression, because the two tools are then being compared at
    // different revisions.
    PathBuf::from(env!("CARGO_BIN_EXE_pickle-cli"))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate has a parent directory")
        .to_path_buf()
}

/// Reduce IR text to the thing the two compilers must agree about.
///
/// The only tolerated difference is trailing whitespace: `pickle-cli ir` ends
/// its output with one more blank line than the self-host `irModuleStr` does,
/// and the two disagree about CRLF. Every other byte is significant, so a
/// changed instruction, a changed operand, a changed extern index or a changed
/// function order all still fail. Blank lines are dropped rather than compared
/// because IRX-style alignment is not part of the contract being gated here.
fn normalize(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `args` from the repo root and return stdout, panicking with the child's
/// own output if it fails.
fn run_capture(args: &[&str]) -> Vec<u8> {
    let out = Command::new(cli())
        .current_dir(repo_root())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", cli().display()));
    assert!(
        out.status.success(),
        "`pickle-cli {}` failed with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

fn native_ir(module: &str) -> String {
    let rel = format!("tests/pickle/{module}");
    normalize(&run_capture(&["ir", &rel]))
}

fn selfhost_ir(module: &str) -> String {
    let rel = format!("tests/pickle/{module}");
    let out = Command::new(cli())
        .current_dir(repo_root())
        .args(["run", "pickle/cli/ir_one.pkl", "--", &rel])
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", cli().display()));
    assert!(
        out.status.success(),
        "the self-host compiler could not lower {module}; this is a build \
         failure, not a parity difference\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    normalize(&out.stdout)
}

/// Lower `module` twice and require the self-host to agree with itself.
///
/// A parity failure is only evidence of a native/self-host divergence if the
/// self-host side is a fixed point. It is not: lowering is one process per run,
/// and the emitter decides where to place its `pickle_exc_pending` checks from
/// state accumulated in an analysis pass, so an occasional run inserts a check
/// the others do not. That is rare -- it did not reproduce in 12 consecutive
/// runs on an idle machine -- but it appears under load, which is exactly what
/// `cargo test --workspace` is, since it runs test binaries concurrently.
///
/// Without this, such a run fails as "the self-host no longer matches native on
/// 1 of 3 oracles", which points at the native compiler and the two
/// implementations when the defect is entirely within one of them. Comparing a
/// second self-host run first separates the two cases and names the real one.
fn selfhost_ir_stable(module: &str) -> String {
    let first = selfhost_ir(module);
    let second = selfhost_ir(module);
    assert!(
        first == second,
        "the self-host compiler lowered {module} two different ways in a row, so \
         this run cannot say anything about native parity\n{}",
        first_difference(&first, &second)
    );
    first
}

/// Report the first line that differs, which is nearly always enough to see
/// what moved without pasting two large IR dumps into the test output.
fn first_difference(native: &str, selfhost: &str) -> String {
    let n: Vec<&str> = native.lines().collect();
    let s: Vec<&str> = selfhost.lines().collect();
    for (i, (a, b)) in n.iter().zip(s.iter()).enumerate() {
        if a != b {
            return format!("line {}:\n  native: {a}\n  self  : {b}", i + 1);
        }
    }
    format!(
        "one side is a prefix of the other: native has {} lines, self-host has {}",
        n.len(),
        s.len()
    )
}

#[test]
fn selfhost_lowering_matches_native_on_the_exception_oracles() {
    let mut drifted: Vec<String> = Vec::new();

    for (module, why) in FIXTURES {
        let native = native_ir(module);
        let selfhost = selfhost_ir_stable(module);

        if native != selfhost {
            drifted.push(format!(
                "\n  {module} ({why})\n    {}",
                first_difference(&native, &selfhost)
            ));
        }

        // Guards against both sides being empty, which would make the
        // comparison above pass for the wrong reason.
        assert!(
            native.lines().count() > 20,
            "{module} lowered to almost nothing natively ({} lines); the parity \
             check would be vacuous",
            native.lines().count()
        );
    }

    assert!(
        drifted.is_empty(),
        "the self-host compiler no longer matches native IR on {} of {} oracles:{}",
        drifted.len(),
        FIXTURES.len(),
        drifted.join("")
    );
}
