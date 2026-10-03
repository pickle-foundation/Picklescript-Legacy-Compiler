//! The propagation check after a call costs a runtime call, so it is emitted
//! only where a throw can actually arrive. [`emit_ir`] lowers every body twice:
//! once to record the call graph, then again for real with the throwing set
//! solved by a fixed point over it.
//!
//! These tests pin the property that makes that sound and the property that
//! makes it worth doing:
//!
//! * sound — a call whose callee may throw *always* gets a check, transitively,
//!   so no throw is ever lost;
//! * precise — a call that provably cannot throw gets no check, and a program
//!   with no `throw` at all references no exception symbol whatsoever.

use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::ir::{Callee, IrInstr, IrModule};

/// The runtime symbol the propagation check calls.
const PENDING: &str = "pickle_exc_pending";

fn lower(src: &str) -> IrModule {
    let d = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = pickle_compiler::front::frontend("test.pk", src, &mut map, &d)
        .unwrap_or_else(|| panic!("frontend rejected the program:\n{src}"));
    let d2 = DiagnosticSink::new();
    let m = pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &d2).unwrap_or_else(|| {
        let diags = d2.diagnostics.borrow();
        let msgs: Vec<_> = diags.iter().map(|d| d.message.clone()).collect();
        panic!("emit_ir failed: {msgs:?}\n{src}")
    });
    m
}

/// How many times the module calls `pickle_exc_pending`, i.e. how many
/// propagation checks it emits in total.
fn pending_checks(m: &IrModule) -> usize {
    let Some(id) = m.externs.iter().position(|e| e.symbol == PENDING) else {
        return 0;
    };
    let mut n = 0;
    for f in &m.funcs {
        for b in &f.blocks {
            for i in &b.instrs {
                if let IrInstr::Call {
                    callee: Callee::Extern(e),
                    ..
                } = i
                {
                    if e.0 == id {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

/// True when any exception runtime symbol is referenced at all.
fn references_any_exc_symbol(m: &IrModule) -> bool {
    m.externs
        .iter()
        .any(|e| e.symbol.starts_with("pickle_exc_"))
}

#[test]
fn a_program_with_no_throw_references_no_exception_symbol() {
    let m = lower("fn g(a: int) -> int { a + 1 }\nfn main() { println(g(1)) }");
    assert_eq!(pending_checks(&m), 0);
    assert!(
        !references_any_exc_symbol(&m),
        "a throw-free program must not link the exception runtime at all"
    );
}

#[test]
fn a_call_to_a_throwing_function_gets_a_check() {
    let m = lower("fn boom() -> int { throw \"x\" }\nfn main() { println(boom()) }");
    assert_eq!(
        pending_checks(&m),
        1,
        "a direct call to a throwing function must be observed"
    );
}

#[test]
fn a_call_to_a_function_that_cannot_throw_gets_no_check() {
    // `g` cannot throw, so the call to it needs no check even though the
    // program does contain a `throw` elsewhere.
    let m = lower(
        "fn g(a: int) -> int { a + 1 }\nfn boom() -> int { throw \"x\" }\nfn main() { println(g(1) + boom()) }",
    );
    // Only the call to `boom` is checked.
    assert_eq!(pending_checks(&m), 1);
}

#[test]
fn the_throwing_set_is_transitive() {
    // f -> g -> h -> throw, so all three levels need a check at their call
    // sites. A non-transitive analysis would only mark `h`.
    let m = lower(
        "fn h() -> int { throw \"x\" }\n\
         fn g() -> int { return h() }\n\
         fn f() -> int { return g() }\n\
         fn main() { println(f()) }",
    );
    assert_eq!(
        pending_checks(&m),
        3,
        "every link in the chain must propagate"
    );
}

#[test]
fn the_fixed_point_iterates_past_the_first_ring() {
    // A diamond three levels deep. One pass of naive propagation would reach
    // `b` and `main` and stop, leaving `a1`/`a2`/`top` unmarked; reaching them
    // takes repeated rounds, which is what the fixed point is for.
    let m = lower(
        "fn leaf() -> int { throw \"x\" }\n\
         fn b() -> int { return leaf() }\n\
         fn a1() -> int { return b() }\n\
         fn a2() -> int { return b() }\n\
         fn top() -> int { return a1() + a2() }\n\
         fn main() { println(top()) }",
    );
    assert_eq!(
        pending_checks(&m),
        6,
        "the whole reachable cone of a throw must be marked, not just the first ring"
    );
}

#[test]
fn a_throw_free_component_sharing_a_program_with_a_throw_stays_unchecked() {
    // `pure` and its caller `via_pure` form a component with no throw and no
    // path to one. Neither call should be checked, even though `main` also
    // calls something that does throw.
    let m = lower(
        "fn pure(a: int) -> int { a * 2 }\n\
         fn via_pure(a: int) -> int { pure(a) + 1 }\n\
         fn boom() -> int { throw \"x\" }\n\
         fn main() { println(via_pure(2) + boom()) }",
    );
    assert_eq!(
        pending_checks(&m),
        1,
        "only the call that can actually receive a throw needs a check"
    );
}

#[test]
fn a_throwing_generic_instantiation_propagates_to_its_caller() {
    // The instance is registered lazily while its caller lowers, so the
    // analysis pass has to run the whole build loop before it can answer.
    let m = lower("fn id<T>(x: T) -> T { throw \"x\" }\nfn main() { println(id<int>(1)) }");
    assert_eq!(
        pending_checks(&m),
        1,
        "a throwing generic instantiation must be observed at its call site"
    );
}

#[test]
fn a_spread_call_to_a_throwing_function_gets_a_check() {
    // A spread call lowers to a synthesized trampoline, but the check is
    // recorded against the real target, which is what the fixed point needs.
    let m = lower(
        "fn g(a: int, b: int) -> int { return a + b }\n\
         fn h(xs: List<int>) -> int { return g(...xs) }\n\
         fn boom(a: int, b: int) -> int { throw \"x\" }\n\
         fn h2(xs: List<int>) -> int { return boom(...xs) }\n\
         fn main() { println(h([1, 2]) + h2([1, 2])) }",
    );
    // `h2` checks its spread call to `boom`, and `main` checks its call to
    // `h2`. The pure `h` contributes nothing.
    assert_eq!(
        pending_checks(&m),
        2,
        "the throw's cone is h2 and main, and the pure spread call adds none"
    );
}

#[test]
fn a_throwing_property_setter_gets_a_check() {
    // The two accessors are separate functions, so only the one that throws
    // needs a check -- the calls that read `b` and assign it stay unchecked.
    let m = lower(
        "class C {\n\
         \x20var b: int = 0\n\
         \x20property p: int {\n\
         \x20 get { return this.b }\n\
         \x20 set { throw \"x\" }\n\
         \x20}\n\
         \x20property q: int {\n\
         \x20 get { return this.b }\n\
         \x20 set { this.b = value }\n\
         \x20}\n\
         }\n\
         fn setq(c: C, n: int) -> int {\n\
         \x20c.q = n\n\
         \x20return c.q\n\
         }\n\
         fn setp(c: C, n: int) -> int {\n\
         \x20c.p = n\n\
         \x20return 0\n\
         }\n\
         fn main() { println(setp(C(), 1) + setq(C(), 2)) }",
    );
    assert_eq!(
        pending_checks(&m),
        2,
        "the throw's cone is setp and main; the pure accessors add no checks"
    );
}

#[test]
fn a_lowering_failure_is_reported_exactly_once() {
    // The analysis pass lowers every body too, so a program that fails to lower
    // would report each diagnostic twice if the first pass were not muted.
    let src = "fn f() -> int { let o: int? = 1\n let some(x) = o\n x }\nfn main() { println(f()) }";
    let d = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = pickle_compiler::front::frontend("test.pk", src, &mut map, &d).unwrap();
    let d2 = DiagnosticSink::new();
    let r = pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &d2);
    assert!(r.is_none(), "the tuple destructuring should not lower");
    let diags = d2.diagnostics.borrow();
    let lowered: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("not lowered yet"))
        .collect();
    assert_eq!(
        lowered.len(),
        1,
        "a codegen diagnostic must be reported once, not once per pass"
    );
}

/// The type checker's messages for `src`.
///
/// `frontend_checked` still returns `Some` when only the type checker objects,
/// so the sink is the signal, not the return value. Uses `frontend_checked`
/// rather than `frontend`, which stops before `check_program` and reports
/// nothing here.
fn diagnostics(src: &str) -> Vec<String> {
    let d = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let _ = pickle_compiler::front::frontend_checked("test.pk", src, &mut map, &d);
    let diags = d.diagnostics.borrow();
    diags.iter().map(|d| d.message.clone()).collect()
}

#[test]
fn a_diverging_body_still_declares_its_return_type() {
    // `boom` never returns, but it is typed `int`, so handing its result to
    // something wanting a `string` is still a type error. Letting a diverging
    // body skip validation would make every call site trivially well-typed.
    let msgs = diagnostics(
        "fn boom() -> int { throw \"x\" }\n\
         fn f() -> string { return boom() }\n\
         fn main() { println(f()) }",
    );
    assert!(
        msgs.iter().any(|m| m.contains("type mismatch")),
        "a diverging callee must not launder its declared type, got {msgs:?}"
    );
}

#[test]
fn a_call_to_a_diverging_function_is_checked() {
    // A diverging call is the one case where no value reaches the stack, so the
    // propagation check has to be there even though the result is discarded.
    let m = lower("fn boom() -> int { throw \"x\" }\nfn f() { boom() }\nfn main() { f() }");
    assert!(
        pending_checks(&m) >= 1,
        "a discarded diverging call still needs a propagation check"
    );
}
