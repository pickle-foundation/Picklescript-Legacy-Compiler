//! Pins the stack reserve that every executable we link is given.
//!
//! Pickle lowers a module by walking its syntax tree recursively, and each
//! frame is large: every managed value live in a function needs a GC root cell
//! in the shadow area, so a single `call` frame grows with the module it is
//! lowering. The compilers themselves are compiled programs, so the amount of
//! stack available to *lowering* is decided by the stack of the compiler
//! binary -- not by the stack of the program being compiled.
//!
//! Every linker that produces a compiler therefore has to hand out far more
//! stack than the toolchain default. When they do not, the symptom is nasty
//! and silent: lowering walks off the end of the stack, the guard page faults
//! inside the *next* function's prologue, and the compiler dies with an access
//! violation while compiling a program that is perfectly valid. That is what
//! happened with a 1 MiB reserve for programs as small as
//! `return 1 + base(1)`, while the identical program compiled fine through the
//! native path -- so the bug presented as a self-hosted emitter defect when it
//! was really a linking one.
//!
//! The failure had no failing assertion to point at it, which is why the
//! invariant is asserted here instead of left to a linker's default:
//!
//! * the reserve must stay far above the 1 MiB default, so that headroom
//!   grows as frames grow rather than sitting exactly at the limit;
//! * the commit must stay far below the reserve, because the reserve is only
//!   reserved address space and committing all of it would make every
//!   compiler pay for pages it never touches;
//! * all three linkers must agree. `cli/build.rs` links the compiler that does
//!   the linking, `cli/src/build.rs` links what the native compiler builds, and
//!   `pickle/compiler/pelink.pkl` links what the self-hosted compiler builds.
//!   A compiler that rebuilds itself with less stack than it was built with is
//!   how this bug reappears one bootstrap later, with no diff in sight.
//!
//! The compiler's own work does not run on the main thread: `cli/src/main.rs`
//! moves it onto a thread with an explicit stack, because a linker flag only
//! sizes the main thread and Unix has no link-time reserve at all. That stack
//! is pinned here too, and has to be at least as large as every reserve --
//! otherwise moving the work onto the thread would trade one exhaustion for
//! another.

use std::path::{Path, PathBuf};

/// The reserve must clear the 1 MiB toolchain default by a wide margin.
///
/// 64 MiB is the floor rather than a target: it is already far more than
/// observed lowering needs, and the point of the assertion is to fail long
/// before the value drops back to something that can be exhausted.
const MIN_RESERVE: u64 = 64 * 1024 * 1024;

/// The commit cannot start out smaller than this and still leave the loader
/// room to grow the stack within the reserve.
const MIN_COMMIT: u64 = 64 * 1024;

/// One of the two PE32+ stack fields, in whichever spelling its site uses.
#[derive(Clone, Copy, Debug)]
enum StackField {
    Reserve,
    Commit,
}

impl StackField {
    /// PE32+ optional-header offset, which is what `pelink.pkl` addresses
    /// relative to the start of the optional header (`head`).
    fn pe_offset(self) -> &'static str {
        match self {
            StackField::Reserve => "E0",
            StackField::Commit => "E8",
        }
    }

    /// Constant name in the two Rust build scripts, which name the field
    /// instead of addressing the header directly.
    fn rust_name(self) -> &'static str {
        match self {
            StackField::Reserve => "STACK_RESERVE",
            StackField::Commit => "STACK_COMMIT",
        }
    }
}

/// A place that decides the stack of a compiler binary.
#[derive(Clone, Copy, Debug)]
enum Linker {
    /// `cli/build.rs`: the Cargo build script, which links `pickle-cli` itself.
    CargoBuildScript,
    /// `cli/src/build.rs`: links the programs the native compiler builds.
    NativeBuildScript,
    /// `pickle/compiler/pelink.pkl`: links what the self-hosted compiler builds.
    SelfHostedPelink,
}

impl Linker {
    fn all() -> [Linker; 3] {
        [
            Linker::CargoBuildScript,
            Linker::NativeBuildScript,
            Linker::SelfHostedPelink,
        ]
    }

    /// Human-readable identity, used in assertion messages.
    fn label(self) -> &'static str {
        match self {
            Linker::CargoBuildScript => "cli/build.rs",
            Linker::NativeBuildScript => "cli/src/build.rs",
            Linker::SelfHostedPelink => "pickle/compiler/pelink.pkl",
        }
    }

    fn path(self) -> PathBuf {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        match self {
            Linker::CargoBuildScript => manifest.join("build.rs"),
            Linker::NativeBuildScript => manifest.join("src").join("build.rs"),
            Linker::SelfHostedPelink => manifest
                .join("..")
                .join("pickle")
                .join("compiler")
                .join("pelink.pkl"),
        }
    }

    /// Reads this linker's stack field.
    ///
    /// The two Rust sites and the self-hosted site spell the same value three
    /// different ways. Parsing all of them here -- rather than duplicating the
    /// numbers -- is what makes the linkers a checked set: change one and this
    /// test fails on the others.
    fn stack_field(self, field: StackField) -> u64 {
        let path = self.path();
        let src = read(&path);
        match self {
            Linker::SelfHostedPelink => parse_pe_field(&src, field, self.label()),
            _ => parse_rust_const(&src, field, self.label()),
        }
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Pulls `write64(head, 0xOFFSET, 0xVALUE)` out of `pelink.pkl`.
///
/// `pelink` builds the PE header field by field, so the stack reserve and
/// commit are literals in that file.
fn parse_pe_field(src: &str, field: StackField, label: &str) -> u64 {
    let needle = format!("write64(head, 0x{},", field.pe_offset());
    let line = src
        .lines()
        .find(|l| l.contains(&needle))
        .unwrap_or_else(|| {
            panic!(
                "no `write64` for PE offset 0x{} in {label}",
                field.pe_offset()
            )
        });

    let value = line
        .rsplit_once(',')
        .unwrap_or_else(|| panic!("cannot parse `{line}`"))
        .1
        .trim()
        .trim_end_matches(')')
        .trim();
    parse_u64(value).unwrap_or_else(|e| panic!("{label}: cannot parse `{value}` as hex: {e}"))
}

/// Pulls `const NAME: u64 = <product>` out of a Rust build script.
///
/// The two build scripts are modules of the `pickle-cli` binary rather than a
/// library, so an integration test cannot import them and has to read the
/// values back out of the source.
fn parse_rust_const(src: &str, field: StackField, label: &str) -> u64 {
    let name = field.rust_name();
    // `cli/src/build.rs` exposes its constants; the Cargo build script keeps
    // them private, so accept either.
    let public = format!("pub const {name}: u64 = ");
    let private = format!("const {name}: u64 = ");

    src.lines()
        .map(str::trim)
        .find_map(|l| {
            let value = l
                .strip_prefix(&public)
                .or_else(|| l.strip_prefix(&private))?;
            eval_u64(value)
        })
        .unwrap_or_else(|| panic!("no `const {name}` in {label}"))
}

/// Evaluates a `u64` constant written as a literal or as a product such as
/// `256 * 1024 * 1024`. Only multiplication is accepted, which is all the
/// byte-size constants in the build scripts use.
fn eval_u64(text: &str) -> Option<u64> {
    let mut total: u64 = 1;
    for factor in text.trim().trim_end_matches(';').split('*') {
        let f = factor.trim();
        let value = match f.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => f.parse::<u64>(),
        };
        total = total.checked_mul(value.ok()?)?;
    }
    Some(total)
}

fn parse_u64(value: &str) -> Result<u64, std::num::ParseIntError> {
    let value = value.trim();
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
}

#[test]
fn stack_reserve_clears_the_linker_default() {
    for linker in Linker::all() {
        let reserve = linker.stack_field(StackField::Reserve);
        assert!(
            reserve >= MIN_RESERVE,
            "{} reserves only {reserve} bytes, leaving less headroom than the \
             {MIN_RESERVE} floor; lowering frames grow with the module being \
             compiled, so a reserve near the 1 MiB toolchain default fails as an \
             access violation partway down a valid program",
            linker.label()
        );
    }
}

#[test]
fn stack_commit_stays_below_the_reserve() {
    for linker in Linker::all() {
        let label = linker.label();
        let reserve = linker.stack_field(StackField::Reserve);
        let commit = linker.stack_field(StackField::Commit);
        assert!(
            commit < reserve,
            "{label} commits {commit} against a reserve of {reserve}; the reserve is only \
             reserved address space, and committing all of it makes every compiler pay for \
             pages it never touches"
        );
        assert!(
            commit >= MIN_COMMIT,
            "{label} commits only {commit} bytes; that is too small, since the loader \
             cannot grow the stack within the reserve without a usable starting page"
        );
    }
}

#[test]
fn every_linker_agrees_on_the_stack() {
    for field in [StackField::Reserve, StackField::Commit] {
        let expected = Linker::CargoBuildScript.stack_field(field);
        for linker in Linker::all() {
            assert_eq!(
                linker.stack_field(field),
                expected,
                "{} sets the stack {:?} differently from cli/build.rs; a compiler that \
                 links itself with less stack than it was built with exhausts it one \
                 bootstrap later",
                linker.label(),
                field
            );
        }
    }
}

/// Reads `WORKER_STACK` out of `cli/src/main.rs`.
fn worker_stack() -> u64 {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("main.rs");
    let src = read(&path);
    src.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("const WORKER_STACK: usize = "))
        .and_then(eval_u64)
        .unwrap_or_else(|| panic!("no `const WORKER_STACK` in {}", path.display()))
}

#[test]
fn the_work_thread_is_not_the_smaller_stack() {
    // The compiler's work runs on this thread, so this is the stack that
    // recursive lowering actually has. If it is smaller than the reserve the
    // linker hands the binary, the thread silently becomes the new limit and
    // the original overflow comes back at a different depth.
    let worker = worker_stack();
    assert!(
        worker >= MIN_RESERVE,
        "WORKER_STACK is {worker} bytes, below the {MIN_RESERVE} floor; this is the \
         stack the compiler's own lowering runs on"
    );
    for linker in Linker::all() {
        let reserve = linker.stack_field(StackField::Reserve);
        assert!(
            worker >= reserve,
            "WORKER_STACK is {worker} bytes but {} reserves only {reserve}; the work \
             runs on the thread, so the smaller of the two is what lowering gets",
            linker.label()
        );
    }
}
