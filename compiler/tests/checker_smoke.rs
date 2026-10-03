use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::front::frontend_checked;

fn check_str(src: &str) -> DiagnosticSink {
    let diags = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let _ = frontend_checked("test.pk", src, &mut map, &diags);
    diags
}

fn has_errors(diags: &DiagnosticSink) -> bool {
    diags.any_error()
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

#[test]
fn accepts_well_typed_module() {
    let d = check_str(
        r#"fn double(x: int) -> int {
            return x * 2
        }

        fn main() {
            let a = double(21)
            let b: int = a + 1
            print("b is {b}")
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_mismatched_return() {
    let d = check_str(
        r#"fn bad() -> int {
            return "hello"
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a type mismatch error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_unknown_name() {
    let d = check_str(
        r#"fn main() {
            print(nope)
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an undeclared-name error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_class_methods_and_fields() {
    let d = check_str(
        r#"class Counter {
            var count: int = 0

            constructor(start: int) {
                count = start
            }

            fn increment(by: int) {
                count = count + by
            }

            fn value() -> int {
                return count
            }
        }

        fn main() {
            let c = Counter(1)
            c.increment(2)
            print(c.value())
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_assign_to_immutable() {
    let d = check_str(
        r#"fn main() {
            let x = 1
            x = 2
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an immutable-assignment error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_option_flows() {
    let d = check_str(
        r#"fn maybe() -> int? {
            return none
        }

        fn main() {
            let m: int? = maybe()
            let v = m ?? 0
            print(v)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn coalesce_yields_inner_type() {
    // `a ?? b` is the unwrapped inner type, so its result assigns to `int`
    // (not `int?`).
    let d = check_str(
        r#"fn maybe() -> int? {
            return none
        }

        fn main() {
            let m: int? = maybe()
            let v: int = m ?? 0
            print(v)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_mismatched_list_elements() {
    let d = check_str(
        r#"fn main() {
            let xs = [1, 2, "three"]
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a list-element type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_generic_function() {
    let d = check_str(
        r#"fn id<T>(x: T) -> T {
            return x
        }

        fn main() {
            let a = id(3)
            let b = id("hi")
            print(a, b)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_uninferrable_generic_call() {
    let d = check_str(
        r#"fn idn<T>(x: T) -> T {
            return x
        }

        fn main() {
            let a = idn()
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for an uninferrable generic call"
    );
    assert!(
        error_msgs(&d).contains("cannot infer the type argument `T` for `idn`"),
        "wrong diagnostics:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_generic_function_explicit_value() {
    // A generic function used as a value with explicit type arguments types as
    // that concrete instantiation's fn signature, so the value is callable and
    // passes as an argument to a plain fn-typed parameter.
    let d = check_str(
        r#"fn id<T>(x: T) -> T {
            return x
        }

        fn apply(f: fn (int) -> int, x: int) -> int {
            return f(x)
        }

        fn main() {
            let f = id<int>
            print(str(f(3)))
            print(str(apply(id<int>, 4)))
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_generic_function_inferred_arg_value() {
    // A bare generic function passed as an argument to a fn-typed parameter
    // whose type pins its type arguments is accepted; the emitter materializes
    // that instantiation at the argument site.
    let d = check_str(
        r#"fn id<T>(x: T) -> T {
            return x
        }

        fn apply_twice<T>(f: fn (T) -> T, x: T) -> T {
            return f(f(x))
        }

        fn apply(f: fn (int) -> int, x: int) -> int {
            return f(x)
        }

        fn main() {
            print(str(apply(id, 3)))
            print(str(apply_twice(id, 3)))
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_unpinned_generic_function_value() {
    // `let f = id` cannot be lowered: no type arguments, no expected fn type to
    // pin `T`. The generic signature leaks `Var(T)` and the checker flags its
    // use rather than silently lowering a half-open value.
    let d = check_str(
        r#"fn id<T>(x: T) -> T {
            return x
        }

        fn main() {
            let f = id
            let y = f(3)
            print(str(y + 1))
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for an unpinned generic fn value"
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("requires numeric or string operands"),
        "wrong diagnostics:\n{msgs}"
    );
}

#[test]
fn rejects_generic_function_value_arity() {
    let d = check_str(
        r#"fn id<T>(x: T) -> T {
            return x
        }

        fn main() {
            let f = id<int, string>
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an arity error for the generic fn value"
    );
    assert!(
        error_msgs(&d).contains("takes 1 type argument(s), found 2"),
        "wrong diagnostics:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_enum_match() {
    let d = check_str(
        r#"enum Color {
            Red
            Blue
            Rgb(r: int, g: int, b: int)
        }

        fn main() {
            let c = Color.Rgb(1, 2, 3)
            match (c) {
                case Color.Red -> print("red")
                case Color.Blue -> print("blue")
                case Color.Rgb(r, g, b) -> print(r, g, b)
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_enum_match_as_value() {
    let d = check_str(
        r#"enum Shape {
            Circle(r: float)
            Rect(w: float, h: float)
        }

        fn area(s: Shape) -> float {
            match (s) {
                case Shape.Circle(r) -> 3.14 * r * r
                case Shape.Rect(w, h) -> w * h
            }
        }

        fn main() {
            let s = Shape.Circle(1.0)
            println(area(s))
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_inconsistent_match_arms() {
    let d = check_str(
        r#"enum E { A(x: int) B }
        fn pick(v: int) -> E {
            if (v > 0) { E.A(1) } else { E.B }
        }
        fn main() {
            let e = pick(1)
            match (e) {
                case E.A(x) -> x
                case E.B -> "text"
            }
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected inconsistent match arm type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_non_enum_match_and_if_let() {
    let d = check_str(
        r#"fn band(n: int) -> string {
            match (n) {
                case x if x < 0 -> "neg"
                case 0 -> "zero"
                case x if x > 9 -> "big"
                case _ -> "small"
            }
        }
        fn greet(k: string) -> string {
            match (k) {
                case "hi" -> "hello"
                case _ -> "?"
            }
        }
        fn opt(m: int?) -> int {
            match (m) {
                case some(v) -> v
                case none -> 0
            }
        }
        fn pick(m: int?, f: int) -> int {
            if (let some(v) = m) { v } else { f }
        }
        fn main() {
            println(band(3), greet("hi"), opt(1 as? int), pick(none, 2))
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_mismatched_literal_match_pattern() {
    let d = check_str(
        r#"fn bad(v: int) -> string {
            match (v) {
                case true -> "bool"
                case _ -> "int"
            }
        }
        fn main() { println(bad(1)) }"#,
    );
    assert!(
        has_errors(&d),
        "expected a literal/scrutinee type mismatch error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_interpolated_string_match_pattern() {
    let d = check_str(
        r#"fn bad(k: string) -> string {
            let x = "a"
            match (k) {
                case "{x}" -> "interpolated"
                case _ -> "?"
            }
        }
        fn main() { println(bad("a")) }"#,
    );
    assert!(
        has_errors(&d),
        "expected an interpolation-in-pattern error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_lambda_higher_order() {
    let d = check_str(
        r#"fn apply(f: fn (int) -> int, x: int) -> int {
            return f(x)
        }

        fn main() {
            let double = (n) => n * 2
            let r = apply(double, 21)
            print(r)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_bad_lambda_call() {
    let d = check_str(
        r#"fn main() {
            let f = (n: int) => n + 1
            let s = f("hi")
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a lambda argument type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_capturing_lambda() {
    let d = check_str(
        r#"fn counter() -> fn (int) -> int {
            let base = 3
            return (n) => n + base
        }

        fn main() {
            let f = counter()
            let r = f(4)
            print(r)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_is_as_casts() {
    let d = check_str(
        r#"class Shape {
            fn area() -> int {
                return 0
            }
        }

        class Circle extends Shape {
            var radius: int

            constructor(r: int) {
                radius = r
            }
        }

        fn main() {
            var s: Shape = Circle(5)
            let flag = s is Circle
            let c = s as Circle
            let maybe = s as? Circle
            let r = c.radius
            print(flag, r, maybe)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn as_question_yields_option() {
    // `as?` produces an option, so its result assigns to `T?`; plain `as` and
    // numeric conversions assign to the target/number type.
    let d = check_str(
        r#"fn main() {
            let x: int = 5
            let y: int? = x as? int
            let z: int = 3.9 as int
            let f: float = x as float
            let b: bool = x is int
            print(y, z, f, b)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_invalid_is_cast() {
    let d = check_str(
        r#"fn main() {
            let x: int = 42
            let flag = x is string
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a cast error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_control_flow_while_for_if() {
    let d = check_str(
        r#"fn main() {
            var sum = 0
            for (i in [1, 2, 3, 4, 5]) {
                if (i > 3) {
                    sum = sum + i
                }
            }
            var k = 0
            while (k < 3) {
                k = k + 1
                if (k == 2) {
                    continue
                }
            }
            print(sum)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_string_concatenation() {
    // `"a" + "b"` must be typed `string`, not `int`.
    let d = check_str(
        r#"fn greet(name: string) -> string {
            "hello " + name
        }

        fn main() {
            let s: string = "a" + "b"
            print(s)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_operators_inference() {
    let d = check_str(
        r#"fn main() {
            let a = 1 + 2 * 3
            let b = 10 / 4
            let c = 7 % 3
            let d = -a
            let e = a < b
            let f = !e
            let g = a == 7
            let h = "x" + "y"
            let neg = -c
            print(a, b, c, d, e, f, g, h, neg)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_op_type_mismatch() {
    let d = check_str(
        r#"fn main() {
            let x = 1 + "two"
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a binary-op type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_bool_compared_with_int() {
    // A chained comparison `x < 3 > x / 2` is `(x < 3) > (x / 2)` — a `bool`
    // compared with an `int`. This used to sail through the checker and crash
    // the JIT verifier on a mismatched-width `icmp` (i8 vs i64).
    let d = check_str(
        r#"fn f(x: int) -> bool {
            if (x < 3 > x / 2) { true } else { false }
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for `bool` compared with `int`, got:\n{}",
        error_msgs(&d)
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("requires comparable operands, found `bool` and `int`"),
        "wrong diagnostics:\n{msgs}"
    );
}

#[test]
fn rejects_char_compared_with_int() {
    let d = check_str(
        r#"fn main() {
            let b = 'a' < 65
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for `char` compared with `int`, got:\n{}",
        error_msgs(&d)
    );
    assert!(
        error_msgs(&d).contains("requires comparable operands, found `char` and `int`"),
        "wrong diagnostics:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_string_ordering() {
    // String `<`/`>` is not lowered (only `==`/`!=` via `pickle_str_cmp`).
    let d = check_str(
        r#"fn main() {
            let b = "a" < "b"
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for string ordering, got:\n{}",
        error_msgs(&d)
    );
    assert!(
        error_msgs(&d).contains("operator `Lt` is not supported for `string` operands"),
        "wrong diagnostics:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_bool_ordering() {
    let d = check_str(
        r#"fn main() {
            let b = false > true
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an error for bool ordering, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_valid_comparisons() {
    // Numbers (mixed int/float too), chars, string/bool equality, and
    // enum `==`/`!=` (match guards) all stay valid.
    let d = check_str(
        r#"enum Color {
            Red
            Blue
        }

        fn main() {
            let a = 1 < 2
            let b = 2.5 > 2
            let c = 'a' < 'z'
            let d = "x" == "x"
            let e = true != false
            let f = Color.Red != Color.Blue
            print(a, b, c, d, e, f)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_option_type_mix() {
    let d = check_str(
        r#"fn main() {
            let m: int? = none
            let n: int = m
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected an option-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_map_typing() {
    let d = check_str(
        r#"fn total(m: Map<string, int>) -> int {
            let a = m["x"]
            let ks = m.keys()
            let vs = m.values()
            return a + len(ks) + len(vs)
        }

        fn main() {
            var m = {"alpha": 10, "beta": 20}
            m["beta"] = m["beta"] + m["missing"]
            let hasit: bool = m.has("alpha")
            print(hasit, total(m))
            for (x in m) {
                print(x)
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_unhashable_map_keys() {
    // Strings, scalars, and composite objects (lists, maps, classes/structs,
    // enums, options, tuples) are legal keys; function values are not a
    // runtime object so stay rejected.
    let d = check_str(
        r#"fn main() {
            var f = (n: int) => n + 1
            var m = {f: "one"}
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a map-key type error, got:\n{}",
        error_msgs(&d)
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("map keys must be"),
        "expected a map-key diagnostics, got:\n{msgs}"
    );
}

#[test]
fn accepts_non_string_map_keys() {
    let d = check_str(
        r#"fn main() {
            var m = {1: "one", 2: "two"}
            var f = {1.5: true}
            var b = {true: "y", false: "n"}
            var c = {'a': 1, 'z': 2}
            var g = {"s": 1}
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_composite_map_keys() {
    let d = check_str(
        r#"class Pt {
            x: int
            y: int
        }
        enum Shape {
            Circle
            Square
        }
        fn main() {
            var li = {[1, 2]: "pair"}
            let a = li[[1, 2]]
            li[[3, 4]] = "other"
            var mm = {{"k": 1}: "nested"}
            let p = Pt(5, 6)
            let p2 = Pt(5, 6)
            var byobj = {p: 10}
            let q = byobj[p2]
            var es = {Shape.Circle: "c"}
            let r = es[Shape.Circle]
            var ixs = {[10, 20]: [1, 2, 3]}
            for ((k, v) in ixs) {
                let s = len(k)
                let t = len(v)
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_function_value_map_keys() {
    let d = check_str(
        r#"fn main() {
            var f = (n: int) => n
            var m = {f: 1}
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a map-key type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_non_string_map_index() {
    let d = check_str(
        r#"fn main() {
            var m = {"a": 1}
            let x = m[42]
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a map-index type error, got:\n{}",
        error_msgs(&d)
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("string"),
        "expected a `string` key diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_int_map_index() {
    let d = check_str(
        r#"fn main() {
            var m = {10: "ten"}
            let x = m[10]
            m[11] = "eleven"
            let y = m.has(10)
            let z = m.get(10)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_static_property_accessors() {
    // Static properties type-qualify through the class name; getters may
    // reference other statics by `Type.prop`; instance properties must not
    // be reached through the type name (or statics through instances).
    let d = check_str(
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
            println(Config.label)
            Config.guarded = 11
            println(Config.limit)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_static_instance_misuse() {
    // Static properties cannot be reached through an instance, and instance
    // properties cannot be reached through the type name.
    let d = check_str(
        r#"class Config {
            static property limit: int {
                get => 10
            }

            property doubled: int { get => limit * 2 }
        }

        fn main() {
            let c = Config()
            println(c.limit)
            println(Config.doubled)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("static property `limit` must be accessed on the type `Config`"),
        "missing instance-to-static diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("instance property `doubled` must be accessed on an instance of `Config`"),
        "missing type-to-instance diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_non_bool_match_guard() {
    // A guard must be a bool expression; an `int` guard types through.
    let d = check_str(
        r#"enum Color {
            Red
            Blue
            Rgb(r: int, g: int, b: int)
        }

        fn pick(c: Color) -> int {
            match (c) {
                case Color.Blue if 3 -> 1
                case other -> 0
            }
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a guard typing error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_guarded_match_arms() {
    // Payload bindings are live in guards, so `if r > 0` sees each binder.
    let d = check_str(
        r#"enum Color {
            Red
            Blue
            Rgb(r: int, g: int, b: int)
        }

        fn pick(c: Color) -> int {
            match (c) {
                case Color.Rgb(r, g, b) if r > 0 && g > 0 && b > 0 -> r + g + b
                case Color.Rgb(_, _, _) -> -1
                case Color.Blue if c != Color.Red -> 2
                case other if true -> 3
            }
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_too_few_constructor_args() {
    // A missing synthesized-ctor parameter used to reach codegen and turn into
    // a JIT verifier crash; the checker must flag the arity mismatch instead.
    let d = check_str(
        r#"class Pair {
            var a: int
            var b: int
        }

        fn main() {
            let p = Pair(1)
            println(p.a)
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a constructor arity error, got:\n{}",
        error_msgs(&d)
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("2 argument(s), found 1"),
        "expected the arity diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_static_field_access() {
    // Static fields type-qualify through the class name (read, write, compound)
    // and are reachable by bare name inside the class's own static methods.
    let d = check_str(
        r#"class Counter {
            static var total: int = 10
            static var flag: bool

            static fn bump(by: int) -> int {
                total = total + by
                return total
            }
        }

        fn main() {
            println(Counter.total)
            Counter.total = Counter.total + 5
            Counter.total += 7
            println(Counter.flag)
            println(Counter.bump(3))
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_static_field_instance_misuse() {
    // Static fields cannot be reached through an instance, and instance fields
    // cannot be reached through the type name (reads or writes).
    let d = check_str(
        r#"class Counter {
            static var total: int = 10
            var local: int = 1
        }

        fn main() {
            let c = Counter()
            println(c.total)
            c.total = 3
            println(Counter.local)
            Counter.local = 4
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains(
            "static field `total` must be accessed on the type `Counter`, not on an instance"
        ),
        "missing instance-to-static read diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains(
            "static field `total` must be assigned on the type `Counter`, not on an instance"
        ),
        "missing instance-to-static write diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("instance field `local` must be accessed on an instance of `Counter`"),
        "missing type-to-instance read diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("instance field `local` must be assigned on an instance of `Counter`"),
        "missing type-to-instance write diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_class_const_access() {
    // Class constants type-qualify through the class name and are reachable by
    // bare name inside the class body (static and instance methods); one
    // constant may reference another.
    let d = check_str(
        r#"class Config {
            const BASE: int = 2
            const DOUBLE: int = BASE * 2
            const NAME: string = "cfg"

            static fn label() -> string {
                return NAME
            }

            fn limit() -> int {
                return DOUBLE + 1
            }
        }

        fn main() {
            println(Config.BASE)
            println(Config.DOUBLE)
            println(Config.NAME)
            println(Config.label())
            let c = Config()
            println(c.limit())
            let x: int = Config.BASE * 10
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_const_misuse() {
    // Constants are immutable and static: assigning one, reading one through an
    // instance, and a value of the wrong type must all be rejected.
    let d = check_str(
        r#"class Config {
            const BASE: int = 2
            const BAD: int = "nope"
        }

        fn main() {
            Config.BASE = 5
            let c = Config()
            println(c.BASE)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("cannot assign to immutable static field `BASE`"),
        "missing const-assignment diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains(
            "static field `BASE` must be accessed on the type `Config`, not on an instance"
        ),
        "missing instance-to-const read diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("const initializer"),
        "missing const initializer type diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_named_constructor() {
    // A named constructor is a static factory whose body is a single
    // `this(...)` delegation to the primary constructor, including when the
    // primary constructor is synthesized from the fields.
    let d = check_str(
        r#"class Point {
            x: int
            y: int = 10

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

        class Box {
            w: int

            constructor.square(s: int) {
                this(s)
            }
        }

        fn main() {
            let a = Point.origin()
            let b = Point.diagonal(3)
            let c = Point(7)
            let d = Box.square(5)
            println(a.x)
            println(a.y)
            println(b.x)
            println(c.x)
            println(d.w)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_named_constructor_misuse() {
    // The body must be exactly one `this(...)` delegation, its arguments must
    // match the primary constructor, names must be unique, and `this(...)` is
    // not valid outside a named constructor.
    let d = check_str(
        r#"class A {
            x: int
            constructor(x: int) { this.x = x }
            constructor.extra() {
                this(1)
                println("side")
            }
            constructor.wrong() { this("nope") }
        }

        class B {
            constructor.dup() { this() }
            constructor.dup() { this() }
        }

        fn main() { this(1) }"#,
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("named constructor body must be a single `this(...)` delegation"),
        "missing delegation-shape diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("expected `int`, found `string`"),
        "missing delegation-argument diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("duplicate member `dup`"),
        "missing duplicate named-constructor diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("`this(...)` can only be used as a named constructor's delegation"),
        "missing stray-delegation diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_deinit_using_this() {
    // A `deinit` finalizer runs with `this` live and returns unit.
    let d = check_str(
        r#"class Widget {
            value: int

            deinit {
                println(this.value)
            }
        }

        fn main() {
            let w = Widget(7)
            println(w.value)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected a clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_deinit_unknown_member() {
    let d = check_str(
        r#"class Widget {
            value: int

            deinit {
                this.missing = 1
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("missing"),
        "expected a member diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_duplicate_deinit() {
    let d = check_str(
        r#"class Widget {
            value: int

            deinit { println(1) }
            deinit { println(2) }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(
        msgs.contains("already has a `deinit` block"),
        "missing duplicate-deinit diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_inheritance_and_hierarchy_casts() {
    // Inherited fields/methods, `super.m()`, subclass-to-superclass
    // assignability, and `is`/`as` across the hierarchy all type-check.
    let d = check_str(
        r#"class Animal {
            name: string
            legs: int = 4

            fn label() -> string {
                return this.name
            }
        }

        class Dog extends Animal {
            breed: string

            fn tag() -> string {
                return super.label()
            }
        }

        fn describe(a: Animal) -> string {
            return a.label()
        }

        fn main() {
            let d = Dog("Rex", "lab")
            println(d.legs)
            println(d.tag())
            println(describe(d))
            if (d is Dog) {
                println("dog")
            }
            if (d is Animal) {
                println("animal")
            }
            let a: Animal = d
            let back: Dog = a as Dog
            println(back.breed)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected a clean program, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_manual_alloc_and_free() {
    let d = check_str(
        r#"class Widget {
            value: int
            constructor(v: int) {
                value = v
            }
        }

        fn main() {
            #[manualAlloc] let w = Widget(1)
            println(w.value)
            w.free()
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_free_on_managed_binding() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            let w = Widget(1)
            w.free()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("manualAlloc"),
        "expected a manualAlloc hint, got:\n{msgs}"
    );
}

#[test]
fn rejects_double_free() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let w = Widget(1)
            w.free()
            w.free()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("already freed"),
        "expected a double-free diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_use_after_free() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let w = Widget(1)
            w.free()
            println(w.value)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("after `free()`"),
        "expected a use-after-free diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_manual_alloc_of_non_class() {
    let d = check_str(
        r#"fn main() {
            #[manualAlloc] let n = 5
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("class or struct"),
        "expected a manual-alloc type diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_unknown_attribute() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[bogus] let w = Widget(1)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("unknown attribute"),
        "expected an unknown-attribute diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_manual_alloc_with_arguments() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc(8)] let w = Widget(1)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("takes no arguments"),
        "expected an argument diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_item_level_attributes() {
    let d = check_str(
        r#"#[foo]
        fn main() {
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("not lowered yet"),
        "expected an item-attribute diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_manual_alloc_function_without_instance_return() {
    let d = check_str(
        r#"#[manualAlloc]
        fn makes() -> int {
            return 1
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("requires a class or struct return type"),
        "expected a return-type diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_manual_move_between_bindings() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let a = Widget(1)
            #[manualAlloc] let b = a
            println(b.value)
            b.free()
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_use_after_move() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let a = Widget(1)
            #[manualAlloc] let b = a
            println(a.value)
            b.free()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("after it was moved"),
        "expected a use-after-move diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_manual_value_into_managed_binding() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] let a = Widget(1)
            let b = a
            a.free()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("managed binding"),
        "expected a managed-binding diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_storing_manual_value_in_managed_field() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        class Holder {
            var widget: Widget?
        }

        fn main() {
            #[manualAlloc] let a = Widget(1)
            let h = Holder(none)
            h.widget = a
            a.free()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("managed field"),
        "expected a managed-field diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_free_in_both_if_branches() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main(flag: bool) {
            #[manualAlloc] let w = Widget(1)
            if (flag) {
                w.free()
            } else {
                w.free()
            }
        }"#,
    );
    assert!(
        !has_errors(&d),
        "freeing on both branches must not be a double free:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_use_after_free_on_only_one_branch() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main(flag: bool) {
            #[manualAlloc] let w = Widget(1)
            if (flag) {
                w.free()
            }
            println(w.value)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("after `free()`") || msgs.contains("after it was moved"),
        "expected a consumption diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_returning_manual_from_managed_fn() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn leak() -> Widget {
            #[manualAlloc] let w = Widget(1)
            return w
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("does not own its result"),
        "expected an ownership diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_overwriting_live_manual_binding() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn main() {
            #[manualAlloc] var w = Widget(1)
            w = Widget(2)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("overwrite") || msgs.contains("unknown attribute"),
        "expected an overwrite diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_owned_param_from_move() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn consume(#[manualAlloc] w: Widget) {
            w.free()
        }

        fn main() {
            #[manualAlloc] let w = Widget(1)
            consume(w)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_owned_param_from_fresh_allocation() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn consume(#[manualAlloc] w: Widget) {
            w.free()
        }

        fn main() {
            consume(Widget(1))
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_owned_param_from_managed_binding() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        fn consume(#[manualAlloc] w: Widget) {
            w.free()
        }

        fn main() {
            let w = Widget(1)
            consume(w)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("takes ownership"),
        "expected an ownership diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_manual_return_consumed_by_binding() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        #[manualAlloc]
        fn make() -> Widget {
            return Widget(1)
        }

        fn main() {
            #[manualAlloc] let w = make()
            w.free()
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_leaked_manual_return() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        #[manualAlloc]
        fn make() -> Widget {
            return Widget(1)
        }

        fn main() {
            make()
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("owned by the caller"),
        "expected a leak diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_returning_owned_call_from_manual_fn() {
    let d = check_str(
        r#"class Widget {
            value: int
        }

        #[manualAlloc]
        fn make() -> Widget {
            return Widget(1)
        }

        #[manualAlloc]
        fn make2() -> Widget {
            return make()
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_owned_field_from_fresh_allocation() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] child: Node = Node(1)
        }

        fn main() {
            #[manualAlloc] let h = Holder()
            h.free()
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_owned_field_from_manual_binding() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] child: Node = Node(0)
        }

        fn main() {
            #[manualAlloc] let h = Holder()
            #[manualAlloc] let n = Node(2)
            h.child = n
            h.free()
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_owned_field_from_managed_value() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] child: Node = Node(0)
        }

        fn main() {
            #[manualAlloc] let h = Holder()
            let n = Node(2)
            h.child = n
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("takes ownership"),
        "expected an ownership diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_owned_field_without_initializer() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] child: Node
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("must be initialized"),
        "expected an initializer diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_owned_field_without_type_annotation() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] child = Node(0)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("explicit type annotation"),
        "expected an annotation diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_owned_field_of_scalar_type() {
    let d = check_str(
        r#"class Holder {
            #[manualAlloc] count: int = 0
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("must be a class or struct type"),
        "expected a type diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_owned_static_field() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc] static var child: Node = Node(0)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("not supported on static fields"),
        "expected a static-field diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_owned_field_with_arguments() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[manualAlloc(8)] child: Node = Node(0)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("takes no arguments"),
        "expected an attribute diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_unknown_field_attribute() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        class Holder {
            #[borrowed] child: Node = Node(0)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("unknown attribute"),
        "expected an attribute diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_unknown_member_in_subclass() {
    let d = check_str(
        r#"class Animal {
            name: string
        }

        class Dog extends Animal {
            breed: string
        }

        fn main() {
            let d = Dog("Rex", "lab")
            println(d.missing)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("missing"),
        "expected a member diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_unsafe_raw_pointer_access() {
    let d = check_str(
        r#"class Vec2 {
            x: int
            y: int
        }

        fn bump(p: *Vec2) {
            unsafe {
                p.x = p.x + 1
                (*p).y = (*p).y + 1
            }
        }

        fn main() {
            #[manualAlloc] let v = Vec2(1, 2)
            unsafe {
                let p: *Vec2 = &v
                bump(p)
                println(p.x)
            }
            v.free()
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_addr_of_outside_unsafe() {
    let d = check_str(
        r#"class Vec2 {
            x: int
        }

        fn main() {
            let v = Vec2(1)
            let p: *Vec2 = &v
            println(p)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`&` may only be used inside an `unsafe` block"),
        "expected an unsafe diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_deref_outside_unsafe() {
    let d = check_str(
        r#"class Vec2 {
            x: int
        }

        fn main() {
            let v = Vec2(1)
            let p: *Vec2 = unsafe { &v }
            println((*p).x)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`*` may only be used inside an `unsafe` block"),
        "expected an unsafe diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_deref_of_non_pointer() {
    let d = check_str(
        r#"fn main() {
            let n = 3
            unsafe {
                println(*n)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("cannot dereference"),
        "expected a deref diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_scalar_raw_pointer_store_through() {
    let d = check_str(
        r#"fn main() {
            var n = 3
            unsafe {
                let p: *int = &n
                (*p) = (*p) + 1
                println(*p)
            }
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_addr_of_non_local_scalar() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                let p: *int = &(1 + 2)
                println(p)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`&` of a scalar requires a local variable"),
        "expected a local-variable diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_scalar_store_through_read_only_value() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                let p: *int = &5
                println(p)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`&` of a scalar requires a local variable"),
        "expected a local-variable diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_raw_buffer_alloc_free_and_index() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                var buf: *int = alloc(int, 8)
                buf[0] = 10
                buf[2] += 5
                var t = buf[0] + buf[1]
                println(t)
                free(buf)
            }
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected no errors, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_alloc_outside_unsafe() {
    let d = check_str(
        r#"fn main() {
            var buf: *int = alloc(int, 8)
            free(buf)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`alloc` may only be used inside an `unsafe` block"),
        "expected an alloc diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("`free` may only be used inside an `unsafe` block"),
        "expected a free diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_alloc_non_scalar() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                var s: *string = alloc(string, 4)
                println(s)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`alloc` currently only supports scalar element types (int, float, bool, char), found `string`"),
        "expected a scalar-only diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_alloc_bad_count() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                let b = alloc(int, "three")
                println(b)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`alloc` count must be an `int`"),
        "expected a count diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_free_non_pointer() {
    let d = check_str(
        r#"fn main() {
            unsafe {
                free(42)
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("`free` expects a pointer argument, found `int`"),
        "expected a pointer diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_pointer_index_outside_unsafe() {
    let d = check_str(
        r#"fn main() {
            var buf: *int = unsafe { alloc(int, 4) }
            buf[0] = 1
            println(buf[0])
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("pointer stores may only be used inside an `unsafe` block"),
        "expected a store diagnostic, got:\n{msgs}"
    );
    assert!(
        msgs.contains("pointer indexing may only be used inside an `unsafe` block"),
        "expected an index diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_indexing_managed_pointer() {
    let d = check_str(
        r#"class C {
            x: int
        }

        fn main() {
            var c = C(1)
            unsafe {
                let p: *C = &c
                println(p[0])
            }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("indexing a pointer to a `C` value is not supported yet"),
        "expected a managed-pointee diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_pointer_member_outside_unsafe() {
    let d = check_str(
        r#"class Vec2 {
            x: int
        }

        fn main() {
            let v = Vec2(1)
            let p: *Vec2 = unsafe { &v }
            println(p.x)
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("pointer access may only be used inside an `unsafe` block"),
        "expected a pointer access diagnostic, got:\n{msgs}"
    );
}

#[test]
fn accepts_implicit_borrow_params() {
    let d = check_str(
        r#"class Point {
            x: int
            y: int
            hits: List<int>

            constructor(x: int, y: int, hits: List<int>) {
                this.x = x
                this.y = y
                this.hits = hits
            }

            fn scale(f: int) -> int {
                return this.x * f
            }
        }

        fn prod(total: &int, p: &Point, q: &List<int>) -> int {
            var n = *total
            n += total[0]
            n += p.x + p.y + p.scale(2)
            for (v in p.hits) {
                n += v
            }
            n += q[0]
            return n
        }

        fn main() {
            let t = 4
            let pt = Point(1, 2, [3])
            let l = [9, 8]
            println(prod(t, pt, l))
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected implicit `&T` borrows to type-check, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_explicit_ampersand_expr_unchanged() {
    let d = check_str(
        r#"fn bump(t: *int) -> int {
            return unsafe { *t }
        }

        fn main() {
            var c = 5
            unsafe {
                let p: *int = &c
                println(bump(p))
            }
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected `&expr` raw pointers to keep working, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_ampersand_for_perfect_ptr_param() {
    let d = check_str(
        r#"fn bump(p: &int) -> int {
            return *p
        }

        fn main() {
            var c = 5
            println(bump(&c))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("expected `&int`, found `*int`"),
        "expected explicit `&c` to be rejected for an `&T` parameter, got:\n{msgs}"
    );
}

#[test]
fn rejects_write_through_deref_borrow() {
    let d = check_str(
        r#"fn bad(p: &int) -> int {
            var v = 2
            (*p) = v
            return *p
        }

        fn main() {
            var t = 3
            println(bad(t))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("cannot write through an immutable reference (`&T`)"),
        "expected a write-through diagnostic, got:\n{msgs}"
    );
    assert!(
        !msgs.contains("`*` may only be used inside an `unsafe` block"),
        "a `&T` deref read must not require `unsafe`, got:\n{msgs}"
    );
}

#[test]
fn rejects_write_through_index_borrow() {
    let d = check_str(
        r#"fn bad(p: &int) -> int {
            var v = 2
            p[0] = v
            return *p
        }

        fn main() {
            var t = 3
            println(bad(t))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("cannot write through an immutable reference (`&T`)"),
        "expected a write-through diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_write_through_member_borrow() {
    let d = check_str(
        r#"class Node {
            value: int
        }

        fn bad(n: &Node, v: int) -> int {
            n.value = v
            return n.value
        }

        fn main() {
            let g = Node(3)
            println(bad(g, 4))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("cannot write to a field `value` through an immutable reference (`&T`)"),
        "expected a field write-through diagnostic, got:\n{msgs}"
    );
}

#[test]
fn rejects_assign_in_unsafe_over_ref_still_errors() {
    let d = check_str(
        r#"fn bad(p: &int) -> int {
            unsafe {
                (*p) = 2
            }
            return *p
        }

        fn main() {
            var t = 3
            println(bad(t))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got none");
    assert!(
        msgs.contains("cannot write through an immutable reference (`&T`)"),
        "expected a write-through diagnostic even inside `unsafe`, got:\n{msgs}"
    );
}

#[test]
fn rejects_bare_scalar_stored_refs() {
    let d = check_str(
        r#"class Box {
            b: &int = 5
        }

        fn badRet() -> &int {
            return 5
        }

        fn main() {
            let x: &int = 5
            const y: &int = 6
            var cx = Box()
            var z: &int = x
            z = 7
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected errors, got none");
    assert!(
        msgs.contains(
            "`&T` references may only be formed from an existing `&T` value or a managed referent"
        ),
        "expected stored-ref diagnostics (field/return/let/const/assignment), got:\n{msgs}"
    );
    assert!(
        !msgs.contains("lists of `&T` references are not supported yet"),
        "plain `List<&T>` must type-check now, got:\n{msgs}"
    );
}

#[test]
fn accepts_stored_refs_from_values() {
    let d = check_str(
        r#"class Point {
            x: int = 0
        }

        fn fwd(p: &int) -> &int {
            return p
        }

        fn main() {
            var t = 3
            let a: &int = fwd(t)
            let b: &int = a
            var o = Point()
            let c: &Point = o
            var refs: List<&int> = [a, b]
            var objs: List<&Point> = [c]
            println(*a)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected stored `&T` to type-check, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_bare_scalar_method_and_lambda_ref_returns() {
    let d = check_str(
        r#"class Point {
            x: int

            fn badRet() -> &int {
                return this.x
            }
        }

        fn take(p: &int) -> int {
            return *p
        }

        fn lambdaRet() -> int {
            let f = fn() -> &int { return 1 }
            return 1
        }

        fn okRet(p: &int) -> &int {
            return p
        }

        fn main() {
            println(take(1))
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected errors, got none");
    assert!(
        msgs.contains(
            "`&T` references may only be formed from an existing `&T` value or a managed referent"
        ),
        "expected stored-ref diagnostics for method/lambda returns, got:\n{msgs}"
    );
}

#[test]
fn accepts_borrow_ctor_param() {
    let d = check_str(
        r#"class Tagged {
            tag: int
            name: string

            constructor(tag: &int, name: string) {
                this.tag = *tag
                this.name = name
            }
        }

        fn main() {
            var t = 7
            let t1 = Tagged(t, "a")
            println(t1.tag)
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected `&T` constructor params to type-check, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn string_index_and_iteration_type_as_byte() {
    // A `byte` binding accepts the string-index element, and the for-in loop
    // element compares with `int`s and ASCII `char` literals.
    let d = check_str(
        r#"fn main() {
            var b: byte = "abc"[1]
            for (c in "abc") {
                if (c == 98) { print("{c}") }
            }
            if ("abc"[2] == 'c') { print("ok") }
            print("{b}")
        }"#,
    );
    assert!(
        !has_errors(&d),
        "expected byte-typed string access to type-check, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn byte_cannot_satisfy_char_context() {
    // "é"[0] is a raw byte (195), never the decoded scalar 'é' (233); a byte
    // must not silently reach a char context.
    let d = check_str(
        r#"fn main() {
            let c: char = "é"[0]
        }"#,
    );
    assert!(has_errors(&d), "expected a type error, got no diagnostics");
}

#[test]
fn byte_vs_non_ascii_char_literal_is_rejected() {
    let d = check_str(
        r#"fn main() {
            let s = "é"
            if (s[0] == 'é') { print("x") }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got no diagnostics");
    assert!(
        msgs.contains("non-ASCII char literal has no `byte` value"),
        "expected the non-ASCII-literal byte diagnostic, got:\n{msgs}"
    );
}

#[test]
fn byte_vs_char_variable_is_rejected() {
    let d = check_str(
        r#"fn main() {
            let s = "é"
            let c = 'é'
            if (s[0] == c) { print("x") }
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected an error, got no diagnostics");
    assert!(
        msgs.contains("`byte` values only compare with `int`s or an ASCII `char` literal"),
        "expected the byte-vs-char diagnostic, got:\n{msgs}"
    );
}

#[test]
fn lexer_rejects_surrogate_escape_in_char_literal() {
    let d = check_str(
        r#"fn main() {
            let c = '\u{D800}'
        }"#,
    );
    let msgs = error_msgs(&d);
    assert!(has_errors(&d), "expected a lexer error, got no diagnostics");
    assert!(
        msgs.contains("invalid unicode escape '\\u{D800}'"),
        "expected the surrogate rejection, got:\n{msgs}"
    );
}

#[test]
fn accepts_fs_builtins() {
    let d = check_str(
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
            if (let some(es) = list_dir("tests/pickle")) {
                println(len(es))
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_delete_non_string_arg() {
    let d = check_str(r#"fn main() { delete(3) }"#);
    assert!(
        has_errors(&d),
        "expected a `delete` argument-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_mkdir_non_string_arg() {
    let d = check_str(r#"fn main() { mkdir(3) }"#);
    assert!(
        has_errors(&d),
        "expected a `mkdir` argument-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_list_dir_wrong_arity() {
    let d = check_str(r#"fn main() { list_dir("a", "b") }"#);
    assert!(
        has_errors(&d),
        "expected a `list_dir` arity error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_write_file_non_string_arg() {
    let d = check_str(
        r#"fn main() {
            write_file("path", 3)
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `write_file` argument-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_read_file_wrong_arity() {
    let d = check_str(
        r#"fn main() {
            let r = read_file("a", "b")
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `read_file` arity error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_list_mutation_methods() {
    let d = check_str(
        r#"fn main() {
            let xi = [1, 2, 3]
            let v = xi.remove(0)
            xi.insert(1, 9)
            xi.sort()
            let xs = ["a", "c"]
            xs.remove(0)
            xs.insert(0, "z")
            xs.sort()
            let xf = [2.5, 0.5]
            xf.sort()
            let xb = [true, false]
            xb.sort()
            println(v, len(xi), len(xs), len(xf), len(xb))
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_sort_on_unsortable_list() {
    let d = check_str(
        r#"class Point {
            var n: int
        }
        fn main() {
            let pts = [Point(1), Point(2)]
            pts.sort()
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `sort` element-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_remove_non_int_index() {
    let d = check_str(
        r#"fn main() {
            let xs = [1, 2, 3]
            let v = xs.remove("a")
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `remove` index-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_bytes_and_str_byte_bridge() {
    let d = check_str(
        r#"fn main() {
            let b = bytes("hi")
            let s = str(b)
            let c: byte = b[0]
            print("{s} {c}")
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_map_remove_option() {
    let d = check_str(
        r#"fn main() {
            let m = {"a": 1}
            let v: int? = m.remove("a")
            let w = m.remove("a") ?? -1
            if (let some(x) = m.remove("a")) {
                print("{x}")
            } else {
                print("none")
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_map_get_option() {
    let d = check_str(
        r#"fn main() {
            let m = {"a": 1}
            let v: int? = m.get("a")
            let w = m.get("a") ?? -1
            if (let some(x) = m.get("a")) {
                print("{x}")
            } else {
                print("none")
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn accepts_map_entries_for() {
    let d = check_str(
        r#"fn main() {
            let m = {"a": 1, "b": 2}
            var sum = 0
            var cat = ""
            for ((k, v) in m) {
                cat += k
                sum += v
            }
            let w = {"x": "s"}
            for ((key, value) in w) {
                print(key, value)
            }
            for (v in m) {
                print(v)
            }
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_map_entries_pattern_arity() {
    let d = check_str(
        r#"fn main() {
            let m = {"a": 1}
            for ((k, v, w) in m) {
                print(k, v, w)
            }
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a map-entries arity error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_map_remove_arity() {
    let d = check_str(
        r#"fn main() {
            let m = {"a": 1}
            let v = m.remove()
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `remove` arity error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_bytes_non_string_arg() {
    let d = check_str(
        r#"fn main() {
            let b = bytes(42)
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `bytes` argument-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn rejects_str_byte_bridge_on_int_list() {
    let d = check_str(
        r#"fn main() {
            let xs = [1, 2, 3]
            let s = str(xs)
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a `str` element-type error, got:\n{}",
        error_msgs(&d)
    );
}

#[test]
fn accepts_string_index_assign() {
    let d = check_str(
        r#"fn main() {
            let s = "abc"
            s[0] = 'H'
            s[1] = 65
            s[2] += 1
            let b = s[0]
            s[1] = b
            println(s)
        }"#,
    );
    assert!(!has_errors(&d), "unexpected errors:\n{}", error_msgs(&d));
}

#[test]
fn rejects_string_index_assign_out_of_range_byte() {
    let d = check_str(
        r#"fn main() {
            let s = "abc"
            s[0] = 256
        }"#,
    );
    assert!(
        has_errors(&d),
        "expected a byte-range error, got:\n{}",
        error_msgs(&d)
    );
}
