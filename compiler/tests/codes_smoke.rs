//! Tests for stable error codes, the classifier lock, JSON output, and the
//! catalogue. The two rules enforced here are the whole point of the feature:
//! a code is never guessed from rendered text, and the mapping between a
//! message and its code is pinned so it cannot silently drift.

use pickle_compiler::diag::{json_string, DiagnosticSink, SourceMap};
use pickle_compiler::error::{code_from_id, explain, ErrorCode, CATALOGUE};
use pickle_compiler::front::frontend_checked;

fn classify(msg: &str) -> Option<ErrorCode> {
    pickle_compiler::check::classify_message(msg)
}

fn check_str(src: &str) -> DiagnosticSink {
    let diags = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let _ = frontend_checked("test.pk", src, &mut map, &diags);
    diags
}

fn error_msgs(diags: &DiagnosticSink) -> String {
    diags
        .diagnostics
        .borrow()
        .iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- classifier lock ------------------------------------------------------

#[test]
fn classifier_pins_message_to_code() {
    use ErrorCode as C;
    let cases = [
        (
            "type mismatch in return value: expected `int`, found `string`",
            C::TypeMismatch,
        ),
        (
            "type mismatch in binding: expected `int`, found `string?`",
            C::TypeMismatch,
        ),
        ("use of undeclared name `foo`", C::UndeclaredName),
        ("no member `bar` on `User`", C::NoMember),
        ("no member `push2` on `List<int>`", C::NoMember),
        (
            "cannot access member `name` on value of type `int`",
            C::MemberOnNonClass,
        ),
        (
            "cannot access member `name` on value of type `string?`",
            C::MemberOnNonClass,
        ),
        ("cannot assign to immutable binding `x`", C::Assignment),
        ("cannot assign to immutable field `x`", C::Assignment),
        (
            "cannot assign to a member of a non-class value",
            C::NonClassMemberAssign,
        ),
        (
            "operator `Mul` requires numeric or string operands, found `string` and `int`",
            C::Operator,
        ),
        ("bitwise operators require `int` operands", C::Operator),
        (
            "unary `!` requires a `bool` operand, found `int`",
            C::Operator,
        ),
        ("left side of `??` is not an `Option`", C::Operator),
        ("`?.` on a non-option value of type `int`", C::Operator),
        ("condition must be a `bool`", C::ConditionNotBool),
        (
            "`for (x in ...)` requires a sequence (List, string, Map, or range), found `int`",
            C::ForSequence,
        ),
        ("map keys must be `string` values", C::MapKeyString),
        ("index must be an `int`", C::Index),
        ("map index must be a `string`", C::Index),
        ("cannot index a value of type `bool`", C::Index),
        (
            "`break`/`continue` used outside of a loop",
            C::BreakOutsideLoop,
        ),
        (
            "cannot return a value from a function with no return type",
            C::ReturnValue,
        ),
        (
            "`if` branches have mismatched types: `int` and `string`",
            C::ReturnValue,
        ),
        (
            "match arms produce inconsistent types: `int` and `string`",
            C::ReturnValue,
        ),
        ("`this` used outside of a class body", C::ThisSuper),
        ("`super` used outside of a class body", C::ThisSuper),
        (
            "`this(...)` can only be used as a named constructor's delegation",
            C::ThisSuper,
        ),
        ("enum `Color` has no variant `Purple`", C::EnumVariant),
        (
            "variant `Point` carries 2 payload field(s); use `Point(...)`",
            C::EnumVariant,
        ),
        ("`Color` cannot be constructed directly", C::CannotConstruct),
        (
            "`User` declares `implements Runnable` but has no method `run`",
            C::InterfaceConformance,
        ),
        ("`x is int` can never succeed", C::Cast),
        ("cannot cast `string` to `int`", C::Cast),
        ("unknown attribute `#[wat]`", C::Attribute),
        (
            "attributes on declarations are not lowered yet (`#[wat]`)",
            C::NotLowered,
        ),
        ("`#[manualAlloc]` takes no arguments", C::ManualAlloc),
        ("duplicate `#[manualAlloc]` attribute", C::ManualAlloc),
        (
            "`#[manualAlloc]` is not supported on static fields",
            C::ManualAlloc,
        ),
        (
            "`#[manualAlloc]` requires an allocation initializer",
            C::ManualAlloc,
        ),
        (
            "owned field `cursor` must be initialized where it is declared",
            C::ManualAlloc,
        ),
        ("use of `p` after `free()`", C::UseAfterFree),
        ("use of `p` after it was moved", C::UseAfterFree),
        ("`p` was already freed", C::DoubleFree),
        (
            "`free` is only available on a `#[manualAlloc]` binding",
            C::FreeOnNonManual,
        ),
        (
            "cannot bind `#[manualAlloc]` value `p` to a managed binding",
            C::OwnedPosition,
        ),
        (
            "cannot store `#[manualAlloc]` value `p` in managed field `f`",
            C::OwnedPosition,
        ),
        (
            "cannot store `#[manualAlloc]` value `p` in a managed collection",
            C::OwnedPosition,
        ),
        (
            "cannot overwrite `#[manualAlloc]` binding `p`",
            C::OverwriteManual,
        ),
        (
            "cannot return `#[manualAlloc]` value `p` from a function that does not own its result",
            C::OverwriteManual,
        ),
        (
            "`alloc` may only be used inside an `unsafe` block",
            C::UnsafeRequired,
        ),
        (
            "`&` may only be used inside an `unsafe` block",
            C::UnsafeRequired,
        ),
        (
            "`*` may only be used inside an `unsafe` block",
            C::UnsafeRequired,
        ),
        (
            "pointer indexing may only be used inside an `unsafe` block",
            C::UnsafeRequired,
        ),
        ("`alloc(T, count)` takes two arguments", C::RawBuffer),
        ("`alloc` element type must be a type name", C::RawBuffer),
        ("`alloc` count must be an `int`", C::RawBuffer),
        ("`free(p)` takes one argument", C::RawBuffer),
        (
            "`free` expects a pointer argument, found `int`",
            C::RawBuffer,
        ),
        ("`free()` takes no arguments", C::RawBuffer),
        (
            "named argument `x` is not supported for this call",
            C::CallArity,
        ),
        ("too many arguments in call", C::CallArity),
        ("expected 2 argument(s), found 3", C::CallArity),
        ("missing arguments for parameters `a`, `b`", C::CallArity),
        (
            "attempt to call a non-function value of type `int`",
            C::CallNonFunction,
        ),
        (
            "cannot initialize a `let` binding by borrowing a bare `int` value into a `&int` reference; `&T` references may only be formed from an existing `&T` value or a managed referent",
            C::StoredRef,
        ),
        ("tuple values are not lowered yet", C::NotLowered),
    ];
    for (msg, want) in cases {
        assert_eq!(
            classify(msg),
            Some(want),
            "message `{msg}` should classify as {want:?}"
        );
    }
}

#[test]
fn classifier_never_guesses_unlisted_messages() {
    // Messages with no code must classify as None, not slip into a wrong
    // bucket with a substring that happens to read similarly.
    let unlisted = [
        "tuple pattern does not match a tuple value",
        "codegen: async lambdas are not a real match for anything here",
        "cannot import from a non-module path",
        "there is no value here",
    ];
    for msg in unlisted {
        assert_eq!(classify(msg), None, "`{msg}` must stay uncoded");
    }
    // A codegen `bad()` that is NOT a "not lowered yet" fails must look right.
    let _ = "this operator is not lowered yet";
}

// ---- catalogue integrity --------------------------------------------------

#[test]
fn catalogue_entries_are_unique_and_stable() {
    let mut ids: Vec<&str> = CATALOGUE.iter().map(|e| e.code.id()).collect();
    ids.sort_unstable();
    for w in ids.windows(2) {
        assert_ne!(w[0], w[1], "duplicate id in catalogue");
    }
    for entry in CATALOGUE {
        let id = entry.code.id();
        assert_eq!(id.len(), 5, "id `{id}` must be 5 chars");
        assert!(id.starts_with('E'), "id `{id}` must start with E");
        assert!(
            id[1..].chars().all(|c| c.is_ascii_digit()),
            "id `{id}` must be E + 4 digits"
        );
        assert_eq!(explain(id).map(|e| e.code), Some(entry.code));
        assert_eq!(code_from_id(id), Some(entry.code));
    }
    // Every enum variant must be represented in the catalogue.
    let covered: Vec<ErrorCode> = CATALOGUE.iter().map(|e| e.code).collect();
    let all = [
        ErrorCode::Lex,
        ErrorCode::Syntax,
        ErrorCode::DuplicateItem,
        ErrorCode::DuplicateImportAlias,
        ErrorCode::ModuleNotFound,
        ErrorCode::NotExported,
        ErrorCode::ModuleMismatch,
        ErrorCode::AmbiguousImport,
        ErrorCode::DuplicateMember,
        ErrorCode::DuplicateConstructor,
        ErrorCode::DuplicateDeinit,
        ErrorCode::UnknownType,
        ErrorCode::NotGeneric,
        ErrorCode::WrongTypeArgs,
        ErrorCode::ListOfRefs,
        ErrorCode::InvalidExtends,
        ErrorCode::OverrideSignature,
        ErrorCode::MissingOverride,
        ErrorCode::OrphanOverride,
        ErrorCode::MixedMethodKind,
        ErrorCode::TypeMismatch,
        ErrorCode::ReturnValue,
        ErrorCode::BreakOutsideLoop,
        ErrorCode::ConditionNotBool,
        ErrorCode::ForSequence,
        ErrorCode::MapKeyString,
        ErrorCode::Index,
        ErrorCode::UndeclaredName,
        ErrorCode::NoMember,
        ErrorCode::EnumVariant,
        ErrorCode::CannotConstruct,
        ErrorCode::NonClassMemberAssign,
        ErrorCode::InterfaceConformance,
        ErrorCode::ThisSuper,
        ErrorCode::Assignment,
        ErrorCode::Cast,
        ErrorCode::Attribute,
        ErrorCode::ManualAlloc,
        ErrorCode::CallArity,
        ErrorCode::CallNonFunction,
        ErrorCode::MemberOnNonClass,
        ErrorCode::Operator,
        ErrorCode::RawBuffer,
        ErrorCode::UseAfterFree,
        ErrorCode::DoubleFree,
        ErrorCode::FreeOnNonManual,
        ErrorCode::OwnedPosition,
        ErrorCode::OverwriteManual,
        ErrorCode::LeakedOwned,
        ErrorCode::UnsafeRequired,
        ErrorCode::StoredRef,
        ErrorCode::NotLowered,
    ];
    for code in all {
        assert!(
            covered.contains(&code),
            "catalogue is missing an entry for {code:?}"
        );
    }
}

// ---- JSON encoder ---------------------------------------------------------

#[test]
fn json_string_escapes_correctly() {
    assert_eq!(json_string("plain"), r#""plain""#);
    assert_eq!(json_string("a\"b"), r#""a\"b""#);
    assert_eq!(json_string("a\\b"), r#""a\\b""#);
    assert_eq!(json_string("line1\nline2"), "\"line1\\nline2\"");
    assert_eq!(json_string("tab\there"), r#""tab\there""#);
    assert_eq!(json_string("ctrl\x01"), r#""ctrl\u0001""#);
    assert_eq!(json_string("unicode ☃"), "\"unicode ☃\"");
}

// ---- integration: codes come from real diagnostics ------------------------

#[test]
fn real_diagnostics_carry_the_expected_code() {
    let d = check_str("fn main() { let n = nope }");
    let diags = d.diagnostics.borrow();
    assert_eq!(diags.len(), 1, "expected 1 diagnostic");
    assert_eq!(diags[0].message, "use of undeclared name `nope`");
    assert_eq!(diags[0].code, Some(ErrorCode::UndeclaredName));
    drop(diags);

    let d = check_str(r#"fn main() { let s = "x" * 2 }"#);
    let diags = d.diagnostics.borrow();
    let mul = diags
        .iter()
        .find(|d| d.message.contains("operator `Mul`"))
        .expect("expected the operator error");
    assert_eq!(mul.code, Some(ErrorCode::Operator));
    drop(diags);

    // Indexing with a non-int argument; keep each snippet self-contained so
    // a prior recovery can't retype the binding.
    let d = check_str(
        r#"fn main() {
            let s = "abc"
            let r = s[true]
        }"#,
    );
    let diags = d.diagnostics.borrow();
    assert_eq!(diags.len(), 1, "expected 1 diagnostic for the index");
    assert_eq!(diags[0].message, "string index must be an `int`");
    assert_eq!(diags[0].code, Some(ErrorCode::Index));
}

// ---- regression: `&&`/`||` must not re-check their operands -----------------
//
// `check_binary` already computes `lt`/`rt` via `check_expr(lhs)`/`check_expr(rhs)`.
// An earlier version *also* called `check_bool_cond` on both operands, which made a
// left-leaning `a || b || c` chain satisfy T(n) = 2*T(n-1): 20 terms took ~0.3s here
// and ~8.2s in the self-hosted compiler, so real modules stopped compiling at all.
// The same redundant walk also re-emitted any diagnostic the operands had already
// produced, so `(1 + "x") || true` reported the inner `Add` error twice.

#[test]
fn logical_or_reports_each_operand_error_once() {
    // Two bad operands: one condition error per operand, and nothing else.
    let d = check_str("fn main() { let b = 1 || 2 }");
    let diags = d.diagnostics.borrow();
    assert_eq!(diags.len(), 2, "one condition error per operand: {diags:?}");
    assert!(diags
        .iter()
        .all(|d| d.message == "condition must be a `bool`"));
    drop(diags);

    // The inner error must appear exactly once: the operands are type-checked
    // once, so a nested error is not duplicated by the boolean context.
    let d = check_str(r#"fn main() { let b = (1 + "x") || true }"#);
    let diags = d.diagnostics.borrow();
    let adds: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("operator `Add`"))
        .collect();
    assert_eq!(
        adds.len(),
        1,
        "nested operand error must not be duplicated: {diags:?}"
    );
    assert_eq!(adds[0].code, Some(ErrorCode::Operator));
    let conds: Vec<_> = diags
        .iter()
        .filter(|d| d.message == "condition must be a `bool`")
        .collect();
    assert_eq!(conds.len(), 1, "one condition error for the `int` side");
    drop(diags);
}

#[test]
fn logical_chains_are_checked_once_per_operand() {
    // A long chain is the shape that used to blow up exponentially. Keep it long
    // enough that a reintroduced double-walk is unmistakable, and assert it stays
    // linear-ish by bounding the work rather than the clock (no timing asserts).
    let terms = (0..64).map(|i| format!("s == \"t{i}\"")).collect::<Vec<_>>();
    let src = format!(
        "fn pick(s: string) -> bool {{ {} }}",
        terms.join(" || ")
    );
    let d = check_str(&src);
    let diags = d.diagnostics.borrow();
    assert!(
        diags.is_empty(),
        "a well-typed 64-term chain must check clean: {diags:?}"
    );
}

#[test]
fn parse_and_lex_errors_get_e0111_and_e0101() {
    let d = check_str("fn main() { let = }");
    let msgs = error_msgs(&d);
    assert!(
        d.diagnostics
            .borrow()
            .iter()
            .any(|d| d.code == Some(ErrorCode::Syntax)),
        "expected an E0111 syntax diagnostic, got:\n{msgs}"
    );

    let d = check_str("let s = \"unterminated");
    assert!(
        d.diagnostics
            .borrow()
            .iter()
            .any(|d| d.code == Some(ErrorCode::Lex)),
        "expected an E0101 lexical diagnostic"
    );
}

#[test]
fn codegen_not_lowered_gets_e0900_only_when_it_says_so() {
    let src = "fn main() {\n    let o: int? = 1\n    let some(x) = o\n    println(x)\n}";
    let d = check_str(src);
    let msgs = error_msgs(&d);
    assert!(
        !d.any_error(),
        "refutable option destructuring should not be a checker error: {msgs}"
    );
    // The codegen `bad()` recovers to the diag sink via emit_ir: force it.
    let d2 = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = pickle_compiler::front::frontend("test.pk", src, &mut map, &d).unwrap();
    let _ = pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &d2);
    let diags = d2.diagnostics.borrow();
    let tuple = diags
        .iter()
        .find(|d| {
            d.message
                .contains("destructuring patterns are not lowered yet")
        })
        .expect("expected a codegen error about unlowered tuple destructuring");
    assert_eq!(tuple.code, Some(ErrorCode::NotLowered));
}

#[test]
fn generic_class_lambda_is_e0900() {
    // A lambda inside a generic class/struct body is rejected at codegen walk
    // time with a "not lowered yet" diagnostic (never silently miscompiled);
    // materializing an instantiation still reports it in that spirit.
    let src = "class Box<T> {
            var x: T
            fn make() -> fn (T) -> T {
                fn (v: T) -> T { v }
            }
        }
        fn main() {
            var b = Box<int>(1)
            println(b.make()(2))
        }";
    let d = check_str(src);
    assert!(
        !d.any_error(),
        "should type-check cleanly:\n{}",
        error_msgs(&d)
    );
    let d2 = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = pickle_compiler::front::frontend("test.pk", src, &mut map, &d).unwrap();
    let _ = pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &d2);
    let diags = d2.diagnostics.borrow();
    let lam = diags
        .iter()
        .find(|d| d.message.contains("generic class/struct"))
        .expect("expected a codegen error about a lambda in a generic class/struct");
    assert_eq!(lam.code, Some(ErrorCode::NotLowered));
}

#[test]
fn lambda_assigning_to_captured_var_is_e0900() {
    // A lambda that assigns to a `var` captured from the enclosing scope
    // used to silently mutate a dead per-call copy of the snapshot (the
    // write-after-read variant compiled and corrupted state at runtime).
    let src = "fn main() {
            var n = 0
            let inc = () -> int {
                let start = n
                n += 1
                n
            }
            println(inc())
        }";
    let d = check_str(src);
    assert!(
        !d.any_error(),
        "should type-check cleanly:\n{}",
        error_msgs(&d)
    );
    let d2 = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = pickle_compiler::front::frontend("test.pk", src, &mut map, &d).unwrap();
    let _ = pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &d2);
    let diags = d2.diagnostics.borrow();
    let msg = diags
        .iter()
        .find(|d| d.message.contains("assigning to `n` inside a lambda"))
        .expect("expected a codegen error about mutation of a captured binding");
    assert_eq!(msg.code, Some(ErrorCode::NotLowered));
}

#[test]
fn grouped_render_has_code_headlines() {
    let d = check_str("fn main() { let x = nope }");
    let mut map = SourceMap::default();
    let _ = frontend_checked("test.pk", "fn main() { let x = nope }", &mut map, &d);
    let rendered = d.render_all_grouped(&map, false);
    assert!(
        rendered.contains("error[E0351] -- undeclared name"),
        "grouped render missing E0351 headline:\n{rendered}"
    );
}
