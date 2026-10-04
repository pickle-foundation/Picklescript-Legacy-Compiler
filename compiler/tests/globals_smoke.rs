//! Top-level globals: POD scalars, managed references, and both kinds together.
//!
//! A scalar global (`int`, `float`, `bool`, `char`) has a static value baked
//! into `.data` before `main` runs, so its initializer must be a literal it can
//! fold at compile time; the checker rejects every other form.
//!
//! A managed global (`string`, `List`, `Map`, `Tuple`, class, struct, enum)
//! has no static form. The slot is zeroed in the image and the emitter
//! synthesizes `pkl_globals_init`, which registers every slot as a GC root and
//! then evaluates and stores each initializer at program start, where the
//! runtime can allocate. That is the first code in the program that may
//! allocate, so it is also the first point a collection can be triggered.
//!
//! The consequence pinned below: `pkl_globals_init` registers all roots *before*
//! running any initializer, which means every slot is still null while
//! initializers run. A managed initializer may therefore call and allocate, but
//! reading another global would dereference null and is rejected.
//!
//! These tests pin all of it: the literal forms must lower to real IR with
//! writable-data initializers, every managed slot type must lower to a pointer
//! with a synthesized root-and-store function, the preamble order must be
//! class-registration -> `pkl_globals_init` -> `pkl_static_init`, and the
//! unsupported forms must be diagnosed rather than silently miscompiled.

use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::emit::emit_ir;
use pickle_compiler::front::frontend;
use pickle_compiler::ir::{Callee, IrBlock, IrFunc, IrInstr, IrModule, IrTy};

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

/// A module exercising one managed global of every pointer-shaped type.
const MANAGED_PTR_GLOBALS: &str = r#"
    class Box {
        var n: int
        constructor(n: int) { this.n = n }
    }

    struct Pair {
        a: int
        b: int
    }

    enum Shade {
        Light
        Dark
    }

    var text: string = "hi"
    var numbers: List<int> = [1, 2, 3]
    var counts: Map<string, int> = { "a": 1 }
    var pairish: (int, int) = (1, 2)
    var boxed: Box = Box(7)
    var made: Pair = Pair(1, 2)
    var shade: Shade = Shade.Light

    fn main() { println("ok") }
"#;

#[test]
fn every_managed_global_type_lowers_to_a_pointer_slot() {
    let m = emit_ok(MANAGED_PTR_GLOBALS);

    // A pointer slot is 8 bytes of zeroed `.data` -- `init` is `None`, never
    // baked-in bytes, because the value cannot exist before the runtime runs.
    // `string` is the one managed type with its own 8-byte slot type, so it is
    // asserted separately below rather than here.
    for name in [
        "pkl_global_numbers",
        "pkl_global_counts",
        "pkl_global_pairish",
        "pkl_global_boxed",
        "pkl_global_made",
        "pkl_global_shade",
    ] {
        let g = m
            .globals
            .iter()
            .find(|g| g.symbol == name)
            .unwrap_or_else(|| panic!("missing global {name}"));
        assert_eq!(g.ty, IrTy::Ptr, "{name} must be a pointer slot");
        assert!(
            g.init.is_none(),
            "{name} must have no static initializer bytes"
        );
    }

    let text = m
        .globals
        .iter()
        .find(|g| g.symbol == "pkl_global_text")
        .expect("missing global pkl_global_text");
    assert_eq!(text.ty, IrTy::Str, "a string keeps its own slot type");
    assert!(text.init.is_none(), "a string has no static bytes");
}

/// The synthesized root-and-store function, or a panic naming what is missing.
fn globals_init<'a>(m: &'a IrModule) -> &'a IrFunc {
    m.funcs
        .iter()
        .find(|f| f.symbol == "pkl_globals_init")
        .expect("a module with managed globals must synthesize pkl_globals_init")
}

/// `true` when `instr` calls the runtime extern named `symbol`.
fn calls_extern(m: &IrModule, instr: &IrInstr, symbol: &str) -> bool {
    match instr {
        IrInstr::Call {
            callee: Callee::Extern(e),
            ..
        } => m.externs[e.0].symbol == symbol,
        _ => false,
    }
}

/// `true` when `instr` calls the compiler-synthesized function named `symbol`.
fn calls_func(m: &IrModule, instr: &IrInstr, symbol: &str) -> bool {
    match instr {
        IrInstr::Call {
            callee: Callee::Func(f),
            ..
        } => m.funcs[f.0].symbol == symbol,
        _ => false,
    }
}

/// Index of the first instruction in `body` that calls `symbol`, if any.
fn position_of_call(m: &IrModule, body: &IrBlock, symbol: &str) -> Option<usize> {
    body.instrs
        .iter()
        .position(|i| calls_func(m, i, symbol))
}

#[test]
fn managed_globals_are_rooted_then_stored() {
    let m = emit_ok(MANAGED_PTR_GLOBALS);
    let f = globals_init(&m);
    let body = &f.blocks[f.entry.0 as usize];

    // Rooting comes first, for every slot, and only then does any initializer
    // run: an initializer may allocate, so a slot that is written but not yet
    // rooted is exactly the window in which a collection would free it.
    let first_store = body
        .instrs
        .iter()
        .position(|i| matches!(i, IrInstr::StoreGlobal { .. }))
        .expect("pkl_globals_init must store every managed initializer");
    let last_root = body
        .instrs
        .iter()
        .enumerate()
        .filter(|(_, i)| calls_extern(&m, i, "pickle_gc_root_add"))
        .map(|(n, _)| n)
        .last()
        .expect("pkl_globals_init must root every managed slot");
    assert!(
        last_root < first_store,
        "all roots must be registered before the first initializer runs"
    );

    // One root and one store per managed global, and each store must carry the
    // slot's own type -- `Str` and `Ptr` are both 8 bytes but are not the same
    // type, and a store typed for the wrong one misreports the IR.
    let roots = body
        .instrs
        .iter()
        .filter(|i| calls_extern(&m, i, "pickle_gc_root_add"))
        .count();
    assert_eq!(roots, 7, "one root per managed global");
    let stores: Vec<IrTy> = body
        .instrs
        .iter()
        .filter_map(|i| match i {
            IrInstr::StoreGlobal { ty, .. } => Some(ty.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(stores.len(), 7, "one store per managed global");
    assert!(
        stores.iter().all(|t| *t == IrTy::Str || *t == IrTy::Ptr),
        "every store must carry its own slot type, got {stores:?}"
    );
    assert_eq!(
        stores.iter().filter(|t| **t == IrTy::Str).count(),
        1,
        "only the string global stores as Str"
    );
}

#[test]
fn preamble_roots_globals_after_class_registration() {
    let m = emit_ok(
        r#"
        class Box {
            var n: int
            constructor(n: int) { this.n = n }
        }
        var boxed: Box = Box(7)
        fn main() { println("ok") }
        "#,
    );
    let main = m.funcs.iter().find(|f| f.is_main).expect("main");
    let body = &main.blocks[main.entry.0 as usize];

    let globals =
        position_of_call(&m, body, "pkl_globals_init").expect("main must call pkl_globals_init");

    // A `Ptr` initializer may call a constructor, and a constructor reads the
    // class descriptor that registration creates. Registering the root and then
    // running the initializer before class registration would hand the
    // constructor a descriptor that does not exist yet.
    if let Some(registers) = body
        .instrs
        .iter()
        .position(|i| calls_extern(&m, i, "pickle_class_register"))
    {
        assert!(
            registers < globals,
            "class registration must precede pkl_globals_init"
        );
    }
}

#[test]
fn preamble_roots_globals_before_static_field_init() {
    let m = emit_ok(
        r#"
        class Holder { static var s: string = "x" }
        var text: string = "y"
        fn main() { println("ok") }
        "#,
    );
    let main = m.funcs.iter().find(|f| f.is_main).expect("main");
    let body = &main.blocks[main.entry.0 as usize];
    let globals = position_of_call(&m, body, "pkl_globals_init").expect("globals init call");
    let statics = position_of_call(&m, body, "pkl_static_init").expect("static init call");
    assert!(
        globals < statics,
        "every managed slot must be rooted before a static initializer can allocate"
    );
}

#[test]
fn a_managed_initializer_may_not_read_another_global() {
    // Every slot is rooted before any initializer runs, so a slot read here is
    // still null: the program would dereference null rather than see a value.
    let err = emit_err(
        r#"
        var first: string = "a"
        var second: string = first
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("reads `first`") && err.contains("null"),
        "missing cross-global read diagnostic:\n{err}"
    );

    // The same read nested inside a collection literal, which is the shape that
    // made this worth a recursive walk rather than a bare-name check.
    let err = emit_err(
        r#"
        var seed: int = 3
        var numbers: List<int> = [seed]
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("reads `seed`"),
        "missing nested cross-global read diagnostic:\n{err}"
    );

    // Nested inside a call argument, and inside a block.
    let err = emit_err(
        r#"
        var seed: int = 3
        var numbers: List<int> = wrap(seed)
        fn wrap(v: int) -> List<int> { [v] }
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("reads `seed`"),
        "missing call-argument cross-global read diagnostic:\n{err}"
    );

    let err = emit_err(
        r#"
        var seed: int = 3
        var table: Map<string, int> = { "v": seed }
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("reads `seed`"),
        "missing map-value cross-global read diagnostic:\n{err}"
    );
}

#[test]
fn a_managed_initializer_may_call_and_allocate() {
    // The point of the guard above: it rejects *reads*, not computation. A
    // managed initializer is real run-time code, so calling a function and
    // building a collection must still lower.
    let m = emit_ok(
        r#"
        fn three() -> int { 3 }
        var numbers: List<int> = [three(), three()]
        fn main() { println(len(numbers)) }
        "#,
    );
    let f = globals_init(&m);
    let body = &f.blocks[f.entry.0 as usize];
    assert!(
        body.instrs
            .iter()
            .any(|i| matches!(i, IrInstr::StoreGlobal { .. })),
        "the call-built initializer must still be stored"
    );
}

#[test]
fn a_lambda_in_a_global_initializer_is_registered() {
    // `register_lambdas` walks function bodies, class members and top-level
    // `const`s, but a managed global's initializer is also real code, so its
    // lambdas have to be registered too. Without that they are missing from the
    // hoisted-body table and lowering fails with "this lambda was not registered
    // for lowering".
    //
    // A module-scope lambda still cannot *capture* a global, which is a
    // separate and pre-existing limitation, so this asserts the diagnostic moved
    // to the capture check. That is the evidence the walk now covers globals:
    // the lambda is found, and the remaining complaint is about scope.
    let err = emit_err(
        r#"
        var seed: int = 3
        var handlers: List<fn () -> int> = [fn() -> int { seed }]
        fn main() { println(len(handlers)) }
        "#,
    );
    assert!(
        !err.contains("was not registered for lowering"),
        "a lambda in a global initializer must be registered:\n{err}"
    );
    assert!(
        err.contains("cannot capture `seed`"),
        "expected the pre-existing module-scope capture limit:\n{err}"
    );
}

#[test]
fn a_fn_typed_global_is_still_not_lowered() {
    // `Fn` is deliberately excluded from the pointer mapping: a closure is a
    // multi-slot object rather than one GC object, so it needs its own lowering
    // rather than being folded in with the rest of the reference types. This
    // pins the exclusion so it cannot be widened by accident.
    let err = emit_err(
        r#"
        var f: fn () -> int = fn() -> int { 1 }
        fn main() { println("x") }
        "#,
    );
    assert!(
        err.contains("no global lowering"),
        "a fn-typed global must report that it has no lowering:\n{err}"
    );
}