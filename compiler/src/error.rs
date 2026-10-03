//! Stable error codes and the error catalogue.
//!
//! Every diagnostic may carry an [`ErrorCode`]. The code is assigned at the
//! point the compiler detects the problem, never guessed from rendered text
//! afterwards, so the code and the message always describe the same fact.
//!
//! Codes are grouped by area (`E01xx` lexing/parsing, `E02xx` name
//! resolution and declarations, `E03xx`-`E04xx` type checking and
//! operations, `E05xx` ownership/memory, `E06xx`-`E07xx` object model and
//! references, `E09xx` the gap between the checker and the code generator).
//!
//! The catalogue entries back the `pickle explain <CODE>` subcommand and the
//! `--json` output's `code` fields. A code is only listed here once a
//! diagnostic actually emits it.

/// Stable, semantic error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrorCode {
    // E01xx: lexical and syntax errors.
    Lex,
    Syntax,
    // E02xx: declarations and name resolution.
    DuplicateItem,
    DuplicateImportAlias,
    ModuleNotFound,
    NotExported,
    ModuleMismatch,
    AmbiguousImport,
    DuplicateMember,
    DuplicateConstructor,
    DuplicateDeinit,
    UnknownType,
    NotGeneric,
    WrongTypeArgs,
    ListOfRefs,
    InvalidExtends,
    OverrideSignature,
    MissingOverride,
    OrphanOverride,
    MixedMethodKind,
    // E0301..E03xx: type checking.
    TypeMismatch,
    ReturnValue,
    BreakOutsideLoop,
    ConditionNotBool,
    ForSequence,
    MapKeyString,
    Index,
    UndeclaredName,
    NoMember,
    EnumVariant,
    CannotConstruct,
    NonClassMemberAssign,
    InterfaceConformance,
    ThisSuper,
    Assignment,
    Cast,
    // E04xx: operations and attributes.
    Attribute,
    ManualAlloc,
    CallArity,
    CallNonFunction,
    MemberOnNonClass,
    Operator,
    RawBuffer,
    // E05xx: ownership and memory.
    UseAfterFree,
    DoubleFree,
    FreeOnNonManual,
    OwnedPosition,
    OverwriteManual,
    LeakedOwned,
    UnsafeRequired,
    StoredRef,
    // E09xx: checker accepted, codegen has no lowering yet.
    NotLowered,
}

impl ErrorCode {
    /// The stable 5-char identifier renderer prints as `[E0308]`.
    pub const fn id(self) -> &'static str {
        use ErrorCode::*;
        match self {
            Lex => "E0101",
            Syntax => "E0111",
            DuplicateItem => "E0201",
            DuplicateImportAlias => "E0202",
            ModuleNotFound => "E0206",
            NotExported => "E0207",
            ModuleMismatch => "E0208",
            AmbiguousImport => "E0209",
            DuplicateMember => "E0203",
            DuplicateConstructor => "E0204",
            DuplicateDeinit => "E0205",
            UnknownType => "E0210",
            NotGeneric => "E0211",
            WrongTypeArgs => "E0212",
            ListOfRefs => "E0213",
            InvalidExtends => "E0220",
            OverrideSignature => "E0221",
            MissingOverride => "E0222",
            OrphanOverride => "E0223",
            MixedMethodKind => "E0224",
            TypeMismatch => "E0308",
            ReturnValue => "E0310",
            BreakOutsideLoop => "E0320",
            ConditionNotBool => "E0340",
            ForSequence => "E0341",
            MapKeyString => "E0342",
            Index => "E0343",
            UndeclaredName => "E0351",
            NoMember => "E0352",
            EnumVariant => "E0353",
            CannotConstruct => "E0354",
            NonClassMemberAssign => "E0355",
            InterfaceConformance => "E0360",
            ThisSuper => "E0361",
            Assignment => "E0362",
            Cast => "E0363",
            Attribute => "E0400",
            ManualAlloc => "E0401",
            CallArity => "E0410",
            CallNonFunction => "E0411",
            MemberOnNonClass => "E0420",
            Operator => "E0430",
            RawBuffer => "E0440",
            UseAfterFree => "E0501",
            DoubleFree => "E0502",
            FreeOnNonManual => "E0503",
            OwnedPosition => "E0504",
            OverwriteManual => "E0505",
            LeakedOwned => "E0506",
            UnsafeRequired => "E0520",
            StoredRef => "E0700",
            NotLowered => "E0900",
        }
    }
}

/// One catalogue entry: what the code means, the rule behind it, and a
/// failing snippet.
#[derive(Debug, Clone, Copy)]
pub struct CatalogueEntry {
    pub code: ErrorCode,
    /// Concise name printed in group headlines, e.g. "type mismatch".
    pub title: &'static str,
    /// The rule the compiler applied.
    pub rule: &'static str,
    /// A short example of a violation.
    pub example: &'static str,
}

/// The full error catalogue. Order shows in `pickle explain` listings.
pub const CATALOGUE: &[CatalogueEntry] = &[
    CatalogueEntry {
        code: ErrorCode::Lex,
        title: "lexical error",
        rule: "the source contains a byte sequence the lexer cannot turn into a token; a literal is unterminated, or a character is not valid in this position.",
        example: "let s = \"unterminated",
    },
    CatalogueEntry {
        code: ErrorCode::Syntax,
        title: "syntax error",
        rule: "the token stream does not form a valid declaration, statement, or expression at this position.",
        example: "fn main() { let = }",
    },
    CatalogueEntry {
        code: ErrorCode::DuplicateItem,
        title: "duplicate declaration",
        rule: "a top-level name (function, class, struct, enum, interface, const) is declared twice in the same module.",
        example: "fn main() {}  fn main() {}",
    },
    CatalogueEntry {
        code: ErrorCode::DuplicateImportAlias,
        title: "duplicate import alias",
        rule: "two module imports resolve to the same alias in this module.",
        example: "import a as util  import b as util",
    },
    CatalogueEntry {
        code: ErrorCode::ModuleNotFound,
        title: "module not found",
        rule: "an import path does not resolve to a file: it is searched relative to the importing file's directory, then under `<cwd>/src/` (or `<cwd>/src/<path>.pkl` when the path maps to a nested folder).",
        example: "import nowhere.utility",
    },
    CatalogueEntry {
        code: ErrorCode::NotExported,
        title: "item not exported by module",
        rule: "an `import { item }` references a name the referenced module does not declare at top level.",
        example: "import { Missing } from text.lexer",
    },
    CatalogueEntry {
        code: ErrorCode::ModuleMismatch,
        title: "module path mismatch",
        rule: "a file declares a `module` path that differs from the path used to load it; resolving the import locates the file under a different module name than the one it declares.",
        example: "import b.tool  // b/tool.pkl declares `module a.tool`",
    },
    CatalogueEntry {
        code: ErrorCode::AmbiguousImport,
        title: "ambiguous imported name",
        rule: "two loaded modules both export the same top-level function or const, and a bare reference in this file cannot tell them apart. Disambiguate with `import <item> from <mod> as <alias>` first.",
        example: "import helper from a.utils  import helper from b.utils  fn main() { helper() }",
    },
    CatalogueEntry {
        code: ErrorCode::DuplicateMember,
        title: "duplicate member",
        rule: "a class or struct declares two members with the same name.",
        example: "class P {\n    var x: int\n    var x: string\n}",
    },
    CatalogueEntry {
        code: ErrorCode::DuplicateConstructor,
        title: "duplicate constructor",
        rule: "a class or struct declares the primary constructor more than once.",
        example: "class P { constructor() {}  constructor() {} }",
    },
    CatalogueEntry {
        code: ErrorCode::DuplicateDeinit,
        title: "duplicate deinit",
        rule: "a class declares more than one `deinit` block.",
        example: "class P { deinit {}  deinit {} }",
    },
    CatalogueEntry {
        code: ErrorCode::UnknownType,
        title: "unknown type",
        rule: "a type name does not resolve to a declared class, struct, enum, interface, generic parameter, or built-in type.",
        example: "fn f(x: Widget) {}",
    },
    CatalogueEntry {
        code: ErrorCode::NotGeneric,
        title: "type is not generic",
        rule: "a generic instantiation `Name<T>` is applied to a type that takes no type parameters.",
        example: "let xs: string<int> = \"\"",
    },
    CatalogueEntry {
        code: ErrorCode::WrongTypeArgs,
        title: "wrong number of type arguments",
        rule: "a generic instantiation supplies fewer or more type arguments than the type declares.",
        example: "class Box<T> {}\nlet b: Box<int, string> = Box<int>()",
    },
    CatalogueEntry {
        code: ErrorCode::ListOfRefs,
        title: "list of references",
        rule: "lists storing `&T` references are not supported yet; a reference cannot outlive the borrowed local it points to inside a collection.",
        example: "let refs: List<&int> = []",
    },
    CatalogueEntry {
        code: ErrorCode::InvalidExtends,
        title: "invalid extends",
        rule: "only a class may extend a class; extends/`implements` targets must be the right kind of type.",
        example: "class Foo extends SomeStruct",
    },
    CatalogueEntry {
        code: ErrorCode::OverrideSignature,
        title: "override signature mismatch",
        rule: "an `override` method must match its parent's parameter and return types exactly.",
        example: "class B extends A { override fn m(x: int) -> int }  // A.m takes string",
    },
    CatalogueEntry {
        code: ErrorCode::MissingOverride,
        title: "missing override",
        rule: "a method redefines an inherited method without the `override` keyword.",
        example: "class B extends A { fn m() {} }  // A already has m()",
    },
    CatalogueEntry {
        code: ErrorCode::OrphanOverride,
        title: "orphan override",
        rule: "a method is marked `override` but no parent in the class chain declares it.",
        example: "class B extends A { override fn n() {} }  // A has no n",
    },
    CatalogueEntry {
        code: ErrorCode::MixedMethodKind,
        title: "mixed method kind",
        rule: "a class cannot redeclare an inherited method under the opposite kind: a `static` method cannot shadow an inherited instance method, and an instance method cannot shadow an inherited `static` method.",
        example: "class B extends A { static fn m() {} }  // A.m is an instance method",
    },
    CatalogueEntry {
        code: ErrorCode::TypeMismatch,
        title: "type mismatch",
        rule: "an expression's type does not satisfy the declared or expected type, after option/subtyping conversions are tried.",
        example: "fn f() -> int { return \"x\" }",
    },
    CatalogueEntry {
        code: ErrorCode::ReturnValue,
        title: "return value error",
        rule: "a function with no return type returns a value, or `if`/`match` branches produce inconsistent result types.",
        example: "fn f() { return 1 }   fn g(b) { if b { 1 } else { \"x\" } }",
    },
    CatalogueEntry {
        code: ErrorCode::BreakOutsideLoop,
        title: "break or continue outside a loop",
        rule: "`break` and `continue` are only valid inside `while`/`for` bodies.",
        example: "fn f() { break }",
    },
    CatalogueEntry {
        code: ErrorCode::ConditionNotBool,
        title: "condition is not bool",
        rule: "`if`, `while`, `for` conditions, and `&&`/`||` operands must be `bool`.",
        example: "if (1) {}",
    },
    CatalogueEntry {
        code: ErrorCode::ForSequence,
        title: "for-in requires a sequence",
        rule: "`for (x in seq)` needs a sequence: a List, a string, a Map, or a range.",
        example: "for (x in 42) {}",
    },
    CatalogueEntry {
        code: ErrorCode::MapKeyString,
        title: "map keys must be a supported type",
        rule: "map keys may be `string`, a scalar (`int`, `float`, `bool`, `char`, `byte`), a composite object (`List`, `Map`, class/struct instance, enum, interface), an option, or a tuple. Keys are hashed and compared structurally. Reference, function, and pointer keys are not supported.",
        example: "(1, 2) as a map key — { (1, 2): \"a\" } is not lowered",
    },
    CatalogueEntry {
        code: ErrorCode::Index,
        title: "index error",
        rule: "the index expression has the wrong type for the indexed value, or the value cannot be indexed at all.",
        example: "let xs = [1,2]  let x = xs[\"a\"]",
    },
    CatalogueEntry {
        code: ErrorCode::UndeclaredName,
        title: "undeclared name",
        rule: "an identifier in expression position is not a local, function, const, type, or member of the surrounding class.",
        example: "fn main() { print(nope) }",
    },
    CatalogueEntry {
        code: ErrorCode::NoMember,
        title: "no such member",
        rule: "a field, property, or method name does not exist on the receiver class, struct, or List/Map built-ins.",
        example: "class P { var x: int }\nlet p = P()  p.y",
    },
    CatalogueEntry {
        code: ErrorCode::EnumVariant,
        title: "enum variant error",
        rule: "a variant name does not exist on the enum, or a bare variant name is used where a payload constructor is required.",
        example: "enum Color { Red  Green }\ncase Color.Purple -> {}",
    },
    CatalogueEntry {
        code: ErrorCode::CannotConstruct,
        title: "cannot construct this type",
        rule: "interfaces and enums are not directly constructible; classes and structs are.",
        example: "let i = SomeInterface()",
    },
    CatalogueEntry {
        code: ErrorCode::NonClassMemberAssign,
        title: "member assignment on non-class value",
        rule: "assigning through `.member` requires the receiver to be a class or struct value.",
        example: "let n = 5  n.x = 3",
    },
    CatalogueEntry {
        code: ErrorCode::InterfaceConformance,
        title: "interface conformance",
        rule: "a type declaring `implements I` must provide every member `I` declares.",
        example: "class P implements Runnable { }  // no run()",
    },
    CatalogueEntry {
        code: ErrorCode::ThisSuper,
        title: "this / super misuse",
        rule: "`this`/`super` are only valid inside a class body; `this(...)` only as a named constructor's single delegation.",
        example: "fn f() { this }",
    },
    CatalogueEntry {
        code: ErrorCode::Assignment,
        title: "invalid assignment",
        rule: "the assignment target is not assignable: it is immutable, undeclared, a wrong receiver kind, a missing setter, or write-through an `&T`.",
        example: "fn f() { let x = 1  x = 2 }",
    },
    CatalogueEntry {
        code: ErrorCode::Cast,
        title: "invalid cast or is",
        rule: "the cast/`is` target is unrelated to the source type (no numeric conversion, option relationship, or inheritance).",
        example: "let n = 5 as bool",
    },
    CatalogueEntry {
        code: ErrorCode::Attribute,
        title: "attribute error",
        rule: "an attribute name is unknown, repeated, or given arguments it does not take.",
        example: "#[wat] let x = 1",
    },
    CatalogueEntry {
        code: ErrorCode::ManualAlloc,
        title: "manualAlloc misuse",
        rule: "`#[manualAlloc]` applies to class/struct types only: its binding needs an allocation initializer, fields need an initializer and, when untyped, an annotation; it is not allowed on statics or constants.",
        example: "#[manualAlloc] let x = 5",
    },
    CatalogueEntry {
        code: ErrorCode::CallArity,
        title: "call argument error",
        rule: "the call passes too many or too few arguments, uses named arguments where the callee takes positional ones, or a generic function name is unknown.",
        example: "fn add(a: int) {}  add(1, 2)",
    },
    CatalogueEntry {
        code: ErrorCode::CallNonFunction,
        title: "call of non-function",
        rule: "only functions, type constructors, and enum variant constructors can be called.",
        example: "let n = 5  n()",
    },
    CatalogueEntry {
        code: ErrorCode::MemberOnNonClass,
        title: "member access on non-class value",
        rule: "`.member` requires a class, struct, enum, or interface receiver; an option (`T?`) must be unwrapped or accessed with `?.` first, and scalars have no members.",
        example: "let n = 5  n.name",
    },
    CatalogueEntry {
        code: ErrorCode::Operator,
        title: "operator operand error",
        rule: "an operator is applied to operands of a type it does not define (non-numeric arithmetic, non-int bitwise and shifts, non-bool `!`, unpacking a non-option, dereferencing a non-pointer).",
        example: "let s = \"a\"  let n = s * 2",
    },
    CatalogueEntry {
        code: ErrorCode::RawBuffer,
        title: "alloc / free misuse",
        rule: "the raw-buffer builtins have a fixed shape: `alloc(T, count)` with a scalar element type and an `int` count, `free(p)` with a pointer argument, zero extra `.free()`/`alloc` arguments when used on bindings.",
        example: "unsafe { alloc(Widget, 4) }  // scalar element types only",
    },
    CatalogueEntry {
        code: ErrorCode::UseAfterFree,
        title: "use after free or move",
        rule: "reading or calling with a `#[manualAlloc]` value after `.free()` or after its ownership moved elsewhere.",
        example: "x.free()  println(x)",
    },
    CatalogueEntry {
        code: ErrorCode::DoubleFree,
        title: "already freed",
        rule: "a `#[manualAlloc]` binding is released twice.",
        example: "x.free()  x.free()",
    },
    CatalogueEntry {
        code: ErrorCode::FreeOnNonManual,
        title: "free on a non-manual value",
        rule: "`x.free()` is only available on a `#[manualAlloc]` binding, which owns its allocation.",
        example: "let x = User()  x.free()",
    },
    CatalogueEntry {
        code: ErrorCode::OwnedPosition,
        title: "owned position",
        rule: "a `#[manualAlloc]` value can only reach an owning position by moving a `#[manualAlloc]` binding or a fresh allocation; it cannot be stored in a managed binding, field, or collection.",
        example: "#[manualAlloc] let y = x  // x must be manual or fresh",
    },
    CatalogueEntry {
        code: ErrorCode::OverwriteManual,
        title: "overwrite or non-owning return of a manual value",
        rule: "a `#[manualAlloc]` binding cannot be overwritten, and a manual value cannot be returned from a function that does not declare `#[manualAlloc]`.",
        example: "x = make()  // x is manual",
    },
    CatalogueEntry {
        code: ErrorCode::LeakedOwned,
        title: "leaked owned result",
        rule: "the result of a `#[manualAlloc]` function is owned by the caller and must be bound, passed to an owned parameter, or returned; discarding it leaks.",
        example: "make()  // result dropped, not consumed",
    },
    CatalogueEntry {
        code: ErrorCode::UnsafeRequired,
        title: "unsafe required",
        rule: "raw pointers, `&`/`*`, pointer access, indexing and stores, and the raw-buffer builtins are only allowed inside `unsafe { }`.",
        example: "let p = &local\n*p = 5",
    },
    CatalogueEntry {
        code: ErrorCode::StoredRef,
        title: "stored or returned reference",
        rule: "a `&T` value may be stored or returned only when the source already is the reference (`&T`) or a managed referent sharing its identity; borrowing a bare scalar local would dangle once the frame dies.",
        example: "let r: &int = 5",
    },
    CatalogueEntry {
        code: ErrorCode::NotLowered,
        title: "not lowered yet",
        rule: "the checker accepted the construct, but the code generator does not lower it yet; the program is rejected instead of miscompiled.",
        example: "let t = (1, 2)  // tuples check but have no lowering",
    },
];

/// Look up a catalogue entry by its rendered id (`"E0308"`).
pub fn explain(id: &str) -> Option<&'static CatalogueEntry> {
    CATALOGUE.iter().find(|e| e.code.id() == id)
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Resolve a rendered id back to a code; `None` for unknown ids.
pub fn code_from_id(id: &str) -> Option<ErrorCode> {
    explain(id).map(|e| e.code)
}
