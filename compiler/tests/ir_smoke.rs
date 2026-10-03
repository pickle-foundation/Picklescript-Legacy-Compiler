//! Smoke tests for PickleIR lowering: the emitter must produce a sane module
//! (functions, blocks, externs, string pool) for slice-1 programs.
//!
//! These programs must pass resolve + type-check, since `emit_ir` aborts on
//! diagnostics; a panic here means front-end bugs as much as emitter bugs.

use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::emit::emit_ir;
use pickle_compiler::front::frontend;
use pickle_compiler::ir::{BinOp, Callee, FuncId, IrConst, IrInstr, IrModule, IrTerm, IrTy};

fn emit_str(src: &str) -> IrModule {
    let mut map = SourceMap::default();
    let diags = DiagnosticSink::new();
    let out = frontend("test.pkl", src, &mut map, &diags).expect("frontend failed");
    emit_ir(&out.program, &out.resolved, &diags)
        .unwrap_or_else(|| panic!("emit failed:\n{}", diags.render_all(&map, false)))
}

#[test]
fn emits_arithmetic_main() {
    let m = emit_str(
        r#"fn main() {
            let x = 2 + 3
            println("hello")
        }"#,
    );
    assert_eq!(m.funcs.len(), 1);
    let main = &m.funcs[0];
    assert!(main.is_main);
    assert_eq!(main.symbol, "pickle_main");
    // The tail is a discarded statement block, so the entry block ends in a
    // plain `return`.
    assert!(main
        .blocks
        .iter()
        .any(|b| matches!(b.term, IrTerm::Return { v: None })));
    let dump = format!("{m}");
    assert!(dump.contains("binop.add"), "dump:\n{dump}");
    assert!(
        m.strings.iter().any(|s| s == b"hello"),
        "string pool: {:?}\ndump:\n{m}",
        m.strings
    );
    let symbols: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(symbols.contains(&"pickle_print_obj"));
    assert!(symbols.contains(&"pickle_print_newline"));
}

#[test]
fn emits_user_function_and_calls() {
    let m = emit_str(
        r#"fn add(a: int, b: int) -> int {
            a + b
        }

        fn main() {
            let v = add(1, 2)
        }"#,
    );
    assert_eq!(m.funcs.len(), 2);
    let add = m.funcs.iter().find(|f| f.name == "add").expect("add");
    assert_eq!(add.symbol, "pkl_add");
    assert_eq!(add.params.len(), 2);
    assert_eq!(add.ret.to_string(), "int64");
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let call_in_main = main.blocks.iter().any(|b| {
        b.instrs.iter().any(|i| {
            matches!(
                i,
                IrInstr::Call { callee: Callee::Func(fid), .. } if fid.0 == add_id(&m)
            )
        })
    });
    assert!(call_in_main, "main must call `add`, dump:\n{m}");
}

fn add_id(m: &IrModule) -> usize {
    m.funcs.iter().position(|f| f.name == "add").unwrap()
}

#[test]
fn emits_if_expression() {
    let m = emit_str(
        r#"fn pick(c: bool) -> int {
            if (c) { 1 } else { 2 }
        }"#,
    );
    let pick = m.funcs.iter().find(|f| f.name == "pick").expect("pick");
    // entry + then + else + join
    assert_eq!(pick.blocks.len(), 4, "dump:\n{m}");
    assert_eq!(pick.slots.len(), 2, "param c plus the if result slot");
    let dump = format!("{m}");
    assert!(dump.contains("branchif"), "dump:\n{dump}");
    // The result flows through a slot: both branches store, join loads.
    let stores = pick
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter(|i| matches!(i, IrInstr::StoreSlot { .. }))
        .count();
    assert_eq!(stores, 2, "dump:\n{dump}");
}

#[test]
fn emits_while_loop() {
    let m = emit_str(
        r#"fn count(n: int) -> int {
            var i = 0
            var total = 0
            while (i < n) {
                total = total + i
                i = i + 1
            }
            return total
        }"#,
    );
    let count = m.funcs.iter().find(|f| f.name == "count").expect("count");
    let branches = count
        .blocks
        .iter()
        .filter(|b| matches!(b.term, IrTerm::BranchIf { .. }))
        .count();
    assert!(branches >= 1, "loop needs a branchif, dump:\n{count}");
    // The last block returns the accumulated total.
    assert!(
        count
            .blocks
            .iter()
            .any(|b| matches!(b.term, IrTerm::Return { v: Some(_) })),
        "expected an explicit return, dump:\n{count}"
    );
}

#[test]
fn emits_for_in_range() {
    let m = emit_str(
        r#"fn sum(n: int) -> int {
            var total = 0
            for (x in 0..n) {
                total = total + x
            }
            return total
        }"#,
    );
    let sum = m.funcs.iter().find(|f| f.name == "sum").expect("sum");
    // The loop var is bound to the idx slot; the loop has cond/body/end blocks.
    assert!(sum.blocks.len() >= 3, "dump:\n{sum}");
    let dump = format!("{sum}");
    assert!(dump.contains("binop.lt"), "dump:\n{dump}");
}

#[test]
fn emits_range_as_value() {
    let m = emit_str(
        r#"fn range_val(n: int) {
            var r = 2..n
            println(len(r))
            println(r[1])
            for (x in r) {
                println(x)
            }
        }

        fn range_inc(v: int) -> int {
            var ri = 0..=v
            return ri[len(ri) - 1]
        }"#,
    );
    let symbols: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(symbols.contains(&"pickle_range"), "{:?}", symbols);
    let dump = format!("{m}");
    // Both materializations call the runtime to build the list.
    assert!(dump.contains("= call extern#"), "dump:\n{dump}");
    // `..=` bumps the end by one before materializing: `end + 1`.
    assert!(dump.contains("binop.add"), "dump:\n{dump}");
}

#[test]
fn emits_ref_forwarding_and_stored_refs() {
    let m = emit_str(
        r#"fn fwd(p: &int) -> &int {
            return p
        }

        fn use_ref(p: &int) -> int {
            var a: &int = p
            var refs: List<&int> = [p]
            return *a + *refs[0]
        }

        fn main() {
            var t = 3
            println(*fwd(t))
        }"#,
    );
    // Storing/returning a `&T` value lowers to plain `int64` (the address),
    // so `a: &int` and `-> &int` need no boxing.
    let symbols: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(!symbols.contains(&"pickle_append"), "{:?}", symbols);
    // `[p]` boxes each address with the scalar box; `*p` derefs with LoadRaw.
    assert!(symbols.contains(&"pickle_box_i64"), "{:?}", symbols);
    let dump = format!("{m}");
    // The implicit borrow `fwd(t)` takes the address of the local `t`.
    assert!(dump.contains(" = addr slot s"), "dump:\n{dump}");
    // Dereferencing a `&int` (the stored one) is a raw load through the
    // address; the return of `fwd` forwards the copied address directly.
    assert!(dump.contains("loadraw."), "dump:\n{dump}");
}

#[test]
fn emits_spread_trampolines_and_unrolls() {
    let m = emit_str(
        r#"class Counter {
            var x: int
            var y: int
            constructor(x: int, y: int) {
                this.x = x
                this.y = y
            }
            static fn add(a: int, b: int) -> int {
                return a + b
            }
            fn bump(dx: int, dy: int) -> int {
                return this.x + this.y + dx + dy
            }
        }

        fn add3(a: int, b: int, c: int) -> int {
            return a + b + c
        }

        fn main() {
            let th: List<int> = [10, 20, 30]
            let tw: List<int> = [40, 50]
            println(add3(...th))
            println(Counter.add(...tw))
            println(Counter(...tw).bump(...tw))
            println("spread-print:", ...th)
        }"#,
    );
    let symbols: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    // The spread boxed list is built, elements fetched and unboxed, and the
    // runtime arity guard is installed.
    assert!(symbols.contains(&"pickle_list_new"), "{:?}", symbols);
    assert!(symbols.contains(&"pickle_list_get"), "{:?}", symbols);
    assert!(symbols.contains(&"pickle_unbox_i64"), "{:?}", symbols);
    assert!(
        symbols.contains(&"pickle_panic_arity_diff"),
        "{:?}",
        symbols
    );
    // `println("spread-print:", ...th)` unrolls with length + element loads.
    assert!(symbols.contains(&"pickle_list_len"), "{:?}", symbols);
    assert!(symbols.contains(&"pickle_print_i64"), "{:?}", symbols);
    let dump = format!("{m}");
    assert!(dump.contains("fn.spread"), "dump:\n{dump}");
}

#[test]
fn emits_string_concat_and_cmp() {
    let m = emit_str(
        r#"fn hi(name: string) -> string {
            "hi " + name
        }

        fn same(a: string, b: string) -> bool {
            a == b
        }"#,
    );
    let symbols: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(symbols.contains(&"pickle_str_concat"), "{:?}", symbols);
    assert!(symbols.contains(&"pickle_str_cmp"), "{:?}", symbols);
    let dump = format!("{m}");
    assert!(dump.contains("binop.eq"), "dump:\n{dump}");
}

#[test]
fn emits_trailing_expr_after_newline_as_return() {
    // Regression: `expr` followed by a newline before `}` was parsed as a
    // discarded statement instead of the block tail, so a value-returning
    // function ended in `unreachable` (a fatal JIT trap at runtime).
    let m = emit_str(
        r#"fn add(a: int, b: int) -> int {
            a + b
        }

        fn main() {
            println(add(1, 2))
        }"#,
    );
    let add = m.funcs.iter().find(|f| f.name == "add").expect("add");
    assert!(
        add.blocks
            .iter()
            .any(|b| matches!(b.term, IrTerm::Return { v: Some(_) })),
        "the tail expression must feed the return, dump:\n{m}"
    );
    assert!(
        !add.blocks
            .iter()
            .any(|b| matches!(b.term, IrTerm::Unreachable)),
        "no dead tail may remain, dump:\n{m}"
    );
}

#[test]
fn emits_for_continue_increments() {
    let m = emit_str(
        r#"fn take_odds(n: int) -> int {
            var total = 0
            for (x in 0..n) {
                if (x == 2) {
                    continue
                }
                total = total + x
            }
            return total
        }"#,
    );
    let f = m.funcs.iter().find(|g| g.name == "take_odds").expect("fn");
    let cond = f
        .blocks
        .iter()
        .find(|b| matches!(b.term, IrTerm::BranchIf { .. }))
        .expect("loop condition block");
    // `continue` must land in an increment block: it bumps the loop var and
    // then re-enters the condition. Jumping straight to the condition skips
    // the increment and loops forever.
    let inc = f.blocks.iter().find(|b| {
        if let IrTerm::Branch { target } = &b.term {
            *target == cond.id
                && b.instrs
                    .iter()
                    .any(|i| matches!(i, IrInstr::BinOp { op: BinOp::Add, .. }))
        } else {
            false
        }
    });
    assert!(inc.is_some(), "missing increment block, dump:\n{f}");
}

#[test]
fn emits_empty_string_literal() {
    let m = emit_str(
        r#"fn blank() -> string {
            ""
        }

        fn main() {
            println(blank())
        }"#,
    );
    assert!(
        m.strings.iter().any(|s| s.is_empty()),
        "the empty string must land in the pool, dump:\n{m}"
    );
}

#[test]
fn emits_short_circuit_and_or() {
    let m = emit_str(
        r#"fn both(a: bool, b: bool) -> bool {
            a && b
        }

        fn either(a: bool, b: bool) -> bool {
            a || b
        }"#,
    );
    let both = m.funcs.iter().find(|f| f.name == "both").expect("both");
    // && and || each materialize a const for the short-circuit value.
    let dump = format!("{m}");
    assert!(dump.contains("const false"), "dump:\n{dump}");
    assert!(dump.contains("const true"), "dump:\n{dump}");
    // &&: rhs block + short block + join, beyond the entry.
    assert!(both.blocks.len() >= 4, "dump:\n{m}");
}

// ---- lists (slice 1.5) ----

fn externs(m: &IrModule) -> Vec<String> {
    m.externs.iter().map(|e| e.symbol.clone()).collect()
}

#[test]
fn emits_array_literal_materializes_list() {
    let m = emit_str(
        r#"fn main() {
            let xs = [1, 2, 3]
            println(len(xs))
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_new"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_push"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_box_i64"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_len"),
        "externs: {syms:?}"
    );
}

#[test]
fn emits_index_read_unboxes() {
    let m = emit_str(
        r#"fn head(xs: List<int>) -> int {
            xs[0]
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_get"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );
}

#[test]
fn emits_index_assign_boxes() {
    let m = emit_str(
        r#"fn put(xs: List<int>, v: int) {
            xs[0] = v
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_set"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_box_i64"),
        "externs: {syms:?}"
    );
}

#[test]
fn emits_list_methods() {
    let m = emit_str(
        r#"fn main() {
            var xs = [1]
            xs.push(2)
            let last = xs.pop()
            println(last)
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_push"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_pop"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_box_i64"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );
}

#[test]
fn emits_for_in_list_with_get_and_unbox() {
    let m = emit_str(
        r#"fn sum(xs: List<int>) -> int {
            var total = 0
            for (x in xs) {
                total = total + x
            }
            return total
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_get"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );
    let sum = m.funcs.iter().find(|f| f.name == "sum").expect("sum");
    let cond = sum
        .blocks
        .iter()
        .find(|b| matches!(b.term, IrTerm::BranchIf { .. }))
        .expect("loop condition block");
    assert!(
        sum.blocks.iter().any(|b| {
            if let IrTerm::Branch { target } = &b.term {
                *target == cond.id
                    && b.instrs.iter().any(|i| {
                        if let IrInstr::Call {
                            callee: Callee::Extern(ex),
                            args,
                            ..
                        } = i
                        {
                            m.externs.get(ex.0).map(|e| e.symbol.as_str())
                                == Some("pickle_list_len")
                                && args.len() == 1
                        } else {
                            false
                        }
                    })
            } else {
                false
            }
        }),
        "the for-loop must bound iterations by `pickle_list_len`, dump:\n{sum}"
    );
}

#[test]
fn emits_char_lists_in_codegen() {
    // `char` elements box through `pickle_box_char`/`pickle_unbox_char`,
    // same as the other scalars, so a char list lowers to real IR.
    let m = emit_str(
        r#"fn main() {
            let cs = ['a', 'b']
            println(cs[1])
        }"#,
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(externs.contains(&"pickle_box_char"), "externs: {externs:?}");
    assert!(
        externs.contains(&"pickle_unbox_char"),
        "externs: {externs:?}"
    );
}

#[test]
fn emits_nested_generic_lists() {
    // Nesting is spelled with a single `>>` token in the source; the parser
    // splits it so `List<List<int>>` resolves, and the emitter passes the
    // outer element (the inner list pointer) through without boxing.
    let m = emit_str(
        r#"fn probe(grid: List<List<int>>) -> int {
            let first = grid[0]
            let second = grid[1]
            let a = first[0]
            let b = second[1]
            return a + b
        }

        fn main() {
            let grid = [[1, 2], [3, 4]]
            println(probe(grid))
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_list_new"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_push"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_get"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );
    let probe = m.funcs.iter().find(|f| f.name == "probe").expect("probe");
    assert!(
        probe
            .blocks
            .iter()
            .flat_map(|b| b.instrs.iter())
            .filter(
                |i| matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
                if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_list_get"))
            )
            .count()
            >= 2,
        "a nested read needs two `pickle_list_get` calls, dump:\n{probe}"
    );
}

#[test]
fn emits_generic_function_instantiation() {
    // A generic call `id<int>(...)` monomorphizes: one concrete function per
    // distinct type-argument list, with a mangled symbol; calls reuse the
    // same instantiation, and nested generic calls reuse the inner one. The
    // generic declaration itself is not lowered as a callable.
    let m = emit_str(
        r#"fn id_fn<T>(x: T) -> T {
            x
        }

        fn twice<T>(x: T) -> T {
            id_fn<T>(id_fn<T>(x))
        }

        fn main() {
            println(id_fn<int>(5))
            println(twice<int>(6))
            println(id_fn<string>("hi"))
        }"#,
    );
    let instantiated: Vec<&str> = m
        .funcs
        .iter()
        .map(|f| f.symbol.as_str())
        .filter(|s| s.starts_with("pkl_id_fn__") || s.starts_with("pkl_twice__"))
        .collect();
    assert_eq!(
        instantiated.len(),
        3,
        "expected id_fn<int>, twice<int>, id_fn<string> instantiations, got {instantiated:?}"
    );
    assert!(!m.funcs.iter().any(|f| f.symbol == "pkl_id_fn"));
    assert!(m.funcs.iter().any(|f| f.symbol == "pkl_id_fn__int"));
    assert!(m.funcs.iter().any(|f| f.symbol == "pkl_id_fn__string"));
    assert!(m.funcs.iter().any(|f| f.symbol == "pkl_twice__int"));
    // The two id_fn<int> calls in `twice` and the one in `main` must share a
    // single instantiation.
    let id_int = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_id_fn__int")
        .expect("id<int> instantiation");
    assert_eq!(
        m.funcs[id_int].ret,
        IrTy::Int,
        "instantiated return type wrong:\n{}",
        m.funcs[id_int]
    );
    let call_count = m
        .funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.instrs.iter())
        .filter(|i| matches!(i, IrInstr::Call { callee: Callee::Func(FuncId(id)), .. } if *id == id_int))
        .count();
    assert_eq!(
        call_count, 3,
        "all three sites must call the single instantiation"
    );
}

#[test]
fn emits_inferred_generic_function_instantiation() {
    // `id_fn(7)` with no explicit type arguments infers `int` from the
    // argument and materializes the SAME instantiation as an explicit
    // `id_fn<int>(7)`: inferring and writing the type arguments by hand land
    // on one shared monomorphized function. Uninferrable generic calls do not
    // lower.
    let m = emit_str(
        r#"fn id_fn<T>(x: T) -> T {
            x
        }

        fn first<T>(xs: List<T>, fallback: T) -> T {
            if (len(xs) > 0) {
                xs[0]
            } else {
                fallback
            }
        }

        fn main() {
            println(id_fn(7))
            println(id_fn(7))
            println(first([1, 2, 3], 0))
        }"#,
    );
    let instantiated: Vec<&str> = m
        .funcs
        .iter()
        .map(|f| f.symbol.as_str())
        .filter(|s| s.starts_with("pkl_id_fn__") || s.starts_with("pkl_first__"))
        .collect();
    assert_eq!(
        instantiated.len(),
        2,
        "expected id_fn<int> + first<int> instantiations, got {instantiated:?}"
    );
    assert!(m.funcs.iter().any(|f| f.symbol == "pkl_id_fn__int"));
    assert!(m.funcs.iter().any(|f| f.symbol == "pkl_first__int"));
    // The two inferred `id_fn(7)` sites share the single `id_fn<int>`.
    let id_int = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_id_fn__int")
        .expect("id<int> instantiation");
    let call_count = m
        .funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.instrs.iter())
        .filter(|i| matches!(i, IrInstr::Call { callee: Callee::Func(FuncId(id)), .. } if *id == id_int))
        .count();
    assert_eq!(
        call_count, 2,
        "both inferred sites must call the single instantiation"
    );
}

#[test]
fn emits_generic_function_as_value() {
    // A generic function used as a VALUE (`let f = id_fn<int>`) materializes
    // the same instantiation an explicit call would, but the callee is wrapped
    // in a zero-capture closure through a forwarder trampoline -- the
    // `fn.value` class function with a `pkl_tramp_*` symbol. A bare generic fn
    // passed as an argument whose fn-typed parameter pins its type args
    // materializes the SAME instantiation at that argument site, so the whole
    // program shares one `pkl_id_fn__int`.
    let m = emit_str(
        r#"fn id_fn<T>(x: T) -> T {
            x
        }

        fn apply(f: fn (int) -> int, x: int) -> int {
            f(x)
        }

        fn main() {
            let f = id_fn<int>
            println(str(f(3)))
            println(str(id_fn<int>(7)))
            println(str(apply(id_fn, 5)))
        }"#,
    );
    let instantiated: Vec<&str> = m
        .funcs
        .iter()
        .map(|f| f.symbol.as_str())
        .filter(|s| s.starts_with("pkl_id_fn__"))
        .collect();
    assert_eq!(
        instantiated,
        vec!["pkl_id_fn__int"],
        "the value, the explicit call, and the inferred arg must share one instantiation: {instantiated:?}"
    );
    // The value is dispatched through a forwarder trampoline (a `fn.value`
    // class function), not the instantiation itself.
    assert!(
        m.funcs
            .iter()
            .any(|f| f.name == "fn.value" && f.symbol.starts_with("pkl_tramp_")),
        "expected a forwarder trampoline for the generic value"
    );
    // The explicit call site still statically calls the instantiation.
    let id_int = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_id_fn__int")
        .expect("id<int> instantiation");
    let static_calls = m
        .funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.instrs.iter())
        .filter(|i| matches!(i, IrInstr::Call { callee: Callee::Func(FuncId(id)), .. } if *id == id_int))
        .count();
    assert!(
        static_calls >= 1,
        "the explicit call must hit the instantiation"
    );
}

#[test]
fn emits_generic_class_instantiation() {
    // `Box<int>(...)` lower like ordinary classes: the materialized ctor and
    // methods carry mangled symbols (a type-argument suffix, deduplicated with
    // `_v{n}` on symbol collisions), a per-instantiation plan table records the
    // substituted layout under the mangled display name, and a single
    // `pickle_class_register` call registers the runtime class. Reusing the
    // same type arguments shares one instantiation.
    let m = emit_str(
        r#"class Box<T> {
            var value: T

            fn read() -> T {
                this.value
            }

            fn write(newValue: T) {
                this.value = newValue
            }
        }

        struct Pair<A, B> {
            var first: A
            var second: B
        }

        fn main() {
            var b = Box<int>(5)
            b.write(7)
            println(b.read())
            var p = Pair<int, string>(1, "one")
            println(p.first, p.second)
        }"#,
    );
    let symbols: Vec<&str> = m.funcs.iter().map(|f| f.symbol.as_str()).collect();
    for want in [
        "pkl_Box_new__int",
        "pkl_Box_read_int",
        "pkl_Box_write_int",
        "pkl_Pair_new__int_string",
    ] {
        assert!(
            symbols.contains(&want),
            "missing instantiation symbol {want:?} in {symbols:?}"
        );
    }
    // The ctor + both methods must all be owned by (and lower under) the same
    // instantiated plan: the plan table is registered under the mangled
    // display name `Box<int>`.
    let regs = m
        .funcs
        .iter()
        .flat_map(|f| f.blocks.iter())
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(
                i,
                IrInstr::Call {
                    callee: Callee::Extern(_),
                    ..
                }
            )
        })
        .count();
    assert!(regs > 0);
    // The class name is interned for `pickle_class_register` and field reads
    // (`pickle_obj_slot_get`).
    let externs = externs(&m);
    for need in [
        "pickle_class_register",
        "pickle_obj_slot_get",
        "pickle_obj_slot_set",
    ] {
        assert!(externs.iter().any(|e| e == need), "missing extern {need:?}");
    }
}

#[test]
fn emits_enum_construct_and_match() {
    // `Enum.Variant(...)` lowers to `pickle_enum_new` + `pickle_enum_set_field`
    // (boxing scalar payloads); `match` lowers to `pickle_enum_tag` checks with
    // `pickle_enum_field` payload extraction. A chain that ends without a
    // catch-all arm calls `pickle_panic_no_match`.
    let m = emit_str(
        r#"enum Color {
            Red
            Blue
            Rgb(r: int, g: int, b: int)
        }

        fn describe(c: Color) -> int {
            match (c) {
                case Color.Red -> 0
                case Color.Blue -> 1
                case Color.Rgb(r, g, b) -> r + g + b
            }
        }

        fn other(c: Color) -> int {
            match (c) {
                case Color.Red -> 10
                case c2 -> 20
            }
        }

        fn main() {
            let c = Color.Rgb(1, 2, 3)
            println(describe(c), other(c))
        }"#,
    );
    let syms = externs(&m);
    for need in [
        "pickle_enum_new",
        "pickle_enum_set_field",
        "pickle_enum_tag",
        "pickle_enum_field",
        "pickle_panic_no_match",
    ] {
        assert!(
            syms.iter().any(|s| s == need),
            "missing {need}, externs: {syms:?}"
        );
    }
    assert!(
        syms.iter().any(|s| s == "pickle_box_i64"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );

    let describe = m
        .funcs
        .iter()
        .find(|f| f.name == "describe")
        .expect("describe");
    let checks = describe
        .blocks
        .iter()
        .filter(|b| matches!(b.term, IrTerm::BranchIf { .. }))
        .count();
    assert!(
        checks >= 3,
        "three variant arms need three tag checks, dump:\n{describe}"
    );
    let field_reads = describe
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
            if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_enum_field"))
        })
        .count();
    assert!(
        field_reads >= 3,
        "binding r, g, b needs three payload reads, dump:\n{describe}"
    );

    let other = m.funcs.iter().find(|f| f.name == "other").expect("other");
    let other_checks = other
        .blocks
        .iter()
        .filter(|b| matches!(b.term, IrTerm::BranchIf { .. }))
        .count();
    assert_eq!(
        other_checks, 1,
        "only the variant arm is tested; the binding arm is a catch-all, dump:\n{other}"
    );
    assert!(
        other
            .blocks
            .iter()
            .flat_map(|b| b.instrs.iter())
            .any(|i| matches!(i, IrInstr::StoreSlot { .. })),
        "the binding arm stores the scrutinee into a slot, dump:\n{other}"
    );
}

#[test]
fn emits_map_operations() {
    // Map literals lower to `pickle_map_new` + `pickle_map_set`; reads go
    // through `pickle_map_get_boxed`; methods/len/iteration use their externs.
    let m = emit_str(
        r#"fn lookup(m: Map<string, int>) -> int {
            let a = m["x"]
            return a + len(m)
        }

        fn main() {
            var m = {"alpha": 10, "beta": 20}
            m["beta"] = 5
            m["gamma"] = m["gamma"] + 1
            let yes = m.has("alpha")
            let ks = m.keys()
            let vs = m.values()
            print(yes, ks, vs, lookup(m))
            for (x in m) {
                print(x)
            }
        }"#,
    );
    let syms = externs(&m);
    for need in [
        "pickle_map_new",
        "pickle_map_set",
        "pickle_map_get_boxed",
        "pickle_map_has",
        "pickle_map_len",
        "pickle_map_keys",
        "pickle_map_values",
    ] {
        assert!(
            syms.iter().any(|s| s == need),
            "missing {need}, externs: {syms:?}"
        );
    }
    assert!(
        syms.iter().any(|s| s == "pickle_box_i64"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_unbox_i64"),
        "externs: {syms:?}"
    );
    let lookup = m.funcs.iter().find(|f| f.name == "lookup").expect("lookup");
    assert!(
        lookup
            .blocks
            .iter()
            .flat_map(|b| b.instrs.iter())
            .filter(
                |i| matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
                if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_map_get_boxed"))
            )
            .count()
            >= 1,
        "a map read needs `pickle_map_get_boxed`, dump:\n{lookup}"
    );
}

#[test]
fn map_read_defaults_to_zero() {
    // An absent key must not unbox a null pointer: the emitter boxes the
    // value type's zero (`0`) and passes it to `pickle_map_get_boxed`.
    let m = emit_str(
        r#"fn peek(m: Map<string, int>) -> int {
            m["missing"]
        }"#,
    );
    let get_boxed = m
        .externs
        .iter()
        .position(|e| e.symbol == "pickle_map_get_boxed")
        .expect("pickle_map_get_boxed extern");
    let box_i64 = m
        .externs
        .iter()
        .position(|e| e.symbol == "pickle_box_i64")
        .expect("pickle_box_i64 extern");
    let peek = m.funcs.iter().find(|f| f.name == "peek").expect("peek");
    let calls: Vec<(usize, bool)> = peek
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                Some((id.0, false))
            } else if let IrInstr::Const {
                c: IrConst::Int(0), ..
            } = i
            {
                Some((usize::MAX, true))
            } else {
                None
            }
        })
        .collect();
    let box_pos = calls
        .iter()
        .position(|(id, _)| *id == box_i64)
        .expect("boxing the zero before the read");
    let get_pos = calls
        .iter()
        .position(|(id, _)| *id == get_boxed)
        .expect("the boxed get");
    assert!(
        box_pos < get_pos,
        "the zero default must be boxed before `pickle_map_get_boxed`"
    );
    assert!(calls.iter().any(|(id, is_zero)| *is_zero || *id == box_i64));
}

#[test]
fn emits_class_ctor_registration_and_fields() {
    let m = emit_str(
        r#"class Point {
            x: int
            y: int
        }

        fn main() {
            let p = Point(1, 2)
            println(p.y)
            p.x = 7
        }"#,
    );
    let syms = externs(&m);
    for s in [
        "pickle_class_register",
        "pickle_class_new",
        "pickle_obj_slot_get",
        "pickle_obj_slot_set",
    ] {
        assert!(syms.iter().any(|x| x == s), "externs: {syms:?}");
    }
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let reg_calls = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str()) == Some("pickle_class_register")
            } else {
                false
            }
        })
        .count();
    assert!(reg_calls >= 1, "main must register every class up front");
}

#[test]
fn emits_class_methods_statics_and_this() {
    let m = emit_str(
        r#"class Counter {
            value: int

            fn add(by: int) {
                this.value = this.value + by
            }

            fn total() -> int {
                return this.value
            }

            static fn zero() -> string {
                return "z"
            }
        }

        fn main() {
            let c = Counter(1)
            c.add(2)
            println(c.total())
            println(Counter.zero())
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    for sym in [
        "pkl_Counter_new",
        "pkl_Counter_add",
        "pkl_Counter_total",
        "pkl_Counter_sm_zero",
    ] {
        assert!(syms.iter().any(|s| s == sym), "symbols: {syms:?}");
    }
    let add = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_add")
        .expect("add");
    assert_eq!(
        add.params.first().map(|p| p.ty),
        Some(IrTy::Ptr),
        "receiver first"
    );
    let zero = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_sm_zero")
        .expect("zero");
    assert!(zero.params.is_empty(), "static method has no receiver");
}

#[test]
fn emits_explicit_ctor_field_inits_and_init_block() {
    let m = emit_str(
        r#"class Counter {
            var count: int = 0

            constructor(start: int) {
                this.count = this.count + start
            }

            init {
                this.count = this.count + 1
            }

            fn total() -> int {
                return this.count
            }
        }

        fn main() {
            let c = Counter(1)
            println(c.total())
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    assert!(
        syms.iter().any(|s| s == "pkl_Counter_new"),
        "symbols: {syms:?}"
    );
    let new = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_new")
        .expect("new");
    assert_eq!(new.params.len(), 1, "explicit ctor params win over fields");
    assert_eq!(new.params[0].name, "start");
    let allocs = new
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str()) == Some("pickle_class_new")
            } else {
                false
            }
        })
        .count();
    assert!(allocs >= 1, "ctor allocates the object");
}

#[test]
fn synthesized_ctor_skips_initialized_fields() {
    let m = emit_str(
        r#"class Point {
            var x: int = 0
            y: int
        }

        class Label {
            text: string
            var times: int = 3
        }

        fn main() {
            let p = Point(5)
            let l = Label("hi")
            println(p.y)
            println(l.text)
        }"#,
    );
    let point = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Point_new")
        .expect("Point.new");
    let names: Vec<&str> = point.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["y"], "initialized field `x` is not a parameter");
    let label = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Label_new")
        .expect("Label.new");
    let names: Vec<&str> = label.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["text"],
        "`times` has an initializer so it is skipped"
    );
}

#[test]
fn emits_property_accessors_and_dispatches() {
    let m = emit_str(
        r#"class Counter {
            var count: int

            property doubled: int {
                get => this.count * 2
            }

            property mirrored: int {
                get => this.count
                set { this.count = value }
            }

            property label: string {
                get { return "c" }
            }
        }

        fn main() {
            let c = Counter(1)
            println(c.doubled)
            c.mirrored = 9
            println(c.mirrored)
            println(c.label)
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    for sym in [
        "pkl_Counter_doubled_get",
        "pkl_Counter_mirrored_get",
        "pkl_Counter_mirrored_set",
        "pkl_Counter_label_get",
    ] {
        assert!(syms.iter().any(|s| s == sym), "symbols: {syms:?}");
    }
    let get = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_doubled_get")
        .expect("get");
    assert_eq!(
        get.params.first().map(|p| p.ty),
        Some(IrTy::Ptr),
        "getter receiver first"
    );
    assert_eq!(get.ret, IrTy::Int, "getter returns the property type");
    let set = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_mirrored_set")
        .expect("set");
    assert_eq!(set.params.len(), 2, "setter takes `this` and `value`");
    assert_eq!(set.params[1].name, "value");
    assert_eq!(set.ret, IrTy::Unit, "setter returns unit");
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let used: Vec<String> = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Func(fid),
                ..
            } = i
            {
                Some(m.funcs.get(fid.0).map(|f| f.symbol.clone()))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    for want in [
        "pkl_Counter_doubled_get",
        "pkl_Counter_mirrored_get",
        "pkl_Counter_mirrored_set",
    ] {
        assert!(used.iter().any(|s| s == want), "main calls: {used:?}");
    }
}

#[test]
fn emits_compound_assignment_across_targets() {
    // `op=` must lower for every target: scalar and string fields/statics,
    // instance and static properties, string list elements, string map values,
    // and scalars through a raw pointer. String concatenation goes through
    // `pickle_str_concat`; string-typed statics/lists that are still their
    // null default rely on the runtime's null-safe concat.
    let m = emit_str(
        r#"class Bag {
            var name: string
            static var counter: int
            static var label: string
            property doubled: int {
                get => Bag.counter * 2
                set { Bag.counter = value / 2 }
            }
            static property dp: int {
                get => Bag.counter * 2
                set { Bag.counter = value / 2 }
            }
            constructor() {
                this.name = "a"
            }
        }

        fn main() {
            var b = Bag()
            b.name += "b"
            Bag.counter += 1
            Bag.label += "s"
            b.doubled += 4
            Bag.dp += 2
            var xs: List<string> = ["p"]
            xs[0] += "q"
            var m: Map<string, string> = { "k": "v" }
            m["k"] += "w"
            unsafe {
                var n = 1
                let p: *int = &n
                (*p) += 5
                println(n)
            }
        }"#,
    );
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let calls: Vec<&str> = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(idx),
                ..
            } = i
            {
                Some(m.externs.get(idx.0).map(|e| e.symbol.as_str()))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert!(
        calls.contains(&"pickle_str_concat"),
        "string field op= concatenates: {calls:?}"
    );
    for sym in [
        "pkl_Bag_doubled_get",
        "pkl_Bag_doubled_set",
        "pkl_Bag_sm_dp_get",
        "pkl_Bag_sm_dp_set",
    ] {
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| b.instrs.iter())
                .filter_map(|i| match i {
                    IrInstr::Call {
                        callee: Callee::Func(fid),
                        ..
                    } => Some(m.funcs.get(fid.0).map(|f| f.symbol.as_str())),
                    _ => None,
                })
                .flatten()
                .any(|s| s == sym),
            "main must call {sym}"
        );
    }
    assert!(
        calls.contains(&"pickle_static_get"),
        "static get: {calls:?}"
    );
    assert!(calls.contains(&"pickle_list_get"), "list get: {calls:?}");
    assert!(
        calls.contains(&"pickle_map_get_boxed"),
        "map get: {calls:?}"
    );
    let loadraw = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .any(|i| matches!(i, IrInstr::LoadRaw { .. } | IrInstr::StoreRaw { .. }));
    assert!(loadraw, "raw-pointer op= loads and stores raw");
}

#[test]
fn emits_let_tuple_destructuring() {
    // `let (a, b) = t` destructuring: the init is evaluated once and each
    // name binds a fresh slot via `pickle_tuple_field` (mirroring the match
    // binder). Nested tuples and the `var`/`let` forms all lower.
    let m = emit_str(
        r#"fn main() {
            var t = (1, 2)
            let (a, b) = t
            println(a)
            println(b)
            var (x, y) = (3, 4)
            println(x + y)
            let (n, (m, _)) = (5, (6, 7))
            println(n)
            println(m)
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_tuple_field"),
        "externs: {syms:?}"
    );
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    // No option resolution: `let some(v)` stays bailing, so the tuple
    // binder must not pull in `opt_resolve`.
    assert!(
        !syms.iter().any(|s| s == "opt_resolve"),
        "externs: {syms:?}"
    );
    // Three tuple destructures: `(a, b)`, `(x, y)`, `(n, (m, _))` (nested
    // tuple is two fields for the inner + one for the outer `n`).
    let tuple_field_calls = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(ex),
                ..
            } = i
            {
                m.externs.get(ex.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_tuple_field")
        .count();
    assert_eq!(
        tuple_field_calls, 8,
        "(a, b): 2, (x, y): 2, (n, (m, _)): outer 2 + inner (m, _) 2, dump:\n{main}"
    );
}

#[test]
fn emits_for_in_list_of_tuples() {
    // `for ((x, y) in xs)` over a `List<(int, int)>`: each iteration reads
    // the boxed tuple element and unboxes its fields, one fresh slot per
    // name (the same binder `let (a, b) = t` uses).
    let m = emit_str(
        r#"fn sum(xs: List<(int, int)>) -> int {
            var total = 0
            for ((x, y) in xs) {
                total = total + x - y
            }
            return total
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_tuple_field"),
        "externs: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_list_get"),
        "externs: {syms:?}"
    );
    let sum = m.funcs.iter().find(|f| f.name == "sum").expect("sum");
    // The element slot holds the boxed tuple; fields are unboxed per trip via
    // `pickle_tuple_field` (twice per iteration, once per part).
    let tuple_field_calls = sum
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(ex),
                ..
            } = i
            {
                m.externs.get(ex.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_tuple_field")
        .count();
    assert_eq!(
        tuple_field_calls, 2,
        "two tuple parts per iteration, dump:\n{sum}"
    );
}

#[test]
fn emits_static_property_accessors_receiverless() {
    // Static property accessors lower as receiver-less functions
    // (`pkl_<T>_sm_<p>_get`/`_set`), dispatch via `Type.prop`, and can
    // reference other statics via the type-qualified path.
    let m = emit_str(
        r#"class Config {
            static property limit: int {
                get => 10
            }

            static property label: string {
                get => "cfg"
            }

            static property guarded: int {
                get => Config.limit
                set {}
            }

            property instanceOnly: int { get => 1 }
        }

        fn main() {
            println(Config.limit)
            Config.guarded = 11
            println(Config.limit)
            println(Config.label)
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    for sym in [
        "pkl_Config_sm_limit_get",
        "pkl_Config_sm_label_get",
        "pkl_Config_sm_guarded_get",
        "pkl_Config_sm_guarded_set",
    ] {
        assert!(syms.iter().any(|s| s == sym), "symbols: {syms:?}");
    }
    let get = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Config_sm_limit_get")
        .expect("get");
    assert!(get.params.is_empty(), "static getter has no receiver");
    assert_eq!(get.ret, IrTy::Int);
    let set = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Config_sm_guarded_set")
        .expect("set");
    assert_eq!(set.params.len(), 1, "static setter takes only `value`");
    assert_eq!(set.params[0].name, "value");
    assert_eq!(set.ret, IrTy::Unit);
    let inst = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Config_instanceOnly_get")
        .expect("instance accessor still lowers");
    assert_eq!(inst.params.first().map(|p| p.ty), Some(IrTy::Ptr));
    let guarded_get = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Config_sm_guarded_get")
        .expect("guarded.get");
    let statics_called: Vec<String> = guarded_get
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Func(fid),
                ..
            } = i
            {
                Some(m.funcs.get(fid.0).map(|f| f.symbol.clone()))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert!(
        statics_called
            .iter()
            .any(|s| s == "pkl_Config_sm_limit_get"),
        "static getter calls the type-qualified static getter: {statics_called:?}"
    );
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let used: Vec<(String, usize)> = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Func(fid),
                args,
                ..
            } = i
            {
                Some((
                    m.funcs
                        .get(fid.0)
                        .map(|f| f.symbol.clone())
                        .unwrap_or_default(),
                    args.len(),
                ))
            } else {
                None
            }
        })
        .collect();
    assert!(
        used.iter()
            .any(|(s, n)| s == "pkl_Config_sm_limit_get" && *n == 0),
        "main calls the getter with no receiver: {used:?}"
    );
    assert!(
        used.iter()
            .any(|(s, n)| s == "pkl_Config_sm_guarded_set" && *n == 1),
        "main calls the setter with just the value: {used:?}"
    );
}

#[test]
fn emits_guarded_match_arms() {
    // Guards evaluate between the tag check and the arm body, with payload
    // bindings live; a false guard falls through to the next arm. A guarded
    // catch-all that can still reject keeps the non-exhaustive panic path.
    let m = emit_str(
        r#"enum Shape {
            Circle(r: float)
            Rect(w: float, h: float)
            Point
        }

        fn area(s: Shape) -> float {
            match (s) {
                case Shape.Circle(r) if r > 0 -> r * r
                case Shape.Circle(r) -> -1.0
                case Shape.Rect(w, h) if w > 0 && h > 0 -> w * h
                case Shape.Rect(_, _) -> -3.0
                case Shape.Point -> 0.0
            }
        }

        fn risky(s: Shape) -> float {
            match (s) {
                case Shape.Circle(r) if r > 0 -> r * r
                case other if false -> -9.0
            }
        }

        fn main() {
            println(area(Shape.Circle(3.0)), risky(Shape.Point))
        }"#,
    );
    let syms = externs(&m);
    for need in [
        "pickle_enum_new",
        "pickle_enum_tag",
        "pickle_enum_field",
        "pickle_box_f64",
        "pickle_unbox_f64",
        "pickle_panic_no_match",
    ] {
        assert!(
            syms.iter().any(|s| s == need),
            "missing {need}, externs: {syms:?}"
        );
    }
    let area = m.funcs.iter().find(|f| f.name == "area").expect("area");
    let branches: Vec<String> = area
        .blocks
        .iter()
        .filter_map(|b| match &b.term {
            IrTerm::BranchIf { .. } => Some(format!("{:?}", b.term)),
            _ => None,
        })
        .collect();
    let checks = branches.len();
    assert!(
        checks >= 5,
        "3 tag checks + 2 guard branches (plus `&&` short-circuit) expected, \
         found {checks}, dump:\n{area}"
    );
    let pest = m.funcs.iter().find(|f| f.name == "risky").expect("risky");
    let panic_instrs = pest
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
            if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_panic_no_match"))
        })
        .count();
    assert_eq!(
        panic_instrs, 1,
        "a guarded catch-all that can reject keeps the nonexhaustive panic, \
         dump:\n{pest}"
    );
}

#[test]
fn emits_non_enum_match_and_if_let() {
    // Int/string/option scrutinees lower to equality/presence tests, `some(v)`
    // unboxes the payload, and `if (let some(v) = m)` branches on presence.
    let m = emit_str(
        r#"fn band(n: int) -> string {
            match (n) {
                case 0 -> "zero"
                case x if x > 9 -> "big"
                case _ -> "small"
            }
        }
        fn name_of(k: string) -> string {
            match (k) {
                case "up" -> "north"
                case _ -> "?"
            }
        }
        fn unwrap(m: int?) -> int {
            match (m) {
                case some(v) -> v
                case none -> 0
            }
        }
        fn pick(m: int?, f: int) -> int {
            if (let some(v) = m) { v } else { f }
        }
        fn main() {
            println(band(3), name_of("up"), unwrap(none), pick(1 as? int, 2))
        }"#,
    );
    let syms = externs(&m);
    for need in [
        "pickle_str_cmp",
        "pickle_panic_no_match",
        "pickle_unbox_i64",
        "pickle_box_i64",
    ] {
        assert!(
            syms.iter().any(|s| s == need),
            "missing {need}, externs: {syms:?}"
        );
    }
    let band = m.funcs.iter().find(|f| f.name == "band").expect("band");
    let eq_checks = band
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| matches!(i, IrInstr::BinOp { op: BinOp::Eq, .. }))
        .count();
    assert!(
        eq_checks >= 1,
        "int literal case needs an equality check:\n{band}"
    );
    let name_of = m
        .funcs
        .iter()
        .find(|f| f.name == "name_of")
        .expect("name_of");
    let cmp_calls = name_of
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
            if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_str_cmp"))
        })
        .count();
    assert_eq!(cmp_calls, 1, "one string case -> one str_cmp:\n{name_of}");
    let unwrap = m.funcs.iter().find(|f| f.name == "unwrap").expect("unwrap");
    let branches = unwrap
        .blocks
        .iter()
        .filter_map(|b| match &b.term {
            IrTerm::BranchIf { .. } => Some(()),
            _ => None,
        })
        .count();
    assert!(
        branches >= 1,
        "option some/none needs a presence branch:\n{unwrap}"
    );
    let pick = m.funcs.iter().find(|f| f.name == "pick").expect("pick");
    assert!(
        pick.blocks
            .iter()
            .any(|b| matches!(b.term, IrTerm::BranchIf { .. })),
        "if-let needs a presence branch:\n{pick}"
    );
}

#[test]
fn property_getter_reads_bare_field_and_this() {
    // Bare field names and bare property names inside accessors resolve
    // through `this`; the getter/setter bodies must lower without unset `this`.
    let m = emit_str(
        r#"class Meter {
            var value: int

            property squared: int {
                get => value * value
            }

            property plus: int {
                get => this.value + this.squared
            }

            init {
                this.value = this.value + 1
            }
        }

        fn main() {
            let x = Meter(3)
            println(x.squared)
            println(x.plus)
        }"#,
    );
    let get = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Meter_squared_get")
        .expect("get");
    let reads: Vec<&str> = get
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_obj_slot_get")
        .collect();
    assert!(
        reads.len() >= 2,
        "bare `value` reads dispatch through `this`, dump:\n{get}"
    );
}

#[test]
fn emits_char_fields_and_collections_boxed() {
    // `char` fields, `List<char>`, and `Map<string, char>` values lower
    // through `pickle_box_char`/`pickle_unbox_char`; the class no longer bails.
    let m = emit_str(
        r#"class Tile {
            var glyph: char = 'x'
            var tag: char
        }

        fn main() {
            let t = Tile('?')
            t.tag = '!'
            println(t.glyph)
            let cs: List<char> = ['a', t.tag]
            println(cs[1])
            let by: Map<string, char> = { "k": 'z' }
            println(by["k"])
        }"#,
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in ["pickle_box_char", "pickle_unbox_char"] {
        assert!(externs.contains(&want), "externs: {externs:?}");
    }
    // RHS-first form: `Tile('?')` records `glyph = 'x'` via a field init, and
    // `tag` becomes the ctor parameter.
    let new = m
        .funcs
        .iter()
        .find(|f| f.name == "Tile.new")
        .expect("Tile.new");
    let names: Vec<&str> = new.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["tag"],
        "`glyph` carries an initializer so it is skipped"
    );
    let tile = m.funcs.iter().find(|f| f.name == "Tile.new").expect("new");
    let stores: Vec<&str> = tile
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        stores.contains(&"pickle_box_char"),
        "new must box the char field init, stores: {stores:?}"
    );
    // The getter path unboxes: `t.glyph` + `cs[1]` + `by["k"]` all want the
    // char scalar back.
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let unboxes = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_unbox_char")
        .count();
    assert!(
        unboxes >= 3,
        "field/get/list/map reads unbox chars, dump:\n{main}"
    );
}

#[test]
fn emits_map_entries_iteration() {
    // `for ((k, v) in m)` lowers to key/value snapshots iterated in lockstep.
    let m = emit_str(
        r#"fn main() {
            var m = {"alpha": 10, "beta": 20}
            var cat = ""
            var sum = 0
            for ((k, v) in m) {
                cat += k
                sum += v
            }
            print(cat, sum)
        }"#,
    );
    let syms = externs(&m);
    for need in [
        "pickle_map_keys",
        "pickle_map_values",
        "pickle_list_get",
        "pickle_list_len",
        "pickle_unbox_i64",
    ] {
        assert!(
            syms.iter().any(|s| s == need),
            "missing {need}, externs: {syms:?}"
        );
    }
    assert!(
        !syms.iter().any(|s| s == "pickle_map_get_boxed"),
        "entries iteration should not need a per-trip map lookup, externs: {syms:?}"
    );
}

#[test]
fn emits_string_index_and_char_iteration() {
    // `s[i]` lowers to `pickle_str_get` (raw `char`, no unbox), and
    // `for (c in s)` iterates through `pickle_str_len` + `pickle_str_get`.
    let m = emit_str(
        r#"class Site {
            var name: string = "pickle"
            var first: char = 'P'
        }

        fn main() {
            let s = "abc"
            let c = s[1]
            println(s[0], s[len(s) - 1], c)
            var marks = 0
            for (ch in "xyz") {
                if (ch == 'y') {
                    marks = marks + 1
                }
            }
            println(marks)
        }"#,
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in ["pickle_str_get", "pickle_str_len"] {
        assert!(externs.contains(&want), "externs: {externs:?}");
    }
    let init = m.funcs.iter().find(|f| f.name == "main").expect("init");
    let get_calls = init
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_str_get")
        .count();
    assert!(
        get_calls >= 4,
        "three index reads + one loop-body read, dump:\n{init}"
    );
    let iter = init
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(id),
                ..
            } = i
            {
                m.externs.get(id.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .filter(|s| *s == "pickle_str_len")
        .count();
    assert!(
        iter >= 1,
        "the loop bound uses pickle_str_len, dump:\n{init}"
    );
}

#[test]
fn emits_option_lift_coalesce_and_unwrap() {
    // `T` initializers lift into `T?` slots by boxing, `none` is a null
    // constant, `??` unboxes/selects, and postfix `?` unwraps with a runtime
    // panic on `none`.
    let m = emit_str(
        r#"fn maybe(flag: bool) -> int? {
            if (flag) {
                return 5
            }
            return none
        }

        fn main() {
            let a: int? = 7
            let b: int? = none
            println(a ?? 0)
            println(b ?? 1)
            println(maybe(true)?)
        }"#,
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in [
        "pickle_box_i64",
        "pickle_unbox_i64",
        "pickle_panic_none_unwrap",
    ] {
        assert!(externs.contains(&want), "externs: {externs:?}");
    }
    let has_null = m.funcs.iter().any(|f| {
        f.blocks.iter().any(|b| {
            b.instrs.iter().any(|i| {
                matches!(
                    i,
                    IrInstr::Const {
                        c: IrConst::Null,
                        ..
                    }
                )
            })
        })
    });
    assert!(has_null, "`none` must lower to a null const, dump:\n{m}");
    let maybe = m.funcs.iter().find(|f| f.name == "maybe").expect("maybe");
    assert_eq!(maybe.ret.to_string(), "ptr", "options are pointers");
}

#[test]
fn emits_optional_access_with_null_test() {
    // `b?.value` null-tests the receiver, reads the field off the present
    // branch, and lifts the scalar member into an option.
    let m = emit_str(
        r#"class Box {
            var value: int
            constructor(value: int) {
                this.value = value
            }
        }

        fn main() {
            let b: Box? = Box(9)
            println(b?.value ?? -1)
        }"#,
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in ["pickle_box_i64", "pickle_obj_slot_get"] {
        assert!(externs.contains(&want), "externs: {externs:?}");
    }
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let nulls = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(
                i,
                IrInstr::Const {
                    c: IrConst::Null,
                    ..
                }
            )
        })
        .count();
    assert!(nulls >= 1, "`?.` none branch stores null, dump:\n{main}");
}

#[test]
fn emits_numeric_and_option_casts() {
    // `as` lowers int<->float via itof/ftoi; `as?` yields an option and
    // null-tests the source; `is` on an option is a presence test.
    let m = emit_str(
        r#"fn main() {
            let i = 7
            let f = i as float
            let g = 3.9 as int
            println(f, g)
            let x: int? = 5
            println(x is int)
            let y = x as? float
            println(y ?? 0.0)
        }"#,
    );
    let dump = format!("{m}");
    assert!(dump.contains("itof"), "int->float uses itof, dump:\n{dump}");
    assert!(dump.contains("ftoi"), "float->int uses ftoi, dump:\n{dump}");
    assert!(
        dump.contains("binop.ne"),
        "option `is` null-tests, dump:\n{dump}"
    );
    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in ["pickle_box_f64", "pickle_unbox_f64"] {
        assert!(
            externs.contains(&want),
            "`as? float` boxes/unboxes floats: {externs:?}"
        );
    }
}

#[test]
fn emits_static_field_cells_and_init() {
    // Static fields lower to runtime cells: every field is default-initialized
    // once by the synthesized `pkl_static_init` (declared value or zero/null),
    // `main` calls it after registration, and bare names inside static methods
    // read/write the same cell.
    let m = emit_str(
        r#"class Counter {
            static var total: int = 10
            static var name: string = "ctr"
            static var flag: bool

            static fn bump(by: int) {
                total = total + by
            }
        }

        fn main() {
            println(Counter.total)
            Counter.total = Counter.total + 5
            Counter.total += 7
            println(Counter.name)
            println(Counter.flag)
            Counter.bump(3)
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    assert!(
        syms.iter().any(|s| s == "pkl_static_init"),
        "symbols: {syms:?}"
    );

    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    for want in ["pickle_static_get", "pickle_static_set"] {
        assert!(
            externs.contains(&want),
            "static cell extern missing: {externs:?}"
        );
    }

    let init = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_static_init")
        .expect("static init fn");
    let init_sets = init
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| {
            matches!(i, IrInstr::Call { callee: Callee::Extern(ex), .. }
                if m.externs.get(ex.0).map(|e| e.symbol.as_str()) == Some("pickle_static_set"))
        })
        .count();
    assert!(
        init_sets >= 3,
        "init stores every static field (initialized + defaulted): {init_sets}"
    );

    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let calls_init = main.blocks.iter().flat_map(|b| b.instrs.iter()).any(|i| {
        matches!(i, IrInstr::Call { callee: Callee::Func(fid), .. }
            if m.funcs.get(fid.0).map(|f| f.symbol.as_str()) == Some("pkl_static_init"))
    });
    assert!(calls_init, "main calls pkl_static_init after registration");

    let bump = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Counter_sm_bump")
        .expect("static method");
    let bump_externs: Vec<&str> = bump
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| {
            if let IrInstr::Call {
                callee: Callee::Extern(ex),
                ..
            } = i
            {
                m.externs.get(ex.0).map(|e| e.symbol.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        bump_externs.contains(&"pickle_static_get"),
        "bare-name static read: {bump_externs:?}"
    );
    assert!(
        bump_externs.contains(&"pickle_static_set"),
        "bare-name static write: {bump_externs:?}"
    );
}

#[test]
fn emits_class_const_inlining() {
    // Class constants are compile-time values: reads of `Type.NAME` (and bare
    // names inside the class body) inline the initializer; no static cell or
    // init function is synthesized.
    let m = emit_str(
        r#"class Config {
            const BASE: int = 2
            const DOUBLE: int = BASE * 2
            const NAME: string = "cfg"

            fn limit() -> int {
                return DOUBLE + 1
            }
        }

        fn main() {
            println(Config.BASE)
            println(Config.NAME)
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    assert!(
        !syms.iter().any(|s| s == "pkl_static_init"),
        "constants must not synthesize a static initializer: {syms:?}"
    );

    let externs: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(
        !externs.contains(&"pickle_static_get") && !externs.contains(&"pickle_static_set"),
        "constants are inlined, not stored in cells: {externs:?}"
    );

    // `Config.BASE` in main inlines to the literal 2.
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let has_two = main.blocks.iter().flat_map(|b| b.instrs.iter()).any(|i| {
        matches!(
            i,
            IrInstr::Const {
                c: IrConst::Int(2),
                ..
            }
        )
    });
    assert!(has_two, "Config.BASE should inline as an int const");

    // A bare-name const read inside a method also inlines (`DOUBLE + 1`,
    // where DOUBLE is `BASE * 2`).
    let limit = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Config_limit")
        .expect("limit");
    let dump = format!("{limit}");
    assert!(
        dump.contains("binop.mul"),
        "DOUBLE inlines BASE * 2, dump:\n{dump}"
    );
    assert!(dump.contains("binop.add"), "limit adds 1, dump:\n{dump}");
}

#[test]
fn emits_named_constructor_redirect() {
    // A named constructor lowers to a static factory `pkl_<Name>_nc_<NAME>`
    // that evaluates the delegation arguments and calls the primary
    // constructor, returning its pointer. It does not get a cell of its own.
    let m = emit_str(
        r#"class Point {
            x: int

            constructor(x: int) {
                this.x = x
            }

            constructor.origin() {
                this(0)
            }

            constructor.diagonal(n: int) {
                this(n * 2)
            }
        }

        fn main() {
            let a = Point.origin()
            let b = Point.diagonal(3)
            println(a.x)
            println(b.x)
        }"#,
    );
    let syms: Vec<String> = m.funcs.iter().map(|f| f.symbol.clone()).collect();
    assert!(
        syms.contains(&"pkl_Point_new".to_string()),
        "symbols: {syms:?}"
    );
    assert!(
        syms.contains(&"pkl_Point_nc_origin".to_string()),
        "symbols: {syms:?}"
    );
    assert!(
        syms.contains(&"pkl_Point_nc_diagonal".to_string()),
        "symbols: {syms:?}"
    );

    let ctor_fid = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Point_new")
        .expect("primary ctor");
    let ctor_fid = pickle_compiler::ir::FuncId(ctor_fid);

    for sym in ["pkl_Point_nc_origin", "pkl_Point_nc_diagonal"] {
        let f = m.funcs.iter().find(|f| f.symbol == sym).expect(sym);
        assert_eq!(f.ret, IrTy::Ptr, "{sym} must return an instance pointer");
        let calls_ctor = f.blocks.iter().flat_map(|b| b.instrs.iter()).any(|i| {
            matches!(
                i,
                IrInstr::Call {
                    callee: Callee::Func(fid),
                    ..
                } if *fid == ctor_fid
            )
        });
        assert!(calls_ctor, "{sym} must delegate to the primary constructor");
    }

    // `main` calls the named constructors.
    let origin_fid = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Point_nc_origin")
        .expect("origin");
    let origin_fid = pickle_compiler::ir::FuncId(origin_fid);
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let main_calls = main.blocks.iter().flat_map(|b| b.instrs.iter()).any(
        |i| matches!(i, IrInstr::Call { callee: Callee::Func(fid), .. } if *fid == origin_fid),
    );
    assert!(main_calls, "main must call `pkl_Point_nc_origin`");
}

#[test]
fn emits_deinit_finalizer_and_registration() {
    // A `deinit` lowers to `pkl_<T>_deinit(this)` returning unit, and the class
    // descriptor carries its address (a `FuncAddr` const) as the finalizer.
    let m = emit_str(
        r#"class Widget {
            value: int

            deinit {
                println(42)
            }
        }

        fn main() {
            let w = Widget(7)
            println(w.value)
        }"#,
    );

    let fid = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Widget_deinit")
        .expect("missing pkl_Widget_deinit");
    let f = &m.funcs[fid];
    assert_eq!(f.ret, IrTy::Unit, "a finalizer returns unit");
    assert_eq!(f.params.len(), 1, "a finalizer takes only `this`");
    assert_eq!(f.params[0].name, "this");
    assert_eq!(f.params[0].ty, IrTy::Ptr);
    assert!(!f.is_main);

    // The registration extern now takes a sixth (parent id) argument.
    let reg = m
        .externs
        .iter()
        .find(|e| e.symbol == "pickle_class_register")
        .expect("pickle_class_register extern");
    assert_eq!(
        reg.params.len(),
        7,
        "register takes addr,len,fields,mask,owned,finalizer,parent"
    );
    assert!(reg.params.iter().all(|t| *t == IrTy::Int));
    assert_eq!(reg.ret, IrTy::Unit);

    // `main` passes the finalizer address to that fifth slot.
    let fid = pickle_compiler::ir::FuncId(fid);
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let args = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .find_map(|i| match i {
            IrInstr::Call {
                callee: Callee::Extern(id),
                args,
                ..
            } if m.externs.get(id.0).map(|e| e.symbol.as_str())
                == Some("pickle_class_register") =>
            {
                Some(args.clone())
            }
            _ => None,
        })
        .expect("class registration call");
    assert_eq!(args.len(), 7);
    let fin = args[5];
    let is_addr = main.blocks.iter().flat_map(|b| b.instrs.iter()).any(|i| {
        matches!(i, IrInstr::Const { dst, c: IrConst::FuncAddr(g) } if *dst == fin && *g == fid)
    });
    assert!(
        is_addr,
        "sixth registration arg must be `addrof pkl_Widget_deinit`"
    );
    assert!(format!("{m}").contains("addrof fn#"), "dump:\n{m}");
}

#[test]
fn class_without_deinit_registers_a_null_finalizer() {
    let m = emit_str(
        r#"class Plain {
            x: int
        }

        fn main() {
            let p = Plain(1)
            println(p.x)
        }"#,
    );
    assert!(
        !m.funcs.iter().any(|f| f.symbol.contains("_deinit")),
        "no finalizer function without a `deinit`"
    );
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let args = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .find_map(|i| match i {
            IrInstr::Call {
                callee: Callee::Extern(id),
                args,
                ..
            } if m.externs.get(id.0).map(|e| e.symbol.as_str())
                == Some("pickle_class_register") =>
            {
                Some(args.clone())
            }
            _ => None,
        })
        .expect("class registration call");
    assert_eq!(args.len(), 7);
    let fin = args[5];
    let is_null = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .any(|i| matches!(i, IrInstr::Const { dst, c: IrConst::Int(0) } if *dst == fin));
    assert!(is_null, "a class without `deinit` registers finalizer 0");
}

#[test]
fn emits_inheritance_layout_and_super_call() {
    let m = emit_str(
        r#"class Animal {
            name: string
            legs: int = 4

            fn label() -> string {
                return this.name
            }

            fn baseTag() -> string {
                return "animal"
            }
        }

        class Dog extends Animal {
            breed: string

            fn tag() -> string {
                return super.baseTag()
            }
        }

        fn main() {
            let d = Dog("Rex", "lab")
            println(d.legs)
            println(d.breed)
            println(d.tag())
        }"#,
    );

    // Both classes register; the superclass is registered first and the
    // subclass records the superclass id as the last argument.
    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let regs: Vec<Vec<pickle_compiler::ir::Temp>> = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter_map(|i| match i {
            IrInstr::Call {
                callee: Callee::Extern(id),
                args,
                ..
            } if m.externs.get(id.0).map(|e| e.symbol.as_str())
                == Some("pickle_class_register") =>
            {
                Some(args.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(regs.len(), 2, "one registration per class");
    for r in &regs {
        assert_eq!(
            r.len(),
            7,
            "register takes addr,len,fields,mask,owned,finalizer,parent"
        );
    }
    let const_int = |t: pickle_compiler::ir::Temp| -> Option<i64> {
        main.blocks
            .iter()
            .flat_map(|b| b.instrs.iter())
            .find_map(|i| match i {
                IrInstr::Const {
                    dst,
                    c: IrConst::Int(v),
                } if *dst == t => Some(*v),
                _ => None,
            })
    };
    let parents: Vec<i64> = regs.iter().filter_map(|r| const_int(r[6])).collect();
    // The root has parent 0; the subclass points at the superclass id (10,
    // the first user class after the builtin stream id 9).
    assert!(
        parents.contains(&0),
        "root class registers parent 0, got {parents:?}"
    );
    assert!(
        parents.contains(&10),
        "subclass registers superclass id 10, got {parents:?}"
    );

    // `super.baseTag()` lowers to a direct call of the superclass method
    // (static dispatch on the shared object).
    let base = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Animal_baseTag")
        .expect("pkl_Animal_baseTag");
    let tag = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Dog_tag")
        .expect("pkl_Dog_tag");
    let calls_parent = tag
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .any(|i| matches!(i, IrInstr::Call { callee: Callee::Func(fid), .. } if fid.0 == base));
    assert!(
        calls_parent,
        "`super.baseTag()` must call the superclass method:\n{m}"
    );
}

#[test]
fn emits_override_dispatch_cascade() {
    // `describe` is overridden down the hierarchy: a call through an
    // ancestor-typed receiver dispatches on the receiver's runtime class id
    // via `pickle_class_is`, deepest-derived first, with the receiver's own
    // implementation as the fallback. `super.describe()` always calls this
    // class's implementation directly, and a never-overridden method (`age`)
    // stays a plain direct call. A mid-hierarchy class that merely inherits
    // `describe` (WorkingDog) still dispatches, because its visible method
    // resolves to the nearest defining ancestor (Animal) while Poodle, deeper
    // down, overrides it.
    let m = emit_str(
        r#"class Animal {
            fn age() -> int {
                return 3
            }

            fn describe() -> string {
                return "animal"
            }
        }

        class Dog extends Animal {
            override fn describe() -> string {
                return "dog"
            }

            fn superDescribe() -> string {
                return super.describe()
            }
        }

        class WorkingDog extends Dog {}

        class Poodle extends WorkingDog {
            override fn describe() -> string {
                return "poodle"
            }
        }

        fn main() {
            let a: Animal = Animal()
            let d: Animal = Dog()
            let p: Animal = Poodle()
            let w: WorkingDog = WorkingDog()
            let wp: WorkingDog = Poodle()
            println(a.age())
            println(a.describe())
            println(d.describe())
            println(p.describe())
            println(w.describe())
            println(wp.describe())
        }"#,
    );

    let symbols: Vec<&str> = m.funcs.iter().map(|f| f.symbol.as_str()).collect();
    for sym in [
        "pkl_Animal_age",
        "pkl_Animal_describe",
        "pkl_Dog_superDescribe",
        "pkl_Dog_describe",
        "pkl_Poodle_describe",
    ] {
        assert!(symbols.contains(&sym), "symbols: {symbols:?}");
    }

    let main = m.funcs.iter().find(|f| f.name == "main").expect("main");
    let is_extern = |i: &IrInstr| -> bool {
        if let IrInstr::Call {
            callee: Callee::Extern(id),
            ..
        } = i
        {
            m.externs.get(id.0).map(|e| e.symbol.as_str()) == Some("pickle_class_is")
        } else {
            false
        }
    };

    // Four dispatch-eligible calls: `a` (Animal: Poodle, Dog = 2 checks),
    // `d` (Animal: 2 checks), `p` (Animal: 2 checks), `w`/`wp`
    // (WorkingDog: Poodle = 1 check each).
    let is_calls = main
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .filter(|i| is_extern(i))
        .count();
    assert_eq!(is_calls, 8, "five calls need 8 class-id checks:\n{m}");

    // The never-overridden `age` method is called directly, no branching.
    let age_pos = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Animal_age")
        .expect("pkl_Animal_age");
    let fast =
        main.blocks.iter().flat_map(|b| b.instrs.iter()).any(
            |i| matches!(i, IrInstr::Call { callee: Callee::Func(fid), .. } if fid.0 == age_pos),
        );
    assert!(fast, "non-overridden methods must stay direct calls:\n{m}");

    // `super.describe()` inside a method is always the superclass
    // implementation, never a dispatch cascade.
    let super_describe = m
        .funcs
        .iter()
        .find(|f| f.symbol == "pkl_Dog_superDescribe")
        .expect("pkl_Dog_superDescribe");
    let describe_pos = m
        .funcs
        .iter()
        .position(|f| f.symbol == "pkl_Animal_describe")
        .expect("pkl_Animal_describe");
    let direct_super = super_describe
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .any(|i| matches!(i, IrInstr::Call { callee: Callee::Func(fid), .. } if fid.0 == describe_pos));
    let no_is = !super_describe
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter())
        .any(is_extern);
    assert!(
        direct_super,
        "`super.describe()` must call the superclass impl:\n{m}"
    );
    assert!(no_is, "`super.describe()` must not dispatch:\n{m}");
}

#[test]
fn emits_class_is_and_as_lowering() {
    let m = emit_str(
        r#"class Animal {
            name: string
        }

        class Dog extends Animal {
            breed: string
        }

        fn main() {
            let d = Dog("Rex", "lab")
            let a: Animal = d
            if (a is Dog) {
                println("dog")
            }
            let back: Dog = a as Dog
            println(back.breed)
        }"#,
    );
    let syms: Vec<&str> = m.externs.iter().map(|e| e.symbol.as_str()).collect();
    assert!(
        syms.contains(&"pickle_class_is"),
        "downcast `is` needs the runtime test:\n{m}"
    );
    assert!(
        syms.contains(&"pickle_class_cast"),
        "downcast `as` needs the checked cast:\n{m}"
    );
}

#[test]
fn emits_manual_alloc_adopt_and_free() {
    let m = emit_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let w = Widget(1)
            println(w.value)
            w.free()
        }"#,
    );
    let syms = externs(&m);
    assert!(
        syms.iter().any(|s| s == "pickle_manual_adopt"),
        "manual let must adopt the allocation: {syms:?}"
    );
    assert!(
        syms.iter().any(|s| s == "pickle_manual_free"),
        "`.free()` must lower to the runtime release: {syms:?}"
    );
    // A managed binding must not adopt or free anything.
    let managed = emit_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            let w = Widget(1)
            println(w.value)
        }"#,
    );
    let msyms = externs(&managed);
    assert!(
        !msyms
            .iter()
            .any(|s| s == "pickle_manual_adopt" || s == "pickle_manual_free"),
        "managed bindings must stay GC-owned: {msyms:?}"
    );
}

#[test]
fn emits_scalar_pointer_address_load_and_store() {
    let m = emit_str(
        r#"fn bump(p: *int) {
            unsafe {
                (*p) = (*p) + 1
            }
        }

        fn main() {
            var n = 10
            unsafe {
                let p: *int = &n
                (*p) = (*p) + 5
                bump(p)
            }
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        dump.contains("addr slot"),
        "a scalar `&local` must take the local's stack address:\n{dump}"
    );
    assert!(
        dump.contains("loadraw.int64"),
        "`*p` must lower to a raw load:\n{dump}"
    );
    assert!(
        dump.contains("storeraw.int64"),
        "`(*p) = v` must lower to a raw store:\n{dump}"
    );
}

#[test]
fn emits_raw_buffer_address_load_store() {
    let m = emit_str(
        r#"fn main() {
            unsafe {
                var buf: *int = alloc(int, 8)
                buf[0] = 10
                buf[2] += 5
                println(buf[0] + buf[2])
                free(buf)
            }
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        dump.contains("storeraw.int64") && dump.contains("loadraw.int64"),
        "`buf[i] = v` / `buf[i]` must lower to raw int64 stores and loads:\n{dump}"
    );
    assert!(
        dump.contains("binop.mul"),
        "element addressing must scale the index by the element stride:\n{dump}"
    );
    assert!(
        dump.contains("binop.add"),
        "element addressing must add `base + i * stride`:\n{dump}"
    );
}

#[test]
fn emits_raw_buffer_float_and_char_strides() {
    let m = emit_str(
        r#"fn main() {
            unsafe {
                var f: *float = alloc(float, 4)
                f[1] = 2.5
                println(f[1])
                free(f)

                var c: *char = alloc(char, 2)
                c[0] = 'A'
                println(c[0])
                free(c)
            }
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        dump.contains("storeraw.float64") && dump.contains("loadraw.float64"),
        "`float` buffers must use float64 raw stores and loads:\n{dump}"
    );
    assert!(
        dump.contains("storeraw.char") && dump.contains("loadraw.char"),
        "`char` buffers must use char raw stores and loads:\n{dump}"
    );
}

#[test]
fn emits_scalar_borrow_param_by_address() {
    let m = emit_str(
        r#"fn bump(total: &int) -> int {
            return *total + total[0]
        }

        fn main() {
            var t = 4
            println(bump(t))
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        dump.contains("= addr slot s"),
        "implicit `&T` borrow of a scalar local must pass its address (`LocalAddr`), got:\n{dump}"
    );
    assert!(
        m.funcs
            .iter()
            .any(|f| f.name == "bump" && dump.contains("loadraw.int64")),
        "`*p` / `p[0]` on a `&int` must lower to a raw int load, got:\n{dump}"
    );
}

#[test]
fn emits_managed_borrow_by_identity() {
    let m = emit_str(
        r#"class Point {
            x: int
            y: int

            constructor(x: int, y: int) {
                this.x = x
                this.y = y
            }
        }

        fn origin(p: &Point) -> int {
            return p.x + p.y
        }

        fn main() {
            let pt = Point(2, 3)
            println(origin(pt))
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        !dump.contains("= addr slot s"),
        "implicit `&T` borrow of a managed value must pass it by identity, got:\n{dump}"
    );
    let origin_id = m.funcs.iter().position(|f| f.name == "origin").unwrap();
    let main = m.funcs.iter().find(|f| f.name == "main").unwrap();
    assert!(
        main.blocks.iter().any(|b| b.instrs.iter().any(|i| matches!(
            i,
            IrInstr::Call {
                callee: Callee::Func(fid),
                ..
            } if fid.0 == origin_id
        ))),
        "main must call `origin` directly, got:\n{dump}"
    );
}

#[test]
fn emits_borrowed_lists_deref_for_iteration() {
    let m = emit_str(
        r#"fn total(q: &List<int>) -> int {
            var n = 0
            for (v in q) {
                n += v
            }
            n += q[0]
            return n
        }

        fn main() {
            let l = [1, 2, 3]
            println(total(l))
        }"#,
    );
    let dump = format!("{m}");
    assert!(
        m.funcs.iter().any(|f| f.name == "total"),
        "expected `total` to be emitted:\n{dump}"
    );
    assert!(
        dump.contains("load slot s"),
        "managed `&T` borrows keep the referent in a slot, got:\n{dump}"
    );
}

#[test]
fn emits_zero_capture_lambda_and_fn_value_trampoline() {
    let m = emit_str(
        r#"fn add(a: int, b: int) -> int {
            a + b
        }

        fn main() {
            let id = add
            let same = (x: int) => x
            println(same(21))
            println(id(40, 2))
        }"#,
    );
    let dump = format!("{m}");
    // Zero-capture lambda -- one hoisted body whose parameters are the closure
    // object plus the lambda's own argument (nothing else).
    let same = m
        .funcs
        .iter()
        .find(|f| f.symbol.starts_with("pkl_closure_"))
        .expect("zero-capture lambda hoist");
    assert_eq!(same.params.len(), 2, "env + x only, got:\n{dump}");
    assert_eq!(
        same.params[0].ty,
        IrTy::Ptr,
        "first param is the closure:\n{dump}"
    );
    // A module function used as a value gets a forwarder trampoline so its ABI
    // matches a hoisted lambda body (closure first, then the real args).
    let tramp = m
        .funcs
        .iter()
        .find(|f| f.symbol.starts_with("pkl_tramp_"))
        .expect("fn-value trampoline");
    assert_eq!(tramp.params.len(), 3, "env + a + b, got:\n{dump}");
    let add_fid = add_id(&m);
    assert!(
        tramp
            .blocks
            .iter()
            .any(|b| b.instrs.iter().any(|i| matches!(
                i,
                IrInstr::Call { callee: Callee::Func(f), .. } if f.0 == add_fid
            ))),
        "trampoline must call `add`:\n{dump}"
    );
    // Dynamic dispatch for the lambda and the fn value.
    let main = m.funcs.iter().find(|f| f.is_main).expect("main");
    assert!(
        main.blocks.iter().any(|b| b
            .instrs
            .iter()
            .any(|i| matches!(i, IrInstr::CallInd { .. }))),
        "main must dispatch closures dynamically:\n{dump}"
    );
    assert!(
        m.externs.iter().any(|e| e.symbol == "pickle_class_new"),
        "closure objects are allocated via the runtime:\n{}",
        dump
    );
}

#[test]
fn emits_generic_body_lambda_per_instantiation() {
    // A lambda declared inside a generic function body has no module-scope
    // hoist: each instantiation that lowers it emits its own `pkl_closure_*`
    // copy, with that instantiation substituted into the signature. This is
    // what lets `make_identity<int>()` and `make_identity<float>()` return
    // distinct closure types.
    let m = emit_str(
        r#"fn make_identity<T>() -> fn (T) -> T {
            (x: T) => x
        }

        fn main() {
            let d = make_identity<int>()
            println(d(5))
            let f = make_identity<float>()
            println(f(1.5))
        }"#,
    );
    let dump = format!("{m}");
    let closures: Vec<usize> = m
        .funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| f.symbol.starts_with("pkl_closure_"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        closures.len(),
        2,
        "one hoisted copy per instantiation, got:\n{dump}"
    );
    let int_copy = m.funcs.iter().any(|f| {
        f.symbol.starts_with("pkl_closure_") && f.params.get(1).is_some_and(|p| p.ty == IrTy::Int)
    });
    let float_copy = m.funcs.iter().any(|f| {
        f.symbol.starts_with("pkl_closure_") && f.params.get(1).is_some_and(|p| p.ty == IrTy::Float)
    });
    assert!(int_copy, "an int-typed closure copy must exist:\n{dump}");
    assert!(float_copy, "a float-typed closure copy must exist:\n{dump}");
    // Both enclosing instantiations lower (and are called), so no bare generic
    // declaration body is emitted.
    assert!(
        m.funcs.iter().any(|f| f.symbol == "pkl_make_identity__int"),
        "missing int instantiation:\n{dump}"
    );
    assert!(
        m.funcs
            .iter()
            .any(|f| f.symbol == "pkl_make_identity__float"),
        "missing float instantiation:\n{dump}"
    );
    assert!(
        !m.funcs.iter().any(|f| f.symbol == "pkl_make_identity"),
        "bare generic body must not lower:\n{dump}"
    );
}

#[test]
fn emits_capturing_lambda_closure() {
    let m = emit_str(
        r#"fn make(base: int) -> fn (int) -> int {
            return (x: int) => x + base
        }

        fn main() {
            let f = make(7)
            println(f(1))
        }"#,
    );
    let dump = format!("{m}");
    // env at slot 0, the lambda's own parameter next (it is delivered by
    // argument order), and the capture read into a slot after every parameter.
    let body = m
        .funcs
        .iter()
        .find(|f| f.symbol.starts_with("pkl_closure_"))
        .expect("capturing closure hoist");
    assert_eq!(body.slots.len(), 3, "env + x + capture:\n{dump}");
    assert!(
        body.blocks.iter().any(|b| b.instrs.iter().any(|i| matches!(
            i,
            IrInstr::Call { callee: Callee::Extern(e), args, .. }
                if m.externs[e.0].symbol == "pickle_obj_slot_get" && args.len() == 2
        ))),
        "hoist must read the capture out of the closure object:\n{dump}"
    );
    // The factory stores the boxed capture into slot 1 of the closure object.
    let make = m.funcs.iter().find(|f| f.name == "make").expect("make");
    assert!(
        make.blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(
            i,
            IrInstr::Call { callee: Callee::Extern(e), args, .. }
                if m.externs[e.0].symbol == "pickle_obj_slot_set" && args.len() == 3
        )),
        "the factory must store the capture into the closure object:\n{dump}"
    );
}

#[test]
fn emits_fs_builtins() {
    let m = emit_str(
        r#"fn main() {
            let path = "tests/pickle/x.txt"
            write_file(path, "hello")
            let r = read_file(path)
            if (let some(t) = r) {
                println(t)
            }
            println(file_exists(path))
            println(delete(path))
            println(mkdir(path))
            let es = list_dir("tests/pickle")
            if (let some(_l) = es) {
                println(1)
            }
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    for want in [
        "pickle_write_file",
        "pickle_read_file",
        "pickle_file_exists",
        "pickle_delete",
        "pickle_mkdir",
        "pickle_list_dir",
    ] {
        assert!(symbols.contains(&want.to_string()), "symbols: {symbols:?}");
    }
}

#[test]
fn emits_list_mutation_builtins() {
    let m = emit_str(
        r#"fn main() {
            let xi = [3, 1, 2]
            xi.sort()
            let v = xi.remove(0)
            xi.insert(1, v)
            xi.push(9)
            println(xi[0], len(xi))
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    for want in [
        "pickle_list_sort",
        "pickle_list_remove",
        "pickle_list_insert",
        "pickle_list_push",
    ] {
        assert!(symbols.contains(&want.to_string()), "symbols: {symbols:?}");
    }
}

#[test]
fn emits_bytes_str_bridge_builtins() {
    let m = emit_str(
        r#"fn main() {
            let b = bytes("hi")
            let s = str(b)
            println(s)
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    for want in ["pickle_str_to_bytes", "pickle_str_from_list"] {
        assert!(symbols.contains(&want.to_string()), "symbols: {symbols:?}");
    }
}

#[test]
fn emits_os_builtins() {
    let m = emit_str(
        r#"fn main() {
            let a = args()
            println(len(a))
            exit(0)
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    for want in ["pickle_args", "pickle_exit"] {
        assert!(symbols.contains(&want.to_string()), "symbols: {symbols:?}");
    }
}

#[test]
fn emits_map_remove_builtin() {
    let m = emit_str(
        r#"fn main() {
            let m = {"a": 1}
            let v = m.remove("a") ?? -1
            println(v, m.has("a"))
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    assert!(
        symbols.contains(&"pickle_map_remove".to_string()),
        "symbols: {symbols:?}"
    );
}

#[test]
fn emits_map_get_builtin() {
    let m = emit_str(
        r#"fn main() {
            let m = {"a": 1}
            let v = m.get("a") ?? -1
            println(v)
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    assert!(
        symbols.contains(&"pickle_map_get".to_string()),
        "symbols: {symbols:?}"
    );
}

#[test]
fn emits_str_index_assign_builtins() {
    let m = emit_str(
        r#"fn main() {
            let s = "abc"
            s[0] = 'H'
            s[1] = 65
            s[2] += 1
            println(s)
        }"#,
    );
    let symbols: Vec<String> = m.externs.iter().map(|e| e.symbol.clone()).collect();
    for want in ["pickle_str_set", "pickle_str_get"] {
        assert!(symbols.contains(&want.to_string()), "symbols: {symbols:?}");
    }
}

#[test]
fn rejects_nested_string_index_assign() {
    let mut map = SourceMap::default();
    let diags = DiagnosticSink::new();
    let out = frontend(
        "test.pkl",
        r#"fn main() {
            let xs = ["abc"]
            xs[0][1] = 'x'
        }"#,
        &mut map,
        &diags,
    )
    .expect("frontend failed");
    assert!(
        emit_ir(&out.program, &out.resolved, &diags).is_none(),
        "expected the emitter to refuse a nested string index-assignment target"
    );
}
