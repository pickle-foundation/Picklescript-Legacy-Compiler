//! Top-level `var name[: int] = <int literal>` globals.
//!
//! Globals are initialized before `main` runs, so an initializer can only be an
//! integer literal: it cannot call a function or read another global, because
//! neither has been given a value yet. The checker rejects those forms with a
//! dedicated message, and reading such a global as a value is a codegen error.
//!
//! These tests pin both halves: the literal forms must lower to real IR with a
//! writable-data initializer, and the unsupported forms must be diagnosed rather
//! than silently miscompiled.

use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::emit::emit_ir;
use pickle_compiler::front::frontend;
use pickle_compiler::ir::IrModule;

/// Run the front end + emitter and return `(result, rendered diagnostics)`.
fn try_emit(src: &str) -> (Option<IrModule>, String) {
    let mut map = SourceMap::default();
    let diags = DiagnosticSink::new();
    let out = frontend("globals.pkl", src, &mut map, &diags).expect("frontend failed");
    let m = emit_ir(&out.program, &out.resolved, &diags);
    (m, diags.render_all(&map, false))
}

fn emit_ok(src: &str) -> IrModule {
    let (m, rendered) = try_emit(src);
    m.unwrap_or_else(|| panic!("emit failed:\n{rendered}"))
}

fn emit_err(src: &str) -> String {
    let (m, rendered) = try_emit(src);
    assert!(m.is_none(), "expected emit to fail, got a module:\n{rendered}");
    rendered
}

fn has_main(m: &IrModule) -> bool {
    m.funcs.iter().any(|f| f.is_main)
}

#[test]
fn literal_globals_lower_and_keep_main() {
    let m = emit_ok(
        r#"
        var zero: int = 0
        var neg: int = -7
        fn main() { println("hi") }
        "#,
    );
    assert!(has_main(&m), "a main must still be emitted alongside globals");
}

#[test]
fn global_initialize_requires_a_scalar_literal() {
    // Reading another global: `base` has no value yet when initializers run.
    let err = emit_err(
        r#"
        var base: int = 7
        var doubled: int = base
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("must be an integer literal"),
        "missing initializer diagnostic:\n{err}"
    );

    // Calling a function: same reason, same message.
    let err = emit_err(
        r#"
        fn mk() -> int { 21 }
        var made: int = mk()
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("must be an integer literal"),
        "missing initializer diagnostic:\n{err}"
    );

    // A type-correct initializer that is still not a literal reaches the
    // emitter, and the message names the type the global actually wants. (A
    // wrongly-typed literal never gets this far: the checker rejects it first.)
    let err = emit_err(
        r#"
        var seed: bool = true
        var flag: bool = seed
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("must be a bool literal"),
        "missing bool initializer diagnostic:\n{err}"
    );

    let err = emit_err(
        r#"
        var seed: char = 'a'
        var letter: char = seed
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("must be a char literal"),
        "missing char initializer diagnostic:\n{err}"
    );

    let err = emit_err(
        r#"
        var seed: float = 1.5
        var ratio: float = seed
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("must be a float literal"),
        "missing float initializer diagnostic:\n{err}"
    );
}

#[test]
fn scalar_globals_of_every_pod_type_lower() {
    let m = emit_ok(
        r#"
        var flag: bool = true
        var letter: char = 'Z'
        var ratio: float = 2.5
        var negRatio: float = -1.5
        var count: int = 42
        fn main() { println("hi") }
        "#,
    );
    assert!(has_main(&m), "a main must still be emitted alongside globals");

    // Each global must carry its declared IR width in `.data`: a bool is one
    // byte and a char is four, so a fixed 8-byte encoding would misrepresent
    // the slot the backend loads and stores.
    let widths: Vec<(String, usize)> = m
        .globals
        .iter()
        .map(|g| (g.symbol.clone(), g.init.as_ref().map(|b| b.len()).unwrap_or(0)))
        .collect();
    let expect = [
        ("pkl_global_flag", 1usize),
        ("pkl_global_letter", 4),
        ("pkl_global_ratio", 8),
        ("pkl_global_negRatio", 8),
        ("pkl_global_count", 8),
    ];
    for (sym, len) in expect {
        let got = widths
            .iter()
            .find(|(s, _)| s == sym)
            .unwrap_or_else(|| panic!("missing global {sym}"));
        assert_eq!(got.1, len, "{sym} should be initialized with {len} bytes");
    }
}

#[test]
fn reading_an_unlowered_global_is_a_codegen_error() {
    // The initializer is rejected, so the global never gets a lowered value;
    // reading it must diagnose instead of emitting a load from nothing.
    let err = emit_err(
        r#"
        var made: int = mk()
        fn mk() -> int { 21 }
        fn main() { println("v={made}") }
        "#,
    );
    assert!(
        err.contains("not lowered yet"),
        "missing codegen diagnostic:\n{err}"
    );
}