use crate::diag::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub module: Option<ModuleDecl>,
    pub imports: Vec<ImportDecl>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub struct ModuleDecl {
    pub path: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ImportKind {
    /// `import a.b` / `import a.b as x`: brings the module namespace into
    /// scope and splices its public items unqualified.
    Module { alias: Option<String> },
    /// `import x from a.b [as z]`, `import x.y from a.b as z`,
    /// `import { x, y as yy } from a.b`: binds each imported symbol by its
    /// local name (alias-or-last-segment). A symbol that names a module file
    /// also splices that module's public items.
    Items { items: Vec<ImportItem> },
    /// `import * from a.b`: binds all public symbols of the source unqualified.
    Wildcard,
}

#[derive(Debug, Clone)]
pub struct ImportItem {
    /// Symbol path within the source (e.g. `x`, or `x.y` for a nested export).
    pub path: Vec<String>,
    pub alias: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    /// `pub import ...` re-exports the imported names from this module.
    pub is_public: bool,
    /// Module source: the dot-path after `from`, or the module path itself for
    /// module imports (`import a.b`). A source may name a package directory or
    /// a module file.
    pub source: Vec<String>,
    pub kind: ImportKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub doc: Vec<String>,
    pub attrs: Vec<Attribute>,
    pub span: Span,
    pub kind: ItemKind,
}

/// `#[name]` or `#[name(args)]` attached to an item or a `let` binding.
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ItemKind {
    Fn(FnDecl),
    Class(ClassDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Interface(InterfaceDecl),
    Const(ConstDecl),
    Test(FnDecl),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Protected,
    Default,
}

impl Visibility {
    pub fn is_explicitly_private(&self) -> bool {
        matches!(
            self,
            Visibility::Private | Visibility::Protected | Visibility::Public
        )
    }
}

#[derive(Debug, Clone)]
pub struct GenericParam {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TypeExpr {
    pub span: Span,
    pub kind: TypeExprKind,
}

#[derive(Debug, Clone)]
pub enum TypeExprKind {
    /// `Foo` or `a.b.Foo`
    Path(Vec<String>),
    /// `Foo<A, B>`
    Generic(Box<TypeExpr>, Vec<TypeExpr>),
    /// `*T`
    Pointer(Box<TypeExpr>),
    /// `&T`
    Ref(Box<TypeExpr>),
    /// `T?`
    Option(Box<TypeExpr>),
    /// `(A, B)`
    Tuple(Vec<TypeExpr>),
    /// `(A, B) -> R` or `(A) -> R` or `fn (A) -> R`
    Fn {
        params: Vec<TypeExpr>,
        ret: Option<Box<TypeExpr>>,
        is_async: bool,
    },
    /// `_`
    Infer,
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub is_async: bool,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub return_ty: Option<TypeExpr>,
    pub body: Option<FnBody>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub rest: bool,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum FnBody {
    Block(Box<Block>),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub expr: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ForHeader {
    /// `for (item in seq)`
    In { pattern: Pattern, sequence: Expr },
    /// `for (init; cond; step)`
    Range {
        init: Box<Stmt>,
        cond: Box<Expr>,
        step: Box<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        pattern: Pattern,
        ty: Option<TypeExpr>,
        init: Option<Expr>,
        mutable: bool,
        attrs: Vec<Attribute>,
        span: Span,
    },
    Const {
        name: String,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Break {
        span: Span,
    },
    Continue {
        span: Span,
    },
    While {
        cond: Box<Expr>,
        body: Box<Block>,
        span: Span,
    },
    For {
        header: ForHeader,
        body: Box<Block>,
        span: Span,
    },
    /// `try B catch (name) H finally F`. Every clause is optional except the
    /// body: `catch` may be absent (a `try`/`finally`), and both may be absent
    /// (a bare `try` that only exists for its `finally`). `binder` is `None`
    /// for a bare `catch (e)`, and `handler` is `None` for a `finally`-only
    /// `try`.
    Try {
        body: Box<Block>,
        binder: Option<String>,
        handler: Option<Box<Block>>,
        finally: Option<Box<Block>>,
        span: Span,
    },
    /// `throw value`. The payload is a `string`; control transfers to the
    /// innermost enclosing `catch`, or out of the function when there is none.
    Throw {
        value: Expr,
        span: Span,
    },
    Expr(Expr),
    Empty(Span),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Lit(Lit),
    Ident(String),
    This,
    Super,
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    Member {
        object: Box<Expr>,
        name: String,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        op: AssignOp,
        value: Box<Expr>,
    },
    Lambda {
        params: Vec<Param>,
        return_ty: Option<TypeExpr>,
        is_async: bool,
        body: FnBody,
    },
    If {
        cond: IfCond,
        then: Box<Block>,
        else_else: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Await(Box<Expr>),
    GenericCall {
        name: String,
        type_args: Vec<TypeExpr>,
    },
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
        kind: CastKind,
    },
    Unsafe(Box<Block>),
    Block(Box<Block>),
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    OptAccess {
        object: Box<Expr>,
        name: String,
    },
    OptUnwrap(Box<Expr>),
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        incl: bool,
    },
}

#[derive(Debug, Clone)]
pub enum IfCond {
    /// `if (cond)`
    Cond(Box<Expr>),
    /// `if (let pattern = expr)`
    Binding { pattern: Pattern, value: Box<Expr> },
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastKind {
    As,
    TryAs,
    Is,
}

#[derive(Debug, Clone)]
pub struct CallArg {
    pub name: Option<String>,
    pub value: Expr,
    pub spread: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Lit {
    Int { value: i128 },
    Float { value: f64 },
    Bool(bool),
    Char(char),
    String(Vec<StrPart>),
    None,
}

/// One piece of a string literal: plain text, or an interpolated expression
/// (already parsed and typed by the checker).
#[derive(Debug, Clone)]
pub enum StrPart {
    Text(String),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Range,
    RangeIncl,
    NullCoalesce,
    Send,
    Is,
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
    Deref,
    AddrOf,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard,
    Binding {
        name: String,
        ty: Option<TypeExpr>,
    },
    Literal(Lit),
    Tuple(Vec<Pattern>),
    /// `Type.Variant` or `Variant(payload...)`
    Variant {
        path: Vec<String>,
        payloads: Vec<Pattern>,
    },
    Or(Vec<Pattern>),
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub extends: Option<TypeExpr>,
    pub implements: Vec<TypeExpr>,
    pub members: Vec<ClassMember>,
}

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub implements: Vec<TypeExpr>,
    pub members: Vec<ClassMember>,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub variants: Vec<EnumVariant>,
    pub implements: Vec<TypeExpr>,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub fields: Vec<EnumField>,
    pub discriminant: Option<i128>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumField {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Debug, Clone)]
pub struct InterfaceDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub generics: Vec<GenericParam>,
    pub extends: Vec<TypeExpr>,
    pub members: Vec<InterfaceMember>,
}

#[derive(Debug, Clone)]
pub enum InterfaceMember {
    Method(MethodDecl),
    Property {
        name: String,
        ty: TypeExpr,
        span: Span,
    },
    Const {
        name: String,
        value: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub name: String,
    pub visibility: Visibility,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ClassMember {
    Field {
        name: String,
        ty: Option<TypeExpr>,
        init: Option<Expr>,
        visibility: Visibility,
        is_static: bool,
        const_: bool,
        attrs: Vec<Attribute>,
        span: Span,
    },
    Method(MethodDecl),
    Constructor(ConstructorDecl),
    Property(PropertyDecl),
    Init(Block),
    Deinit(Block),
    Const {
        name: String,
        visibility: Visibility,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct MethodDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub is_static: bool,
    pub is_override: bool,
    pub is_async: bool,
    pub is_test: bool,
    pub operator: Option<String>,
    pub generics: Vec<GenericParam>,
    pub params: Vec<Param>,
    pub return_ty: Option<TypeExpr>,
    pub body: Option<FnBody>,
}

#[derive(Debug, Clone)]
pub struct ConstructorDecl {
    pub name: Option<String>,
    pub span: Span,
    pub visibility: Visibility,
    pub params: Vec<Param>,
    pub body: Block,
}

#[derive(Debug, Clone)]
pub struct PropertyDecl {
    pub name: String,
    pub span: Span,
    pub visibility: Visibility,
    pub ty: Option<TypeExpr>,
    pub get: Option<PropertyAccessor>,
    pub set: Option<PropertyAccessor>,
    pub is_static: bool,
}

#[derive(Debug, Clone)]
pub enum PropertyAccessor {
    /// `=> expr`
    Expr(Expr),
    /// `{ ... }`
    Block(Block),
}

/// Assertion methods recognized on an `expect(...)` chain (testing framework).
pub fn is_expect_method(name: &str) -> bool {
    matches!(
        name,
        "toBe"
            | "toEqual"
            | "toBeTruthy"
            | "toBeFalsy"
            | "toBeNull"
            | "toExist"
            | "toHaveLength"
            | "toContain"
            | "toBeGreaterThan"
            | "toBeLessThan"
    )
}
