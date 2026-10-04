//! Lower the checked AST to PickleIR.
//!
//! The checker runs first (collecting `expr -> Ty` into a side table); this
//! module walks the typed AST and emits the flat block/slot/temp IR from
//! `ir.rs`. Constructs outside the slice-1 subset are rejected with a
//! "not lowered yet" diagnostic rather than miscompiled.

//! Lower the checked AST to PickleIR.
//!
//! The checker runs first (collecting `expr -> Ty` into a side table); this
//! module walks the typed AST and emits the flat block/slot/temp IR from
//! `ir.rs`. Constructs outside the slice-1 subset are rejected with a
//! "not lowered yet" diagnostic rather than miscompiled.
//!
//! # `try`/`catch`/`throw`/`finally`
//!
//! Compiled frames carry no unwind information, so a throw cannot skip a frame.
//! [`try_stmt`] therefore lowers a throw to *cooperating* propagation:
//!
//! - `throw v` calls the runtime to publish `v` and raise a per-thread flag,
//!   then branches straight to the innermost enclosing `catch` (or, with none,
//!   runs the `finally` chain and returns, leaving the flag for the caller).
//! - A call that can propagate is followed by a check of that flag; when it is
//!   set the enclosing function returns immediately. Every frame between the
//!   throw and its handler therefore leaves *in order*, so the collector's
//!   shadow stack stays balanced.
//! - `catch` consumes the flag and binds the payload.
//!
//! Because the IR is a flat CFG with no unwind tables, `finally` is lowered by
//! *duplication*: [`exit_through_finallys`] re-emits each active `finally` on
//! the way out of the block, so a `finally` also runs on a `return`, a
//! `break`/`continue`, and a propagating throw. No new IR variant is needed for
//! any of this, so both backends and the textual printer are untouched.
//!
//! The check after a call costs a runtime call, so it is emitted only where a
//! throw is actually possible: [`Emitter::solve_may_throw`] solves a least fixed
//! point over the call graph first, and a call to a function that provably cannot
//! throw gets no check at all. That analysis needs the call graph, which only
//! exists once the bodies have been walked, so emission runs twice — see
//! [`Emitter::emit`].

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ast::BinOp as AstBinOp;
use crate::ast::UnOp as AstUnOp;
use crate::ast::*;
use crate::diag::{Diagnostic, DiagnosticSink, Span};
use crate::ir::BinOp as IrBinOp;
use crate::ir::UnOp as IrUnOp;
use crate::ir::*;
use crate::resolve::{
    CallableInfo, ClassTable, CtorInfo, EnumTable, FieldInfo, ParamInfo, PropertyInfo,
    ResolvedProgram, TypeTableEntry,
};
use crate::ty::Ty;

/// Make a function symbol from a type's display name: keep alphanumerics,
/// replace every other character (commas, spaces, parens, `?`, `&`, ...) with
/// `_`, so instantiation suffixes stay valid linker symbols.
fn sanitize_symbol(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Runtime class ids for user classes start at this id: the runtime reserves
/// 0..=6 for the builtin boxed types (string/list/map/int/float/bool/char),
/// 7 for `PEnum`, 8 for `PTuple`, and 9 for `PStream`, so the first user class
/// registered at runtime gets id `PICKLE_CLASS_USER_BASE`. The compiler assigns
/// `base + index` in declaration order, matching the runtime's allocation
/// order.
const PICKLE_CLASS_USER_BASE: i64 = 10;

/// Front-end subset that emits IR. The module must already pass the checker.
///
/// Returns `None` when type-checking failed or a construct in an emitted body
/// is outside the current lowering subset (an error diagnostic is also
/// emitted).
pub fn emit_ir(
    prog: &Program,
    resolved: &ResolvedProgram,
    diags: &DiagnosticSink,
) -> Option<IrModule> {
    let types = crate::check::collect_expr_types(prog, resolved, diags);
    if diags.any_error() {
        return None;
    }
    let mut em = Emitter {
        prog,
        resolved,
        diags,
        types,
        module: IrModule::default(),
        consts_inits: HashMap::new(),
        global_ids: HashMap::new(),
        global_decls: HashMap::new(),
        global_failures: Vec::new(),
        managed_global_inits: Vec::new(),
        class_consts: HashMap::new(),
        const_inlining: Vec::new(),
        fid_list: Vec::new(),
        fsource: HashMap::new(),
        finfo: HashMap::new(),
        src_param_tys: HashMap::new(),
        fn_decls: HashMap::new(),
        instantiations: HashMap::new(),
        instanton_subst: HashMap::new(),
        classes: Vec::new(),
        class_by_name: HashMap::new(),
        class_decls: HashMap::new(),
        generic_type_members: HashMap::new(),
        struct_members: HashMap::new(),
        class_inst_by_args: HashMap::new(),
        class_inst_subst: HashMap::new(),
        fid_owner: HashMap::new(),
        fid_generics: HashMap::new(),
        generic_ctx_name: None,
        generic_fn_ctx: None,
        class_attempted: Vec::new(),
        ctor_ids: HashMap::new(),
        named_ctor_ids: HashMap::new(),
        deinit_ids: HashMap::new(),
        method_ids: HashMap::new(),
        virtual_dispatch: HashMap::new(),
        property_ids: HashMap::new(),
        static_property_ids: HashMap::new(),
        static_inits: Vec::new(),
        static_init_id: None,
        globals_init_id: None,
        test_setup_id: None,
        owner: None,
        closure_tables: HashMap::new(),
        lambda_fids: HashMap::new(),
        lambda_caps: HashMap::new(),
        lambda_owners: HashMap::new(),
        lambda_exprs: HashMap::new(),
        collect_writes: false,
        next_closure: 0,
        tramp_fids: HashMap::new(),
        spread_tramps: HashMap::new(),
        method_tramp_fids: HashMap::new(),
        fn_tramp: HashMap::new(),
        fname: String::new(),
        symbol: String::new(),
        current_subst: HashMap::new(),
        next_iface_id: 1,
        iface_inst_ids: HashMap::new(),
        iface_members: HashMap::new(),
        class_iface_buckets: HashMap::new(),
        current_fid: None,
        fn_generics: Vec::new(),
        fparams: Vec::new(),
        fret: IrTy::Unit,
        fslots: Vec::new(),
        env: Vec::new(),
        manual_env: Vec::new(),
        blocks: Vec::new(),
        cur: BlockId(0),
        next_block: 0,
        next_temp: 0,
        loops: Vec::new(),
        finallys: Vec::new(),
        handlers: Vec::new(),
        may_throw: HashMap::new(),
        prog_may_throw: false,
        analyzing: false,
        call_edges: HashMap::new(),
        direct_throw: HashSet::new(),
        suppressed: Vec::new(),
        in_throw_transfer: false,
        failed: false,
    };
    em.emit();
    // Globals that could not be lowered are reported here rather than at the
    // registration site so the rest of the program still emits (and so the
    // diagnostic can be emitted without borrowing conflicts).
    for (_name, span, msg) in &em.global_failures {
        em.diags
            .emit(Diagnostic::error_at(*span, msg.clone()));
    }
    if em.failed || em.diags.any_error() {
        None
    } else {
        Some(em.module)
    }
}

/// A `break`/`continue` target pair.
struct LoopCtx {
    continue_target: BlockId,
    break_target: BlockId,
}

/// An enclosing `catch` that a `throw` in the same function can branch to
/// directly, without a flag round-trip through the caller.
#[derive(Clone)]
struct HandlerCtx {
    /// First block of the handler body.
    target: BlockId,
    /// Index in [`Emitter::finallys`] of the `finally` belonging to the `try`
    /// that owns this handler, or the length at the time the handler was pushed
    /// when that `try` has no `finally`. Read together with [`Self::own_finally`]
    /// to find the first `finally` that is still owed on the way to the handler.
    fin_base: usize,
    /// Whether the owning `try` has a `finally` of its own. That one is *not*
    /// run on the way to the handler: `try_stmt` emits it once, after the
    /// handler body, on the path the branch takes. Without this flag the two
    /// cases are indistinguishable, because a `try` with no `finally` pushes
    /// nothing and leaves an inner `finally` sitting exactly at `fin_base`.
    own_finally: bool,
}

/// Where a registered function's body and signature come from.
#[derive(Clone)]
enum FnSource<'a> {
    /// Top-level `fn` (or `test`) declaration.
    TopLevel(&'a FnDecl),
    /// A lambda expression's hoisted body: slot 0 is the closure object, whose
    /// slots 1..N hold the captured values copied in at entry; the lambda's own
    /// parameters come after the closure object.
    Lambda {
        lambda: &'a Expr,
        captures: Vec<Capture>,
        /// The lambda's parameter types (substituted under the enclosing
        /// instantiation for a per-instantiation copy, and parallel to the
        /// lambda's `params` order).
        pty: Vec<Ty>,
        ret: Ty,
        owner: Option<i64>,
    },
    /// A visible `async fn`: a wrapper with the fn's real ABI that builds a
    /// closure over the caller-supplied parameters (the future) and returns it.
    /// Calling it runs no body code; the body runs only under `await`.
    AsyncFn { f: &'a FnDecl, hidden: FuncId },
    /// The hidden 0-arg future body of an `async fn`, sharing the hoisted
    /// lambda ABI: slot 0 is the closure object whose slots 1..N hold the async
    /// fn's parameters, read as named captures inside the body.
    AsyncBody {
        f: &'a FnDecl,
        pty: Vec<Ty>,
        ret: Ty,
    },
    /// A visible `async` method: the method itself, with its real
    /// (receiver-first for instance methods) ABI, building a closure over
    /// `this` (if any) plus the call parameters — the future — and returning
    /// it without running the body. The body runs only when `await` drives the
    /// future's 0-arg call.
    AsyncMethod {
        table: ClassTable,
        md: &'a MethodDecl,
        hidden: FuncId,
        is_static: bool,
        this_ty: Option<Ty>,
    },
    /// The hidden 0-arg future body of an `async` method. Its closure slots hold
    /// `this` (instance methods) then the parameters; `this` is re-declared as a
    /// named capture so the body's receiver-fallback (`self.owner`) applies,
    /// exactly as in a regular method body.
    AsyncMethodBody {
        table: ClassTable,
        md: &'a MethodDecl,
        pty: Vec<Ty>,
        ret: Ty,
        is_static: bool,
    },
    /// The constructor of a class/struct: synthesized from the fields unless an
    /// explicit `constructor(...)` body is present.
    Ctor {
        table: ClassTable,
    /// (slot, initializer) for each instance field with an initializer,
        /// in declaration order.
        inits: Vec<(usize, &'a Expr)>,
        /// `init { ... }` blocks, in declaration order.
        init_blocks: Vec<&'a Block>,
        /// The explicit `constructor(...) { ... }` declaration, if any.
        ctor: Option<&'a ConstructorDecl>,
    },
    /// A named constructor: a static factory that delegates to the primary
    /// constructor via `this(...)`.
    NamedCtor {
        table: ClassTable,
        ctor: &'a ConstructorDecl,
    },
    /// A `deinit` finalizer: runs at GC sweep on a dead instance, off the
    /// allocation path. Slot 0 is `this`; returns unit.
    Deinit { table: ClassTable, body: &'a Block },
    /// A class/struct method (instance or static).
    Method {
        table: ClassTable,
        md: &'a MethodDecl,
    },
    /// A property accessor: slot 0 is `this` (instance only; static
    /// properties are receiver-less), and setters take a `value` parameter,
    /// over the resolved property signature.
    Property {
        table: ClassTable,
        pd: &'a PropertyDecl,
        info: PropertyInfo,
        is_set: bool,
        is_static: bool,
    },
    /// The synthesized static-field initializer: one assignment per static
    /// field, evaluated once at program start. `None` means the field had no
    /// initializer and is stored as its default (zero/null) value.
    StaticInit {
        inits: Vec<(u32, usize, Option<&'a Expr>)>,
    },
    /// The synthesized `pkl_globals_init` function: registers a GC root for every
    /// managed top-level `var` and then stores its initializer. Managed globals
    /// have no static initializer bytes, so their value cannot exist before
    /// `main`; this is the first code that may allocate, so every slot is rooted
    /// first.
    GlobalsInit {
        inits: Vec<(usize, Expr)>,
    },
    /// A dynamically-callable forwarder for a top-level function used as a
    /// value. Slot 0 is the closure object (ignored; module functions capture
    /// nothing), the real arguments occupy slots 1..N, and the body calls
    /// `target` and returns its result. This gives a module function the same
    /// `(env, ...)` ABI as a hoisted lambda body, so `fn_value_call` can
    /// dispatch to it uniformly through a zero-capture closure object.
    Trampoline {
        span: Span,
        target: FuncId,
        pty: Vec<Ty>,
        ret: Ty,
    },
    /// A dynamically-callable forwarder for an instance method bound to a
    /// receiver. Slot 0 is the closure object, slot 1 holds the bound receiver
    /// (captured like a lambda capture), and the real arguments occupy slots
    /// 2..N; the body loads the receiver and calls `target` with it first, then
    /// returns the result. This gives a bound method the same `(env, ...)` ABI
    /// as a hoisted lambda body, so `fn_value_call` dispatches to it uniformly.
    /// When the method is overridden (`branches` non-empty), the body dispatches
    /// on the runtime class of the loaded receiver exactly like a virtual call.
    MethodTrampoline {
        span: Span,
        target: FuncId,
        pty: Vec<Ty>,
        ret: Ty,
        branches: Vec<(u32, FuncId)>,
    },
    /// The synthesized `pkl_test_setup` function: registers every user class
    /// and runs `pkl_static_init` before the test runner starts (test modules
    /// have no `main`, so the preamble cannot ride on one).
    TestSetup,
    /// A dynamically-callable forwarder for a fixed-arity call site that was
    /// written with a spread (`...list`). Slot 0 is the receiver for an
    /// instance-method target (else absent), the final slot holds a boxed
    /// `List` of the spread elements, and the intermediate slots carry the
    /// explicitly-supplied leading arguments, each boxed by its own parameter
    /// representation. The body unboxes every argument back into a freshly
    /// allocated spill slot (using the parameter's own representation), frees
    /// nothing, and calls `target` with the ordinary `(slot0, ...)` ABI.
    /// Registered once per target so every caller shares the dispatch.
    SpreadTrampoline {
        span: Span,
        target: FuncId,
        /// The target's full parameter types (receiver excluded), exactly as
        /// `src_param_tys` computed them at the original call site.
        pty: Vec<Ty>,
        ret: Ty,
        receiver: bool,
    },
}

/// A registered, lowerable user class/struct.
struct ClassPlan {
    name: String,
    class_id: u32,
    /// Superclass id for a class that `extends` another, else `None`.
    parent: Option<u32>,
    table: ClassTable,
}

/// One value a lambda body reads from its creating scope. Captures are copied
/// (snapshotted) into the closure object at creation time and are read-only
/// inside the hoisted body.
#[derive(Clone)]
struct Capture {
    name: String,
    ty: Ty,
    span: Span,
}

/// Closures may capture up to this many values; beyond it the synthetic
/// `__closure_N` layout (one slot per capture) is rejected loudly.
const MAX_LAMBDA_CAPTURES: usize = 8;

/// Runtime symbol names used to build and call closure values.
const CLOSURE_CLASS_PREFIX: &str = "__closure_";

/// The slot holding the hoisted body's code address inside a closure object.
const CLOSURE_FN_SLOT: usize = 0;

/// Runtime symbols for cooperative exception propagation. See the module
/// docs: a throw raises a per-thread flag that enclosing frames observe
/// after each call, and `finally` is lowered by duplication.
mod exc {
    /// Publish `payload` and raise this thread's pending flag.
    pub const THROW: &str = "pickle_exc_throw";
    /// Is a throw in flight on this thread?
    pub const PENDING: &str = "pickle_exc_pending";
    /// Consume the in-flight throw, returning its payload.
    pub const TAKE: &str = "pickle_exc_take";
}

/// How control leaves a block once its `finally` chain has run.
enum Exit {
    /// Leave the function with this value.
    Return(Option<Temp>),
    /// Jump to an existing block (a `break`/`continue` target).
    Branch(BlockId),
    /// Leave the function because a throw is propagating. A function with a
    /// result must still produce one, so a zero of that type is returned;
    /// the caller checks the pending flag before it reads the value.
    Propagate,
}

/// The number of parameters on a lambda expression.
fn params_len(lambda: &Expr) -> usize {
    if let ExprKind::Lambda { params, .. } = &lambda.kind {
        params.len()
    } else {
        0
    }
}

/// A lambda parameter list's bound names.
fn bound_params(params: &[Param]) -> HashSet<String> {
    params.iter().map(|p| p.name.clone()).collect()
}

/// Is `name` bound in one of the open lexical scopes at or above `from`?
/// Scopes below `from` belong to the enclosing function and are invisible to
/// a hoisted lambda body, so they are NOT treated as bound here (they must be
/// captured instead).
fn scope_contains_from(scope: &[HashSet<String>], from: usize, name: &str) -> bool {
    for (i, s) in scope.iter().enumerate().rev() {
        if i < from {
            break;
        }
        if s.contains(name) {
            return true;
        }
    }
    false
}

/// Bind `name` in the innermost open scope.
fn scope_bind(scope: &mut [HashSet<String>], name: &str) {
    if let Some(top) = scope.last_mut() {
        top.insert(name.to_string());
    }
}

/// The names a pattern binds (only `Pattern::Binding` introduces names).
fn pattern_binds(p: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    collect_binds(p, &mut out);
    out
}

fn collect_binds(p: &Pattern, out: &mut Vec<String>) {
    match p {
        Pattern::Binding { name, .. } => out.push(name.clone()),
        Pattern::Tuple(xs) | Pattern::Or(xs) => {
            for x in xs {
                collect_binds(x, out);
            }
        }
        Pattern::Variant { payloads, .. } => {
            for x in payloads {
                collect_binds(x, out);
            }
        }
        Pattern::Wildcard | Pattern::Literal(_) => {}
    }
}

/// The single `this(...)` delegation expression of a named-constructor body, or
/// `None` if the body is not exactly one such expression (as a statement or a
/// block tail).
fn named_ctor_delegation(body: &Block) -> Option<&Expr> {
    fn this_call(e: &Expr) -> Option<&Expr> {
        match &e.kind {
            ExprKind::Call { callee, .. } if matches!(&callee.kind, ExprKind::This) => Some(e),
            _ => None,
        }
    }
    match (body.stmts.as_slice(), body.expr.as_deref()) {
        ([], Some(e)) => this_call(e),
        ([Stmt::Expr(e)], None) => this_call(e),
        _ => None,
    }
}

/// The leading `super(...)` delegation expression of a primary-constructor
/// body, when it is the first statement. The call's arguments are evaluated
/// in the current function's scope and fed to the parent constructor.
fn ctor_super_delegation(body: &Block) -> Option<&Expr> {
    let e = match body.stmts.first() {
        Some(Stmt::Expr(e)) => e,
        _ => return None,
    };
    match &e.kind {
        ExprKind::Call { callee, .. } if matches!(&callee.kind, ExprKind::Super) => Some(e),
        _ => None,
    }
}

/// Static slot number of static field `name` (its position among static
/// fields).
fn static_field_slot(table: &ClassTable, name: &str) -> Option<usize> {
    table
        .fields
        .iter()
        .filter(|f| f.is_static)
        .position(|f| f.name == name)
}

/// The declared info of static field `name`, when present.
fn static_field_info(table: &ClassTable, name: &str) -> Option<FieldInfo> {
    table
        .fields
        .iter()
        .find(|f| f.is_static && f.name == name)
        .cloned()
}

/// One entry of the flat (task and synchronisation) builtin table: the runtime
/// symbol to call, the IR type of each argument, the IR type of the result (`None`
/// for a call that returns nothing), and the argument names used only in
/// diagnostics.
type FlatBuiltin = (
    &'static str,
    Vec<IrTy>,
    Option<IrTy>,
    &'static [&'static str],
);

/// Runtime representation of a `List<T>` element.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemRep {
    /// Pass the managed value through as a pointer (strings, classes, lists…).
    Ptr,
    /// Boxed/unboxed at the runtime boundary; `(box_sym, unbox_sym, ir_ty)`.
    Scalar(&'static str, &'static str, IrTy),
}

/// The IR type a `List<T>` element value carries outside the runtime.
fn elem_ir(elem: &Ty) -> IrTy {
    match elem {
        Ty::Int => IrTy::Int,
        Ty::Float => IrTy::Float,
        Ty::Bool => IrTy::Bool,
        Ty::Byte => IrTy::Int,
        Ty::Char => IrTy::Char,
        Ty::String => IrTy::Str,
        _ => IrTy::Ptr,
    }
}

struct Emitter<'a> {
    prog: &'a Program,
    resolved: &'a ResolvedProgram,
    diags: &'a DiagnosticSink,
    types: HashMap<Span, Ty>,
    module: IrModule,
    /// Top-level `const` name -> its initializer (inlined at use sites).
    consts_inits: HashMap<String, &'a Expr>,
    /// Top-level `var` name -> global id. Kept separate from `consts_inits`
    /// because a global is *not* inlined: it becomes a data symbol.
    global_ids: HashMap<String, usize>,
    /// Top-level `var` declarations by name, for type lookup during emission.
    global_decls: HashMap<String, &'a GlobalDecl>,
    /// Globals whose init could not be reduced to bytes, for diagnostics.
    global_failures: Vec<(String, Span, String)>,
    /// Managed globals as `(global id, initializer expr)`, in declaration
    /// order. Their value cannot be static bytes, so it is compiled into the
    /// synthetic `pkl_globals_init` function together with the
    /// `pickle_gc_root_add` registration for each slot.
    managed_global_inits: Vec<(usize, Expr)>,
    /// `(class id, const name)` -> declared info + initializer, inlined at use
    /// sites (`Type.NAME`, or the bare name inside the class body).
    class_consts: HashMap<(u32, String), (FieldInfo, &'a Expr)>,
    const_inlining: Vec<String>,
    /// Registered functions in emission order (mirrors `module.funcs`).
    fid_list: Vec<FuncId>,
    fsource: HashMap<FuncId, FnSource<'a>>,
    finfo: HashMap<FuncId, CallableInfo>,
    /// Source parameter types of every registered callable, by function id.
    /// These drive implicit borrows for `&T` reference parameters at callsites
    /// (the IR `IrFunc.params` are lowered `IrTy`s with no source type).
    src_param_tys: HashMap<FuncId, Vec<Ty>>,
    /// Declaration of each top-level `fn`/`test`, by name — consumed when a
    /// generic function is instantiated at a call site.
    fn_decls: HashMap<String, &'a FnDecl>,
    /// Generic function instantiation key -> its function id. The key is the
    /// callee name plus the concrete (monomorphized) type arguments.
    instantiations: HashMap<(String, Vec<Ty>), FuncId>,
    /// Function id -> substitution map applied while building it (empty for
    /// non-generic / not-yet-instantiated functions).
    instanton_subst: HashMap<FuncId, HashMap<String, Ty>>,
    /// Registered user classes in id order.
    classes: Vec<ClassPlan>,
    /// Class/struct name -> assigned runtime class id (registered only).
    class_by_name: HashMap<String, u32>,
    /// Class name -> its declaration (for ancestor field initializers /
    /// `init` blocks and ancestor-first registration).
    class_decls: HashMap<String, &'a ClassDecl>,
    /// Generic class/struct name -> its member declarations, stashed instead
    /// of being registered bare. Instantiations materialize on first use.
    generic_type_members: HashMap<String, &'a [ClassMember]>,
    /// Instantiation key (class/struct name, concrete type args) -> class id.
    class_inst_by_args: HashMap<(String, Vec<Ty>), u32>,
    /// Class id -> the generic substitution its members were lowered under
    /// (present only for generic class/struct instantiations).
    class_inst_subst: HashMap<u32, HashMap<String, Ty>>,
    /// Class/struct function id -> the class id it belongs to (so an
    /// instantiated method/ctor body builds under the right implicit receiver).
    fid_owner: HashMap<FuncId, i64>,
    /// Class/struct function id -> the declaring generic parameter names (so a
    /// nested generic call inside an instantiated member body can resolve
    /// `T`-style type arguments in its own namespace).
    fid_generics: HashMap<FuncId, Vec<String>>,
    /// While pre-registering lambdas, the generic class/struct whose members are
    /// being walked (lambdas inside generic bodies are rejected loudly).
    generic_ctx_name: Option<String>,
    /// While pre-registering lambdas, the generic top-level function whose body
    /// is being walked. Lambdas declared inside one cannot be registered yet
    /// (their signatures carry `Var`s), so they are stashed for a lazily-
    /// registered per-instantiation copy when the body lowers.
    generic_fn_ctx: Option<String>,
    /// Names whose registration has been attempted, so ancestors are pulled in
    /// before descendants exactly once (and cycles terminate).
    class_attempted: Vec<String>,
    /// Struct name -> its member declarations (field initializers and `init`
    /// blocks are collected from here, exactly as `class_decls` serves classes).
    struct_members: HashMap<String, &'a [ClassMember]>,
    /// Class id -> implicit-constructor function.
    ctor_ids: HashMap<u32, FuncId>,
    /// (Class id, named-constructor name) -> redirect function.
    named_ctor_ids: HashMap<(u32, String), FuncId>,
    /// Class id -> `deinit` finalizer function.
    deinit_ids: HashMap<u32, FuncId>,
    /// (Class id, method name) -> (function, is_static).
    method_ids: HashMap<(u32, String), (FuncId, bool)>,
    /// (Static receiver class id, method name) -> dispatch-cascade entries for
    /// an instance method that is overridden somewhere in the program, ordered
    /// deepest-derived first. Each entry is `(descendant class id, its own
    /// implementation)`. A call through an ancestor-typed receiver must route
    /// on the runtime class id (`pickle_class_is`), so it falls into the first
    /// branch whose class the receiver is an instance of; `super.m(...)` and
    /// non-overridden methods stay direct static calls. Absent here = direct.
    virtual_dispatch: HashMap<(u32, String), Vec<(u32, FuncId)>>,
    /// Next runtime interface id. Interface ids occupy their own integer
    /// space (0 is the "not an interface" sentinel): a class id never collides
    /// with an interface id because interfaces never appear as object class
    /// ids — they are consulted only via `pickle_class_implements` /
    /// `pickle_iface_method` tables.
    next_iface_id: u32,
    /// Interface instantiation key (bare name, concrete type args) -> runtime
    /// interface id. `Container<int>` and `Container<string>` are distinct
    /// ids, so `x is Container<int>` probes exactly the interfaces the class
    /// (transitively) implements.
    iface_inst_ids: HashMap<(String, Vec<Ty>), u32>,
    /// Interface id -> its members in declaration order: `(method name, method
    /// index)`. The index is the position among the interface's *methods*
    /// (property/const members are not dispatch calls).
    iface_members: HashMap<u32, Vec<(String, u32)>>,
    /// Class id -> its direct `implements` declarations: `(interface id,
    /// method-index -> the implementation's function)`. Interfaces a class
    /// inherits from a superclass are resolved at runtime by walking the
    /// parent chain, so only each class's own `implements` list is recorded
    /// here (mirroring `pickle_class_register`'s parent links).
    #[allow(clippy::type_complexity)]
    class_iface_buckets: HashMap<u32, Vec<(u32, Vec<(u32, FuncId)>)>>,
    /// (Class id, property name, is_setter) -> accessor function.
    property_ids: HashMap<(u32, String, bool), FuncId>,
    /// (Class id, static property name, is_setter) -> accessor function.
    static_property_ids: HashMap<(u32, String, bool), FuncId>,
    /// Static-field initializers in declaration order: `(class id, static slot,
    /// value expr)`. `None` means "no initializer" (store the default value).
    /// Emitted into the synthesized `pkl_static_init` function.
    static_inits: Vec<(u32, usize, Option<&'a Expr>)>,
    /// The synthesized static-initializer function, called once at the top of
    /// `main` after class registration.
    static_init_id: Option<FuncId>,
    /// The synthesized pkl_globals_init function, when the module has a
/// managed top-level `var`. The class-registration preamble calls it.
    globals_init_id: Option<FuncId>,
    /// The synthesized `pkl_test_setup` function, when the module has tests
    /// (and no `main` to carry the class-registration preamble).
    test_setup_id: Option<FuncId>,
    /// Class id of the method/ctor currently being built (implicit receiver).
    owner: Option<i64>,
    /// Synthetic `__closure_N` class tables, so field helpers (`table_of`,
    /// `all_instance_fields`, ...) resolve them exactly like user classes.
    closure_tables: HashMap<String, ClassTable>,
    /// A lambda expression keyed by the enclosing function that introduces it
    /// (`None` = module scope) -> its hoisted body function. Module-scope
    /// lambdas are assigned during pre-registration; a lambda declared inside a
    /// generic function body gets one entry per enclosing instantiation,
    /// registered lazily the first time that body lowers the lambda.
    lambda_fids: HashMap<(Option<FuncId>, Span), FuncId>,
    /// The lambda expression's captured values its closure object must load
    /// (parallel to `lambda_fids`).
    lambda_caps: HashMap<(Option<FuncId>, Span), Vec<Capture>>,
    /// A lambda expression's span -> the class owner id its declaring scope
    /// had at walk time, for per-instantiation copies registered during build.
    lambda_owners: HashMap<Span, Option<i64>>,
    /// A lambda expression's span -> its `&'a Expr`, re-fetched from the walk
    /// when a per-instantiation copy registers during build (the build-time
    /// expression handle is not known to live for `'a`).
    lambda_exprs: HashMap<Span, &'a Expr>,
    /// While re-walking a lambda body to collect its assigned bare names, the
    /// walk pushes `(name, span)` write targets into the accumulator instead of
    /// free-name reads. The mutable-capture guard bails on any capture that is
    /// also an assignment target (a hoisted body would mutate a snapshot).
    collect_writes: bool,
    /// Monotonic id for hoisted lambda body names (`pkl_closure_<n>`).
    next_closure: u32,
    /// Top-level function id -> its value-use trampoline (`FnSource::Trampoline`),
    /// so a function referenced as a value is wrapped once regardless of how many
    /// sites reference it.
    tramp_fids: HashMap<FuncId, FuncId>,
    /// Target function id -> its spread-trampoline (`FnSource::SpreadTrampoline`).
    /// One trampoline per target serves every call site that spread a
    /// (`...list`) argument into that target.
    spread_tramps: HashMap<FuncId, FuncId>,
    /// Method bound-value trampolines, keyed by (static receiver class, method
    /// name, resolved fid): a bound instance method used as a value needs one
    /// forwarder per receiver class/method, and the forwarder dispatches
    /// virtually when that method is overridden.
    method_tramp_fids: HashMap<(u32, String, FuncId), FuncId>,
    /// Span of a module-fn value reference -> its trampoline function, assigned
    /// during pre-registration (parallel to `lambda_fids`).
    fn_tramp: HashMap<Span, FuncId>,

    // ---- per-function state ----
    fname: String,
    symbol: String,
    /// Generic parameter substitutions in effect while building the current
    /// function body (empty outside an instantiated generic function). Every
    /// inferred type read out of the checker's `types` map is substituted
    /// through this before being mapped to IR.
    current_subst: HashMap<String, Ty>,
    /// The function currently being built (`None` during pre-registration).
    /// Keys per-instantiation lambda copies so each generic instantiation that
    /// lowers the same lambda expression emits its own hoisted body.
    current_fid: Option<FuncId>,
    /// The current function's generic parameter names, needed to resolve the
    /// type arguments of a nested generic call in its body.
    fn_generics: Vec<String>,
    fparams: Vec<IrParam>,
    fret: IrTy,
    fslots: Vec<IrTy>,
    /// Lexical scopes; bottom is outermost (params live there).
    env: Vec<HashMap<String, Slot>>,
    /// Names bound with `#[manualAlloc]`, parallel to `env`.
    manual_env: Vec<HashSet<String>>,
    blocks: Vec<IrBlock>,
    cur: BlockId,
    next_block: u32,
    next_temp: u32,
    loops: Vec<LoopCtx>,
    /// `finally` blocks in scope, outermost first. A `return`, `break`,
    /// `continue`, or propagating `throw` re-emits each one on the way out.
    /// Held by value: a `finally` has to outlive the borrow of the statement
    /// that introduced it, and `stmt` is handed borrows shorter than `'a`.
    finallys: Vec<Block>,
    /// Enclosing `catch`es, innermost last, while their `try` body is emitted.
    handlers: Vec<HandlerCtx>,
    /// Functions that can propagate a throw, by [`FuncId`], as solved by
    /// [`Emitter::solve_may_throw`]. An id that is absent is treated as
    /// propagating, so this map only ever removes checks.
    may_throw: HashMap<FuncId, bool>,
    /// Whether the program contains a `throw` at all, read off the analysis
    /// pass. When false no call site can propagate one, so no propagation check
    /// is emitted anywhere.
    prog_may_throw: bool,
    /// True while the analysis pass is running.
    ///
    /// Emission runs twice: once to *discover* the call graph with no checks
    /// emitted, then again for real once the throwing set is known. The first
    /// pass only records, so a program that fails to lower reports its
    /// diagnostic once (from the second pass) rather than twice.
    analyzing: bool,
    /// Caller -> callees, recorded by [`Emitter::guard_propagation`] during the
    /// analysis pass. Only ever a superset of the real call graph, which is what
    /// makes the fixed point sound: a missing check would lose a throw, so every
    /// edge that might exist is kept.
    call_edges: HashMap<FuncId, Vec<FuncId>>,
    /// Functions whose own body contains a `throw`, recorded during the
    /// analysis pass. These seed the fixed point.
    direct_throw: HashSet<FuncId>,
    /// Diagnostics raised during the analysis pass, replayed only if that pass
    /// is the one that fails.
    suppressed: Vec<Diagnostic>,
    /// True while emitting a `finally` that runs *because* of a pending throw.
    ///
    /// The flag is already set on those paths, so a propagation check after any
    /// call in the `finally` would read the throw we are already handling and
    /// divert out of the transfer we are in the middle of. Suppressing the
    /// checks keeps the transfer straight through; a call in the `finally` that
    /// throws anyway replaces the payload, which is the same rule the handler
    /// path already relies on.
    in_throw_transfer: bool,
    failed: bool,
}

/// A spread call target captured at each instantiation site, bundling the
/// parameters, result type, optional receiver, and argument list so the
/// lowering entry point stays under the clippy argument-count limit.
struct SpreadCall<'a> {
    span: Span,
    fid: FuncId,
    pty: &'a [Ty],
    ret: &'a Ty,
    receiver: Option<Temp>,
    args: &'a [CallArg],
}

/// The receiver plus generic-instantiation context for one async method.
///
/// Grouped for the same reason as [`SpreadCall`]: carrying these as loose
/// parameters made `register_async_method` an eleven-argument list whose call
/// sites had to be read against the signature to tell `args` from `generics`.
struct AsyncRecv<'a> {
    class: &'a str,
    cid: u32,
    args: &'a [Ty],
    is_struct: bool,
    generics: Vec<String>,
}

impl<'a> Emitter<'a> {
    fn emit(&mut self) {
        for item in &self.prog.items {
            if let ItemKind::Const(c) = &item.kind {
                self.consts_inits.insert(c.name.clone(), &c.value);
            }
            if let ItemKind::Global(g) = &item.kind {
                self.global_decls.insert(g.name.clone(), g);
            }
            if let ItemKind::Fn(f) | ItemKind::Test(f) = &item.kind {
                self.fn_decls.insert(f.name.clone(), f);
            }
            if let ItemKind::Class(c) = &item.kind {
                self.class_decls.insert(c.name.clone(), c);
            }
        }
        let prog = self.prog;
        // Globals must be registered before any body is walked, because a body
        // may read or write one by bare name.
        self.register_globals();
        for item in &prog.items {
            match &item.kind {
                ItemKind::Fn(f) => self.register_fn(f, false, &item.attrs),
                ItemKind::Test(f) => self.register_fn(f, true, &item.attrs),
                ItemKind::Class(c) => self.register_class_by_name(&c.name),
                ItemKind::Struct(s) => self.register_struct_item(s),
                _ => {}
            }
        }
        // Every class is now registered and `method_ids` holds each method's
        // static resolution. Discover which instance methods are overridden
        // somewhere in the program so calls through an ancestor-typed receiver
        // can route them on the runtime class id.
        self.build_virtual_dispatch();
        // Static fields are initialized once at program start. Register the
        // initializer only after every class id is assigned, and before the
        // build loop so `main`'s preamble can call it.
        if !self.static_inits.is_empty() {
            let inits = std::mem::take(&mut self.static_inits);
            let fid = self.push_class_func(
                "static.init",
                "pkl_static_init",
                FnSource::StaticInit { inits },
            );
            self.static_init_id = Some(fid);
        }
        // Managed top-level `var`s get the same treatment: a synthesized function
        // that roots each slot and stores its initializer, called from the same
        // preamble as `pkl_static_init`. Roots are registered before static-field
        // initializers run, because those may allocate while a managed global is
        // still null.
        if !self.managed_global_inits.is_empty() {
            let inits = std::mem::take(&mut self.managed_global_inits);
            let fid = self.push_class_func(
                "globals.init",
                "pkl_globals_init",
                FnSource::GlobalsInit { inits },
            );
            self.globals_init_id = Some(fid);
        }
        // Test modules have no `main`, so the class-registration preamble
        // cannot ride on one. Synthesize a `pkl_test_setup` the runner calls
        // before executing hooks/tests (only when there is anything to set up).
        let has_tests = prog
            .items
            .iter()
            .any(|i| matches!(i.kind, ItemKind::Test(_)));
        if has_tests
            && (!self.classes.is_empty()
                || self.static_init_id.is_some()
                || self.globals_init_id.is_some())
        {
            let fid = self.push_class_func("test.setup", "pkl_test_setup", FnSource::TestSetup);
            self.test_setup_id = Some(fid);
        }
        self.register_lambdas();
        // Pass 1: lower every body with the propagation checks suppressed,
        // purely to record which function calls which and which bodies contain
        // a `throw` of their own. Instantiated generics and the synthesized
        // trampolines are registered lazily *during* this loop, so this is also
        // the pass that discovers them; the loop keeps consuming newly appended
        // ids rather than snapshotting the list up front.
        //
        // A program with no `throw` anywhere cannot propagate one, so it needs no
        // check at any call site. That fact is read back off `direct_throw`
        // rather than from the parser's own global, which is shared mutable
        // state: two compilations in one process could interleave and leave it
        // describing the other program. What was actually lowered cannot.
        self.analyzing = true;
        self.build_all();
        self.analyzing = false;
        if self.failed {
            // Nothing to re-lower: report what the analysis pass found.
            for d in std::mem::take(&mut self.suppressed) {
                self.diags.emit(d);
            }
            return;
        }
        self.suppressed.clear();
        self.prog_may_throw = !self.direct_throw.is_empty();
        if self.prog_may_throw {
            self.solve_may_throw();
        }
        // Pass 2: the real thing. `build_func` replaces each function entry
        // wholesale, and every cache that hands out a `FuncId` is keyed, so
        // re-running the loop over the same ids overwrites pass 1 cleanly
        // instead of appending to it.
        self.build_all();
    }

    /// Lower every registered function body, including any registered lazily by
    /// an earlier one.
    fn build_all(&mut self) {
        let mut ix = 0;
        while ix < self.fid_list.len() {
            let fid = self.fid_list[ix];
            self.build_func(fid);
            ix += 1;
        }
    }

    /// Solve the least fixed point of "can propagate" over the recorded call
    /// graph.
    ///
    /// The seeds are the bodies that contain a `throw` themselves; every caller
    /// of a throwing function then inherits it, transitively. Running to a fixed
    /// point matters because the graph has cycles (a recursive throwing function
    /// seeds itself through its own edge).
    ///
    /// The result records an answer for *every* function the analysis pass
    /// lowered, including the ones that came out `false`. That is the whole
    /// point: [`Emitter::check_propagates`] treats an absent id as propagating,
    /// so recording only the throwing set would leave every safe callee looking
    /// unknown and every call site checked. Only a function the pass never
    /// reached stays absent, and that can only cost a branch.
    fn solve_may_throw(&mut self) {
        let mut throws: HashSet<FuncId> = self.direct_throw.clone();
        loop {
            let mut changed = false;
            for (caller, callees) in &self.call_edges {
                if throws.contains(caller) {
                    continue;
                }
                if callees.iter().any(|c| throws.contains(c)) {
                    throws.insert(*caller);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.may_throw = self
            .fid_list
            .iter()
            .map(|fid| (*fid, throws.contains(fid)))
            .collect();
    }

    // ---- lambda pre-registration ----------------------------------------

    /// Collect every lambda in the program and register its hoisted body and
    /// closure class BEFORE the build loop, so a `Const(FuncAddr)` for a
    /// hoisted body and a `pickle_class_new` for its closure object can always
    /// reference ids that already exist. Runs after every class/function is
    /// registered (class ids drive owner-scoped capture decisions).
    fn register_lambdas(&mut self) {
        for item in &self.prog.items {
            match &item.kind {
                ItemKind::Fn(f) | ItemKind::Test(f) => {
                    let mut scope = Vec::new();
                    let mut acc = Vec::new();
                    if let Some(b) = &f.body {
                        let generic = !f.generics.is_empty();
                        let saved = self.generic_fn_ctx.clone();
                        if generic {
                            self.generic_fn_ctx = Some(f.name.clone());
                        }
                        self.walk_fn_body(b, None, None, 0, &mut scope, &mut acc);
                        self.generic_fn_ctx = saved;
                    }
                }
                ItemKind::Class(c) => self.walk_class_members(&c.members, &c.name),
                ItemKind::Struct(s) => self.walk_class_members(&s.members, &s.name),
                ItemKind::Const(c) => {
                    let mut scope = Vec::new();
                    let mut acc = Vec::new();
                    self.walk_expr(&c.value, None, None, 0, &mut scope, &mut acc);
                }
                _ => {}
            }
        }
    }

    /// A generic class/struct member body's types carry `Var`s that only an
    /// instantiation can substitute, so a lambda declared inside one cannot be
    /// lowered yet (its registration happens before instantiations exist). Track
    /// the generic class whose members are being walked so `walk_expr` can
    /// reject them loudly.
    fn generic_class_ctx(&self, cls: &str) -> bool {
        self.resolved.types.get(cls).is_some_and(|t| match t {
            TypeTableEntry::Class(c) => !c.generics.is_empty(),
            TypeTableEntry::Struct(s) => !s.generics.is_empty(),
            _ => false,
        })
    }

    fn walk_class_members(&mut self, members: &'a [ClassMember], cls: &str) {
        let generic_ctx = self.generic_class_ctx(cls);
        let saved_ctx = self.generic_ctx_name.clone();
        if generic_ctx {
            self.generic_ctx_name = Some(cls.to_string());
        }
        let owner_cid = self.class_by_name.get(cls).copied().map(|x| x as i64);
        for m in members {
            let mut scope = Vec::new();
            let mut acc = Vec::new();
            match m {
                ClassMember::Field { init: Some(e), .. } => {
                    self.walk_expr(e, owner_cid, Some(cls), 0, &mut scope, &mut acc)
                }
                ClassMember::Field { .. } | ClassMember::Constructor(_) => {}
                ClassMember::Method(md) => {
                    if let Some(b) = &md.body {
                        self.walk_fn_body(b, owner_cid, Some(cls), 0, &mut scope, &mut acc);
                    }
                }
                ClassMember::Property(pd) => {
                    if let Some(a) = &pd.get {
                        self.walk_accessor(a, owner_cid, Some(cls));
                    }
                    if let Some(a) = &pd.set {
                        self.walk_accessor(a, owner_cid, Some(cls));
                    }
                }
                ClassMember::Init(b) | ClassMember::Deinit(b) => {
                    self.walk_block(b, owner_cid, Some(cls), 0, &mut scope, &mut acc)
                }
                ClassMember::Const { value, .. } => {
                    self.walk_expr(value, owner_cid, Some(cls), 0, &mut scope, &mut acc)
                }
            }
        }
        // Explicit constructor bodies carry lambdas too (initializer-only ctors
        // have none).
        for m in members {
            if let ClassMember::Constructor(cd) = m {
                let mut scope = Vec::new();
                let mut acc = Vec::new();
                self.walk_block(&cd.body, owner_cid, Some(cls), 0, &mut scope, &mut acc);
            }
        }
        self.generic_ctx_name = saved_ctx;
    }

    fn walk_accessor(
        &mut self,
        a: &'a PropertyAccessor,
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
    ) {
        let mut scope = Vec::new();
        let mut acc = Vec::new();
        match a {
            PropertyAccessor::Expr(e) => {
                self.walk_expr(e, owner_cid, owner_name, 0, &mut scope, &mut acc)
            }
            PropertyAccessor::Block(b) => {
                self.walk_block(b, owner_cid, owner_name, 0, &mut scope, &mut acc)
            }
        }
    }

    fn walk_fn_body(
        &mut self,
        fb: &'a FnBody,
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
        lmark: usize,
        scope: &mut Vec<HashSet<String>>,
        acc: &mut Vec<(String, Span)>,
    ) {
        match fb {
            FnBody::Block(b) => self.walk_block(b, owner_cid, owner_name, lmark, scope, acc),
            FnBody::Expr(e) => self.walk_expr(e, owner_cid, owner_name, lmark, scope, acc),
        }
    }

    fn walk_block(
        &mut self,
        b: &'a Block,
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
        lmark: usize,
        scope: &mut Vec<HashSet<String>>,
        acc: &mut Vec<(String, Span)>,
    ) {
        for s in &b.stmts {
            self.walk_stmt(s, owner_cid, owner_name, lmark, scope, acc);
        }
        if let Some(e) = &b.expr {
            self.walk_expr(e, owner_cid, owner_name, lmark, scope, acc);
        }
    }

    fn walk_stmt(
        &mut self,
        s: &'a Stmt,
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
        lmark: usize,
        scope: &mut Vec<HashSet<String>>,
        acc: &mut Vec<(String, Span)>,
    ) {
        match s {
            Stmt::Let {
                pattern,
                init: Some(e),
                ..
            } => {
                self.walk_expr(e, owner_cid, owner_name, lmark, scope, acc);
                for n in pattern_binds(pattern) {
                    scope_bind(scope, &n);
                }
            }
            Stmt::Let { .. } => {}
            Stmt::Const { value, name, .. } => {
                self.walk_expr(value, owner_cid, owner_name, lmark, scope, acc);
                scope_bind(scope, name);
            }
            Stmt::Return { value: Some(e), .. } => {
                self.walk_expr(e, owner_cid, owner_name, lmark, scope, acc)
            }
            Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Empty(_) => {}
            Stmt::Throw { value, .. } => {
                self.walk_expr(value, owner_cid, owner_name, lmark, scope, acc)
            }
            Stmt::Try {
                body,
                binder,
                handler,
                finally,
                ..
            } => {
                scope.push(HashSet::new());
                self.walk_block(body, owner_cid, owner_name, lmark, scope, acc);
                scope.pop();
                if let Some(handler) = handler {
                    scope.push(HashSet::new());
                    if let Some(name) = binder {
                        scope_bind(scope, name);
                    }
                    self.walk_block(handler, owner_cid, owner_name, lmark, scope, acc);
                    scope.pop();
                }
                if let Some(finally) = finally {
                    scope.push(HashSet::new());
                    self.walk_block(finally, owner_cid, owner_name, lmark, scope, acc);
                    scope.pop();
                }
            }
            Stmt::Expr(e) => self.walk_expr(e, owner_cid, owner_name, lmark, scope, acc),
            Stmt::While { cond, body, .. } => {
                self.walk_expr(cond, owner_cid, owner_name, lmark, scope, acc);
                scope.push(HashSet::new());
                self.walk_block(body, owner_cid, owner_name, lmark, scope, acc);
                scope.pop();
            }
            Stmt::For { header, body, .. } => match header {
                ForHeader::In { sequence, pattern } => {
                    self.walk_expr(sequence, owner_cid, owner_name, lmark, scope, acc);
                    scope.push(HashSet::new());
                    for n in pattern_binds(pattern) {
                        scope_bind(scope, &n);
                    }
                    self.walk_block(body, owner_cid, owner_name, lmark, scope, acc);
                    scope.pop();
                }
                ForHeader::Range { init, cond, step } => {
                    scope.push(HashSet::new());
                    self.walk_stmt(init, owner_cid, owner_name, lmark, scope, acc);
                    self.walk_expr(cond, owner_cid, owner_name, lmark, scope, acc);
                    self.walk_expr(step, owner_cid, owner_name, lmark, scope, acc);
                    self.walk_block(body, owner_cid, owner_name, lmark, scope, acc);
                    scope.pop();
                }
            },
        }
    }

    fn walk_expr(
        &mut self,
        e: &'a Expr,
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
        lmark: usize,
        scope: &mut Vec<HashSet<String>>,
        acc: &mut Vec<(String, Span)>,
    ) {
        match &e.kind {
            ExprKind::Ident(name) => {
                if self.collect_writes {
                    return;
                }
                if !scope_contains_from(scope, lmark, name) {
                    acc.push((name.clone(), e.span));
                }
                // A module function used as a value needs a dynamic-call
                // trampoline: the hoisted lambda bodies accept `(env, ...)`,
                // but a bare top-level function does not. Register it now so the
                // build loop (which runs after registration) compiles its body.
                let shadowed = scope.last().map(|s| s.contains(name)).unwrap_or(false);
                if !shadowed
                    && self.module.funcs_by_name.contains_key(name)
                    && matches!(self.types.get(&e.span), Some(Ty::Fn(..)))
                {
                    self.register_trampoline(e.span, name);
                }
            }
            ExprKind::This => acc.push(("this".to_string(), e.span)),
            ExprKind::Super => acc.push(("__super".to_string(), e.span)),
            ExprKind::Lit(_) | ExprKind::GenericCall { .. } => {}
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee, owner_cid, owner_name, lmark, scope, acc);
                for a in args {
                    self.walk_expr(&a.value, owner_cid, owner_name, lmark, scope, acc);
                }
            }
            ExprKind::Member { object, name } => {
                self.walk_expr(object, owner_cid, owner_name, lmark, scope, acc);
                // An instance method used as a bound value (`c.method`) needs a
                // method trampoline: its closure object carries the receiver in
                // slot 1, and the forwarder calls the real method with it.
                // `method_call` handles direct `c.method(...)` calls (which also
                // walk the member), so registering here is harmless: the
                // trampoline is only consumed at value-use sites.
                let ot = self.ty_of(&object.span);
                if let Some(otv) = ot {
                    if matches!(otv, Ty::Class(..) | Ty::Struct(..)) {
                        if let Ok(Some(cid)) = self.class_id_of(&otv, object.span) {
                            if let Some(&(fid, is_static)) =
                                self.method_ids.get(&(cid, name.clone()))
                            {
                                if !is_static && matches!(self.types.get(&e.span), Some(Ty::Fn(..)))
                                {
                                    self.register_method_trampoline(e.span, fid, cid, name);
                                }
                            }
                        }
                    }
                }
            }
            ExprKind::Index { object, index } => {
                self.walk_expr(object, owner_cid, owner_name, lmark, scope, acc);
                self.walk_expr(index, owner_cid, owner_name, lmark, scope, acc);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs, owner_cid, owner_name, lmark, scope, acc);
                self.walk_expr(rhs, owner_cid, owner_name, lmark, scope, acc);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_expr(operand, owner_cid, owner_name, lmark, scope, acc)
            }
            ExprKind::Assign { target, value, .. } => {
                if self.collect_writes {
                    if let ExprKind::Ident(name) = &target.kind {
                        if !scope_contains_from(scope, lmark, name) {
                            acc.push((name.clone(), target.span));
                        }
                    }
                    return;
                }
                self.walk_expr(target, owner_cid, owner_name, lmark, scope, acc);
                self.walk_expr(value, owner_cid, owner_name, lmark, scope, acc);
            }
            ExprKind::Lambda {
                params,
                is_async,
                body,
                ..
            } => {
                if self.collect_writes {
                    // Writes inside a nested lambda belong to that lambda's own
                    // guard, not the enclosing one.
                    return;
                }
                if let Some(cls) = &self.generic_ctx_name {
                    let _ = self.bad::<()>(
                        e.span,
                        format!(
                            "a lambda inside the generic class/struct `{cls}` is not lowered yet"
                        ),
                    );
                    return;
                }
                if *is_async {
                    let _ = self.bad::<()>(e.span, "async lambdas are not lowered yet");
                    return;
                }
                for p in params {
                    if p.rest || p.default.is_some() {
                        let _ = self.bad::<()>(
                            p.span,
                            "lambda parameters cannot have defaults or rest markers",
                        );
                        return;
                    }
                }
                // Free names are collected against THIS lambda's own binding
                // boundary: its parameter scope (and anything it binds below)
                // is not captured, while every enclosing name — including the
                // enclosing function's locals/params AND enclosing lambda
                // internals — must be captured, because a hoisted body is a
                // separate function that only sees its own closure slots.
                let child_marker = scope.len();
                let before = acc.len();
                scope.push(bound_params(params));
                self.walk_fn_body(body, owner_cid, owner_name, child_marker, scope, acc);
                let mut mine = acc.split_off(before);
                let free_names = std::mem::take(&mut mine);
                // A second pass over the same body collects the names the
                // lambda assigns as bare identifiers. A captured binding is
                // snapshotted into the closure object and re-copied into a
                // fresh slot on every call, so assigning to one inside the
                // hoisted body would mutate a dead copy on every invocation.
                // Reject it loudly instead of silently miscompiling (the
                // escape hatch is to mutate a captured *object*'s field, which
                // is shared by reference).
                let before = acc.len();
                self.collect_writes = true;
                self.walk_fn_body(body, owner_cid, owner_name, child_marker, scope, acc);
                self.collect_writes = false;
                let written = acc.split_off(before);
                for (name, refs) in written {
                    if self.is_enclosing_global(&name) {
                        continue;
                    }
                    if let Some(cid) = owner_cid {
                        if self.instance_field_index(cid, &name).is_some()
                            || self.property_ids.contains_key(&(
                                cid as u32,
                                name.to_string(),
                                false,
                            ))
                            || self.static_field(cid, &name).is_some()
                            || self.class_const_defined(cid, &name)
                        {
                            continue;
                        }
                    }
                    let _ = self.bad::<()>(
                        refs,
                        format!(
                            "assigning to `{name}` inside a lambda is not lowered yet: it is captured by value, and a hoisted body cannot mutate the enclosing binding (assign through a captured object's field instead)"
                        ),
                    );
                    return;
                }
                scope.pop();
                let captures = self.decide_captures(&free_names, owner_cid, owner_name, e.span);
                let captures = match captures {
                    Ok(c) => c,
                    Err(()) => return,
                };
                if self.generic_fn_ctx.is_some() {
                    // Lambdas declared inside a generic function body cannot be
                    // registered during the walk: their signatures carry `Var`s
                    // only an instantiation can substitute. Stash the captures
                    // and owner so a per-instantiation copy registers lazily the
                    // first time the lowered body reaches this lambda.
                    self.lambda_caps.insert((None, e.span), captures);
                    self.lambda_owners.insert(e.span, owner_cid);
                    self.lambda_exprs.insert(e.span, e);
                } else {
                    self.register_lambda(e, captures, owner_cid);
                }
                // Propagate to the parent only the names the parent must also
                // capture: those it cannot already see in ITS own scopes. A
                // name bound in the parent-lambda's internals lives in the
                // parent hoisted body's env at our creation site, so the
                // parent must NOT re-capture it.
                for (n, s) in &free_names {
                    if !scope_contains_from(scope, lmark, n) {
                        acc.push((n.clone(), *s));
                    }
                }
            }
            ExprKind::If {
                cond,
                then,
                else_else,
            } => {
                match cond {
                    IfCond::Cond(c) => {
                        self.walk_expr(c, owner_cid, owner_name, lmark, scope, acc);
                        self.walk_block(then, owner_cid, owner_name, lmark, scope, acc);
                    }
                    IfCond::Binding { pattern, value } => {
                        self.walk_expr(value, owner_cid, owner_name, lmark, scope, acc);
                        scope.push(HashSet::new());
                        for n in pattern_binds(pattern) {
                            scope_bind(scope, &n);
                        }
                        self.walk_block(then, owner_cid, owner_name, lmark, scope, acc);
                        scope.pop();
                    }
                }
                if let Some(ee) = else_else {
                    self.walk_expr(ee, owner_cid, owner_name, lmark, scope, acc);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee, owner_cid, owner_name, lmark, scope, acc);
                for arm in arms {
                    scope.push(HashSet::new());
                    for n in pattern_binds(&arm.pattern) {
                        scope_bind(scope, &n);
                    }
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g, owner_cid, owner_name, lmark, scope, acc);
                    }
                    self.walk_expr(&arm.body, owner_cid, owner_name, lmark, scope, acc);
                    scope.pop();
                }
            }
            ExprKind::Await(x) => self.walk_expr(x, owner_cid, owner_name, lmark, scope, acc),
            ExprKind::Cast { expr, .. } => {
                self.walk_expr(expr, owner_cid, owner_name, lmark, scope, acc)
            }
            ExprKind::Unsafe(b) => {
                scope.push(HashSet::new());
                self.walk_block(b, owner_cid, owner_name, lmark, scope, acc);
                scope.pop();
            }
            ExprKind::Block(b) => {
                scope.push(HashSet::new());
                self.walk_block(b, owner_cid, owner_name, lmark, scope, acc);
                scope.pop();
            }
            ExprKind::Tuple(xs) | ExprKind::Array(xs) => {
                for x in xs {
                    self.walk_expr(x, owner_cid, owner_name, lmark, scope, acc);
                }
            }
            ExprKind::Map(kvs) => {
                for (k, v) in kvs {
                    self.walk_expr(k, owner_cid, owner_name, lmark, scope, acc);
                    self.walk_expr(v, owner_cid, owner_name, lmark, scope, acc);
                }
            }
            ExprKind::OptAccess { object, .. } => {
                self.walk_expr(object, owner_cid, owner_name, lmark, scope, acc)
            }
            ExprKind::OptUnwrap(inner) => {
                self.walk_expr(inner, owner_cid, owner_name, lmark, scope, acc)
            }
            ExprKind::Range { start, end, .. } => {
                self.walk_expr(start, owner_cid, owner_name, lmark, scope, acc);
                self.walk_expr(end, owner_cid, owner_name, lmark, scope, acc);
            }
        }
    }

    /// Decide which free names are truly captured (globals and the enclosing
    /// class's statics/consts resolve statically), the capture's type, and
    /// whether the implicit `this` must ride along.
    fn decide_captures(
        &mut self,
        free: &[(String, Span)],
        owner_cid: Option<i64>,
        owner_name: Option<&str>,
        span: Span,
    ) -> Result<Vec<Capture>, ()> {
        let mut seen = HashSet::new();
        let mut caps: Vec<Capture> = Vec::new();
        let mut needs_this = false;
        for (name, refs) in free {
            if name == "__super" {
                return self.bad(*refs, "`super` inside a lambda is not lowered yet");
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if self.is_enclosing_global(name) {
                continue;
            }
            if let Some(cid) = owner_cid {
                // Instance members resolve through the receiver; the lambda
                // needs the object (captured as `this`).
                if self.instance_field_index(cid, name).is_some()
                    || self
                        .property_ids
                        .contains_key(&(cid as u32, name.to_string(), false))
                    || self
                        .method_ids
                        .get(&(cid as u32, name.to_string()))
                        .map(|&(_, is_static)| !is_static)
                        .unwrap_or(false)
                {
                    needs_this = true;
                    continue;
                }
                // Statics/consts resolve by the declaring class alone.
                if self.static_field(cid, name).is_some() || self.class_const_defined(cid, name) {
                    continue;
                }
            }
            let ty = match self.types.get(refs) {
                Some(t) => t.clone(),
                None => {
                    return self.bad(*refs, format!("cannot infer the type of captured `{name}`"))
                }
            };
            caps.push(Capture {
                name: name.clone(),
                ty,
                span: *refs,
            });
        }
        if needs_this {
            let Some(oname) = owner_name else {
                unreachable!("owner member with no owner name");
            };
            caps.push(Capture {
                name: "this".to_string(),
                ty: Ty::Class(oname.to_string(), Vec::new()),
                span,
            });
        }
        // Deterministic extraction order (slot assignment order) regardless of
        // the walk's traversal order.
        caps.sort_by(|a, b| a.name.cmp(&b.name));
        if caps.len() > MAX_LAMBDA_CAPTURES {
            return self.bad(
                span,
                format!(
                    "this lambda captures {} values, more than the supported limit of {MAX_LAMBDA_CAPTURES}",
                    caps.len()
                ),
            );
        }
        Ok(caps)
    }

    /// Whether a free name inside a lambda body refers to something outside it
    /// (a top-level `fn`, a class, a `const`) rather than to a capture.
    fn is_enclosing_global(&self, name: &str) -> bool {
        self.module.funcs_by_name.contains_key(name)
            || self.class_decls.contains_key(name)
            || self.consts_inits.contains_key(name)
            // Every builtin callable is seeded by the resolver with a synthetic
            // span (file 0, empty range), which is what tells a builtin apart
            // from a user's top-level `fn` of the same name. Asking the resolver
            // keeps this from becoming a second list of every builtin.
            || self.resolved.fns.get(name).is_some_and(|infos| {
                infos.iter().any(|c| c.span.file.0 == 0 && c.span.end == 0)
            })
            // These three are dispatched without a `CallableInfo` entry (raw
            // allocation and the test helper), so they are named here.
            || matches!(name, "alloc" | "free" | "expect")
    }

    /// Register a lambda's hoisted body and its closure class.
    fn register_lambda(
        &mut self,
        lambda: &'a Expr,
        captures: Vec<Capture>,
        owner_cid: Option<i64>,
    ) {
        self.register_lambda_at(lambda, captures, owner_cid, &HashMap::new(), None);
    }

    /// Register a hoisted lambda body under the (enclosing function, span) key
    /// `base`, substituting `subst` into the signature, captures, and return
    /// type. Module-scope lambdas register during the walk with an empty
    /// substitution and `base == None`; a lambda declared inside a generic
    /// function body registers lazily once per enclosing instantiation with
    /// that instantiation's substitution and `base == Some(inst fid)`. The
    /// copy's body then builds under the same substitution (`instanton_subst`),
    /// so every `Var` inside it resolves to the concrete instantiation.
    fn register_lambda_at(
        &mut self,
        lambda: &'a Expr,
        captures: Vec<Capture>,
        owner_cid: Option<i64>,
        subst: &HashMap<String, Ty>,
        base: Option<FuncId>,
    ) -> Option<FuncId> {
        let ExprKind::Lambda { body, .. } = &lambda.kind else {
            return None;
        };
        let _ = self.register_closure_class(captures.len(), lambda.span);
        let fnty = self.types.get(&lambda.span).cloned().unwrap_or(Ty::Unknown);
        // Only lambdas with a fully-resolved checker signature are registered;
        // otherwise leave it unregistered and bail loudly when it is lowered.
        let Ty::Fn(pts, _) = &fnty else {
            return None;
        };
        if params_len(lambda) != pts.len() {
            return None;
        }
        let pts: Vec<Ty> = pts.iter().map(|t| t.subst(subst)).collect();
        if pts.iter().any(|t| self.map_ty(t, lambda.span).is_err()) {
            return None;
        }
        let mut ret = match &fnty {
            Ty::Fn(_, r) if !matches!(r.as_ref(), Ty::Unknown) => (**r).clone(),
            _ => self.infer_lambda_ret(body),
        };
        if matches!(ret, Ty::Unknown) {
            ret = self.infer_lambda_ret(body);
        }
        let ret = ret.subst(subst);
        if self.map_ty(&ret, lambda.span).is_err() {
            return None;
        }
        let caps: Vec<Capture> = captures
            .iter()
            .map(|c| Capture {
                ty: c.ty.subst(subst),
                ..c.clone()
            })
            .collect();
        let fid = self.push_class_func(
            "lambda",
            &format!("pkl_closure_{}", self.next_closure),
            FnSource::Lambda {
                lambda,
                captures: caps.clone(),
                pty: pts,
                ret,
                owner: owner_cid,
            },
        );
        self.next_closure += 1;
        // A per-instantiation copy's body carries `Var`s until its own build;
        // keep the substitution live for it exactly as for a generic function.
        self.instanton_subst.insert(fid, subst.clone());
        self.lambda_fids.insert((base, lambda.span), fid);
        self.lambda_caps.insert((base, lambda.span), caps);
        Some(fid)
    }

    /// A lambda body's inferred return type: its trailing expression, or unit
    /// when none (checked annotations win first, in `register_lambda`).
    fn infer_lambda_ret(&self, body: &FnBody) -> Ty {
        match body {
            FnBody::Expr(e) => self.types.get(&e.span).cloned().unwrap_or(Ty::Unknown),
            FnBody::Block(b) => match &b.expr {
                Some(e) => self.types.get(&e.span).cloned().unwrap_or(Ty::Empty),
                None => Ty::Empty,
            },
        }
    }

    // ---- registration -----------------------------------------------------

    /// Collect the `#[tag("…")]` values from an item's attributes. Arguments
    /// that are not plain string literals are ignored (the checker reports
    /// them; the emitter runs without checking on some paths).
    fn item_tags(&self, attrs: &[Attribute]) -> Vec<String> {
        let mut out = Vec::new();
        for a in attrs {
            if a.name != "tag" {
                continue;
            }
            for arg in &a.args {
                if let ExprKind::Lit(Lit::String(parts)) = &arg.kind {
                    if parts.iter().all(|p| matches!(p, StrPart::Text(_))) {
                        let text: String = parts
                            .iter()
                            .map(|p| match p {
                                StrPart::Text(t) => t.clone(),
                                StrPart::Expr(_) => String::new(),
                            })
                            .collect();
                        if !text.is_empty() {
                            out.push(text);
                        }
                    }
                }
            }
        }
        out
    }

    fn register_fn(&mut self, f: &'a FnDecl, is_test: bool, attrs: &[Attribute]) {
        if !f.generics.is_empty() {
            return;
        }
        if f.params.iter().any(|p| p.default.is_some() || p.rest) {
            return;
        }
        let Some(info) = self.user_callable(&f.name) else {
            return;
        };
        let ok_params = info
            .params
            .iter()
            .map(|p| self.map_ty(&p.ty, f.span))
            .collect::<Result<Vec<_>, _>>()
            .is_ok();
        let ok_ret = self.map_ty(&info.ret, f.span).is_ok();
        if !ok_params || !ok_ret {
            return;
        }
        if f.is_async {
            self.register_async_fn(f, is_test, attrs, info);
            return;
        }
        let fid = FuncId(self.module.funcs.len());
        let symbol = self.symbol_for_attrs(&f.name, is_test, attrs);
        self.module.funcs_by_name.insert(f.name.clone(), fid);
        self.module.funcs.push(IrFunc {
            name: f.name.clone(),
            symbol,
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            is_main: !is_test && f.name == "main",
            is_test,
            tags: self.item_tags(attrs),
        });
        self.fid_list.push(fid);
        self.fsource.insert(fid, FnSource::TopLevel(f));
        self.finfo.insert(fid, info.clone());
        self.src_param_tys
            .insert(fid, info.params.iter().map(|p| p.ty.clone()).collect());
    }

    /// Register an `async fn` as two lowerable functions connected by the
    /// confirmed design (no new runtime primitives, no new `Ty` variant):
    ///
    ///   * the hidden future body — a 0-arg hoisted body with the lambda ABI
    ///     (`env = closure object`), whose closure slots 1..N snapshot each of
    ///     the async fn's parameters under its own name; and
    ///   * the visible wrapper — a normal module function with the async fn's
    ///     real ABI that allocates that closure and returns it. Calling it
    ///     runs no user code; the body runs only when `await` drives the
    ///     future's 0-arg call.
    ///
    /// The hidden entry is registered first so the wrapper can name it.
    fn register_async_fn(
        &mut self,
        f: &'a FnDecl,
        is_test: bool,
        attrs: &[Attribute],
        info: &CallableInfo,
    ) {
        let symbol = self.symbol_for(&f.name, is_test);
        let hidden_name = format!("{}__future", f.name);
        let hidden_symbol = format!("{symbol}__future");
        let hidden = FuncId(self.module.funcs.len());
        self.module
            .funcs_by_name
            .insert(hidden_name.clone(), hidden);
        self.module.funcs.push(IrFunc {
            name: hidden_name,
            symbol: hidden_symbol,
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            is_main: false,
            is_test: false,
            tags: Vec::new(),
        });
        self.fid_list.push(hidden);
        self.fsource.insert(
            hidden,
            FnSource::AsyncBody {
                f,
                pty: info.params.iter().map(|p| p.ty.clone()).collect(),
                ret: info.ret.clone(),
            },
        );
        let mut hidden_info = info.clone();
        hidden_info.params = Vec::new();
        self.finfo.insert(hidden, hidden_info);

        let wfid = FuncId(self.module.funcs.len());
        self.module.funcs_by_name.insert(f.name.clone(), wfid);
        self.module.funcs.push(IrFunc {
            name: f.name.clone(),
            symbol,
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            is_main: !is_test && f.name == "main",
            is_test,
            tags: self.item_tags(attrs),
        });
        self.fid_list.push(wfid);
        self.fsource.insert(wfid, FnSource::AsyncFn { f, hidden });
        self.finfo.insert(wfid, info.clone());
        self.src_param_tys
            .insert(wfid, info.params.iter().map(|p| p.ty.clone()).collect());
    }

    /// Register an async method as two lowerable functions, mirroring
    /// `register_async_fn` for methods:
    ///
    ///   * the hidden future body, whose closure slots hold the receiver
    ///     (`this`, instance methods) followed by the parameters; and
    ///   * the visible wrapper — the method itself — with the method's real
    ///     ABI (receiver-first for instance methods), allocating that closure
    ///     and returning it without running the body. `method_ids` maps the
    ///     method name to the wrapper, so instance/static calls and (via
    ///     `method_ids` inheritance + `build_virtual_dispatch`) overrides
    ///     dispatch exactly like regular async methods do.
    ///
    /// `pty` for the hidden body is [this_ty] + parameter types (instance) or
    /// parameter types alone (static); `suffix` disambiguates generic-class
    /// instantiation symbols, and `subst_info` carries the instantiated
    /// signature when the receiver class is a generic instantiation.
    fn register_async_method(
        &mut self,
        recv: &AsyncRecv<'_>,
        table: &ClassTable,
        md: &'a MethodDecl,
        info: &CallableInfo,
        suffix: &str,
        subst: Option<HashMap<String, Ty>>,
    ) {
        let class = recv.class;
        let cid = recv.cid;
        let args = recv.args;
        let is_struct = recv.is_struct;
        let this_ty = if info.is_static {
            None
        } else if is_struct {
            Some(Ty::Struct(class.to_string(), args.to_vec()))
        } else {
            Some(Ty::Class(class.to_string(), args.to_vec()))
        };
        let pty: Vec<Ty> = this_ty
            .clone()
            .into_iter()
            .chain(info.params.iter().map(|p| p.ty.clone()))
            .collect();
        let stem = if info.is_static {
            format!("pkl_{class}_sm_{}", md.name)
        } else {
            format!("pkl_{class}_{}", md.name)
        };
        let hidden = self.push_class_func(
            &format!("{}.{}.future", class, md.name),
            &format!("{stem}{suffix}__future"),
            FnSource::AsyncMethodBody {
                table: table.clone(),
                md,
                pty,
                ret: info.ret.clone(),
                is_static: info.is_static,
            },
        );
        let mut hidden_info = info.clone();
        hidden_info.params = Vec::new();
        self.finfo.insert(hidden, hidden_info);
        self.fid_owner.insert(hidden, cid as i64);
        if let Some(s) = subst.clone() {
            self.instanton_subst.insert(hidden, s.clone());
            self.fid_generics.insert(hidden, recv.generics.clone());
        }

        let wfid = self.push_class_func(
            &format!("{}.{}", class, md.name),
            &format!("{stem}{suffix}"),
            FnSource::AsyncMethod {
                table: table.clone(),
                md,
                hidden,
                is_static: info.is_static,
                this_ty: this_ty.clone(),
            },
        );
        self.finfo.insert(wfid, info.clone());
        self.src_param_tys
            .insert(wfid, info.params.iter().map(|p| p.ty.clone()).collect());
        self.method_ids
            .insert((cid, md.name.clone()), (wfid, info.is_static));
        self.fid_owner.insert(wfid, cid as i64);
        if let Some(s) = subst {
            self.instanton_subst.insert(wfid, s);
            self.fid_generics.insert(wfid, recv.generics.clone());
        }
    }

    // ---- class/struct registration ---------------------------------------

    /// Register a class and, first, its ancestors, so a superclass always has a
    /// lower class id than its subclasses (field layout and the runtime parent
    /// links rely on it).
    fn register_class_by_name(&mut self, name: &str) {
        if self.class_attempted.iter().any(|n| n == name) {
            return;
        }
        self.class_attempted.push(name.to_string());
        let Some(decl) = self.class_decls.get(name).copied() else {
            return;
        };
        let table = match self.resolved.types.get(name) {
            Some(TypeTableEntry::Class(t)) => t.clone(),
            _ => return,
        };
        if let Some(p) = table
            .extends
            .as_ref()
            .and_then(|t| t.named().map(str::to_string))
        {
            if self.class_decls.contains_key(&p) {
                self.register_class_by_name(&p);
            }
        }
        self.maybe_register_class(name, &decl.members, table, decl.span);
    }

    fn register_struct_item(&mut self, s: &'a StructDecl) {
        self.struct_members.insert(s.name.clone(), &s.members[..]);
        let table = match self.resolved.types.get(&s.name) {
            Some(TypeTableEntry::Struct(t)) => t.clone(),
            _ => return,
        };
        self.maybe_register_class(&s.name, &s.members, table, s.span);
    }

    /// Register the synthetic `__closure_N` class (or return its id). Slot 0
    /// holds the hoisted body's code address (a boxed int); slots 1..N hold the
    /// captured values, boxed exactly like list elements. Every slot is traced
    /// by the GC, so a closure keeps its captures alive. Kept on the emitter's
    /// own tables so the normal field-layout helpers resolve them.
    fn register_closure_class(&mut self, capture_count: usize, span: Span) -> Result<u32, ()> {
        if capture_count > MAX_LAMBDA_CAPTURES {
            return self.bad(
                span,
                format!(
                    "this lambda captures {capture_count} values, more than the supported limit of {MAX_LAMBDA_CAPTURES}"
                ),
            );
        }
        let name = format!("{CLOSURE_CLASS_PREFIX}{capture_count}");
        if let Some(&cid) = self.class_by_name.get(&name) {
            return Ok(cid);
        }
        let mut fields = Vec::with_capacity(capture_count + 1);
        for i in 0..=capture_count {
            fields.push(FieldInfo {
                name: if i == CLOSURE_FN_SLOT {
                    "__fn".to_string()
                } else {
                    format!("__c{}", i - 1)
                },
                visibility: Visibility::Private,
                is_static: false,
                mutable: false,
                const_: false,
                manual: false,
                ty: Ty::Int,
                span,
            });
        }
        let table = ClassTable {
            name: name.clone(),
            span,
            visibility: Visibility::Private,
            generics: Vec::new(),
            extends: None,
            implements: Vec::new(),
            fields,
            methods: Vec::new(),
            properties: Vec::new(),
            ctor: None,
            named_ctors: Vec::new(),
            consts: Vec::new(),
        };
        let cid = (PICKLE_CLASS_USER_BASE + self.classes.len() as i64) as u32;
        self.closure_tables.insert(name.clone(), table.clone());
        self.class_by_name.insert(name.clone(), cid);
        self.classes.push(ClassPlan {
            name,
            class_id: cid,
            parent: None,
            table,
        });
        Ok(cid)
    }

    /// Register the forwarder (`FnSource::Trampoline`) that lets a top-level
    /// function be called through a closure object: it accepts the closure as
    /// slot 0, forwards the real arguments to the target, and returns its
    /// result. One trampoline per target function; every value-reference site
    /// of that function is mapped to it in `fn_tramp`.
    fn register_trampoline(&mut self, span: Span, fn_name: &str) {
        let Some(&fid) = self.module.funcs_by_name.get(fn_name) else {
            return;
        };
        let Some(Ty::Fn(pty, ret)) = self.types.get(&span).cloned() else {
            return;
        };
        if pty.iter().any(|t| self.map_ty(t, span).is_err()) {
            return;
        }
        if self.map_ty(&ret, span).is_err() {
            return;
        }
        let tramp = match self.tramp_fids.get(&fid) {
            Some(&t) => t,
            None => {
                let t = self.push_class_func(
                    "fn.value",
                    &format!("pkl_tramp_{}", fid.0),
                    FnSource::Trampoline {
                        span,
                        target: fid,
                        pty,
                        ret: *ret,
                    },
                );
                self.tramp_fids.insert(fid, t);
                t
            }
        };
        self.fn_tramp.insert(span, tramp);
    }

    /// Register the forwarder (`FnSource::MethodTrampoline`) that lets an
    /// instance method bound to a receiver be used as a value: it accepts the
    /// closure as slot 0, loads the bound receiver from closure slot 1, then
    /// forwards the real arguments to the target method. One trampoline per
    /// (receiver class, method); every value-reference site of that method is
    /// mapped to it in `fn_tramp`. When `(cid, name)` has overrides recorded, the
    /// forwarder dispatches on the receiver's runtime class instead of calling
    /// `fid` outright.
    fn register_method_trampoline(&mut self, span: Span, fid: FuncId, cid: u32, name: &str) {
        let Some(Ty::Fn(pty, ret)) = self.types.get(&span).cloned() else {
            return;
        };
        if pty.iter().any(|t| self.map_ty(t, span).is_err()) {
            return;
        }
        if self.map_ty(&ret, span).is_err() {
            return;
        }
        let branches = self
            .virtual_dispatch
            .get(&(cid, name.to_string()))
            .cloned()
            .unwrap_or_default();
        let key = (cid, name.to_string(), fid);
        let tramp = match self.method_tramp_fids.get(&key) {
            Some(&t) => t,
            None => {
                let t = self.push_class_func(
                    "fn.method.value",
                    &format!("pkl_mtramp_{}_{}_{}", cid, name, fid.0),
                    FnSource::MethodTrampoline {
                        span,
                        target: fid,
                        pty,
                        ret: *ret,
                        branches,
                    },
                );
                self.method_tramp_fids.insert(key, t);
                t
            }
        };
        self.fn_tramp.insert(span, tramp);
    }

    // ---- inheritance layout helpers ---------------------------------------

    /// The class/struct table for `name`, from the resolver (or, for the
    /// synthetic closure classes and generic-class instantiations, from the
    /// emitter's own tables). An instantiation's plan carries its *substituted*
    /// table under its mangled name (`Box<int>`), so the layout helpers resolve
    /// concrete field types directly.
    fn table_of(&self, name: &str) -> Option<ClassTable> {
        match self.resolved.types.get(name) {
            Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => Some(t.clone()),
            _ => self
                .classes
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.table.clone())
                .or_else(|| self.closure_tables.get(name).cloned()),
        }
    }

    /// The superclass name of `name`, when it extends another class.
    fn parent_name(&self, name: &str) -> Option<String> {
        self.table_of(name)?
            .extends
            .as_ref()
            .and_then(|t| t.named().map(str::to_string))
    }

    /// Ancestry from the rootmost superclass down to `name` inclusive.
    fn ancestry(&self, name: &str) -> Vec<String> {
        let mut chain = vec![name.to_string()];
        let mut cur = name.to_string();
        for _ in 0..64 {
            match self.parent_name(&cur) {
                Some(p) => {
                    chain.push(p.clone());
                    cur = p;
                }
                None => break,
            }
        }
        chain.reverse();
        chain
    }

    fn own_instance_fields(&self, name: &str) -> Vec<FieldInfo> {
        self.table_of(name)
            .map(|t| t.fields.into_iter().filter(|f| !f.is_static).collect())
            .unwrap_or_default()
    }

    /// Instance fields of `name` and every ancestor, root first (the runtime
    /// slot order: superclass fields occupy the low slots).
    fn all_instance_fields(&self, name: &str) -> Vec<FieldInfo> {
        let mut out = Vec::new();
        for cname in self.ancestry(name) {
            out.extend(self.own_instance_fields(&cname));
        }
        out
    }

    /// Total instance-field slot count of `name` including inherited fields.
    fn total_instance_slot_count(&self, name: &str) -> usize {
        self.ancestry(name)
            .iter()
            .map(|c| self.own_instance_fields(c).len())
            .sum()
    }

    /// Absolute (parent-first) slot of instance field `field` reachable from
    /// `name`, whether declared on `name` or inherited.
    fn abs_instance_field_slot(&self, name: &str, field: &str) -> Option<usize> {
        let mut base = 0usize;
        for cname in self.ancestry(name) {
            let own = self.own_instance_fields(&cname);
            if let Some(i) = own.iter().position(|f| f.name == field) {
                return Some(base + i);
            }
            base += own.len();
        }
        None
    }

    /// The nearest explicit-primary-constructor ancestor of `name` (the
    /// deepest class in the chain that declares a primary `constructor`),
    /// together with the absolute end slot of that ancestor's own instance
    /// fields. Classes with only *named* constructors are still synthesized,
    /// so they never count as explicit here.
    fn nearest_explicit_primary_ancestor(&self, name: &str) -> Option<(String, usize)> {
        let mut found = None;
        let mut base = 0usize;
        for cname in self.ancestry(name) {
            let own = self.own_instance_fields(&cname);
            if self
                .table_of(&cname)
                .map(|t| t.ctor.is_some())
                .unwrap_or(false)
            {
                found = Some((cname.to_string(), base + own.len()));
            }
            base += own.len();
        }
        found
    }

    /// Parameter metadata for a synthesized constructor of `name`, in
    /// parameter order: the absolute instance-field slot each parameter must
    /// be stored into (`None` = a leading explicit-ancestor constructor
    /// parameter, forwarded to the chained `super(...)`), plus the nearest
    /// explicit-primary ancestor to chain into and how many leading
    /// parameters that chain consumes. With no explicit-primary ancestor the
    /// whole chain's uninitialized fields become the parameters (one per
    /// field slot) and nothing is chained.
    fn synthesized_ctor_param_meta(
        &self,
        name: &str,
    ) -> (Vec<Option<usize>>, Option<(String, usize)>) {
        let ancestry = self.ancestry(name);
        let ancestor = self.nearest_explicit_primary_ancestor(name);
        let empty: Vec<usize> = self
            .collect_instance_inits(name)
            .iter()
            .map(|(i, _)| *i)
            .collect();
        let mut slots = Vec::new();
        let mut cparam_count = 0usize;
        let cut_base = match &ancestor {
            Some((a, end)) => {
                if let Some(t) = self.table_of(a) {
                    cparam_count = t.ctor.as_ref().map(|c| c.params.len()).unwrap_or(0);
                    slots.extend(std::iter::repeat_n(None, cparam_count));
                }
                *end
            }
            None => 0usize,
        };
        let mut base = 0usize;
        for cname in &ancestry {
            let own = self.own_instance_fields(cname);
            // Only classes strictly below the nearest explicit ancestor
            // contribute field parameters (its own fields are assigned by its
            // constructor body); classes above it flow through that
            // ancestor's own chain.
            if ancestor.is_none() || base >= cut_base {
                for (i, _) in own.iter().enumerate() {
                    if !empty.contains(&(base + i)) {
                        slots.push(Some(base + i));
                    }
                }
            }
            base += own.len();
        }
        let chain = ancestor.map(|(a, _)| (a, cparam_count));
        (slots, chain)
    }

    /// The generic-plan name behind `name`: for a materialized instantiation
    /// like `Counter<int>` the plan name `Counter` (declarations and
    /// initializers live under the plan key); otherwise `name` unchanged.
    /// Generic plan names never contain `<`, so the first `<` reliably splits
    /// the instantiation suffix off.
    fn plan_name<'b>(&self, name: &'b str) -> &'b str {
        match name.find('<') {
            Some(i) => &name[..i],
            None => name,
        }
    }

    /// Instance-field initializers for `name` and its ancestors, root first,
    /// recorded at absolute slot numbers.
    fn collect_instance_inits(&self, name: &str) -> Vec<(usize, &'a Expr)> {
        let mut out = Vec::new();
        let mut base = 0usize;
        for cname in self.ancestry(name) {
            let own = self.own_instance_fields(&cname);
            let decl = self.plan_name(&cname);
            let members = self
                .class_decls
                .get(decl)
                .map(|d| &d.members[..])
                .or_else(|| self.struct_members.get(decl).copied());
            if let Some(members) = members {
                for m in members {
                    if let ClassMember::Field {
                        name: fname,
                        init: Some(x),
                        is_static: false,
                        ..
                    } = m
                    {
                        if let Some(i) = own.iter().position(|f| &f.name == fname) {
                            out.push((base + i, x));
                        }
                    }
                }
            }
            base += own.len();
        }
        out
    }

    /// `init` blocks for `name` and its ancestors, root first.
    fn collect_init_blocks(&self, name: &str) -> Vec<&'a Block> {
        let mut out = Vec::new();
        for cname in self.ancestry(name) {
            let decl = self.plan_name(&cname);
            let members = self
                .class_decls
                .get(decl)
                .map(|d| &d.members[..])
                .or_else(|| self.struct_members.get(decl).copied());
            if let Some(members) = members {
                for m in members {
                    if let ClassMember::Init(b) = m {
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    /// True when `anc` is `desc` itself or one of its ancestors.
    fn is_ancestor(&self, anc: &str, desc: &str) -> bool {
        self.ancestry(desc).iter().any(|c| c == anc)
    }

    /// Discover every instance method that is overridden somewhere in the
    /// program and record, per static receiver class, the descendant classes
    /// that provide a different implementation (deepest-derived first). A call
    /// whose receiver is statically typed as one of these classes must dispatch
    /// on the receiver's runtime class id; all other calls keep the current
    /// direct static call. Runs once after every class is registered (generic
    /// classes cannot extend, so they never contribute descendants here).
    fn build_virtual_dispatch(&mut self) {
        let classes: Vec<(u32, String)> = self
            .classes
            .iter()
            .map(|p| (p.class_id, p.name.clone()))
            .collect();
        // The instance methods visible at each static receiver class. `method_ids`
        // already carries inheritance (a subclass copies every ancestor entry with
        // `or_insert`), so a class that merely inherits `m` resolves it exactly as
        // a call through it would — including deeper overrides, which is the point.
        let mut visible: HashMap<u32, Vec<(String, FuncId)>> = HashMap::new();
        for (&(rcid, ref mname), &(fid, is_static)) in &self.method_ids {
            if !is_static {
                visible.entry(rcid).or_default().push((mname.clone(), fid));
            }
        }
        let mut mixed_static_override: Option<(String, String)> = None;
        for (cid, cname) in &classes {
            let Some(names) = visible.get(cid) else {
                continue;
            };
            for (mname, rfid) in names {
                // Own-defined overrides of `mname` among the descendants of
                // `cname` only — a descendant that merely inherits the method
                // resolves to its nearest defining ancestor, which
                // `pickle_class_is` already covers through that ancestor's
                // branch (an inheriting descendant is never a branch target).
                let mut branches: Vec<(u32, i64, FuncId)> = Vec::new();
                for (dcid, dname) in &classes {
                    if *dcid == *cid || !self.is_ancestor(cname, dname) {
                        continue;
                    }
                    let Some(dt) = self.table_of(dname) else {
                        continue;
                    };
                    if !dt.methods.iter().any(|di| di.name == *mname) {
                        continue;
                    }
                    let Some(&(dfid, dstatic)) = self.method_ids.get(&(*dcid, mname.clone()))
                    else {
                        continue;
                    };
                    if dstatic {
                        if mixed_static_override.is_none() {
                            mixed_static_override = Some((dname.clone(), mname.clone()));
                        }
                        continue;
                    }
                    if dfid == *rfid {
                        // Same implementation the receiver resolves to (e.g. a
                        // deferred declaration): adds no dispatch.
                        continue;
                    }
                    let depth = self
                        .ancestry(dname)
                        .iter()
                        .position(|c| c == cname)
                        .map(|i| (self.ancestry(dname).len() - 1 - i) as i64)
                        .unwrap_or(0);
                    branches.push((*dcid, depth, dfid));
                }
                if branches.is_empty() {
                    continue;
                }
                branches.sort_by_key(|(_, depth, _)| std::cmp::Reverse(*depth));
                self.virtual_dispatch.insert(
                    (*cid, mname.clone()),
                    branches
                        .into_iter()
                        .map(|(dcid, _, dfid)| (dcid, dfid))
                        .collect(),
                );
            }
        }
        if let Some((cname, mname)) = mixed_static_override {
            let span = self
                .class_decls
                .get(&cname)
                .map(|d| d.span)
                .unwrap_or_else(|| Span::new(crate::diag::FileId(0), 0, 0));
            let _: Result<(), ()> = self.bad(
                span,
                format!("`override` mixing static and instance methods (`{cname}.{mname}`) is not lowered yet"),
            );
        }
    }

    /// A non-generic user class type resolved to its registered runtime id.
    fn user_class_id(&self, ty: &Ty) -> Option<(String, u32)> {
        if let Ty::Class(n, args) = ty {
            if args.is_empty() {
                if let Some(&cid) = self.class_by_name.get(n) {
                    return Some((n.clone(), cid));
                }
            }
        }
        None
    }

    /// The interface type a `Ty` denotes, unwrapping one `Ref` level (an
    /// interface receiver is always a `Ref(Interface(..))` at call sites).
    fn iface_of(&self, ty: &Ty) -> Option<(String, Vec<Ty>)> {
        match ty {
            Ty::Interface(name, args) => Some((name.clone(), args.clone())),
            Ty::Ref(inner) => self.iface_of(inner),
            _ => None,
        }
    }

    /// Resolve an interface instantiation `name<args>` to its runtime id,
    /// assigning a fresh one on first use and recording its member plan
    /// (method name + declaration index). Two instantiations of the same
    /// interface with different type args get distinct ids, so `is`/`as` on
    /// interface types probe exactly the interfaces a class declared.
    fn iface_id_for(&mut self, name: &str, args: &[Ty], span: Span) -> Result<u32, ()> {
        let key = (name.to_string(), args.to_vec());
        if let Some(&id) = self.iface_inst_ids.get(&key) {
            return Ok(id);
        }
        let Some(TypeTableEntry::Interface(t)) = self.resolved.types.get(name) else {
            let _: Result<(), ()> = self.bad(span, format!("`{name}` is not an interface"));
            return Err(());
        };
        let id = self.next_iface_id;
        self.next_iface_id += 1;
        let members = self.iface_members.entry(id).or_insert_with(|| {
            let mut plan = Vec::new();
            let mut mi = 0u32;
            for m in &t.members {
                if !m.is_property {
                    plan.push((m.name.clone(), mi));
                    mi += 1;
                }
            }
            plan
        });
        let _ = members;
        let _ = &t.generics;
        self.iface_inst_ids.insert(key, id);
        Ok(id)
    }

    /// Record a class's own `implements` declarations into its runtime bucket:
    /// each interface id gets an empty bucket (so `pickle_class_implements`
    /// answers for zero-method interfaces too) plus one entry per method
    /// mapping its declaration index to the class's resolved implementation
    /// function. Interfaces inherited from a superclass stay the superclass's
    /// bucket (the runtime walks the parent chain).
    fn register_implements(
        &mut self,
        cname: &str,
        cid: u32,
        ifaces: &[Ty],
        span: Span,
    ) -> Result<(), ()> {
        for t in ifaces {
            let Some((iname, iargs)) = self.iface_of(t) else {
                let _: Result<(), ()> = self.bad(
                    span,
                    format!(
                        "`{cname}` declares `implements` on a non-interface type `{}`",
                        t.bare_name()
                    ),
                );
                return Err(());
            };
            let iface_id = self.iface_id_for(&iname, &iargs, span)?;
            let members = self
                .iface_members
                .get(&iface_id)
                .cloned()
                .unwrap_or_default();
            let mut slots = Vec::with_capacity(members.len());
            for (mname, m_idx) in &members {
                match self.method_ids.get(&(cid, mname.clone())) {
                    Some(&(fid, false)) => slots.push((*m_idx, fid)),
                    Some(&(_, true)) => {
                        let _: Result<(), ()> = self.bad(
                            span,
                            format!(
                                "interface method `{mname}` must be an instance method on `{cname}`"
                            ),
                        );
                        return Err(());
                    }
                    None => {
                        let _: Result<(), ()> = self.bad(
                            span,
                            format!("`{cname}` implements `{iname}` but has no method `{mname}`"),
                        );
                        return Err(());
                    }
                }
            }
            self.class_iface_buckets
                .entry(cid)
                .or_default()
                .push((iface_id, slots));
        }
        Ok(())
    }

    /// Whether a registered class (or one of its ancestors) directly declared
    /// `implements` for `iface_id`. The caller resolves ancestor ids through
    /// the class plans' `parent` links.
    fn class_implements_iface(&self, cid: u32, iface_id: u32) -> bool {
        let mut cur = Some(cid);
        for _ in 0..64 {
            let Some(c) = cur else { return false };
            if self
                .class_iface_buckets
                .get(&c)
                .is_some_and(|b| b.iter().any(|(i, _)| *i == iface_id))
            {
                return true;
            }
            cur = self
                .classes
                .iter()
                .find(|p| p.class_id == c)
                .and_then(|p| p.parent);
        }
        false
    }

    /// The element type `T` of an `Iterable<T>` value (mirror of the
    /// checker's rule): either the interface type `Iterable<int>` directly, or
    /// a class/struct that transitively declares `implements Iterable<...>`
    /// with its own generic args substituted (so `MyList<string>` yields
    /// `string`). `none` when the value is not iterable via the protocol.
    fn iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        if let Ty::Interface(n, args) = ty {
            return (n == "Iterable").then(|| args.first().cloned().unwrap_or(Ty::Unknown));
        }
        let (name, args) = match ty {
            Ty::Class(n, a) | Ty::Struct(n, a) => (n.clone(), a.clone()),
            _ => return None,
        };
        let args_map: HashMap<String, Ty> = match self.resolved.types.get(&name) {
            Some(entry) => entry
                .generics()
                .iter()
                .cloned()
                .zip(args.iter().cloned().chain(std::iter::repeat(Ty::Unknown)))
                .collect(),
            None => HashMap::new(),
        };
        let mut chain: Vec<String> = vec![name];
        let mut seen: Vec<String> = Vec::new();
        while let Some(cname) = chain.pop() {
            if seen.contains(&cname) {
                continue;
            }
            seen.push(cname.clone());
            let table = match self.resolved.types.get(&cname) {
                Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => t.clone(),
                _ => continue,
            };
            for i in &table.implements {
                if let Ty::Interface(iname, iargs) = i {
                    if iname == "Iterable" {
                        let t = iargs.first().cloned().unwrap_or(Ty::Unknown);
                        return Some(t.subst(&args_map));
                    }
                }
            }
            if let Some(p) = table
                .extends
                .as_ref()
                .and_then(|t| t.named().map(str::to_string))
            {
                chain.push(p);
            }
        }
        None
    }
    /// function, and its method functions. Members outside the slice are
    /// rejected loudly rather than miscompiled. Single inheritance is lowered
    /// (parent-first field layout, inherited methods, hierarchy casts); generic
    /// classes/structs are stashed for instantiation on first use, and
    /// interface-implementing classes are still skipped entirely.
    fn maybe_register_class(
        &mut self,
        name: &str,
        members: &'a [ClassMember],
        table: ClassTable,
        span: Span,
    ) {
        if !table.generics.is_empty() {
            // A generic class/struct is not registered bare. Its concrete
            // instantiations materialize on first use from this stash
            // (`register_class_instantiation`).
            self.generic_type_members.insert(name.to_string(), members);
            return;
        }
        let parent = table
            .extends
            .as_ref()
            .and_then(|t| t.named().map(str::to_string));
        let parent_cid = match &parent {
            Some(p) => match self.class_by_name.get(p) {
                Some(&pcid) => Some(pcid),
                // The superclass itself was not lowerable (generic/interface).
                None => return,
            },
            None => None,
        };
        let ctor_decl = members.iter().find_map(|m| match m {
            ClassMember::Constructor(cd) if cd.name.is_none() => Some(cd),
            _ => None,
        });
        // Explicit constructors in a hierarchy need `super(...)` chaining: a
        // class with a constructor whose parent declares an explicit
        // constructor must open its body with a `super(...)` delegation. Named
        // constructors delegate through their class's primary constructor, so
        // they are fine wherever the primary is. A class that relies on the
        // synthesized constructor below an explicit-primary ancestor chains
        // into it with a synthesized `super(...)` (see
        // `synthesized_ctor_param_meta`).
        let parent_explicit = parent
            .as_ref()
            .map(|p| {
                self.table_of(p)
                    .map(|pt| pt.ctor.is_some())
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if let Some(cd) = ctor_decl {
            if parent_explicit && ctor_super_delegation(&cd.body).is_none() {
                let _: Result<(), ()> = self.bad(
                    cd.span,
                    format!(
                        "`{name}` constructor must call `super(...)` first to chain into `{}`",
                        parent.as_deref().unwrap_or("")
                    ),
                );
                return;
            }
        }
        // Instance-field initializers across the whole hierarchy, root first,
        // at absolute (parent-first) slot numbers.
        let inits: Vec<(usize, &'a Expr)> = self.collect_instance_inits(name);
        let init_blocks: Vec<&'a Block> = self.collect_init_blocks(name);
        let deinit_block: Option<&'a Block> = members.iter().find_map(|m| match m {
            ClassMember::Deinit(b) => Some(b),
            _ => None,
        });

        // Every static field gets an initializer entry: the declared value, or
        // `None` for the default (zero/null) so a read never dereferences the
        // unset cell.
        let mut static_inits: Vec<(usize, Option<&'a Expr>)> = Vec::new();
        for f in table.fields.iter().filter(|f| f.is_static) {
            let Some(slot) = static_field_slot(&table, &f.name) else {
                continue;
            };
            let init = members
                .iter()
                .find_map(|m| match m {
                    ClassMember::Field {
                        name,
                        init,
                        is_static: true,
                        ..
                    } if name == &f.name => Some(init.as_ref()),
                    _ => None,
                })
                .flatten();
            static_inits.push((slot, init));
        }

        let cid = (PICKLE_CLASS_USER_BASE + self.classes.len() as i64) as u32;
        for (slot, x) in static_inits {
            self.static_inits.push((cid, slot, x));
        }
        // Class constants are compile-time values: record the declared info and
        // the initializer so reads can inline it (`Type.NAME` / bare name).
        for f in &table.consts {
            let Some(value) = members.iter().find_map(|m| match m {
                ClassMember::Const { name, value, .. } if name == &f.name => Some(value),
                _ => None,
            }) else {
                continue;
            };
            self.class_consts
                .insert((cid, f.name.clone()), (f.clone(), value));
        }

        // Constructor: `pkl_<Name>_new(...) -> Ptr`. Its parameters are the
        // explicit constructor's params when one is declared, otherwise the
        // fields without initializers (defaults are filled while running the
        // field initializers).

        let ctor_fid = self.push_class_func(
            &format!("{name}.new"),
            &format!("pkl_{name}_new"),
            FnSource::Ctor {
                table: table.clone(),
                inits,
                init_blocks,
                ctor: ctor_decl,
            },
        );
        self.ctor_ids.insert(cid, ctor_fid);
        // The primary constructor's source parameter types drive implicit
        // borrows at Ctor callsites (`f(Value(...))` for `&T` params).
        let ctor_params = table
            .ctor
            .as_ref()
            .map(|c| c.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        self.src_param_tys.insert(ctor_fid, ctor_params);

        // Named constructors: `constructor.NAME(...) { this(...) }`. Each is a
        // static factory `pkl_<Name>_nc_<NAME>` that evaluates the delegation
        // arguments and calls the primary constructor.
        for cd in members.iter().filter_map(|m| match m {
            ClassMember::Constructor(cd) if cd.name.is_some() => Some(cd),
            _ => None,
        }) {
            let cname = cd.name.clone().unwrap_or_default();
            let fid = self.push_class_func(
                &format!("{name}.{cname}"),
                &format!("pkl_{name}_nc_{cname}"),
                FnSource::NamedCtor {
                    table: table.clone(),
                    ctor: cd,
                },
            );
            self.named_ctor_ids.insert((cid, cname), fid);
            let ncinfo = table
                .named_ctors
                .iter()
                .find(|(n, _)| cd.name.as_deref() == Some(n.as_str()))
                .map(|(_, c)| c);
            self.src_param_tys.insert(
                fid,
                ncinfo
                    .map(|c| c.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
        }

        // `deinit`: a finalizer `pkl_<Name>_deinit(this)` registered on the
        // class descriptor and run by the collector at sweep.
        if let Some(body) = deinit_block {
            let fid = self.push_class_func(
                &format!("{name}.deinit"),
                &format!("pkl_{name}_deinit"),
                FnSource::Deinit {
                    table: table.clone(),
                    body,
                },
            );
            self.deinit_ids.insert(cid, fid);
        }

        // Methods: `pkl_<Name>_<m>` (instance) and `pkl_<Name>_sm_<m>` (static).
        for md in members.iter().filter_map(|m| match m {
            ClassMember::Method(md) => Some(md),
            _ => None,
        }) {
            if md.body.is_none() || !md.generics.is_empty() {
                continue;
            }
            let Some(info) = table.methods.iter().find(|m| m.name == md.name) else {
                continue;
            };
            if info.params.iter().any(|p| p.has_default || p.rest) {
                continue;
            }
            if md.is_async {
                self.register_async_method(
                    &AsyncRecv {
                        class: name,
                        cid,
                        args: &[],
                        is_struct: false,
                        generics: Vec::new(),
                    },
                    &table,
                    md,
                    info,
                    "",
                    None,
                );
                continue;
            }
            let symbol = if info.is_static {
                format!("pkl_{name}_sm_{}", md.name)
            } else {
                format!("pkl_{name}_{}", md.name)
            };
            let mid = self.push_class_func(
                &format!("{name}.{}", md.name),
                &symbol,
                FnSource::Method {
                    table: table.clone(),
                    md,
                },
            );
            self.finfo.insert(mid, info.clone());
            self.src_param_tys
                .insert(mid, info.params.iter().map(|p| p.ty.clone()).collect());
            self.method_ids
                .insert((cid, md.name.clone()), (mid, info.is_static));
        }

        // Property accessors: `pkl_<Name>_<p>_get`/`_set` (instance, slot 0 is
        // `this`) and `pkl_<Name>_sm_<p>_get`/`_set` (static, receiver-less).
        for pd in members.iter().filter_map(|m| match m {
            ClassMember::Property(pd) => Some(pd),
            _ => None,
        }) {
            let Some(info) = table.properties.iter().find(|i| i.name == pd.name) else {
                continue;
            };
            let stem = if pd.is_static {
                format!("pkl_{name}_sm_{}_", pd.name)
            } else {
                format!("pkl_{name}_{}_", pd.name)
            };
            if info.has_get {
                let fid = self.push_class_func(
                    &format!("{}.{}.get", name, pd.name),
                    &format!("{stem}get"),
                    FnSource::Property {
                        table: table.clone(),
                        pd,
                        info: info.clone(),
                        is_set: false,
                        is_static: pd.is_static,
                    },
                );
                if pd.is_static {
                    self.static_property_ids
                        .insert((cid, pd.name.clone(), false), fid);
                } else {
                    self.property_ids.insert((cid, pd.name.clone(), false), fid);
                }
            }
            if info.has_set {
                let fid = self.push_class_func(
                    &format!("{}.{}.set", name, pd.name),
                    &format!("{stem}set"),
                    FnSource::Property {
                        table: table.clone(),
                        pd,
                        info: info.clone(),
                        is_set: true,
                        is_static: pd.is_static,
                    },
                );
                if pd.is_static {
                    self.static_property_ids
                        .insert((cid, pd.name.clone(), true), fid);
                } else {
                    self.property_ids.insert((cid, pd.name.clone(), true), fid);
                }
            }
        }

        // Inherit the superclass's method and property slots; a member the
        // subclass declares itself wins (`or_insert`).
        if let Some(pcid) = parent_cid {
            for ((id, mname), v) in self.method_ids.clone() {
                if id == pcid {
                    self.method_ids.entry((cid, mname)).or_insert(v);
                }
            }
            for ((id, pname, is_set), fid) in self.property_ids.clone() {
                if id == pcid {
                    self.property_ids.entry((cid, pname, is_set)).or_insert(fid);
                }
            }
            for ((id, pname, is_set), fid) in self.static_property_ids.clone() {
                if id == pcid {
                    self.static_property_ids
                        .entry((cid, pname, is_set))
                        .or_insert(fid);
                }
            }
        }

        let implements = table.implements.clone();
        self.classes.push(ClassPlan {
            name: name.to_string(),
            class_id: cid,
            parent: parent_cid,
            table,
        });
        self.class_by_name.insert(name.to_string(), cid);
        if !implements.is_empty() {
            let _: Result<(), ()> = self.register_implements(name, cid, &implements, span);
        }
    }

    /// The registered runtime class id of a resolved class/struct type.
    /// Non-generic types resolve through `class_by_name`; a generic type with
    /// concrete arguments materializes its instantiation on first use. `None`
    /// means the type is not (yet) lowerable: an uninstantiated generic (`Var`
    /// arguments, resolved lazily inside a generic body) or an unregistered
    /// name.
    fn class_id_of(&mut self, ty: &Ty, span: Span) -> Result<Option<u32>, ()> {
        let (n, args) = match ty {
            Ty::Class(n, a) | Ty::Struct(n, a) => (n, a),
            _ => return Ok(None),
        };
        if args.iter().any(|t| t.has_var()) {
            return Ok(None);
        }
        if args.is_empty() {
            return Ok(self.class_by_name.get(n).copied());
        }
        let key = (n.clone(), args.clone());
        let table_generics = match self.resolved.types.get(n) {
            Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => t.generics.len(),
            _ => return Ok(None),
        };
        if table_generics != args.len() {
            return Ok(None);
        }
        if self.class_inst_by_args.contains_key(&key) {
            return Ok(Some(self.class_inst_by_args[&key]));
        }
        self.register_class_instantiation(n, args, span).map(Some)
    }

    /// Materialize one concrete instantiation of a generic class/struct:
    /// substitute its members under the type arguments, reserve a class id, and
    /// register the instantiated constructor and instance methods (symbols are
    /// mangled with the argument suffix; bodies lower under the instantiation's
    /// substitution). Layout is uniform across instantiations (every slot is a
    /// pointer), so the plan records its substituted table under the mangled
    /// name and the shared layout helpers resolve it. Members whose lowering
    /// only exists for concrete classes are rejected loudly.
    fn register_class_instantiation(
        &mut self,
        name: &str,
        args: &[Ty],
        span: Span,
    ) -> Result<u32, ()> {
        if let Some(&cid) = self
            .class_inst_by_args
            .get(&(name.to_string(), args.to_vec()))
        {
            return Ok(cid);
        }
        let (table, members) = match self.resolved.types.get(name) {
            Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => {
                (t.clone(), self.generic_type_members.get(name).copied())
            }
            _ => return self.bad(span, format!("`{name}` is not a class or struct")),
        };
        if table.generics.len() != args.len() {
            return self.bad(
                span,
                format!(
                    "`{name}` takes {} type argument(s), found {}",
                    table.generics.len(),
                    args.len()
                ),
            );
        }
        let Some(members) = members else {
            return self.bad(span, format!("`{name}` is not lowerable"));
        };
        if table.extends.is_some() {
            return self.bad(span, format!("`{name}` with `extends` is not lowered yet"));
        }
        if members
            .iter()
            .any(|m| matches!(m, ClassMember::Method(md) if md.is_override))
        {
            return self.bad(
                span,
                format!("`{name}` uses `override`, which is not lowered yet"),
            );
        }
        if members
            .iter()
            .any(|m| matches!(m, ClassMember::Constructor(cd) if cd.name.is_some()))
        {
            return self.bad(
                span,
                format!("`{name}` uses named constructors, which are not lowered yet"),
            );
        }
        if members.iter().any(|m| matches!(m, ClassMember::Deinit(_))) {
            return self.bad(
                span,
                format!(
                    "`{name}` uses `deinit`, which is not lowered yet for generic classes/structs"
                ),
            );
        }
        if members
            .iter()
            .any(|m| matches!(m, ClassMember::Property(_)))
        {
            return self.bad(
                span,
                format!("`{name}` uses properties, which are not lowered yet for generic classes/structs"),
            );
        }
        if table.fields.iter().any(|f| f.is_static) || !table.consts.is_empty() {
            return self.bad(
                span,
                format!("`{name}` uses static state, which is not lowered yet for generic classes/structs"),
            );
        }
        let map: HashMap<String, Ty> = table
            .generics
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        // The implements list, with the instantiation's type arguments
        // substituted in so each entry is a concrete interface instantiation
        // (`Iterable<int>`).
        let implements: Vec<Ty> = table.implements.iter().map(|t| t.subst(&map)).collect();
        let suffix = args
            .iter()
            .map(|t| sanitize_symbol(&t.bare_name()))
            .collect::<Vec<_>>()
            .join("_");
        let display = format!(
            "{name}<{}>",
            args.iter()
                .map(|t| t.bare_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let stable = ClassTable {
            name: display.clone(),
            span: table.span,
            visibility: table.visibility,
            generics: table.generics.clone(),
            extends: None,
            implements: Vec::new(),
            fields: table
                .fields
                .iter()
                .map(|f| FieldInfo {
                    ty: f.ty.subst(&map),
                    ..f.clone()
                })
                .collect(),
            methods: table
                .methods
                .iter()
                .map(|m| CallableInfo {
                    params: m
                        .params
                        .iter()
                        .map(|p| ParamInfo {
                            ty: p.ty.subst(&map),
                            ..p.clone()
                        })
                        .collect(),
                    ret: m.ret.subst(&map),
                    ..m.clone()
                })
                .collect(),
            properties: Vec::new(),
            ctor: table.ctor.as_ref().map(|c| CtorInfo {
                params: c
                    .params
                    .iter()
                    .map(|p| ParamInfo {
                        ty: p.ty.subst(&map),
                        ..p.clone()
                    })
                    .collect(),
                ..c.clone()
            }),
            named_ctors: Vec::new(),
            consts: Vec::new(),
        };
        let ctor_decl = members.iter().find_map(|m| match m {
            ClassMember::Constructor(cd) if cd.name.is_none() => Some(cd),
            _ => None,
        });
        let inits = self.collect_instance_inits(name);
        let init_blocks = self.collect_init_blocks(name);

        let cid = (PICKLE_CLASS_USER_BASE + self.classes.len() as i64) as u32;
        let uniq = |em: &mut Self, base: &str| -> String {
            let mut symbol = base.to_string();
            let mut n = 0;
            while em.module.funcs.iter().any(|f| f.symbol == symbol) {
                n += 1;
                symbol = format!("{base}_v{n}");
            }
            symbol
        };

        let ctor_symbol = uniq(self, &format!("pkl_{name}_new__{suffix}"));
        let ctor_fid = self.push_class_func(
            &format!("{display}.new"),
            &ctor_symbol,
            FnSource::Ctor {
                table: stable.clone(),
                inits,
                init_blocks,
                ctor: ctor_decl,
            },
        );
        self.ctor_ids.insert(cid, ctor_fid);
        self.src_param_tys.insert(
            ctor_fid,
            stable
                .ctor
                .as_ref()
                .map(|c| c.params.iter().map(|p| p.ty.clone()).collect())
                .unwrap_or_default(),
        );
        self.instanton_subst.insert(ctor_fid, map.clone());
        self.fid_owner.insert(ctor_fid, cid as i64);
        self.fid_generics.insert(ctor_fid, table.generics.clone());

        for md in members.iter().filter_map(|m| match m {
            ClassMember::Method(md) => Some(md),
            _ => None,
        }) {
            if md.is_override || md.body.is_none() || !md.generics.is_empty() {
                continue;
            }
            let Some(info) = stable.methods.iter().find(|m| m.name == md.name).cloned() else {
                continue;
            };
            if info.params.iter().any(|p| p.has_default || p.rest) {
                continue;
            }
            if md.is_async {
                let sub_info = CallableInfo {
                    params: info
                        .params
                        .iter()
                        .map(|p| ParamInfo {
                            ty: p.ty.subst(&map),
                            ..p.clone()
                        })
                        .collect(),
                    ret: info.ret.subst(&map),
                    ..info.clone()
                };
                let is_struct = matches!(
                    self.resolved.types.get(name),
                    Some(TypeTableEntry::Struct(_))
                );
                self.register_async_method(
                    &AsyncRecv {
                        class: &display,
                        cid,
                        args,
                        is_struct,
                        generics: table.generics.clone(),
                    },
                    &stable,
                    md,
                    &sub_info,
                    &format!("_{suffix}"),
                    Some(map.clone()),
                );
                continue;
            }
            let symbol = if info.is_static {
                format!("pkl_{name}_sm_{}_{suffix}", md.name)
            } else {
                format!("pkl_{name}_{}_{suffix}", md.name)
            };
            let symbol = uniq(self, &symbol);
            let mid = self.push_class_func(
                &format!("{display}.{}", md.name),
                &symbol,
                FnSource::Method {
                    table: stable.clone(),
                    md,
                },
            );
            self.finfo.insert(mid, info.clone());
            self.src_param_tys
                .insert(mid, info.params.iter().map(|p| p.ty.clone()).collect());
            self.method_ids
                .insert((cid, md.name.clone()), (mid, info.is_static));
            self.instanton_subst.insert(mid, map.clone());
            self.fid_owner.insert(mid, cid as i64);
            self.fid_generics.insert(mid, table.generics.clone());
        }

        self.classes.push(ClassPlan {
            name: display.clone(),
            class_id: cid,
            parent: None,
            table: stable,
        });
        self.class_inst_by_args
            .insert((name.to_string(), args.to_vec()), cid);
        self.class_inst_subst.insert(cid, map);
        if !implements.is_empty() {
            let _: Result<(), ()> = self.register_implements(&display, cid, &implements, span);
        }
        Ok(cid)
    }

    fn push_class_func(&mut self, name: &str, symbol: &str, src: FnSource<'a>) -> FuncId {
        let fid = FuncId(self.module.funcs.len());
        self.module.funcs.push(IrFunc {
            name: name.to_string(),
            symbol: symbol.to_string(),
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            is_main: false,
            is_test: false,
            tags: Vec::new(),
        });
        self.fid_list.push(fid);
        self.fsource.insert(fid, src);
        fid
    }

    /// Linker symbol for a top-level `var`. The `pkl_global_` prefix is reserved:
    /// a function named `global_x` would otherwise collide with a global `x`.
    fn global_symbol(name: &str) -> String {
        format!("pkl_global_{name}")
    }

    /// Source `Ty` -> IR slot type, or `None` when this slice has no lowering
    /// for it yet.
fn global_ir_ty(ty: &Ty) -> Option<IrTy> {
        match ty {
            Ty::Int => Some(IrTy::Int),
            Ty::Bool => Some(IrTy::Bool),
            Ty::Char => Some(IrTy::Char),
            Ty::Float => Some(IrTy::Float),
            // Managed. The slot is zeroed statically and both the initial value
            // and the GC root registration are emitted into the synthetic
            // `pkl_globals_init` function, which the host runs before `main`.
            Ty::String => Some(IrTy::Str),
            _ => None,
        }
    }

/// Slot type for a global declared without a `: T` annotation.
///
/// Inference is deliberately as narrow as `global_init_bytes`: only an integer
/// literal (optionally negated) determines a type. Anything richer would have
/// to be a general static-evaluator, which this slice does not have.
fn global_infer_ir_ty(e: &Expr) -> Option<IrTy> {
        match &e.kind {
            ExprKind::Lit(Lit::Int { .. }) => Some(IrTy::Int),
            ExprKind::Lit(Lit::Float { .. }) => Some(IrTy::Float),
            ExprKind::Lit(Lit::Bool(_)) => Some(IrTy::Bool),
            ExprKind::Lit(Lit::Char(_)) => Some(IrTy::Char),
            ExprKind::Lit(Lit::String(_)) => Some(IrTy::Str),
            ExprKind::Unary { op: AstUnOp::Neg, operand } => match &operand.kind {
                ExprKind::Lit(Lit::Int { .. }) => Some(IrTy::Int),
                ExprKind::Lit(Lit::Float { .. }) => Some(IrTy::Float),
                _ => None,
            },
            _ => None,
        }
    }

/// Build the module-level global table in source order.
///
/// Only *static* initializers are accepted, because these bytes are written
/// into the image before `main` runs. Anything that needs a call, a load, or
/// an allocation is rejected here with a diagnostic rather than being
/// silently evaluated at some other time.
fn register_globals(&mut self) {
   let items: Vec<(String, GlobalDecl)> = self
      .global_decls
      .iter()
      .map(|(k, v)| (k.clone(), (*v).clone()))
      .collect();
   // Source order, not hash order, so the data blob is reproducible.
   let mut ordered: Vec<(String, GlobalDecl)> = Vec::new();
   for item in &self.prog.items {
      if let ItemKind::Global(g) = &item.kind {
         ordered.push((g.name.clone(), g.clone()));
      }
   }
   debug_assert_eq!(items.len(), ordered.len());
   for (name, g) in ordered {
      let declared = self
         .resolved
         .globals
         .iter()
         .find(|f| f.name == name)
         .map(|f| f.ty.clone())
         .unwrap_or(Ty::Unknown);
let ty_explicit = g.ty.is_some();
// An unannotated `var` takes its type from the initializer, but only when
// that inference is unambiguous for this slice.
let declared_ir = Self::global_ir_ty(&declared).or_else(|| {
if ty_explicit {
None
} else {
Self::global_infer_ir_ty(&g.value)
}
});
match declared_ir {
Some(it) => {
            let init = match self.global_init_bytes(&g.value, it, &name) {
               Ok(b) => b,
               Err(msg) => {
                  self.global_failures.push((name.clone(), g.span, msg));
                  continue;
               }
            };
            let id = self.module.globals.len();
            self.module.globals.push(IrGlobal {
               symbol: Self::global_symbol(&name),
               ty: it,
               ty_explicit,
               init,
            });
            self.global_ids.insert(name, id);
            if it == IrTy::Str {
                self.managed_global_inits.push((id, g.value.clone()));
            }
         }
         None => {
            self.global_failures.push((
               name.clone(),
               g.span,
               format!("a top-level `var` of type `{}` is not supported yet; only `int64` slots have a lowering", declared),
            ));
         }
      }
   }
}

/// Reduce a global initializer to little-endian bytes, or explain why not.
///
/// The supported set is deliberately just integer literals (optionally
/// negated): enough to prove the `.data` path end to end without inventing a
/// general static-evaluator.
fn global_init_bytes(&mut self, e: &Expr, ty: IrTy, name: &str) -> Result<Option<Vec<u8>>, String> {
      let mismatch = || {
            // The article is part of the type phrase: "an integer" but "a bool".
            let want = match ty {
                IrTy::Bool => "a bool",
                IrTy::Char => "a char",
                IrTy::Float => "a float",
                _ => "an integer",
            };
            format!(
                "the initializer of top-level `var {name}` must be {want} literal; it runs before `main` and cannot call or read other globals yet"
            )
        };
      // Negation is the one unary that still yields a static scalar.
      let negated = match &e.kind {
          ExprKind::Unary {
              op: crate::ast::UnOp::Neg,
              operand,
          } => Some(&**operand),
          _ => None,
      };
      match ty {
          IrTy::Int => {
              let v = match &e.kind {
                  ExprKind::Lit(Lit::Int { value: v }) => *v as i64,
                  _ => match negated {
                      Some(inner) => match &inner.kind {
                          ExprKind::Lit(Lit::Int { value: v }) => (*v as i64).wrapping_neg(),
                          _ => return Err(mismatch()),
                      },
                      None => return Err(mismatch()),
                  },
              };
              Ok(Some(v.to_le_bytes().to_vec()))
          }
          IrTy::Float => {
              let v = match &e.kind {
                  ExprKind::Lit(Lit::Float { value: v }) => *v,
                  _ => match negated {
                      Some(inner) => match &inner.kind {
                          ExprKind::Lit(Lit::Float { value: v }) => -*v,
                          _ => return Err(mismatch()),
                      },
                      None => return Err(mismatch()),
                  },
              };
              Ok(Some(v.to_le_bytes().to_vec()))
          }
          IrTy::Bool => match &e.kind {
              ExprKind::Lit(Lit::Bool(b)) => Ok(Some(vec![if *b { 1 } else { 0 }])),
              _ => Err(mismatch()),
          },
          IrTy::Char => match &e.kind {
              ExprKind::Lit(Lit::Char(c)) => Ok(Some((*c as u32).to_le_bytes().to_vec())),
              _ => Err(mismatch()),
          },
            IrTy::Unit => Ok(None),
            // Managed: no static bytes. The COFF writer already emits an
            // all-zero 8-byte slot for `init: None`, and the value plus the
            // `pickle_gc_root_add` registration are emitted into
            // `pkl_globals_init` instead.
            IrTy::Str => Ok(None),
            other => Err(format!(
                "a top-level `var {name}` of type `{other}` has no static initializer yet"
            )),
      }
  }

fn symbol_for(&self, name: &str, is_test: bool) -> String {
   if is_test {
            format!("pickle_test_{name}")
        } else if name == "main" {
            "pickle_main".to_string()
        } else {
            format!("pkl_{name}")
        }
    }

    /// Collect an `#[symbol("...")]` override from an item's attributes, giving a
    /// top-level function an exact C ABI symbol name instead of the `pkl_<name>`
    /// default. This is what lets the ported runtime define `pickle_*` ABI
    /// functions directly in Pickle. Returns `None` when absent.
    fn item_symbol(&self, attrs: &[Attribute]) -> Option<String> {
        for a in attrs {
            if a.name != "symbol" {
                continue;
            }
            for arg in &a.args {
                if let ExprKind::Lit(Lit::String(parts)) = &arg.kind {
                    if parts.iter().all(|p| matches!(p, StrPart::Text(_))) {
                        let text: String = parts
                            .iter()
                            .map(|p| match p {
                                StrPart::Text(t) => t.clone(),
                                StrPart::Expr(_) => String::new(),
                            })
                            .collect();
                        if !text.is_empty() {
                            return Some(text);
                        }
                    }
                }
            }
        }
        None
    }

    /// `symbol_for`, but honoring an `#[symbol("...")]` attribute override so a
    /// function can claim an exact ABI symbol (e.g. a ported runtime's
    /// `pickle_str_len`).
    fn symbol_for_attrs(&self, name: &str, is_test: bool, attrs: &[Attribute]) -> String {
        match self.item_symbol(attrs) {
            Some(s) => s,
            None => self.symbol_for(name, is_test),
        }
    }

    /// The single user (non-builtin, non-overloaded) callable for `name`.
    fn user_callable(&self, name: &str) -> Option<&'a CallableInfo> {
        let infos = self.resolved.fns.get(name)?;
        let mut users = infos
            .iter()
            .filter(|c| !(c.span.file.0 == 0 && c.span.end == 0));
        let first = users.next()?;
        if users.next().is_some() {
            return None;
        }
        Some(first)
    }

    // ---- per-function build ----------------------------------------------

    fn build_func(&mut self, fid: FuncId) {
        self.fname = self.module.funcs[fid.0].name.clone();
        self.symbol = self.module.funcs[fid.0].symbol.clone();
        self.fparams = Vec::new();
        self.fslots = Vec::new();
        self.env = vec![HashMap::new()];
        self.manual_env = vec![HashSet::new()];
        self.blocks = vec![IrBlock {
            id: BlockId(0),
            instrs: Vec::new(),
            term: IrTerm::Unreachable,
        }];
        self.next_block = 1;
        self.next_temp = 0;
        self.loops = Vec::new();
        self.cur = BlockId(0);
        self.owner = None;
        self.current_fid = Some(fid);

        // An instantiated generic function's body is checked over its generic
        // parameters, so every inferred type in it carries `Var`s. Apply the
        // instantiation's substitution while this body compiles. Lambdas,
        // trampolines, and non-instantiated functions carry none.
        self.current_subst = self.instanton_subst.get(&fid).cloned().unwrap_or_default();
        self.fn_generics =
            self.fid_generics
                .get(&fid)
                .cloned()
                .unwrap_or_else(|| match &self.fsource.get(&fid) {
                    Some(FnSource::TopLevel(f)) => {
                        f.generics.iter().map(|g| g.name.clone()).collect()
                    }
                    _ => Vec::new(),
                });

        let Some(src) = self.fsource.get(&fid).cloned() else {
            return;
        };
        let is_main = self.module.funcs[fid.0].is_main;
        match src {
            FnSource::TopLevel(f) => {
                let Some(info) = self.finfo.get(&fid).cloned() else {
                    return;
                };
                self.declare_params(&info.params);
                self.adopt_manual_params(&f.params);
                self.fret = self.map_ty(&info.ret, f.span).unwrap_or(IrTy::Unit);
                self.emit_body(&f.body);
            }
            FnSource::Lambda {
                lambda,
                captures,
                pty,
                ret,
                owner,
            } => self.build_lambda_body(lambda, &captures, &pty, &ret, owner, fid),
            FnSource::AsyncFn { f, hidden } => {
                let Some(info) = self.finfo.get(&fid).cloned() else {
                    return;
                };
                self.declare_params(&info.params);
                self.adopt_manual_params(&f.params);
                // The wrapper returns the future: a closure snapshotting each
                // parameter under its own name (slot 0 is the hidden body).
                self.fret = IrTy::Ptr;
                let captures: Vec<Capture> = info
                    .params
                    .iter()
                    .map(|p| Capture {
                        name: p.name.clone(),
                        ty: p.ty.clone(),
                        span: p.span,
                    })
                    .collect();
                if let Ok(obj) = self.closure_obj_span(f.span, hidden, &captures) {
                    self.term(IrTerm::Return { v: Some(obj) });
                }
            }
            FnSource::AsyncBody { f, pty, ret } => {
                self.build_async_body(f, &pty, &ret, fid);
            }
            FnSource::AsyncMethod {
                table,
                md,
                hidden,
                is_static,
                this_ty,
            } => {
                let Some(info) = self.finfo.get(&fid).cloned() else {
                    return;
                };
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                self.build_async_method_wrapper(md, &info, hidden, is_static, this_ty);
            }
            FnSource::AsyncMethodBody {
                table,
                md,
                pty,
                ret,
                is_static,
            } => {
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                self.build_async_method_body(md, &pty, &ret, is_static);
            }
            FnSource::Trampoline {
                span,
                target,
                pty,
                ret,
            } => {
                self.owner = None;
                self.build_trampoline(span, target, &pty, &ret);
            }
            FnSource::MethodTrampoline {
                span,
                target,
                pty,
                ret,
                branches,
            } => {
                self.owner = None;
                self.build_method_trampoline(span, target, &pty, &ret, &branches);
            }
            FnSource::SpreadTrampoline {
                span,
                target,
                pty,
                ret,
                receiver,
            } => {
                self.owner = None;
                self.build_spread_trampoline(span, target, &pty, &ret, receiver);
            }
            FnSource::Ctor {
                table,
                inits,
                init_blocks,
                ctor,
            } => {
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                let _ = self.build_ctor_body(&table, &inits, &init_blocks, ctor);
            }
            FnSource::NamedCtor { table, ctor } => {
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                let _ = self.build_named_ctor(&table, ctor);
            }
            FnSource::Deinit { table, body } => {
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                let _ = self.build_deinit_body(&table, body);
            }
            FnSource::Method { table, md } => {
                let Some(info) = self.finfo.get(&fid).cloned() else {
                    return;
                };
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                let _ = self.build_method_body(&table, md, &info);
            }
            FnSource::Property {
                table,
                pd,
                info,
                is_set,
                is_static,
            } => {
                self.owner = self.fid_owner.get(&fid).copied().or_else(|| {
                    self.class_by_name
                        .get(&table.name)
                        .copied()
                        .map(|x| x as i64)
                });
                let _ = self.build_property_body(&table, pd, &info, is_set, is_static);
            }
            FnSource::StaticInit { inits } => {
                self.fret = IrTy::Unit;
                // Default-initialize every cell first, so an initializer that
                // reads another static (including a later one) sees the field
                // type's zero/null rather than an unset cell. This mirrors
                // `class_new` zeroing instance slots before field inits run.
                for (cid, slot, _) in &inits {
                    if self.build_static_default(*cid, *slot).is_err() {
                        return;
                    }
                }
                for (cid, slot, x) in &inits {
                    let Some(x) = x else {
                        continue;
                    };
                    // Static-field initializers resolve bare names against the
                    // declaring class (there is no `this`).
                    self.owner = Some(*cid as i64);
                    if self.build_static_init_one(*cid, *slot, x).is_err() {
                        return;
                    }
                }
                self.term(IrTerm::Return { v: None });
            }
            FnSource::GlobalsInit { inits } => {
                self.fret = IrTy::Unit;
                self.build_globals_init(&inits);
                self.term(IrTerm::Return { v: None });
            }
            FnSource::TestSetup => {
                // Class/struct registration calls run first, before any
                // statement, so every descriptor exists when the class functions
                // run. `pkl_static_init` (when present) runs right after.
                self.fret = IrTy::Unit;
                self.inject_class_registrations();
                self.term(IrTerm::Return { v: None });
            }
        }
        if self.failed {
            return;
        }

        // Class/struct registration calls run first inside `main`, before any
        // user statement, so every descriptor exists when the class functions
        // are called (the id each call site hardcodes is the runtime's too).
        // The runtime appends descriptors, so registration must happen exactly
        // once per process: `main` when present, otherwise the dedicated
        // `pkl_test_setup` function (test modules have no `main`). The first
        // test only carries the preamble when no setup function was synthesized
        // (no classes/statics to register, so the call is a no-op anyway).
        let is_first_test = self.test_setup_id.is_none()
            && self.module.funcs[fid.0].is_test
            && !self.module.funcs[..fid.0].iter().any(|f| f.is_test);
        if is_main || is_first_test {
            self.inject_class_registrations();
        }

        self.module.funcs[fid.0] = IrFunc {
            name: self.fname.clone(),
            symbol: self.symbol.clone(),
            params: std::mem::take(&mut self.fparams),
            ret: self.fret,
            slots: std::mem::take(&mut self.fslots),
            entry: BlockId(0),
            blocks: std::mem::take(&mut self.blocks),
            is_main: self.module.funcs[fid.0].is_main,
            is_test: self.module.funcs[fid.0].is_test,
            tags: std::mem::take(&mut self.module.funcs[fid.0].tags),
        };
    }

    /// Build the hoisted body of a lambda. Slot 0 is the closure object; the
    /// captured values are copied out of it into fresh locals before the body
    /// runs (read-only snapshots), and the lambda's own parameters follow.
    fn build_lambda_body(
        &mut self,
        lambda: &'a Expr,
        captures: &[Capture],
        pty: &[Ty],
        ret: &Ty,
        owner: Option<i64>,
        _fid: FuncId,
    ) {
        let ExprKind::Lambda { params, body, .. } = &lambda.kind else {
            let _ = self.bad::<()>(lambda.span, "lambda lost its body");
            return;
        };
        let pty: Vec<Ty> = if pty.is_empty() {
            match self.types.get(&lambda.span) {
                Some(Ty::Fn(pts, _)) => pts.clone(),
                _ => {
                    let _ = self.bad::<()>(
                        lambda.span,
                        "lambda parameter types are not statically known",
                    );
                    return;
                }
            }
        } else {
            pty.to_vec()
        };
        let _ = self.map_ty(ret, lambda.span).map(|ir| self.fret = ir);
        self.owner = owner;

        // Slot 0 is the closure object (storage managed by the caller). It must
        // stay at fparam index 0 so the JIT hands the closure to slot 0.
        let env_ir = IrTy::Ptr;
        let env_slot = self.new_slot(env_ir);
        self.fparams.push(IrParam {
            name: "env".to_string(),
            ty: env_ir,
        });
        self.declare("env", env_slot);
        let env_obj = self.load(env_slot);

        // The lambda's own parameters, typed from the checker's Fn signature.
        // These occupy slots 1..=n, matching their argument order, because the
        // JIT delivers call arguments by fparam index.
        for (i, p) in params.iter().enumerate() {
            let t = pty.get(i).cloned().unwrap_or(Ty::Unknown);
            let ir = self.map_ty(&t, p.span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            self.fparams.push(IrParam {
                name: p.name.clone(),
                ty: ir,
            });
            self.declare(&p.name, slot);
        }

        // Captured values go into fresh slots after every parameter; a capture
        // whose name a parameter shadows is skipped (`declare` would otherwise
        // clobber the parameter binding).
        for (i, cap) in captures.iter().enumerate() {
            let v = match self.field_read(cap.span, env_obj, &cap.ty, 1 + i) {
                Ok(v) => v,
                Err(()) => return,
            };
            let ir = self.map_ty(&cap.ty, cap.span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            self.instr(IrInstr::StoreSlot { slot, v });
            let shadowed = self.env.is_empty() || params.iter().any(|p| p.name == cap.name);
            if !shadowed {
                self.declare(&cap.name, slot);
            }
        }

        let _ = self.emit_body(&Some(body.clone()));
    }

    /// Build the hidden future body of an `async fn`. It shares the hoisted
    /// lambda ABI with zero parameters: slot 0 is the closure object, whose
    /// slots 1..N snapshot the async fn's parameters (built by the wrapper via
    /// `closure_obj_span` in parameter order). Those parameters are declared as
    /// named captures, then the fn's real body is emitted with its return type.
    fn build_async_body(&mut self, f: &'a FnDecl, pty: &[Ty], ret: &Ty, _fid: FuncId) {
        let _ = self.map_ty(ret, f.span).map(|ir| self.fret = ir);
        self.owner = None;
        let env_ir = IrTy::Ptr;
        let env_slot = self.new_slot(env_ir);
        self.fparams.push(IrParam {
            name: "env".to_string(),
            ty: env_ir,
        });
        self.declare("env", env_slot);
        let env_obj = self.load(env_slot);
        for (i, p) in f.params.iter().enumerate() {
            let t = pty.get(i).cloned().unwrap_or(Ty::Unknown);
            let v = match self.field_read(p.span, env_obj, &t, 1 + i) {
                Ok(v) => v,
                Err(()) => return,
            };
            let ir = self.map_ty(&t, p.span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            self.instr(IrInstr::StoreSlot { slot, v });
            self.declare(&p.name, slot);
        }
        let _ = self.emit_body(&f.body);
    }

    /// Build the visible wrapper of an `async` method: it has the method's real
    /// ABI (`this` at slot 0 for instance methods, then the parameters, exactly
    /// like `build_method_body`) but returns the future — a closure over the
    /// receiver (if any) and the parameters — without running the body.
    fn build_async_method_wrapper(
        &mut self,
        md: &'a MethodDecl,
        info: &CallableInfo,
        hidden: FuncId,
        is_static: bool,
        this_ty: Option<Ty>,
    ) {
        let mut d = 0usize;
        let mut captures: Vec<Capture> = Vec::new();
        if !is_static {
            let this_slot = Slot(0);
            self.fslots.push(IrTy::Ptr);
            self.fparams.push(IrParam {
                name: "this".to_string(),
                ty: IrTy::Ptr,
            });
            self.declare("this", this_slot);
            captures.push(Capture {
                name: "this".to_string(),
                ty: this_ty.unwrap_or(Ty::Unknown),
                span: md.span,
            });
            d = 1;
        }
        for (i, p) in info.params.iter().enumerate() {
            let ir = self.map_ty(&p.ty, p.span).unwrap_or(IrTy::Ptr);
            let slot = Slot((d + i) as u32);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: p.name.clone(),
                ty: ir,
            });
            self.declare(&p.name, slot);
            captures.push(Capture {
                name: p.name.clone(),
                ty: p.ty.clone(),
                span: p.span,
            });
        }
        self.fret = IrTy::Ptr;
        if let Ok(obj) = self.closure_obj_span(md.span, hidden, &captures) {
            self.term(IrTerm::Return { v: Some(obj) });
        }
    }

    /// Build the hidden future body of an `async` method. The closure slots hold
    /// the receiver (instance methods) then the parameters; the receiver is
    /// re-declared as `this` so the body's receiver-fallback operates exactly as
    /// in a regular method body.
    fn build_async_method_body(
        &mut self,
        md: &'a MethodDecl,
        pty: &[Ty],
        ret: &Ty,
        is_static: bool,
    ) {
        let _ = self.map_ty(ret, md.span).map(|ir| self.fret = ir);
        let env_ir = IrTy::Ptr;
        let env_slot = self.new_slot(env_ir);
        self.fparams.push(IrParam {
            name: "env".to_string(),
            ty: env_ir,
        });
        self.declare("env", env_slot);
        let env_obj = self.load(env_slot);
        let base = if is_static { 0usize } else { 1usize };
        if !is_static {
            let t = pty.first().cloned().unwrap_or(Ty::Unknown);
            match self.field_read(md.span, env_obj, &t, 1) {
                Ok(v) => {
                    let ir = self.map_ty(&t, md.span).unwrap_or(IrTy::Ptr);
                    let slot = self.new_slot(ir);
                    self.instr(IrInstr::StoreSlot { slot, v });
                    self.declare("this", slot);
                }
                Err(()) => return,
            }
        }
        for (i, p) in md.params.iter().enumerate() {
            let t = pty.get(base + i).cloned().unwrap_or(Ty::Unknown);
            let v = match self.field_read(p.span, env_obj, &t, 1 + base + i) {
                Ok(v) => v,
                Err(()) => return,
            };
            let ir = self.map_ty(&t, p.span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            self.instr(IrInstr::StoreSlot { slot, v });
            self.declare(&p.name, slot);
        }
        let _ = self.emit_body(&md.body);
    }

    /// Declare function parameters as slots 0..n (used by top-level fns).
    fn declare_params(&mut self, params: &[ParamInfo]) {
        for (i, p) in params.iter().enumerate() {
            let ir = self.map_ty(&p.ty, p.span).unwrap_or(IrTy::Ptr);
            let slot = Slot(i as u32);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: p.name.clone(),
                ty: ir,
            });
            self.declare(&p.name, slot);
        }
    }

    /// Build the body of a function-value trampoline: a forwarder with the
    /// hoisted-lambda signature `(env, args...) -> ret` whose body simply calls
    /// the wrapped top-level function and returns its result. The closure
    /// object (slot 0) is unused: module functions capture nothing.
    fn build_trampoline(&mut self, span: Span, target: FuncId, pty: &[Ty], ret: &Ty) {
        let _ = self.map_ty(ret, span).map(|ir| self.fret = ir);
        let env_ir = IrTy::Ptr;
        let env_slot = self.new_slot(env_ir);
        self.fparams.push(IrParam {
            name: "env".to_string(),
            ty: env_ir,
        });
        self.declare("env", env_slot);
        let mut args = Vec::with_capacity(pty.len());
        for (i, t) in pty.iter().enumerate() {
            let ir = self.map_ty(t, span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            let pname = format!("arg{i}");
            self.fparams.push(IrParam {
                name: pname.clone(),
                ty: ir,
            });
            self.declare(&pname, slot);
            args.push(self.load(slot));
        }
        if matches!(self.fret, IrTy::Unit) {
            self.instr(IrInstr::Call {
                dst: None,
                callee: Callee::Func(target),
                args,
            });
            self.term(IrTerm::Return { v: None });
        } else {
            let dst = self.temp();
            self.instr(IrInstr::Call {
                dst: Some(dst),
                callee: Callee::Func(target),
                args,
            });
            self.term(IrTerm::Return { v: Some(dst) });
        }
    }

    /// Build the body of a bound-method trampoline: a forwarder with the
    /// hoisted-lambda signature `(env, args...) -> ret` whose closure subject
    /// slot 1 holds the bound receiver. The body loads that receiver and calls
    /// the wrapped instance method `target` with it first, then returns the
    /// result. This lets `c.method` (an instance method used as a value) be a
    /// one-capture closure object, dispatching uniformly through
    /// `fn_value_call`.
    fn build_method_trampoline(
        &mut self,
        span: Span,
        target: FuncId,
        pty: &[Ty],
        ret: &Ty,
        branches: &[(u32, FuncId)],
    ) {
        let _ = self.map_ty(ret, span).map(|ir| self.fret = ir);
        let env_ir = IrTy::Ptr;
        let env_slot = self.new_slot(env_ir);
        self.fparams.push(IrParam {
            name: "env".to_string(),
            ty: env_ir,
        });
        self.declare("env", env_slot);
        // Slot 1 of the closure object is the bound receiver (a class object,
        // so `box_for_store`/`elem_rep` store it as its identity pointer).
        let env = self.load(env_slot);
        let one = self.int_const(1);
        let receiver = match self.extern_call_t1(
            "pickle_obj_slot_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![env, one],
        ) {
            Ok(t) => t,
            Err(()) => return,
        };
        let mut real: Vec<Temp> = Vec::new();
        for (i, t) in pty.iter().enumerate() {
            let ir = self.map_ty(t, span).unwrap_or(IrTy::Ptr);
            let slot = self.new_slot(ir);
            let pname = format!("arg{i}");
            self.fparams.push(IrParam {
                name: pname.clone(),
                ty: ir,
            });
            self.declare(&pname, slot);
            real.push(self.load(slot));
        }
        if branches.is_empty() {
            let mut args = vec![receiver];
            args.extend(real);
            if matches!(self.fret, IrTy::Unit) {
                self.instr(IrInstr::Call {
                    dst: None,
                    callee: Callee::Func(target),
                    args,
                });
                self.term(IrTerm::Return { v: None });
            } else {
                let dst = self.temp();
                self.instr(IrInstr::Call {
                    dst: Some(dst),
                    callee: Callee::Func(target),
                    args,
                });
                self.term(IrTerm::Return { v: Some(dst) });
            }
            return;
        }
        // Overridden: dispatch on the runtime class of the captured receiver,
        // exactly like `virtual_method_call`, then fall back to `target`.
        let anchor = Expr {
            span,
            kind: ExprKind::Ident(String::new()),
        };
        let ret_slot = self.new_slot(self.fret);
        let join = self.new_block();
        for &(dcid, dfid) in branches {
            let cb = self.new_block();
            let next = self.new_block();

            let dc = self.int_const(dcid as i64);
            let found = match self.extern_call_t1(
                "pickle_class_is",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Bool,
                vec![receiver, dc],
            ) {
                Ok(t) => t,
                Err(()) => return,
            };
            self.term(IrTerm::BranchIf {
                cond: found,
                then: cb,
                else_: next,
            });

            self.cur = cb;
            let mut args = vec![receiver];
            args.extend(real.iter().copied());
            let val = match self.emit_call_to(dfid, args, &anchor) {
                Ok(v) => v,
                Err(()) => return,
            };
            self.instr(IrInstr::StoreSlot {
                slot: ret_slot,
                v: val,
            });
            self.term(IrTerm::Branch { target: join });

            self.cur = next;
        }
        let mut args = vec![receiver];
        args.extend(real.iter().copied());
        let val = match self.emit_call_to(target, args, &anchor) {
            Ok(v) => v,
            Err(()) => return,
        };
        self.instr(IrInstr::StoreSlot {
            slot: ret_slot,
            v: val,
        });
        self.term(IrTerm::Branch { target: join });

        self.cur = join;
        let dst = self.temp();
        self.instr(IrInstr::LoadSlot {
            dst,
            slot: ret_slot,
        });
        self.term(IrTerm::Return { v: Some(dst) });
    }

    /// Build the body of a spread trampoline: a forwarder whose parameters are
    /// `(recv)?, boxed` — an optional receiver slot followed by a boxed `List`
    /// that holds every argument value (fixed positions boxed by each
    /// parameter's own representation, spread elements in place). The body
    /// unboxes each entry back by the parameter's own representation and calls
    /// `target` with the ordinary `(slot0, ...)` ABI, returning its result.
    /// The caller's arity guard guarantees exactly `pty.len()` entries.
    fn build_spread_trampoline(
        &mut self,
        span: Span,
        target: FuncId,
        pty: &[Ty],
        ret: &Ty,
        receiver: bool,
    ) {
        let _ = self.map_ty(ret, span).map(|ir| self.fret = ir);
        let recv_slot = if receiver {
            let recv_slot = self.new_slot(IrTy::Ptr);
            self.fparams.push(IrParam {
                name: "recv".to_string(),
                ty: IrTy::Ptr,
            });
            self.declare("recv", recv_slot);
            recv_slot
        } else {
            Slot(0)
        };
        let list_slot = self.new_slot(IrTy::Ptr);
        self.fparams.push(IrParam {
            name: "boxed".to_string(),
            ty: IrTy::Ptr,
        });
        self.declare("boxed", list_slot);
        let list = self.load(list_slot);
        let mut real: Vec<Temp> = Vec::new();
        for (i, t) in pty.iter().enumerate() {
            let idx = self.int_const(i as i64);
            let raw = match self.extern_call_t1(
                "pickle_list_get",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![list, idx],
            ) {
                Ok(t) => t,
                Err(()) => return,
            };
            match self.elem_rep(t, span) {
                Ok(ElemRep::Scalar(_, unbox, ir)) => {
                    let v = self.extern_call_t1(unbox, vec![IrTy::Ptr], ir, vec![raw]);
                    let v = match v {
                        Ok(v) => v,
                        Err(()) => return,
                    };
                    real.push(v);
                }
                Ok(ElemRep::Ptr) => real.push(raw),
                Err(()) => return,
            }
        }
        let mut args = Vec::new();
        if receiver {
            args.push(self.load(recv_slot));
        }
        args.extend(real);
        if matches!(self.fret, IrTy::Unit) {
            self.instr(IrInstr::Call {
                dst: None,
                callee: Callee::Func(target),
                args,
            });
            self.term(IrTerm::Return { v: None });
        } else {
            let dst = self.temp();
            self.instr(IrInstr::Call {
                dst: Some(dst),
                callee: Callee::Func(target),
                args,
            });
            self.term(IrTerm::Return { v: Some(dst) });
        }
    }

    /// Lower a constructor. Parameters are the explicit `constructor(...)`'s
    /// params when one is declared, otherwise the instance fields without
    /// initializers (in declaration order). The body allocates the object,
    /// runs each field initializer in declaration order, runs the explicit
    /// constructor body followed by any `init` blocks, and returns the object.
    fn build_ctor_body(
        &mut self,
        table: &ClassTable,
        inits: &[(usize, &'a Expr)],
        init_blocks: &[&'a Block],
        ctor: Option<&'a ConstructorDecl>,
    ) -> Result<(), ()> {
        // All instance fields, superclass first, so the object's slots and the
        // synthesized constructor parameters cover inherited state too.
        let ifields: Vec<FieldInfo> = self.all_instance_fields(&table.name);
        // `(name, ty, span, field_slot)`: synthesized constructor parameters
        // carry the field slot they must be stored into; explicit constructor
        // parameters are plain locals assigned by the body.
        let params: Vec<(String, Ty, Span, Option<usize>)> = if let Some(c) = &table.ctor {
            c.params
                .iter()
                .map(|p| (p.name.clone(), p.ty.clone(), p.span, None))
                .collect()
        } else {
            // Synthesized constructor: the nearest explicit-primary ancestor's
            // parameters (forwarded to the chained `super(...)`, `None`) then
            // the uninitialized instance fields strictly below that ancestor
            // (`Some(absolute field slot)`).
            let (slots, _chain) = self.synthesized_ctor_param_meta(&table.name);
            let mut out = Vec::new();
            if let Some((anc, _)) = self.nearest_explicit_primary_ancestor(&table.name) {
                if let Some(t) = self.table_of(&anc) {
                    if let Some(c) = &t.ctor {
                        for p in &c.params {
                            out.push((p.name.clone(), p.ty.clone(), p.span, None));
                        }
                    }
                }
            }
            for slot in slots.iter().flatten() {
                let f = &ifields[*slot];
                out.push((f.name.clone(), f.ty.clone(), f.span, Some(*slot)));
            }
            out
        };
        for (i, (name, ty, _span, _)) in params.iter().enumerate() {
            let ir = self.map_ty(ty, table.span).unwrap_or(IrTy::Ptr);
            let slot = Slot(i as u32);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: name.clone(),
                ty: ir,
            });
            self.declare(name, slot);
        }
        self.fret = IrTy::Ptr;

        let cid = self.owner.unwrap_or(PICKLE_CLASS_USER_BASE) as u32;
        let this_slot = self.new_slot(IrTy::Ptr);
        self.declare("this", this_slot);

        let cid_t = self.int_const(cid as i64);
        let n_t = self.int_const(ifields.len() as i64);
        let this = self.extern_call_t1(
            "pickle_class_new",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![cid_t, n_t],
        )?;
        self.instr(IrInstr::StoreSlot {
            slot: this_slot,
            v: this,
        });

        let this = self.load(this_slot);

        // Field initializers, in declaration order; they may read earlier
        // fields through `this`.
        for &(slot, init) in inits {
            let ty = ifields[slot].ty.clone();
            let manual = ifields[slot].manual;
            let v = self.expr(init)?;
            let v = if manual {
                self.extern_call_t1("pickle_manual_adopt", vec![IrTy::Ptr], IrTy::Ptr, vec![v])?
            } else {
                v
            };
            let rep = self.elem_rep(&ty, table.span)?;
            let vt = self.ty_of(&init.span).unwrap_or(Ty::Unknown);
            let packed = self.pack_for_pointer_boundary(&rep, v, &vt, init.span)?;
            let boxed = self.box_for_store(&rep, packed, elem_ir(&ty))?;
            self.field_store(this, slot as i64, &ty, boxed);
        }

        // Store every synthesized parameter into its field slot (explicit
        // constructor parameters are locals the body assigns itself).
        for (i, (_, ty, span, field_slot)) in params.iter().enumerate() {
            if let Some(field_slot) = field_slot {
                let v = self.load(Slot(i as u32));
                let rep = self.elem_rep(ty, *span)?;
                let packed = self.pack_for_pointer_boundary(&rep, v, ty, *span)?;
                let boxed = self.box_for_store(&rep, packed, elem_ir(ty))?;
                self.field_store(this, *field_slot as i64, ty, boxed);
            }
        }

        // Explicit constructor body, then any `init` blocks. A leading
        // `super(...)` runs the parent's constructor chain inlined first
        // (field initializers across the ancestry already ran above, and
        // `init` blocks for the whole hierarchy run below).
        if let Some(cd) = ctor {
            if let Some(delegation) = ctor_super_delegation(&cd.body) {
                self.emit_super_chain(&table.name, delegation)?;
                self.emit_ctor_block_from(&cd.body, 1)?;
            } else {
                self.emit_ctor_block(&cd.body)?;
            }
        }
        // A class with no explicit constructor that sits below an
        // explicit-primary ancestor chains into it via a synthesized
        // `super(...)`: the leading constructor parameters are forwarded and
        // the ancestor's body (and any further chain) runs inline.
        if ctor.is_none() {
            if let Some((ancestor, count)) = self.synthesized_ctor_param_meta(&table.name).1 {
                self.emit_synthesized_super_chain(&ancestor, count)?;
            }
        }
        for b in init_blocks {
            self.emit_ctor_block(b)?;
        }

        let done = self.load(this_slot);
        self.term(IrTerm::Return { v: Some(done) });
        let _ = self.tail_cleanup();
        Ok(())
    }

    /// Run a `Block` without turning its tail expression into the function's
    /// return value (used for constructor bodies and `init` blocks, where the
    /// constructor always returns `this`).
    fn emit_ctor_block(&mut self, b: &Block) -> Result<(), ()> {
        self.emit_ctor_block_from(b, 0)
    }

    /// Run a constructor `Block` starting at statement `from`, skipping the
    /// leading statements (i.e. the consumed `super(...)` delegation).
    fn emit_ctor_block_from(&mut self, b: &Block, from: usize) -> Result<(), ()> {
        self.push_scope();
        for s in &b.stmts[from..] {
            if self.stmt(s).is_err() {
                self.pop_scope();
                return Err(());
            }
        }
        if let Some(e) = &b.expr {
            let _ = self.expr(e);
        }
        self.pop_scope();
        Ok(())
    }

    /// Lower a `super(...)` delegation at the front of a primary constructor
    /// body: evaluate the arguments in the current scope, then inline the
    /// parent's constructor chain.
    fn emit_super_chain(&mut self, owner_name: &str, delegation: &Expr) -> Result<(), ()> {
        let ExprKind::Call {
            callee: _super,
            args,
        } = &delegation.kind
        else {
            return self.bad(delegation.span, "`super(...)` delegation is malformed");
        };
        let Some(parent) = self.table_of(owner_name).and_then(|t| {
            t.extends
                .as_ref()
                .and_then(|e| e.named().map(str::to_string))
        }) else {
            return self.bad(delegation.span, "`super(...)` requires a superclass");
        };
        let Some(pt) = self.table_of(&parent) else {
            return self.bad(delegation.span, format!("`{parent}` is not a class"));
        };
        let mut vals = Vec::new();
        for a in args {
            if a.spread {
                return self.bad(a.span, "spread arguments are not lowered yet");
            }
            let v = self.expr(&a.value)?;
            let ty = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
            vals.push((v, ty, a.value.span));
        }
        self.emit_ctor_chain(&parent, &pt, &vals, delegation.span)
    }

    /// Lower the synthesized `super(...)` of a class with no explicit
    /// constructor that sits below an explicit-primary ancestor: forward the
    /// first `count` constructor parameters (already in the function's local
    /// slots) to the ancestor's inlined constructor chain.
    fn emit_synthesized_super_chain(&mut self, ancestor: &str, count: usize) -> Result<(), ()> {
        let Some(at) = self.table_of(ancestor) else {
            return Ok(());
        };
        let cparams: Vec<ParamInfo> = at
            .ctor
            .as_ref()
            .map(|c| c.params.clone())
            .unwrap_or_default();
        let mut vals = Vec::new();
        for (i, p) in cparams.iter().take(count).enumerate() {
            let v = self.load(Slot(i as u32));
            vals.push((v, p.ty.clone(), p.span));
        }
        self.emit_ctor_chain(ancestor, &at, &vals, at.span)
    }

    /// Run the constructor chain for class `cname`, whose constructor receives
    /// the already-evaluated `vals`. An explicit constructor has its
    /// parameters bound to the values (option-wrapped where the parameter is a
    /// pointer) and its body emitted, recursing on a leading `super(...)`. A
    /// synthesized constructor stores the values into its uninitialized
    /// instance-field slots. `this` stays live in the current scope because
    /// the chain is inlined into the leaf constructor. Field initializers and
    /// `init` blocks for the whole hierarchy are run by the leaf constructor,
    /// so they are not repeated here.
    fn emit_ctor_chain(
        &mut self,
        cname: &str,
        ct: &ClassTable,
        vals: &[(Temp, Ty, Span)],
        span: Span,
    ) -> Result<(), ()> {
        self.push_scope();
        let cd = self.class_decls.get(cname).and_then(|d| {
            d.members.iter().find_map(|m| match m {
                ClassMember::Constructor(cd) if cd.name.is_none() => Some(cd),
                _ => None,
            })
        });
        if ct.ctor.is_some() {
            let Some(cd) = cd else {
                self.pop_scope();
                return self.bad(span, format!("`{cname}` primary constructor is missing"));
            };
            let cparams = ct
                .ctor
                .as_ref()
                .map(|c| c.params.clone())
                .unwrap_or_default();
            if cparams.len() != vals.len() {
                self.pop_scope();
                return self.bad(
                    span,
                    format!(
                        "`super(...)` passes {} argument(s) but `{cname}` expects {}",
                        vals.len(),
                        cparams.len()
                    ),
                );
            }
            for (p, (v, ty, vspan)) in cparams.iter().zip(vals.iter()) {
                let ir = self.map_ty(&p.ty, p.span).unwrap_or(IrTy::Ptr);
                let slot = self.new_slot(ir);
                let stored = if matches!(ir, IrTy::Ptr) {
                    self.option_wrap(*v, ty, *vspan)?
                } else {
                    *v
                };
                self.instr(IrInstr::StoreSlot { slot, v: stored });
                self.declare(&p.name, slot);
            }
            if let Some(delegation) = ctor_super_delegation(&cd.body) {
                self.emit_super_chain(cname, delegation)?;
            }
            if ctor_super_delegation(&cd.body).is_some() {
                self.emit_ctor_block_from(&cd.body, 1)?;
            } else {
                self.emit_ctor_block(&cd.body)?;
            }
        } else {
            // Synthesized parent: the arguments are the nearest
            // explicit-primary ancestor's constructor parameters (forwarded
            // up its chain) followed by the uninitialized instance fields
            // strictly below that ancestor (stored into their absolute
            // parent-first slots).
            let (slots, chain) = self.synthesized_ctor_param_meta(cname);
            if slots.len() != vals.len() {
                self.pop_scope();
                return self.bad(
                    span,
                    format!(
                        "`super(...)` passes {} argument(s) but `{cname}` expects {}",
                        vals.len(),
                        slots.len()
                    ),
                );
            }
            let Some(this_slot) = self.lookup("this") else {
                self.pop_scope();
                return self.bad(span, "`this` is not available in the constructor chain");
            };
            let this = self.load(this_slot);
            if let Some((ancestor, count)) = &chain {
                // Forward the leading constructor parameters into the nearest
                // explicit-primary ancestor's chain.
                let cand_vals = vals[..*count].to_vec();
                if let Some(at) = self.table_of(ancestor) {
                    self.emit_ctor_chain(ancestor, &at, &cand_vals, span)?;
                }
            }
            let ifields = self.all_instance_fields(cname);
            for (slot, (v, ty, vspan)) in slots.iter().zip(vals.iter()) {
                let Some(field_slot) = slot else { continue };
                let f = &ifields[*field_slot];
                let rep = self.elem_rep(&f.ty, f.span)?;
                let packed = self.pack_for_pointer_boundary(&rep, *v, ty, *vspan)?;
                let boxed = self.box_for_store(&rep, packed, elem_ir(&f.ty))?;
                self.field_store(this, *field_slot as i64, &f.ty, boxed);
            }
        }
        self.pop_scope();
        Ok(())
    }

    /// Lower a named constructor: a static factory that evaluates the
    /// delegation arguments and calls the primary constructor.
    fn build_named_ctor(
        &mut self,
        table: &ClassTable,
        ctor: &'a ConstructorDecl,
    ) -> Result<(), ()> {
        let info = table
            .named_ctors
            .iter()
            .find(|(n, _)| ctor.name.as_deref() == Some(n.as_str()))
            .map(|(_, c)| c);
        let params: Vec<ParamInfo> = info.map(|c| c.params.clone()).unwrap_or_default();
        for (i, p) in params.iter().enumerate() {
            let ir = self.map_ty(&p.ty, p.span).unwrap_or(IrTy::Ptr);
            let slot = Slot(i as u32);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: p.name.clone(),
                ty: ir,
            });
            self.declare(&p.name, slot);
        }
        self.fret = IrTy::Ptr;

        let Some(delegation) = named_ctor_delegation(&ctor.body) else {
            return self.bad(
                ctor.span,
                "named constructor body must be a single `this(...)` delegation to the primary constructor",
            );
        };
        let ExprKind::Call { args, .. } = &delegation.kind else {
            return self.bad(ctor.span, "named constructor delegation is malformed");
        };

        let cid = self
            .class_by_name
            .get(&table.name)
            .copied()
            .unwrap_or(PICKLE_CLASS_USER_BASE as u32);
        let Some(&primary) = self.ctor_ids.get(&cid) else {
            return self.bad(ctor.span, format!("`{}` has no constructor", table.name));
        };
        let fparams = self.module.funcs[primary.0].params.clone();
        let mut arg_temps = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if a.spread {
                return self.bad(a.span, "spread arguments are not lowered yet");
            }
            let t = self.expr(&a.value)?;
            let t = if matches!(fparams.get(i).map(|p| p.ty), Some(IrTy::Ptr)) {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                self.option_wrap(t, &vt, a.value.span)?
            } else {
                t
            };
            arg_temps.push(t);
        }
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Func(primary),
            args: arg_temps,
        });
        self.term(IrTerm::Return { v: Some(dst) });
        Ok(())
    }

    /// Lower a `deinit` finalizer: slot 0 is `this`, the body runs with `this`
    /// live, and the function returns unit. Deinit runs off the allocation path
    /// during GC sweep (see `docs/05-memory.md`).
    fn build_deinit_body(&mut self, _table: &ClassTable, body: &'a Block) -> Result<(), ()> {
        self.fslots.push(IrTy::Ptr);
        self.fparams.push(IrParam {
            name: "this".to_string(),
            ty: IrTy::Ptr,
        });
        self.declare("this", Slot(0));
        self.fret = IrTy::Unit;
        self.emit_ctor_block(body)?;
        self.tail_cleanup();
        Ok(())
    }

    /// Lower a method: slot 0 is `this` (instance methods), followed by the
    /// declared parameters.
    fn build_method_body(
        &mut self,
        _table: &ClassTable,
        md: &'a MethodDecl,
        info: &CallableInfo,
    ) -> Result<(), ()> {
        let mut d = 0usize;
        if !info.is_static {
            let this_slot = Slot(0);
            self.fslots.push(IrTy::Ptr);
            self.fparams.push(IrParam {
                name: "this".to_string(),
                ty: IrTy::Ptr,
            });
            self.declare("this", this_slot);
            d = 1;
        }
        for (i, p) in info.params.iter().enumerate() {
            let ir = self.map_ty(&p.ty, p.span).unwrap_or(IrTy::Ptr);
            let slot = Slot((d + i) as u32);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: p.name.clone(),
                ty: ir,
            });
            self.declare(&p.name, slot);
        }
        self.fret = self.map_ty(&info.ret, md.span).unwrap_or(IrTy::Unit);
        self.emit_body(&md.body);
        let _ = self.tail_cleanup();
        Ok(())
    }

    /// Lower a property accessor. Slot 0 is `this` (always; static properties
    /// are deferred). Getters return the property value; setters take `value`
    /// in slot 1, run the accessor body, and return unit.
    fn build_property_body(
        &mut self,
        _table: &ClassTable,
        pd: &PropertyDecl,
        info: &PropertyInfo,
        is_set: bool,
        is_static: bool,
    ) -> Result<(), ()> {
        let mut d = 0usize;
        if !is_static {
            self.fslots.push(IrTy::Ptr);
            self.fparams.push(IrParam {
                name: "this".to_string(),
                ty: IrTy::Ptr,
            });
            self.declare("this", Slot(0));
            d = 1;
        }
        if is_set {
            let ir = self.map_ty(&info.ty, pd.span).unwrap_or(IrTy::Ptr);
            self.fslots.push(ir);
            self.fparams.push(IrParam {
                name: "value".to_string(),
                ty: ir,
            });
            self.declare("value", Slot(d as u32));
            self.fret = IrTy::Unit;
        } else {
            self.fret = self.map_ty(&info.ty, pd.span).unwrap_or(IrTy::Ptr);
        }
        let accessor = if is_set { &pd.set } else { &pd.get };
        if let Some(a) = accessor {
            match a {
                PropertyAccessor::Expr(e) => {
                    if self.fret.is_unit() {
                        let _ = self.expr(e);
                        self.term(IrTerm::Return { v: None });
                    } else if let Ok(t) = self.expr(e) {
                        self.term(IrTerm::Return { v: Some(t) });
                    }
                }
                PropertyAccessor::Block(b) => {
                    let _ = self.emit_body(&Some(FnBody::Block(Box::new(b.clone()))));
                }
            }
        }
        let _ = self.tail_cleanup();
        Ok(())
    }

    /// Register every user class descriptor at the top of `main`'s entry block.
    /// Each is a
    /// `pickle_class_register(name_ptr, name_len, slot_count, mask, owned_mask,
    /// finalizer, parent)` void call; the returned id is discarded (call sites hardcode
    /// it). `finalizer` is the address of the class's `pkl_<Name>_deinit`
    /// function, or 0 when it has no `deinit` block; `parent` is the
    /// superclass id, or 0 for a root class.
    fn inject_class_registrations(&mut self) {
if self.classes.is_empty()
            && self.static_init_id.is_none()
            && self.globals_init_id.is_none()
        {
            return;
        }
        let plans: Vec<(String, usize, u32, u32, u64)> = self
            .classes
            .iter()
            .map(|p| {
                let fields = self.all_instance_fields(&p.name);
                let mut owned: u64 = 0;
                for (i, f) in fields.iter().enumerate() {
                    if f.manual && i < 64 {
                        owned |= 1u64 << i;
                    }
                }
                (
                    p.name.clone(),
                    self.total_instance_slot_count(&p.name),
                    p.class_id,
                    p.parent.unwrap_or(0),
                    owned,
                )
            })
            .collect();
        let mut instrs: Vec<IrInstr> = Vec::new();
        for (name, field_count, cid, parent, owned) in plans {
            let sid = StrId(self.intern_string(&name));
            let addr = self.temp();
            instrs.push(IrInstr::Const {
                dst: addr,
                c: IrConst::StrAddr(sid),
            });
            let len = self.temp();
            instrs.push(IrInstr::Const {
                dst: len,
                c: IrConst::Int(name.len() as i64),
            });
            let n = self.temp();
            instrs.push(IrInstr::Const {
                dst: n,
                c: IrConst::Int(field_count as i64),
            });
            let mask = self.temp();
            let m = if field_count >= 64 {
                u64::MAX
            } else {
                (1u64 << field_count) - 1
            };
            instrs.push(IrInstr::Const {
                dst: mask,
                c: IrConst::Int(m as i64),
            });
            let owned_mask = self.temp();
            instrs.push(IrInstr::Const {
                dst: owned_mask,
                c: IrConst::Int(owned as i64),
            });
            let fin = self.temp();
            let c = match self.deinit_ids.get(&cid) {
                Some(fid) => IrConst::FuncAddr(*fid),
                None => IrConst::Int(0),
            };
            instrs.push(IrInstr::Const { dst: fin, c });
            let par = self.temp();
            instrs.push(IrInstr::Const {
                dst: par,
                c: IrConst::Int(parent as i64),
            });
            let ex = self.module.extern_id(IrExtern {
                symbol: "pickle_class_register".to_string(),
                params: vec![
                    IrTy::Int,
                    IrTy::Int,
                    IrTy::Int,
                    IrTy::Int,
                    IrTy::Int,
                    IrTy::Int,
                    IrTy::Int,
                ],
                ret: IrTy::Unit,
            });
            instrs.push(IrInstr::Call {
                dst: None,
                callee: Callee::Extern(ex),
                args: vec![addr, len, n, mask, owned_mask, fin, par],
            });
            // Interface buckets: mark the class as implementing each interface
            // (empty buckets included, so `pickle_class_implements` answers for
            // zero-method interfaces) and publish one method-table entry per
            // dispatch method.
            let buckets = self
                .class_iface_buckets
                .get(&cid)
                .cloned()
                .unwrap_or_default();
            for (iface_id, slots) in buckets {
                let cid_t = self.temp();
                instrs.push(IrInstr::Const {
                    dst: cid_t,
                    c: IrConst::Int(cid as i64),
                });
                let iid_t = self.temp();
                instrs.push(IrInstr::Const {
                    dst: iid_t,
                    c: IrConst::Int(iface_id as i64),
                });
                let ex_mark = self.module.extern_id(IrExtern {
                    symbol: "pickle_class_add_interface".to_string(),
                    params: vec![IrTy::Int, IrTy::Int],
                    ret: IrTy::Unit,
                });
                instrs.push(IrInstr::Call {
                    dst: None,
                    callee: Callee::Extern(ex_mark),
                    args: vec![cid_t, iid_t],
                });
                for (m_idx, fid) in slots {
                    let idx_t = self.temp();
                    instrs.push(IrInstr::Const {
                        dst: idx_t,
                        c: IrConst::Int(m_idx as i64),
                    });
                    let fn_t = self.temp();
                    instrs.push(IrInstr::Const {
                        dst: fn_t,
                        c: IrConst::FuncAddr(fid),
                    });
                    let ex_m = self.module.extern_id(IrExtern {
                        symbol: "pickle_class_add_iface_method".to_string(),
                        params: vec![IrTy::Int, IrTy::Int, IrTy::Int, IrTy::Int],
                        ret: IrTy::Unit,
                    });
                    instrs.push(IrInstr::Call {
                        dst: None,
                        callee: Callee::Extern(ex_m),
                        args: vec![cid_t, iid_t, idx_t, fn_t],
                    });
                }
            }
        }
        if let Some(fid) = self.globals_init_id {
            instrs.insert(
                0,
                IrInstr::Call {
                    dst: None,
                    callee: Callee::Func(fid),
                    args: Vec::new(),
                },
            );
        }
        if let Some(fid) = self.static_init_id {
            instrs.push(IrInstr::Call {
                dst: None,
                callee: Callee::Func(fid),
                args: Vec::new(),
            });
        }
        let blk = &mut self.blocks[0];
        blk.instrs.splice(0..0, instrs);
    }

    fn int_const(&mut self, v: i64) -> Temp {
        let t = self.temp();
        self.instr(IrInstr::Const {
            dst: t,
            c: IrConst::Int(v),
        });
        t
    }

    /// Emit the function body; the current block is left terminator-clean.
    fn emit_body(&mut self, body: &Option<FnBody>) -> bool {
        let body = match body {
            Some(b) => b,
            None => return true,
        };
        match body {
            FnBody::Block(b) => {
                self.push_scope();
                self.block_tail_as_return(b);
                self.pop_scope();
            }
            FnBody::Expr(e) => {
                if self.fret.is_unit() {
                    let _ = self.expr(e);
                } else if let Ok(t) = self.expr(e) {
                    let t = if self.fret == IrTy::Ptr {
                        let vt = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                        self.option_wrap(t, &vt, e.span).unwrap_or(t)
                    } else {
                        t
                    };
                    self.term(IrTerm::Return { v: Some(t) });
                }
            }
        }
        let _ = self.tail_cleanup();
        true
    }

    /// After a block body, close the current block: emit the body's statements,
    /// then if the function returns a value the tail expression *is* the return
    /// value; otherwise it is discarded.
    fn block_tail_as_return(&mut self, b: &Block) {
        let try_slot = match self.stmts_with_try_tail(&b.stmts, self.fret) {
            Ok(slot) => slot,
            Err(()) => return,
        };
        if let Some(slot) = try_slot {
            // `block_into_slot` already option-boxed a scalar tail into a `Ptr`
            // result slot, so the loaded value matches `fret` as-is.
            let t = self.load(slot);
            self.term(IrTerm::Return { v: Some(t) });
            return;
        }
        for s in &b.stmts {
            if self.stmt(s).is_err() {
                return;
            }
        }
        match &b.expr {
            Some(e) if !self.fret.is_unit() => {
                if let Ok(t) = self.expr(e) {
                    let t = if self.fret == IrTy::Ptr {
                        let vt = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                        self.option_wrap(t, &vt, e.span).unwrap_or(t)
                    } else {
                        t
                    };
                    self.term(IrTerm::Return { v: Some(t) });
                }
            }
            Some(e) => {
                let _ = self.expr(e);
                self.term(IrTerm::Return { v: None });
            }
            None if self.fret.is_unit() => {
                self.term(IrTerm::Return { v: None });
            }
            None => {
                // A value-returning function reaches here only when the last
                // statement already terminated this block (`return`), so the
                // current block is dead. Closing it with an empty `return`
                // would not type-check against the value signature, so leave
                // it `Unreachable`; a live block with no tail value is a
                // missing-return program and is rejected upstream by the
                // checker.
            }
        }
    }

    /// Ensure the final block is closed. Returns false iff the function's last
    /// block is dead (shorter than a missing return). Only unit-returning
    /// functions get the trailing empty `return`; a value-returning function
    /// whose last statement already returned leaves its dead block
    /// `Unreachable` (an empty `return` would fail Cranelift's verifier).
    fn tail_cleanup(&mut self) -> bool {
        let block = &mut self.blocks[self.cur.0 as usize];
        if matches!(block.term, IrTerm::Unreachable) && self.fret.is_unit() {
            block.term = IrTerm::Return { v: None };
        }
        true
    }

    // ---- statements ----

    fn stmt(&mut self, s: &Stmt) -> Result<(), ()> {
        match s {
            Stmt::Let {
                pattern,
                ty,
                init,
                mutable: _,
                attrs,
                span,
            } => {
                let manual = attrs.iter().any(|a| a.name == "manualAlloc");
                match pattern {
                    Pattern::Binding { ty: pat_ty, .. } => {
                        let annot: &Option<TypeExpr> = if pat_ty.is_some() { pat_ty } else { ty };
                        let ann_option = matches!(
                            annot.as_ref().map(|t| &t.kind),
                            Some(TypeExprKind::Option(_))
                        );
                        let slot_ty = if ann_option {
                            IrTy::Ptr
                        } else {
                            match init {
                                Some(e) => self.irty(e.span)?,
                                None => self.annot_ty(annot, *span)?,
                            }
                        };
                        let slot = self.new_slot(slot_ty);
                        if let Some(e) = init {
                            let t = self.expr(e)?;
                            let v = if slot_ty == IrTy::Ptr {
                                let vt = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                                self.option_wrap(t, &vt, e.span)?
                            } else {
                                t
                            };
                            let v = if manual {
                                self.extern_call_t1(
                                    "pickle_manual_adopt",
                                    vec![IrTy::Ptr],
                                    IrTy::Ptr,
                                    vec![v],
                                )?
                            } else {
                                v
                            };
                            self.instr(IrInstr::StoreSlot { slot, v });
                        }
                        if let Pattern::Binding { name, .. } = pattern {
                            self.declare(name, slot);
                            if manual {
                                self.declare_manual(name);
                            }
                        }
                        Ok(())
                    }
                    Pattern::Wildcard => {
                        if let Some(e) = init {
                            let _ = self.expr(e)?;
                        }
                        Ok(())
                    }
                    // `let (a, b) = <tuple>` irrefutable destructuring (matches
                    // `bind_pattern` on the checker side). Tuple patterns are
                    // irrefutable, so every name is bound unconditionally; the
                    // init is evaluated once and each name binds to a fresh slot
                    // (the same binder the match arms use). A tuple
                    // destructuring requires an initializer. Refutable patterns
                    // (`let some(v) = ...`) keep bailing: resolving an absent
                    // option here would dereference null.
                    Pattern::Tuple(_) => {
                        let Some(e) = init else {
                            return self
                                .bad(*span, "destructuring patterns require an initializer");
                        };
                        let st = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                        let v = self.expr(e)?;
                        self.bind_match_value(pattern, &st, v, e.span)?;
                        Ok(())
                    }
                    _ => self.bad(*span, "destructuring patterns are not lowered yet"),
                }
            }
            Stmt::Const { name, value, .. } => {
                let t = self.expr(value)?;
                let ty = self.irty(value.span)?;
                let slot = self.new_slot(ty);
                let v = if ty == IrTy::Ptr {
                    let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
                    self.option_wrap(t, &vt, value.span)?
                } else {
                    t
                };
                self.instr(IrInstr::StoreSlot { slot, v });
                self.declare(name, slot);
                Ok(())
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => {
                        let t = self.expr(e)?;
                        if self.fret == IrTy::Ptr {
                            let vt = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                            Some(self.option_wrap(t, &vt, e.span)?)
                        } else {
                            Some(t)
                        }
                    }
                    None => None,
                };
                self.exit_through_finallys(Exit::Return(v))?;
                Ok(())
            }
            Stmt::Break { span: _ } => {
                let Some(target) = self.loops.last().map(|l| l.break_target) else {
                    return self.bad_span_note("unbalanced `break`");
                };
                self.exit_through_finallys(Exit::Branch(target))?;
                Ok(())
            }
            Stmt::Continue { span: _ } => {
                let Some(target) = self.loops.last().map(|l| l.continue_target) else {
                    return self.bad_span_note("unbalanced `continue`");
                };
                self.exit_through_finallys(Exit::Branch(target))?;
                Ok(())
            }
            Stmt::While { cond, body, span } => self.while_stmt(cond, body, *span),
            Stmt::For { header, body, span } => self.for_stmt(header, body, *span),
            Stmt::Try {
                body,
                binder,
                handler,
                finally,
                span,
            } => self.try_stmt(
                body,
                binder.as_deref(),
                handler.as_deref(),
                finally.as_deref(),
                *span,
                None,
            ),
            Stmt::Throw { value, span } => self.throw_stmt(value, *span),
            Stmt::Expr(e) => {
                let _ = self.expr(e)?;
                Ok(())
            }
            Stmt::Empty(_) => Ok(()),
        }
    }

    /// Emit a statement sequence, giving a trailing `try` a slot to leave its
    /// value in. Returns that slot when the last statement was a `try`, so the
    /// caller can read the value the `try` produced.
    fn stmts_with_try_tail(&mut self, stmts: &[Stmt], want: IrTy) -> Result<Option<Slot>, ()> {
        // Peek before emitting: on a miss the caller still has to emit the
        // whole sequence itself, and emitting it twice would duplicate effects.
        let is_try = matches!(stmts.last(), Some(Stmt::Try { .. }));
        if !is_try {
            return Ok(None);
        }
        let (last, rest) = stmts.split_last().expect("checked non-empty");
        for s in rest {
            self.stmt(s)?;
        }
        let Stmt::Try {
            body,
            binder,
            handler,
            finally,
            span,
        } = last
        else {
            unreachable!("guarded by is_try");
        };
        let slot = self.new_slot(want);
        self.try_stmt(
            body,
            binder.as_deref(),
            handler.as_deref(),
            finally.as_deref(),
            *span,
            Some(slot),
        )?;
        Ok(Some(slot))
    }

    /// Emit `b` as a statement, optionally capturing its tail value into `res`.
    /// A block with no tail expression leaves `res` untouched, which is fine:
    /// every path that reaches it stores a value of its own.
    fn emit_block(&mut self, b: &Block, res: Option<Slot>) -> Result<(), ()> {
        match res {
            Some(slot) => self.block_into_slot(b, Some(slot)),
            None => self.block_body_only(b),
        }
    }

    /// Emit `exit` after every `finally` currently in scope, innermost first.
    ///
    /// Each `finally` is re-emitted here rather than shared, because a flat CFG
    /// has no way to return to a single copy from several exits. The
    /// `finallys` stack is cleared while they are emitted, so a `return`,
    /// `break`, or `throw` written *inside* a `finally` means "discard the
    /// pending exit", which is what Java and C# do.
    fn exit_through_finallys(&mut self, exit: Exit) -> Result<(), ()> {
        if self.finallys.is_empty() {
            return self.perform_exit(exit);
        }
        let chain = core::mem::take(&mut self.finallys);
        // The flag is set on every path that reaches here through a throw, so
        // the `finally` bodies run with propagation checks suppressed.
        let saved = self.in_throw_transfer;
        self.in_throw_transfer = true;
        for fin in chain.iter().rev() {
            // One copy of the `finally` per level, then on to the next level.
            let after = self.new_block();
            self.push_scope();
            self.block_body_only(fin)?;
            self.pop_scope();
            self.term(IrTerm::Branch { target: after });
            self.cur = after;
        }
        self.in_throw_transfer = saved;
        self.finallys = chain;
        self.perform_exit(exit)
    }

    /// The exit itself, once every `finally` has run.
    fn perform_exit(&mut self, exit: Exit) -> Result<(), ()> {
        match exit {
            Exit::Return(v) => {
                self.term(IrTerm::Return { v });
            }
            Exit::Branch(target) => {
                self.term(IrTerm::Branch { target });
            }
            Exit::Propagate => {
                let v = match self.fret {
                    IrTy::Unit => None,
                    IrTy::Ptr => Some(self.zero_ptr()),
                    other => Some(self.const_temp(Self::zero_const(other))),
                };
                self.term(IrTerm::Return { v });
            }
        }
        self.cur = self.new_block();
        Ok(())
    }

    /// The zero value of a lowered type, for a return path abandoned by a
    /// propagating throw. A caller checks the pending flag before reading the
    /// result, so this only has to be a well-formed value of the right type.
    fn zero_const(ty: IrTy) -> IrConst {
        match ty {
            IrTy::Bool => IrConst::Bool(false),
            IrTy::Char => IrConst::Char(0),
            IrTy::Float => IrConst::Float(0),
            _ => IrConst::Int(0),
        }
    }

    /// A null pointer constant, for the propagate path of a reference-returning
    /// function whose real result was abandoned by a throw.
    fn zero_ptr(&mut self) -> Temp {
        self.const_temp(IrConst::Null)
    }

    /// `throw value`: publish the payload, then leave.
    ///
    /// The transfer is a direct branch to the innermost enclosing `catch`, so a
    /// `throw` inside the `try` it matches does not pay for a flag round-trip.
    /// With no `catch` in this function the flag stays set for the caller, which
    /// observes it after the call.
    fn throw_stmt(&mut self, value: &Expr, span: Span) -> Result<(), ()> {
        let _ = span;
        let payload = self.expr(value)?;
        if self.fret == IrTy::Ptr {
            let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
            let payload = self.option_wrap(payload, &vt, value.span)?;
            self.throw_with(payload)
        } else {
            self.throw_with(payload)
        }
    }

    /// Publish `payload` and transfer control.
    ///
    /// With an enclosing `catch` in this function, control branches straight to
    /// it after running any `finally` from *inside* that `catch`'s own `try`, so
    /// a local `throw` costs no flag round-trip. The payload still goes through
    /// `pickle_exc_throw`, which roots it in the GC for the jump and lets the
    /// handler's `pickle_exc_take` release it. With no `catch` here the flag
    /// stays set and the caller observes it after the call.
    fn throw_with(&mut self, payload: Temp) -> Result<(), ()> {
        if self.analyzing {
            // A `throw` in this body is the seed for its own fixed point; the
            // transfer that follows is pass 2's problem.
            if let Some(fid) = self.current_fid {
                self.direct_throw.insert(fid);
            }
            return Ok(());
        }
        self.extern_call_void(exc::THROW, vec![IrTy::Ptr], vec![payload]);
        self.transfer_to_handler_or_propagate()
    }

    /// Leave the current block for the innermost `catch` in this function, or
    /// out of the function when there is none.
    ///
    /// The `finally`s still owed on the way are only the ones from *inside* the
    /// handler's own `try`, and they run innermost first. The handler's own
    /// `finally` is deliberately left on the stack: `try_stmt` emits it once,
    /// after the handler body, on the path this branch takes.
    ///
    /// This only *emits*; it never mutates the `handlers`/`finallys` stacks.
    /// Both callers lower a path that does not necessarily commit — a
    /// `check_pending` guard resumes into the same handler scope afterwards, and
    /// a `throw` statement is only one of several statements still to be emitted
    /// — so a transfer that consumed the enclosing context would strand every
    /// later `throw` in the same body. `try_stmt` balances its own push once,
    /// when the body and the handler are both fully emitted.
    fn transfer_to_handler_or_propagate(&mut self) -> Result<(), ()> {
        let Some(handler) = self.handlers.last().cloned() else {
            return self.exit_through_finallys(Exit::Propagate);
        };

        let chain = core::mem::take(&mut self.finallys);
        // The `try` emits its own `finally` after the handler body, not before,
        // so the first `finally` still owed on the way is one past it when the
        // owning `try` has one of its own, and exactly at the base when it does
        // not -- an inner `try`'s `finally` then occupies the base itself.
        let inner_base = handler.fin_base + usize::from(handler.own_finally);
        // The flag is still set here, so the inner `finally` bodies run with
        // propagation checks suppressed: a check would see the throw we are
        // transferring on and divert straight to the handler, skipping the
        // remaining `finally`s.
        let saved = self.in_throw_transfer;
        self.in_throw_transfer = true;
        for i in (inner_base..chain.len()).rev() {
            let after = self.new_block();
            self.push_scope();
            self.block_body_only(&chain[i])?;
            self.pop_scope();
            self.term(IrTerm::Branch { target: after });
            self.cur = after;
        }
        self.in_throw_transfer = saved;
        // The whole chain goes back, including the handler's own `finally`: the
        // handler body is emitted with it live, so a `return` inside the handler
        // still runs it.
        self.finallys = chain;

        self.term(IrTerm::Branch {
            target: handler.target,
        });
        self.cur = self.new_block();
        Ok(())
    }

    /// `try B catch (name) H finally F`.
    ///
    /// `B` is emitted inline. The end of `B` checks the pending flag and either
    /// falls through to `finally` or enters `H`, which consumes the flag and
    /// binds the payload to a frame slot (the IR has no block parameters, so the
    /// `logic` idiom of a `Slot` written in one block and read in another is
    /// what carries the value across the branch). A `throw` written inside `B`
    /// skips the flag and branches to `H` directly.
    ///
    /// All three ways out — body normal, handler, and a propagating throw — run
    /// `F`, so `F` is emitted once per way out. The owning `try`'s `finally` is
    /// on the `finallys` stack while `B` and `H` are emitted, so a `return`
    /// inside either runs it; a single pop then balances it before the
    /// fall-through copies are emitted, so those copies cannot re-enter it.
    fn try_stmt(
        &mut self,
        body: &Block,
        binder: Option<&str>,
        handler: Option<&Block>,
        finally: Option<&Block>,
        span: Span,
        res: Option<Slot>,
    ) -> Result<(), ()> {
        let _ = span;
        let after_body = self.new_block();
        let handler_id = self.new_block();
        let handler_fin_id = self.new_block();
        let end_id = self.new_block();

        // A `throw` in the body targets the handler directly, so it must be in
        // scope while the body is emitted.
        let fin_base = self.finallys.len();
        if let Some(fin) = finally {
            self.finallys.push(fin.clone());
        }
        if handler.is_some() {
            self.handlers.push(HandlerCtx {
                target: handler_id,
                fin_base,
                own_finally: finally.is_some(),
            });
        }

        self.push_scope();
        self.emit_block(body, res)?;
        self.pop_scope();

        if handler.is_some() {
            // A call in the body may have thrown: hand over to the handler.
            let pending = self.extern_call_t1(exc::PENDING, vec![], IrTy::Bool, vec![])?;
            self.term(IrTerm::BranchIf {
                cond: pending,
                then: handler_id,
                else_: after_body,
            });
        } else {
            self.term(IrTerm::Branch { target: after_body });
        }

        // The body can no longer `throw` to this handler, so a `throw` in `H`
        // propagates outward instead. Only the context this `try` pushed goes
        // away: a `try` with no `catch` of its own must leave an enclosing
        // handler in scope for the code that follows it.
        if handler.is_some() {
            self.handlers.pop();
        }

        self.cur = handler_id;
        if let Some(handler) = handler {
            // Consume the throw: this both clears the flag and yields the
            // payload, and it is the only thing that unroots the payload.
            let payload = self.extern_call_t1(exc::TAKE, vec![], IrTy::Ptr, vec![])?;
            self.push_scope();
            if let Some(name) = binder {
                let slot = self.new_slot(IrTy::Ptr);
                self.instr(IrInstr::StoreSlot { slot, v: payload });
                self.declare(name, slot);
            }
            self.emit_block(handler, res)?;
            self.pop_scope();
        }

        if let Some(fin) = finally {
            // A single compile-time pop balances the push from before the body.
            // The handler's fall-through copy and the body's fall-through copy
            // are two emissions of the *same* `finally`, so the stack is already
            // balanced by the time the body copy is emitted.
            self.finallys.pop();
            if handler.is_some() {
                self.term(IrTerm::Branch {
                    target: handler_fin_id,
                });
                self.cur = handler_fin_id;
                self.push_scope();
                self.block_body_only(fin)?;
                self.pop_scope();
            }
        }
        self.term(IrTerm::Branch { target: end_id });

        self.cur = after_body;
        if let Some(fin) = finally {
            self.push_scope();
            self.block_body_only(fin)?;
            self.pop_scope();
        }
        self.term(IrTerm::Branch { target: end_id });
        self.cur = end_id;
        Ok(())
    }

    fn while_stmt(&mut self, cond: &Expr, body: &Block, span: Span) -> Result<(), ()> {
        let _ = span;
        let cond_id = self.new_block();
        let body_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let c = self.expr(cond)?;
        self.term(IrTerm::BranchIf {
            cond: c,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        self.loops.push(LoopCtx {
            continue_target: cond_id,
            break_target: end_id,
        });
        self.push_scope();
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }

    fn for_stmt(&mut self, header: &ForHeader, body: &Block, span: Span) -> Result<(), ()> {
        match header {
            ForHeader::Range { init, cond, step } => {
                self.stmt(init)?;
                let cond_id = self.new_block();
                let body_id = self.new_block();
                let step_id = self.new_block();
                let end_id = self.new_block();
                self.term(IrTerm::Branch { target: cond_id });
                self.cur = cond_id;
                let c = self.expr(cond)?;
                self.term(IrTerm::BranchIf {
                    cond: c,
                    then: body_id,
                    else_: end_id,
                });
                self.cur = body_id;
                self.loops.push(LoopCtx {
                    // `continue` must still run the step expression, so it
                    // targets the step block, not the condition.
                    continue_target: step_id,
                    break_target: end_id,
                });
                self.push_scope();
                self.block_body_only(body)?;
                self.pop_scope();
                self.loops.pop();
                self.term(IrTerm::Branch { target: step_id });
                self.cur = step_id;
                let _ = self.expr(step);
                self.term(IrTerm::Branch { target: cond_id });
                self.cur = end_id;
                Ok(())
            }
            ForHeader::In { pattern, sequence } => self.for_in(pattern, sequence, body, span),
        }
    }

    /// `for (x in <seq>)`: slice-1 supports integer range sequences and lists.
    /// An immutable `&T` borrow iterates through its referent.
    fn for_in(
        &mut self,
        pattern: &Pattern,
        sequence: &Expr,
        body: &Block,
        span: Span,
    ) -> Result<(), ()> {
        let seq_ot = match self.ty_of(&sequence.span) {
            Some(Ty::Ref(inner)) => Some((*inner).clone()),
            other => other,
        };
        // `for ((k, v) in m)` over a `Map<K, V>`: bind both the key and the
        // value per trip (scalar keys are unboxed in `for_in_map_entries`).
        if let Some(Ty::Map(k, v)) = &seq_ot {
            if let Pattern::Tuple(parts) = pattern {
                if parts.len() != 2 {
                    return self.bad(
                        span,
                        "map entries pattern must bind exactly two names `(k, v)`",
                    );
                }
                let kname = match &parts[0] {
                    Pattern::Binding { name, .. } => name.clone(),
                    _ => return self.bad(span, "map entries sub-patterns must be plain names"),
                };
                let vname = match &parts[1] {
                    Pattern::Binding { name, .. } => name.clone(),
                    _ => return self.bad(span, "map entries sub-patterns must be plain names"),
                };
                let map_t = self.expr(sequence)?;
                return self.for_in_map_entries(
                    &kname,
                    &vname,
                    map_t,
                    k.as_ref().clone(),
                    v.as_ref().clone(),
                    body,
                    span,
                );
            }
        }
        let seq_ot = match self.ty_of(&sequence.span) {
            Some(Ty::Ref(inner)) => Some((*inner).clone()),
            other => other,
        };
        if let Some(Ty::String) = &seq_ot {
            let Pattern::Binding { name, .. } = pattern else {
                return self.bad(
                    span,
                    "iteration patterns other than a binding are not lowered yet",
                );
            };
            let seq_t = self.expr(sequence)?;
            return self.for_in_string(name, seq_t, body);
        }
        if let Some(Ty::List(inner)) = &seq_ot {
            let elem = inner.as_ref().clone();
            let seq_t = self.expr(sequence)?;
            return self.for_in_values(pattern, seq_t, elem, body, span);
        }
        if let Some(Ty::Map(_, v)) = &seq_ot {
            let Pattern::Binding { .. } = pattern else {
                return self.bad(
                    span,
                    "iteration patterns other than a binding are not lowered yet",
                );
            };
            let map_t = self.expr(sequence)?;
            let vals =
                self.extern_call_t1("pickle_map_values", vec![IrTy::Ptr], IrTy::Ptr, vec![map_t])?;
            return self.for_in_values(pattern, vals, v.as_ref().clone(), body, span);
        }
        // `Iterable<T>` protocol: the sequence is an interface-typed value or
        // a class/struct that transitively implements `Iterable<elem>` (builtin
        // List/Map/String/Range keep their direct lowering above).
        if let Some(elem) = self.iterable_elem(&seq_ot.clone().unwrap_or(Ty::Unknown)) {
            let seq_t = self.expr(sequence)?;
            return self.for_in_protocol(pattern, &elem, seq_t, body, span);
        }
        let (start, end, incl) = match &sequence.kind {
            ExprKind::Binary {
                op: AstBinOp::Range,
                lhs,
                rhs,
            } => (lhs, rhs, false),
            ExprKind::Binary {
                op: AstBinOp::RangeIncl,
                lhs,
                rhs,
            } => (lhs, rhs, true),
            _ => {
                // A stored range value (`let r = 1..5; for (x in r)`): it is a
                // real `List<int>`, so iterate it like a list. Literal ranges
                // keep the direct counter loop above (their syntax matches
                // `Binary { Range }`, which is handled first).
                if let Some(Ty::Range(inner)) = &seq_ot {
                    let elem = inner.as_ref().clone();
                    let seq_t = self.expr(sequence)?;
                    return self.for_in_values(pattern, seq_t, elem, body, span);
                }
                return self.bad(
                    sequence.span,
                    "`for (x in ...)` over non-range, non-list sequences is not lowered yet",
                );
            }
        };
        self.ensure_int(start)?;
        self.ensure_int(end)?;
        let start_t = self.expr(start)?;
        let end_t = self.expr(end)?;
        let idx = self.new_slot(IrTy::Int);
        let end_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot {
            slot: idx,
            v: start_t,
        });
        self.instr(IrInstr::StoreSlot {
            slot: end_slot,
            v: end_t,
        });

        let cond_id = self.new_block();
        let body_id = self.new_block();
        let next_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let cur_t = self.load(idx);
        let end_l = self.load(end_slot);
        let cmp = self.temp();
        self.instr(IrInstr::BinOp {
            dst: cmp,
            op: if incl { IrBinOp::Le } else { IrBinOp::Lt },
            a: cur_t,
            b: end_l,
        });
        self.term(IrTerm::BranchIf {
            cond: cmp,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        self.loops.push(LoopCtx {
            // `continue` must still advance the loop variable, so it targets
            // the increment block, not the condition.
            continue_target: next_id,
            break_target: end_id,
        });
        self.push_scope();
        let Pattern::Binding { name, .. } = pattern else {
            return self.bad(
                span,
                "iteration patterns other than a binding are not lowered yet",
            );
        };
        self.declare(name, idx);
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: next_id });
        self.cur = next_id;
        let c = self.load(idx);
        let one = self.temp();
        self.instr(IrInstr::Const {
            dst: one,
            c: IrConst::Int(1),
        });
        let nxt = self.temp();
        self.instr(IrInstr::BinOp {
            dst: nxt,
            op: IrBinOp::Add,
            a: c,
            b: one,
        });
        self.instr(IrInstr::StoreSlot { slot: idx, v: nxt });
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }

    /// `for (x in seq)` over a sequence value. `seq` is a boxed (managed)
    /// sequence — a `List` produced directly by the sequence expression or by
    /// `pickle_map_values` for a map. Iterates by index; scalar elements are
    /// unboxed each trip. The sequence lives in a managed slot so the collector
    /// keeps it and its boxed elements reachable for the whole loop.
    fn for_in_values(
        &mut self,
        pattern: &Pattern,
        seq_t: Temp,
        elem: Ty,
        body: &Block,
        span: Span,
    ) -> Result<(), ()> {
        let rep = self.elem_rep(&elem, span)?;
        let elem_ir = elem_ir(&elem);
        let seq_slot = self.new_slot(IrTy::Ptr);
        let idx_slot = self.new_slot(IrTy::Int);
        let len_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot {
            slot: seq_slot,
            v: seq_t,
        });
        let zero = self.temp();
        self.instr(IrInstr::Const {
            dst: zero,
            c: IrConst::Int(0),
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: zero,
        });
        let seq_l = self.load(seq_slot);
        let len_t =
            self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![seq_l])?;
        self.instr(IrInstr::StoreSlot {
            slot: len_slot,
            v: len_t,
        });

        let cond_id = self.new_block();
        let body_id = self.new_block();
        let next_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let cur_t = self.load(idx_slot);
        let end_l = self.load(len_slot);
        let cmp = self.temp();
        self.instr(IrInstr::BinOp {
            dst: cmp,
            op: IrBinOp::Lt,
            a: cur_t,
            b: end_l,
        });
        self.term(IrTerm::BranchIf {
            cond: cmp,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        self.loops.push(LoopCtx {
            continue_target: next_id,
            break_target: end_id,
        });
        self.push_scope();
        let cur_i = self.load(idx_slot);
        let seq_l = self.load(seq_slot);
        let raw = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![seq_l, cur_i],
        )?;
        let v = match rep {
            ElemRep::Scalar(_, unbox_sym, _) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], elem_ir, vec![raw])?
            }
            ElemRep::Ptr => raw,
        };
        let elem_slot = self.new_slot(elem_ir);
        self.instr(IrInstr::StoreSlot { slot: elem_slot, v });
        match pattern {
            Pattern::Binding { name, .. } => self.declare(name, elem_slot),
            Pattern::Tuple(_) => {
                // `for ((a, b) in xs)` over a `List<(int, int)>`: read the
                // tuple fields out of the boxed element into each part slot.
                let ev = self.load(elem_slot);
                self.bind_match_value(pattern, &elem, ev, span)?;
            }
            _ => {
                return self.bad(
                    span,
                    "iteration patterns other than a binding or tuple are not lowered yet",
                )
            }
        }
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: next_id });
        self.cur = next_id;
        let c = self.load(idx_slot);
        let one = self.temp();
        self.instr(IrInstr::Const {
            dst: one,
            c: IrConst::Int(1),
        });
        let nxt = self.temp();
        self.instr(IrInstr::BinOp {
            dst: nxt,
            op: IrBinOp::Add,
            a: c,
            b: one,
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: nxt,
        });
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }

    /// `for (x in seq)` over an `Iterable<T>` protocol value. `seq` is the
    /// already-lowered iterable (a managed pointer) and `elem` its element
    /// type. Lowering is:
    ///     let it = seq.iterator()       // `Iterable<elem>` view
    ///     loop: v? = it.next(); if v? == none -> end     // `Iterator<elem>` view
    ///           body{ x = resolve(v?) }; continue -> loop
    /// `next()` returning `none` terminates; `continue` re-tests. Both the
    /// iterator and the returned option live in managed slots so the collector
    /// keeps them reachable across trips.
    fn for_in_protocol(
        &mut self,
        pattern: &Pattern,
        elem: &Ty,
        seq_t: Temp,
        body: &Block,
        span: Span,
    ) -> Result<(), ()> {
        let _ = self.elem_rep(elem, span)?;
        let iterable_ift = Ty::Interface("Iterable".into(), vec![elem.clone()]);
        let itv =
            self.interface_zero_arg_call(span, &iterable_ift, seq_t, "iterator", IrTy::Ptr)?;
        let it_slot = self.new_slot(IrTy::Ptr);
        self.instr(IrInstr::StoreSlot {
            slot: it_slot,
            v: itv,
        });

        let iterator_ift = Ty::Interface("Iterator".into(), vec![elem.clone()]);
        let elem_ir = elem_ir(elem);
        let opt_slot = self.new_slot(IrTy::Ptr);

        let cond_id = self.new_block();
        let body_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let it = self.load(it_slot);
        let next_v = self.interface_zero_arg_call(span, &iterator_ift, it, "next", IrTy::Ptr)?;
        self.instr(IrInstr::StoreSlot {
            slot: opt_slot,
            v: next_v,
        });
        let opt = self.load(opt_slot);
        let present = self.opt_is_present(opt)?;
        self.term(IrTerm::BranchIf {
            cond: present,
            then: body_id,
            else_: end_id,
        });

        self.cur = body_id;
        self.loops.push(LoopCtx {
            continue_target: cond_id,
            break_target: end_id,
        });
        self.push_scope();
        let raw = self.load(opt_slot);
        let v = self.opt_resolve(raw, elem, span)?;
        let elem_slot = self.new_slot(elem_ir);
        self.instr(IrInstr::StoreSlot { slot: elem_slot, v });
        match pattern {
            Pattern::Binding { name, .. } => self.declare(name, elem_slot),
            Pattern::Tuple(_) => {
                let ev = self.load(elem_slot);
                self.bind_match_value(pattern, elem, ev, span)?;
            }
            _ => {
                return self.bad(
                    span,
                    "iteration patterns other than a binding or tuple are not lowered yet",
                )
            }
        }
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }
    /// values list in lockstep — both `pickle_map_keys` / `pickle_map_values`
    /// snapshots walk the same internal entry array, so index i of the keys
    /// list pairs with index i of the values list. Keys unbox per their
    /// `ElemRep` (string keys are pass-through pointers; composite keys too);
    /// values unbox per `elem_rep`.
    #[allow(clippy::too_many_arguments)]
    fn for_in_map_entries(
        &mut self,
        kname: &str,
        vname: &str,
        map_t: Temp,
        kty: Ty,
        vty: Ty,
        body: &Block,
        span: Span,
    ) -> Result<(), ()> {
        let vrep = self.elem_rep(&vty, span)?;
        let v_ir = elem_ir(&vty);
        let krep = self.key_rep(&kty, span)?;
        let k_ir = match &krep {
            ElemRep::Scalar(_, _, ir) => *ir,
            ElemRep::Ptr => IrTy::Ptr,
        };
        let map_slot = self.new_slot(IrTy::Ptr);
        let keys_slot = self.new_slot(IrTy::Ptr);
        let vals_slot = self.new_slot(IrTy::Ptr);
        let idx_slot = self.new_slot(IrTy::Int);
        let len_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot {
            slot: map_slot,
            v: map_t,
        });
        let map_l = self.load(map_slot);
        let keys_t =
            self.extern_call_t1("pickle_map_keys", vec![IrTy::Ptr], IrTy::Ptr, vec![map_l])?;
        let vals_t =
            self.extern_call_t1("pickle_map_values", vec![IrTy::Ptr], IrTy::Ptr, vec![map_l])?;
        self.instr(IrInstr::StoreSlot {
            slot: keys_slot,
            v: keys_t,
        });
        self.instr(IrInstr::StoreSlot {
            slot: vals_slot,
            v: vals_t,
        });
        let zero = self.temp();
        self.instr(IrInstr::Const {
            dst: zero,
            c: IrConst::Int(0),
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: zero,
        });
        let keys_l = self.load(keys_slot);
        let len_t =
            self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![keys_l])?;
        self.instr(IrInstr::StoreSlot {
            slot: len_slot,
            v: len_t,
        });

        let cond_id = self.new_block();
        let body_id = self.new_block();
        let next_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let cur_t = self.load(idx_slot);
        let end_l = self.load(len_slot);
        let cmp = self.temp();
        self.instr(IrInstr::BinOp {
            dst: cmp,
            op: IrBinOp::Lt,
            a: cur_t,
            b: end_l,
        });
        self.term(IrTerm::BranchIf {
            cond: cmp,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        self.loops.push(LoopCtx {
            continue_target: next_id,
            break_target: end_id,
        });
        self.push_scope();
        let i = self.load(idx_slot);
        let keys_l = self.load(keys_slot);
        let vals_l = self.load(vals_slot);
        let kraw = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![keys_l, i],
        )?;
        let k = match &krep {
            ElemRep::Scalar(_, unbox_sym, _) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], k_ir, vec![kraw])?
            }
            ElemRep::Ptr => kraw,
        };
        let k_slot = self.new_slot(k_ir);
        self.instr(IrInstr::StoreSlot { slot: k_slot, v: k });
        self.declare(kname, k_slot);
        let vraw = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![vals_l, i],
        )?;
        let v = match vrep {
            ElemRep::Scalar(_, unbox_sym, _) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], v_ir, vec![vraw])?
            }
            ElemRep::Ptr => vraw,
        };
        let v_slot = self.new_slot(v_ir);
        self.instr(IrInstr::StoreSlot { slot: v_slot, v });
        self.declare(vname, v_slot);
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: next_id });
        self.cur = next_id;
        let c = self.load(idx_slot);
        let one = self.temp();
        self.instr(IrInstr::Const {
            dst: one,
            c: IrConst::Int(1),
        });
        let nxt = self.temp();
        self.instr(IrInstr::BinOp {
            dst: nxt,
            op: IrBinOp::Add,
            a: c,
            b: one,
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: nxt,
        });
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }

    /// `for (c in s)` over a string: iterates byte indices, binds the raw
    /// 0..=255 byte from `pickle_str_get` (no unbox) as a `byte` (`int` ABI).
    /// The string lives in a managed slot for the whole loop.
    fn for_in_string(&mut self, name: &str, seq_t: Temp, body: &Block) -> Result<(), ()> {
        let seq_slot = self.new_slot(IrTy::Ptr);
        let idx_slot = self.new_slot(IrTy::Int);
        let len_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot {
            slot: seq_slot,
            v: seq_t,
        });
        let zero = self.temp();
        self.instr(IrInstr::Const {
            dst: zero,
            c: IrConst::Int(0),
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: zero,
        });
        let seq_l = self.load(seq_slot);
        let len_t =
            self.extern_call_t1("pickle_str_len", vec![IrTy::Ptr], IrTy::Int, vec![seq_l])?;
        self.instr(IrInstr::StoreSlot {
            slot: len_slot,
            v: len_t,
        });

        let cond_id = self.new_block();
        let body_id = self.new_block();
        let next_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let cur_t = self.load(idx_slot);
        let end_l = self.load(len_slot);
        let cmp = self.temp();
        self.instr(IrInstr::BinOp {
            dst: cmp,
            op: IrBinOp::Lt,
            a: cur_t,
            b: end_l,
        });
        self.term(IrTerm::BranchIf {
            cond: cmp,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        self.loops.push(LoopCtx {
            continue_target: next_id,
            break_target: end_id,
        });
        self.push_scope();
        let cur_i = self.load(idx_slot);
        let seq_l = self.load(seq_slot);
        let v = self.extern_call_t1(
            "pickle_str_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Int,
            vec![seq_l, cur_i],
        )?;
        let elem_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot { slot: elem_slot, v });
        self.declare(name, elem_slot);
        self.block_body_only(body)?;
        self.pop_scope();
        self.loops.pop();
        self.term(IrTerm::Branch { target: next_id });
        self.cur = next_id;
        let c = self.load(idx_slot);
        let one = self.temp();
        self.instr(IrInstr::Const {
            dst: one,
            c: IrConst::Int(1),
        });
        let nxt = self.temp();
        self.instr(IrInstr::BinOp {
            dst: nxt,
            op: IrBinOp::Add,
            a: c,
            b: one,
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: nxt,
        });
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }

    fn block_body_only(&mut self, b: &Block) -> Result<(), ()> {
        for s in &b.stmts {
            self.stmt(s)?;
        }
        if let Some(e) = &b.expr {
            let _ = self.expr(e)?;
        }
        Ok(())
    }

    // ---- expressions ----

    fn expr(&mut self, e: &Expr) -> Result<Temp, ()> {
        let _ = self.irty(e.span)?;
        match &e.kind {
            ExprKind::Lit(l) => self.lit(l),
            ExprKind::Ident(name) => self.ident_expr(e, name),
            ExprKind::Call { callee, args } => self.call(e, callee, args),
            ExprKind::Binary { op, lhs, rhs } => self.binary(e, *op, lhs, rhs),
            ExprKind::Unary { op, operand } => self.unary(e, *op, operand),
            ExprKind::Assign { target, op, value } => self.assign(e, target, *op, value),
            ExprKind::If {
                cond,
                then,
                else_else,
            } => self.if_expr(e, cond, then, else_else.as_deref()),
            ExprKind::Block(b) => {
                self.push_scope();
                let r = self.block_value(b);
                self.pop_scope();
                r
            }
            ExprKind::This => self.this_value(e),
            // `super` denotes the same receiver as `this`; the checker types it
            // as the superclass, so `super.m(...)` statically dispatches to the
            // superclass's method on the same object.
            ExprKind::Super => self.this_value(e),
            ExprKind::Member { object, name } => self.member_value(e, object, name),
            ExprKind::Index { object, index } => self.index_read(e, object, index),
            ExprKind::OptAccess { object, name } => self.opt_access(e, object, name),
            ExprKind::OptUnwrap(inner) => self.opt_unwrap(e, inner),
            ExprKind::Lambda { .. } => self.lambda_value(e),
ExprKind::Match { scrutinee, arms } => self.match_expr(e, scrutinee, arms),
            // `await <future>` drives the future (a zero-argument function from
            // an `async fn`) to completion synchronously and yields its value:
            // it lowers to a dynamic call of the closure with no arguments.
            ExprKind::Await(inner) => match self.ty_of(&inner.span) {
                Some(Ty::Fn(ps, prt)) if ps.is_empty() => {
                    let obj = self.expr(inner)?;
                    self.fn_value_call_from(e, obj, &Ty::Fn(Vec::new(), prt.clone()), &[])
                }
                Some(Ty::Unknown) => self.bad(
                    inner.span,
                    "cannot `await` a value whose type is unknown; an `async fn` call returns a `() -> T` future",
                ),
                other => {
                    let shown = match other {
                        Some(Ty::Fn(ps, _)) if !ps.is_empty() => {
                            "a function taking arguments; a future is a zero-argument function"
                        }
                        Some(t) => return self.bad(inner.span, format!("cannot `await` a value of type `{t}`; a future is the `() -> T` value returned by an `async fn`")),
                        None => "an untyped value",
                    };
                    self.bad(inner.span, format!("cannot `await` {shown}"))
                }
            },
            ExprKind::GenericCall { name, type_args } => self.generic_fn_value(e, name, type_args),
            ExprKind::Cast { expr, ty, kind } => self.cast(e, expr, ty, *kind),
            ExprKind::Unsafe(b) => {
                self.push_scope();
                let r = self.block_value(b);
                self.pop_scope();
                r
            }
            ExprKind::Tuple(items) => self.tuple_literal(e, items),
            ExprKind::Array(items) => self.array_literal(e, items),
            ExprKind::Map(pairs) => self.map_literal(e, pairs),
            ExprKind::Range { .. } => self.bad(e.span, "range values are not lowered yet"),
        }
    }

    fn block_value(&mut self, b: &Block) -> Result<Temp, ()> {
        let want = self
            .ty_of(&b.span)
            .map(|t| elem_ir(&t))
            .unwrap_or(IrTy::Unit);
        if let Some(slot) = self.stmts_with_try_tail(&b.stmts, want)? {
            return Ok(self.load(slot));
        }
        for s in &b.stmts {
            self.stmt(s)?;
        }
        match &b.expr {
            Some(e) => self.expr(e),
            None => Ok(self.unit_temp()),
        }
    }

    fn lit(&mut self, l: &Lit) -> Result<Temp, ()> {
        let dst = self.temp();
        let c = match l {
            Lit::Int { value } => match i64::try_from(*value) {
                Ok(v) => IrConst::Int(v),
                Err(_) => {
                    return self.bad(
                        self.fname_span_fallback(),
                        "integer literal out of range for i64",
                    )
                }
            },
            Lit::Float { value } => IrConst::Float(value.to_bits()),
            Lit::Bool(v) => IrConst::Bool(*v),
            Lit::Char(c) => IrConst::Char(*c as u32),
            Lit::String(parts) => {
                return self.string_literal(parts);
            }
            Lit::None => return self.null_temp(),
        };
        self.instr(IrInstr::Const { dst, c });
        Ok(dst)
    }

    fn fname_span_fallback(&self) -> Span {
        // Literal range errors shouldn't happen for checked i128 ints that
        // overflow i64; give a zero span if the expr span is unavailable.
        Span::new(crate::diag::FileId(0), 0, 0)
    }

    /// Lower a string literal. Plain text becomes a `Const` string temp; an
    /// interpolated expression is emitted, stringified via a `pickle_str_from_*`
    /// helper when needed, and the parts are concatenated with
    /// `pickle_str_concat`.
    fn string_literal(&mut self, parts: &[StrPart]) -> Result<Temp, ()> {
        let mut acc: Option<Temp> = None;
        for part in parts {
            let v = match part {
                StrPart::Text(text) => {
                    let sid = StrId(self.intern_string(text));
                    let t = self.temp();
                    self.instr(IrInstr::Const {
                        dst: t,
                        c: IrConst::Str(sid),
                    });
                    t
                }
                StrPart::Expr(e) => {
                    let t = self.expr(e)?;
                    // Keyed on the checker type, not the IR type: a `byte` is an
                    // `int` at the ABI but must interpolate as one raw byte, not
                    // as digits (`{c}` in `for (c in "é")` must round-trip).
                    if self.ty_of(&e.span) == Some(Ty::Byte) {
                        self.extern_call_t1(
                            "pickle_str_from_byte",
                            vec![IrTy::Int],
                            IrTy::Str,
                            vec![t],
                        )?
                    } else {
                        match self.irty(e.span)? {
                            IrTy::Str => t,
                            IrTy::Int => self.extern_call_t1(
                                "pickle_str_from_i64",
                                vec![IrTy::Int],
                                IrTy::Str,
                                vec![t],
                            )?,
                            IrTy::Float => self.extern_call_t1(
                                "pickle_str_from_f64",
                                vec![IrTy::Float],
                                IrTy::Str,
                                vec![t],
                            )?,
                            IrTy::Bool => self.extern_call_t1(
                                "pickle_str_from_bool",
                                vec![IrTy::Bool],
                                IrTy::Str,
                                vec![t],
                            )?,
                            IrTy::Char => self.extern_call_t1(
                                "pickle_str_from_char",
                                vec![IrTy::Char],
                                IrTy::Str,
                                vec![t],
                            )?,
                            other => {
                                return self.bad(
                                    e.span,
                                    format!("interpolation of `{other:?}` is not lowered yet"),
                                )
                            }
                        }
                    }
                }
            };
            acc = Some(match acc {
                None => v,
                Some(a) => self.extern_call_t1(
                    "pickle_str_concat",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Str,
                    vec![a, v],
                )?,
            });
        }
        match acc {
            Some(t) => Ok(t),
            None => {
                // `""`: the lexer drops empty text parts, so there are no
                // parts to concatenate. Still intern the empty string so a
                // zero-length string-data symbol exists downstream.
                let sid = StrId(self.intern_string(""));
                let t = self.temp();
                self.instr(IrInstr::Const {
                    dst: t,
                    c: IrConst::Str(sid),
                });
                Ok(t)
            }
        }
    }

fn ident_expr(&mut self, e: &Expr, name: &str) -> Result<Temp, ()> {
   if let Some(slot) = self.lookup(name) {
      return Ok(self.load(slot));
   }
      // A top-level `var` is a real data slot: read it. This must be checked
      // *before* the const inline so a global is never mistaken for a value.
      if let Some(&id) = self.global_ids.get(name) {
         let ty = self.module.globals[id].ty;
         let dst = self.temp();
         self.instr(IrInstr::LoadGlobal { dst, id, ty });
         return Ok(dst);
      }
      // Inline a top-level const initializer.
      if let Some(init) = self.consts_inits.get(name).copied() {
            if self.const_inlining.iter().any(|n| n == name) {
                return self.bad(e.span, format!("cyclic `const` initialization of `{name}`"));
            }
            self.const_inlining.push(name.to_string());
            let t = self.expr(init);
            self.const_inlining.pop();
            return t;
        }
        // Inside an instance method a bare field name reads `this.field`.
        if let Some(cid) = self.owner {
            if let Some(slot) = self.instance_field_index(cid, name) {
                let field_ty = self.field_at(cid, slot).ty.clone();
                let this = self.this_value(e)?;
                return self.field_read(e.span, this, &field_ty, slot);
            }
            // Inside an instance method a bare property name reads the getter.
            if let Some(&fid) = self
                .property_ids
                .get(&(cid as u32, name.to_string(), false))
            {
                let this = self.this_value(e)?;
                return self.call_method(e, fid, &[], Some(this));
            }
            // A bare name may also be a static field of the enclosing class.
            if let Some((dcid, slot, info)) = self.static_field(cid, name) {
                return self.static_read(e.span, dcid, slot, &info.ty);
            }
            // ... or a constant of the enclosing class.
            if let Some(r) = self.read_class_const(e.span, cid, name) {
                return r;
            }
        }
        // A bare module function used as a value: wrap it in a zero-capture
        // closure whose slot 0 holds its address (the same shape a lambda
        // body gets).
        if let Some(&fid) = self.module.funcs_by_name.get(name) {
            return self.fn_value_closure(e, fid);
        }
        if self.generic_user_fn(name) {
            return self.bad(
                e.span,
                format!(
                    "using the generic function `{name}` as a value is not lowered yet; \
                     call it with explicit type arguments instead"
                ),
            );
        }
        self.bad(
            e.span,
            format!("using `{name}` as a value is not lowered yet"),
        )
    }

    fn this_value(&mut self, e: &Expr) -> Result<Temp, ()> {
        match self.lookup("this") {
            Some(slot) => Ok(self.load(slot)),
            None => self.bad(e.span, "`this` is not available here"),
        }
    }

    fn unary(&mut self, e: &Expr, op: AstUnOp, operand: &Expr) -> Result<Temp, ()> {
        match op {
            AstUnOp::Neg | AstUnOp::BitNot => {
                let v = self.expr(operand)?;
                let dst = self.temp();
                self.instr(IrInstr::UnOp {
                    dst,
                    op: if matches!(op, AstUnOp::Neg) {
                        IrUnOp::Neg
                    } else {
                        IrUnOp::BitNot
                    },
                    v,
                });
                Ok(dst)
            }
            AstUnOp::Not => {
                let v = self.expr(operand)?;
                let dst = self.temp();
                self.instr(IrInstr::UnOp {
                    dst,
                    op: IrUnOp::Not,
                    v,
                });
                Ok(dst)
            }
            AstUnOp::Deref => {
                let scalar = match self.ty_of(&operand.span) {
                    Some(Ty::Ptr(inner) | Ty::Ref(inner)) => Self::scalar_ir(&inner),
                    _ => None,
                };
                match scalar {
                    Some(ty) => {
                        let addr = self.expr(operand)?;
                        let dst = self.temp();
                        self.instr(IrInstr::LoadRaw { dst, addr, ty });
                        Ok(dst)
                    }
                    None => self.expr(operand),
                }
            }
            AstUnOp::AddrOf => {
                let t = self.ty_of(&operand.span);
                if t.as_ref().and_then(Self::scalar_ir).is_some() {
                    let ExprKind::Ident(name) = &operand.kind else {
                        return self.bad(e.span, "`&` of a scalar requires a local variable");
                    };
                    if let Some(slot) = self.lookup(name) {
                        let dst = self.temp();
                        self.instr(IrInstr::LocalAddr { dst, slot });
                        return Ok(dst);
                    }
                    // A top-level `var` is addressable too: the cell lives in the
                    // image's `.data`, so `&g` is the address of that symbol and a
                    // write through it lands in the global, not a copy. This is
                    // what lets a ported runtime pass a mutable cell to an extern
                    // (e.g. a per-thread frame head) instead of returning a value.
                    if let Some(&id) = self.global_ids.get(name.as_str()) {
                        let dst = self.temp();
                        self.instr(IrInstr::GlobalAddr { dst, id });
                        return Ok(dst);
                    }
                    return self.bad(e.span, "`&` of an unknown local");
                }
                match t {
                    Some(Ty::Class(..) | Ty::Struct(..) | Ty::Ptr(..)) => self.expr(operand),
                    _ => self.bad(
                        e.span,
                        "`&` currently only supports class, struct, pointer, and scalar local variables",
                    ),
                }
            }
        }
    }

    fn binary(&mut self, e: &Expr, op: AstBinOp, lhs: &Expr, rhs: &Expr) -> Result<Temp, ()> {
        let lt = self.ty_of(&lhs.span);
        let rt = self.ty_of(&rhs.span);
        let both_str = matches!(lt, Some(Ty::String)) && matches!(rt, Some(Ty::String));
        // Enums are managed pointers; a raw `icmp eq` would test address
        // identity, so `==`/`!=` route through the runtime structural compare
        // (`pickle_enum_eq`) which tests tag equality and payload fields.
        let both_enum = matches!(lt, Some(Ty::Enum(..))) && matches!(rt, Some(Ty::Enum(..)));
        match op {
            AstBinOp::And | AstBinOp::Or => return self.logic(e, op, lhs, rhs),
            AstBinOp::NullCoalesce => return self.null_coalesce(e, lhs, rhs),
            // `a..b` / `a..=b` as a *value*: materialize a `List<int>` via
            // `pickle_range` (end-exclusive, step 1); `..=` bumps the end by
            // one. Bounds must be `int` (the checker guarantees it). The
            // direct for-in form never reaches here — `for_in` lowers literal
            // ranges as a counter loop without building a list.
            AstBinOp::Range | AstBinOp::RangeIncl => {
                self.ensure_int(lhs)?;
                self.ensure_int(rhs)?;
                let start = self.expr(lhs)?;
                let end = self.expr(rhs)?;
                let one = self.int_const(1);
                if op == AstBinOp::Range {
                    return self.extern_call_t1(
                        "pickle_range",
                        vec![IrTy::Int, IrTy::Int, IrTy::Int],
                        IrTy::Ptr,
                        vec![start, end, one],
                    );
                }
                let end1 = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: end1,
                    op: IrBinOp::Add,
                    a: end,
                    b: one,
                });
                return self.extern_call_t1(
                    "pickle_range",
                    vec![IrTy::Int, IrTy::Int, IrTy::Int],
                    IrTy::Ptr,
                    vec![start, end1, one],
                );
            }
            AstBinOp::Send | AstBinOp::Is | AstBinOp::In | AstBinOp::Pow => {
                return self.bad(e.span, "this operator is not lowered yet")
            }
            AstBinOp::Add if both_str => {
                let a = self.expr(lhs)?;
                let b = self.expr(rhs)?;
                return self.extern_call_t1(
                    "pickle_str_concat",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Str,
                    vec![a, b],
                );
            }
            AstBinOp::Eq | AstBinOp::Ne if both_str => {
                let a = self.expr(lhs)?;
                let b = self.expr(rhs)?;
                let cmp = self.extern_call_t1(
                    "pickle_str_cmp",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Int,
                    vec![a, b],
                )?;
                let zero = self.temp();
                self.instr(IrInstr::Const {
                    dst: zero,
                    c: IrConst::Int(0),
                });
                let dst = self.temp();
                self.instr(IrInstr::BinOp {
                    dst,
                    op: if op == AstBinOp::Eq {
                        IrBinOp::Eq
                    } else {
                        IrBinOp::Ne
                    },
                    a: cmp,
                    b: zero,
                });
                return Ok(dst);
            }
            AstBinOp::Eq | AstBinOp::Ne if both_enum => {
                let a = self.expr(lhs)?;
                let b = self.expr(rhs)?;
                let eq = self.extern_call_t1(
                    "pickle_enum_eq",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    IrTy::Bool,
                    vec![a, b],
                )?;
                if op == AstBinOp::Eq {
                    return Ok(eq);
                }
                // `!=` is the logical negation of the structural compare.
                let dst = self.temp();
                self.instr(IrInstr::UnOp {
                    dst,
                    op: IrUnOp::Not,
                    v: eq,
                });
                return Ok(dst);
            }
            _ => {}
        }
        let mut a = self.expr(lhs)?;
        let mut b = self.expr(rhs)?;
        let mut aty = self.irty(lhs.span).ok();
        let mut bty = self.irty(rhs.span).ok();
        if matches!(aty, Some(IrTy::Float)) && matches!(bty, Some(IrTy::Int)) {
            let t = self.temp();
            self.instr(IrInstr::Itof { dst: t, v: b });
            b = t;
        } else if matches!(aty, Some(IrTy::Int)) && matches!(bty, Some(IrTy::Float)) {
            let t = self.temp();
            self.instr(IrInstr::Itof { dst: t, v: a });
            a = t;
        }
        // A `byte` compared with an ASCII `char` literal: the checker slots the
        // literal into the byte domain, so here it is lowered as an `int` const
        // so both operands share the `i64` ABI.
        let a_byte = self.ty_of(&lhs.span) == Some(Ty::Byte);
        let b_byte = self.ty_of(&rhs.span) == Some(Ty::Byte);
        if a_byte || b_byte {
            let mut ascii_lit_int = |x: &Expr| -> Option<Temp> {
                if let ExprKind::Lit(Lit::Char(c)) = &x.kind {
                    if *c as u32 <= 0x7F {
                        Some(self.int_const(*c as u8 as i64))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if a_byte {
                if let Some(t) = ascii_lit_int(rhs) {
                    b = t;
                    bty = Some(IrTy::Int);
                }
            } else if let Some(t) = ascii_lit_int(lhs) {
                a = t;
                aty = Some(IrTy::Int);
            }
        }
        // Defensive: a comparison whose operand IR types cannot share an
        // `icmp`/`fcmp` (bool vs int, char vs int, ...) must never reach the
        // verifier. The checker rejects these; anything slipping through here
        // (e.g. under a substitution) bails loudly instead of miscompiling.
        if matches!(
            op,
            AstBinOp::Lt | AstBinOp::Le | AstBinOp::Gt | AstBinOp::Ge | AstBinOp::Eq | AstBinOp::Ne
        ) && !cmp_types_compatible(aty, bty)
        {
            return self.bad(
                e.span,
                format!("comparison `{op:?}` with incompatible operand types is not lowered yet"),
            );
        }
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: binary_opcode(op),
            a,
            b,
        });
        Ok(dst)
    }

    /// Short-circuit `&&` / `||` via branches and a result slot.
    fn logic(&mut self, _e: &Expr, op: AstBinOp, lhs: &Expr, rhs: &Expr) -> Result<Temp, ()> {
        let res_slot = self.new_slot(IrTy::Bool);
        let is_and = op == AstBinOp::And;
        let a = self.expr(lhs)?;
        let rhs_id = self.new_block();
        let short_id = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: a,
            then: if is_and { rhs_id } else { short_id },
            else_: if is_and { short_id } else { rhs_id },
        });
        self.cur = rhs_id;
        let b = self.expr(rhs)?;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: b,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = short_id;
        let sv = self.temp();
        self.instr(IrInstr::Const {
            dst: sv,
            c: IrConst::Bool(!is_and),
        });
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: sv,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        Ok(self.load(res_slot))
    }

    /// Combine two boolean temps (`&&` when `is_and`, else `||`) into one
    /// `Bool` temp. Like `logic`, this is branch-based because the IR has no
    /// boolean and/or instruction.
    fn combine_cond(&mut self, is_and: bool, a: Temp, b: Temp) -> Temp {
        let res_slot = self.new_slot(IrTy::Bool);
        let rhs_id = self.new_block();
        let short_id = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: a,
            then: if is_and { rhs_id } else { short_id },
            else_: if is_and { short_id } else { rhs_id },
        });
        self.cur = rhs_id;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: b,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = short_id;
        let sv = self.temp();
        self.instr(IrInstr::Const {
            dst: sv,
            c: IrConst::Bool(!is_and),
        });
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: sv,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        self.load(res_slot)
    }

    /// Test a pointer-valued option for presence (`ptr != null`), yielding a
    /// `Bool` temp so it can drive a branch.
    fn opt_is_present(&mut self, p: Temp) -> Result<Temp, ()> {
        let null = self.null_temp()?;
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: IrBinOp::Ne,
            a: p,
            b: null,
        });
        Ok(dst)
    }

    /// Resolve a present option pointer to its inner value: scalars are
    /// unboxed to the element IR type; managed values pass through.
    fn opt_resolve(&mut self, p: Temp, inner: &Ty, span: Span) -> Result<Temp, ()> {
        match self.map_ty(inner, span)? {
            IrTy::Int => {
                self.extern_call_t1("pickle_unbox_i64", vec![IrTy::Ptr], IrTy::Int, vec![p])
            }
            IrTy::Float => {
                self.extern_call_t1("pickle_unbox_f64", vec![IrTy::Ptr], IrTy::Float, vec![p])
            }
            IrTy::Bool => {
                self.extern_call_t1("pickle_unbox_bool", vec![IrTy::Ptr], IrTy::Bool, vec![p])
            }
            IrTy::Char => {
                self.extern_call_t1("pickle_unbox_char", vec![IrTy::Ptr], IrTy::Char, vec![p])
            }
            _ => Ok(p),
        }
    }

    /// `a ?? b`: yield the unwrapped `a` when present, else `b`. `a` is an
    /// option pointer; the result is the inner type (scalar or managed).
    fn null_coalesce(&mut self, e: &Expr, lhs: &Expr, rhs: &Expr) -> Result<Temp, ()> {
        let Some(inner) = self.ty_of(&lhs.span).and_then(|t| t.inner_option()) else {
            return self.bad(e.span, "left side of `??` is not an option");
        };
        let res_ir = self.map_ty(&inner, e.span)?;
        let res_slot = self.new_slot(res_ir);
        let a = self.expr(lhs)?;
        let present = self.opt_is_present(a)?;
        let some_id = self.new_block();
        let rhs_id = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: present,
            then: some_id,
            else_: rhs_id,
        });
        self.cur = some_id;
        let av = self.opt_resolve(a, &inner, e.span)?;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: av,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = rhs_id;
        let b = self.expr(rhs)?;
        let b = if res_ir == IrTy::Ptr {
            let vt = self.ty_of(&rhs.span).unwrap_or(Ty::Unknown);
            self.option_wrap(b, &vt, rhs.span)?
        } else {
            b
        };
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: b,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        Ok(self.load(res_slot))
    }

    /// `a?`: unwrap `a` to its inner value, panicking at runtime on `none`.
    fn opt_unwrap(&mut self, e: &Expr, operand: &Expr) -> Result<Temp, ()> {
        let Some(inner) = self.ty_of(&operand.span).and_then(|t| t.inner_option()) else {
            return self.bad(e.span, "`?` operand is not an option");
        };
        let p = self.expr(operand)?;
        self.opt_unwrap_val(e.span, p, &inner)
    }

    /// Unwrap an already-evaluated option pointer `p` (type `inner?`) to its
    /// inner value, panicking at runtime on `none`.
    fn opt_unwrap_val(&mut self, span: Span, p: Temp, inner: &Ty) -> Result<Temp, ()> {
        let res_ir = self.map_ty(inner, span)?;
        let res_slot = self.new_slot(res_ir);
        let present = self.opt_is_present(p)?;
        let some_id = self.new_block();
        let none_id = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: present,
            then: some_id,
            else_: none_id,
        });
        self.cur = none_id;
        self.extern_call_void("pickle_panic_none_unwrap", vec![], vec![]);
        self.term(IrTerm::Branch { target: join });
        self.cur = some_id;
        let v = self.opt_resolve(p, inner, span)?;
        self.instr(IrInstr::StoreSlot { slot: res_slot, v });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        Ok(self.load(res_slot))
    }

    /// `a?.name`: when `a` is present, read `name` off the receiver and lift it
    /// to `T?`; when it is `none`, the result is `none`. The receiver is
    /// evaluated exactly once.
    fn opt_access(&mut self, e: &Expr, object: &Expr, name: &str) -> Result<Temp, ()> {
        let Some(inner) = self.ty_of(&object.span).and_then(|t| t.inner_option()) else {
            return self.bad(e.span, "`?.` requires an option receiver");
        };
        let o = self.expr(object)?;
        let res_slot = self.new_slot(IrTy::Ptr);
        let present = self.opt_is_present(o)?;
        let some_id = self.new_block();
        let none_id = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: present,
            then: some_id,
            else_: none_id,
        });
        self.cur = none_id;
        let n = self.null_temp()?;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: n,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = some_id;
        let (member_ty, mv) = self.opt_member_read(e, o, &inner, name)?;
        let mv = self.option_wrap(mv, &member_ty, e.span)?;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: mv,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        Ok(self.load(res_slot))
    }

    /// Read field `name` (or dispatch the getter for property `name`) from an
    /// already-evaluated class/struct receiver, returning its declared type and
    /// the value temp.
    fn opt_member_read(
        &mut self,
        e: &Expr,
        obj: Temp,
        recv_ty: &Ty,
        name: &str,
    ) -> Result<(Ty, Temp), ()> {
        let (cid, cname) = match recv_ty {
            Ty::Class(cn, _) | Ty::Struct(cn, _) => {
                (self.class_id_of(recv_ty, e.span)?, cn.clone())
            }
            _ => (None, String::new()),
        };
        let Some(cid) = cid else {
            return self.bad(
                e.span,
                "optional access is only lowered for class/struct members",
            );
        };
        if let Some(slot) = self.instance_field_index(cid as i64, name) {
            let field_ty = self.field_at(cid as i64, slot).ty.clone();
            let v = self.field_read(e.span, obj, &field_ty, slot)?;
            return Ok((field_ty, v));
        }
        if let Some(&fid) = self.property_ids.get(&(cid, name.to_string(), false)) {
            let pty = self.property_ty_of(&cname, name);
            let v = self.call_method(e, fid, &[], Some(obj))?;
            return Ok((pty, v));
        }
        self.bad(
            e.span,
            format!("no field or property `{name}` for optional access"),
        )
    }

    /// The declared type of property `name` on class `cname`.
    fn property_ty_of(&self, cname: &str, name: &str) -> Ty {
        match self.resolved.types.get(cname) {
            Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => t
                .properties
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.ty.clone())
                .unwrap_or(Ty::Unknown),
            _ => Ty::Unknown,
        }
    }

    // ---- casts -----------------------------------------------------------

    /// `x is T` / `x as T` / `x as? T`. The result stays in the typed IR value
    /// domain: `is` -> bool, `as` -> `T`, `as?` -> `T?` (a pointer).
    fn cast(
        &mut self,
        e: &Expr,
        operand: &Expr,
        ty: &TypeExpr,
        kind: CastKind,
    ) -> Result<Temp, ()> {
        let src = self.ty_of(&operand.span).unwrap_or(Ty::Unknown);
        let dst = self.resolved.resolve_ty(ty, &[], self.diags);
        match kind {
            CastKind::Is => self.cast_is(e, operand, &src, &dst),
            CastKind::As => self.cast_as(e, operand, &src, &dst),
            CastKind::TryAs => {
                // `x as? T` always yields an option, never panics.
                let v = self.expr(operand)?;
                let inner = dst.inner_option().unwrap_or_else(|| dst.clone());
                self.cast_to_option(e.span, v, &src, &inner)
            }
        }
    }

    fn cast_is(&mut self, e: &Expr, operand: &Expr, src: &Ty, dst: &Ty) -> Result<Temp, ()> {
        // A cast strips at most one option layer on each side; the underlying
        // (non-option) types decide whether the test can succeed, because in
        // this statically-typed subset the runtime type of a value is known
        // from its static type.
        let src_opt = src.inner_option();
        let dst_opt = dst.inner_option();
        let base_src = src_opt.clone().unwrap_or_else(|| src.clone());
        let base_dst = dst_opt.clone().unwrap_or_else(|| dst.clone());
        let v = self.expr(operand)?;
        if base_src == base_dst {
            if src_opt.is_some() && dst_opt.is_none() {
                // `x: T?` is `T` -> "is it present?".
                return self.opt_is_present(v);
            }
            // `T is T`, `T is T?`, `T? is T?` -> always true.
            return Ok(self.bool_const(true));
        }
        if base_src.is_numeric() && base_dst.is_numeric() {
            // `int is float` etc. is a static mismatch; `v` was evaluated above.
            return Ok(self.bool_const(false));
        }
        // Class/struct types along the inheritance chain: an upcast (the target
        // is an ancestor of the static type) always holds; a downcast needs a
        // runtime identity test; unrelated classes can never match.
        if let (Some((sn, _)), Some((dn, dcid))) =
            (self.user_class_id(&base_src), self.user_class_id(&base_dst))
        {
            if self.is_ancestor(&dn, &sn) {
                return Ok(self.bool_const(true));
            }
            if self.is_ancestor(&sn, &dn) {
                let dc = self.int_const(dcid as i64);
                return self.extern_call_t1(
                    "pickle_class_is",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Bool,
                    vec![v, dc],
                );
            }
            return Ok(self.bool_const(false));
        }
        // Interface tests probe the runtime tables: an interface target
        // (`pickle_class_implements`) answers exactly whether the object's
        // class — directly or through its ancestors — declared that interface
        // instantiation, so `Container<int>` vs `Container<string>` stays
        // precise. A class target under an interface source is an ordinary
        // runtime class test.
        if let Some((dst_in, dst_iargs)) = self.iface_of(&base_dst) {
            let iface_id = self.iface_id_for(&dst_in, &dst_iargs, e.span)?;
            let dc = self.int_const(iface_id as i64);
            return self.extern_call_t1(
                "pickle_class_implements",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Bool,
                vec![v, dc],
            );
        }
        if self.iface_of(&base_src).is_some() {
            let Some((_, dcid)) = self.user_class_id(&base_dst) else {
                let _: Result<(), ()> =
                    self.bad(e.span, format!("`{src} is {dst}` is not lowered yet"));
                return Err(());
            };
            let dc = self.int_const(dcid as i64);
            return self.extern_call_t1(
                "pickle_class_is",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Bool,
                vec![v, dc],
            );
        }
        self.bad(
            e.span,
            format!("`{src} is {dst}` is not lowered yet (class/interface tests need inheritance)"),
        )
    }

    fn cast_as(&mut self, e: &Expr, operand: &Expr, src: &Ty, dst: &Ty) -> Result<Temp, ()> {
        let v = self.expr(operand)?;
        if let Some(inner) = dst.inner_option() {
            // Casting to an option type never panics: `none` stays `none`.
            return self.cast_to_option(e.span, v, src, &inner);
        }
        if let Some(si) = src.inner_option() {
            // `x: T?` as `U` asserts presence: unwrap (panics on none) then
            // convert the inner value.
            let ival = self.opt_unwrap_val(e.span, v, &si)?;
            return self.convert(e.span, ival, &si, dst);
        }
        self.convert(e.span, v, src, dst)
    }

    /// Produce the option `inner?` from an already-evaluated value `v` of type
    /// `src`. A managed source passes through; scalars are boxed. When `src` is
    /// itself an option, `none` is preserved and the present value is converted.
    fn cast_to_option(&mut self, span: Span, v: Temp, src: &Ty, inner: &Ty) -> Result<Temp, ()> {
        if let Some(si) = src.inner_option() {
            if &si == inner {
                // Already the requested option; `none` is preserved.
                return Ok(v);
            }
            if !(si.is_numeric() && inner.is_numeric()) {
                return self.bad(span, format!("`{src} as? {inner}` is not lowered yet"));
            }
            // Present -> convert the inner; absent -> none.
            let res_slot = self.new_slot(IrTy::Ptr);
            let present = self.opt_is_present(v)?;
            let some_id = self.new_block();
            let none_id = self.new_block();
            let join = self.new_block();
            self.term(IrTerm::BranchIf {
                cond: present,
                then: some_id,
                else_: none_id,
            });
            self.cur = none_id;
            let n = self.null_temp()?;
            self.instr(IrInstr::StoreSlot {
                slot: res_slot,
                v: n,
            });
            self.term(IrTerm::Branch { target: join });
            self.cur = some_id;
            let iv = self.opt_resolve(v, &si, span)?;
            let cv = self.convert(span, iv, &si, inner)?;
            let wrapped = self.option_wrap(cv, inner, span)?;
            self.instr(IrInstr::StoreSlot {
                slot: res_slot,
                v: wrapped,
            });
            self.term(IrTerm::Branch { target: join });
            self.cur = join;
            return Ok(self.load(res_slot));
        }
        let cv = self.convert(span, v, src, inner)?;
        self.option_wrap(cv, inner, span)
    }

    /// Convert a non-option value between related (non-option) types. Identity
    /// and numeric conversions are supported; anything else bails.
    fn convert(&mut self, span: Span, v: Temp, from: &Ty, to: &Ty) -> Result<Temp, ()> {
        if from == to {
            return Ok(v);
        }
        if from.is_numeric() && to.is_numeric() {
            // `int` and `byte` share the same machine `Int` domain, so
            // int<->byte casts are identity register moves.
            if matches!((from, to), (Ty::Int, Ty::Byte) | (Ty::Byte, Ty::Int)) {
                return Ok(v);
            }
            let dst = self.temp();
            let instr = match (from, to) {
                (Ty::Int, Ty::Float) => IrInstr::Itof { dst, v },
                (Ty::Float, Ty::Int) => IrInstr::Ftoi { dst, v },
                _ => return self.bad(span, format!("cannot cast `{from}` to `{to}`")),
            };
            self.instr(instr);
            return Ok(dst);
        }
        // Char <-> integer casts reinterpret the code point scalar: `char as
        // int` zero-extends the i32 code point to a word; `int as char`
        // narrows the word down to the i32 code point (the frontend keeps
        // values in range by construction).
        if matches!((from, to), (Ty::Char, Ty::Int) | (Ty::Char, Ty::Byte)) {
            let dst = self.temp();
            self.instr(IrInstr::Chartoi { dst, v });
            return Ok(dst);
        }
        if matches!((from, to), (Ty::Int, Ty::Char) | (Ty::Byte, Ty::Char)) {
            let dst = self.temp();
            self.instr(IrInstr::Itochar { dst, v });
            return Ok(dst);
        }
        // Class/struct casts: an upcast reinterprets the same object; a
        // downcast is checked by the runtime and panics on a mismatch.
        if let (Some((sn, _)), Some((dn, dcid))) =
            (self.user_class_id(from), self.user_class_id(to))
        {
            if self.is_ancestor(&dn, &sn) {
                return Ok(v);
            }
            let dc = self.int_const(dcid as i64);
            return self.extern_call_t1(
                "pickle_class_cast",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![v, dc],
            );
        }
        // Interface casts re-test the object against the target's runtime
        // tables: up to an interface via `pickle_iface_cast` (panics when the
        // object's class does not implement that instantiation), and an
        // interface-typed source down to a class via `pickle_class_cast`.
        if let Some((cin, ciargs)) = self.iface_of(to) {
            let iface_id = self.iface_id_for(&cin, &ciargs, span)?;
            let dc = self.int_const(iface_id as i64);
            return self.extern_call_t1(
                "pickle_iface_cast",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![v, dc],
            );
        }
        if self.iface_of(from).is_some() {
            let Some((_, dcid)) = self.user_class_id(to) else {
                let _: Result<(), ()> =
                    self.bad(span, format!("`{from} as {to}` is not lowered yet"));
                return Err(());
            };
            let dc = self.int_const(dcid as i64);
            return self.extern_call_t1(
                "pickle_class_cast",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![v, dc],
            );
        }
        self.bad(
            span,
            format!("`{from} as {to}` is not lowered yet (class/interface casts need inheritance)"),
        )
    }

    fn bool_const(&mut self, b: bool) -> Temp {
        let dst = self.temp();
        self.instr(IrInstr::Const {
            dst,
            c: IrConst::Bool(b),
        });
        dst
    }

    fn char_const(&mut self, v: u32) -> Temp {
        let dst = self.temp();
        self.instr(IrInstr::Const {
            dst,
            c: IrConst::Char(v),
        });
        dst
    }

    fn float_const(&mut self, v: f64) -> Temp {
        let dst = self.temp();
        self.instr(IrInstr::Const {
            dst,
            c: IrConst::Float(v.to_bits()),
        });
        dst
    }

    fn assign(&mut self, e: &Expr, target: &Expr, op: AssignOp, value: &Expr) -> Result<Temp, ()> {
        let span = target.span;
        if let ExprKind::Unary {
            op: AstUnOp::Deref,
            operand,
        } = &target.kind
        {
            let pt = self.ty_of(&operand.span).unwrap_or(Ty::Unknown);
            if let Ty::Ptr(inner) = &pt {
                if let Some(ty) = Self::scalar_ir(inner) {
                    let addr = self.expr(operand)?;
                    if op == AssignOp::Assign {
                        let v = self.expr(value)?;
                        self.instr(IrInstr::StoreRaw { addr, v, ty });
                        return Ok(v);
                    }
                    // `*p op= v`: read-modify-write on the dereferenced cell.
                    let cur = self.temp();
                    self.instr(IrInstr::LoadRaw { dst: cur, addr, ty });
                    let v = self.expr(value)?;
                    let dst = self.temp();
                    self.instr(IrInstr::BinOp {
                        dst,
                        op: assign_opcode(op),
                        a: cur,
                        b: v,
                    });
                    self.instr(IrInstr::StoreRaw { addr, v: dst, ty });
                    return Ok(dst);
                }
            }
            if op != AssignOp::Assign {
                return self.bad(
                    span,
                    "compound assignment through a raw pointer is not lowered yet",
                );
            }
            return match pt {
                Ty::Class(..) | Ty::Struct(..) | Ty::Ptr(..) => self.assign(e, operand, op, value),
                Ty::Ref(_) => self.bad(
                    span,
                    "cannot write through an immutable reference (`&T`)",
                ),
                _ => self.bad(
                    span,
                    "assignment through a raw pointer is only supported for class, struct, pointer, and scalar values",
                ),
            };
        }
        if let ExprKind::Index { object, index } = &target.kind {
            return self.index_assign(e, op, object, index, value);
        }
        if let ExprKind::Member { object, name } = &target.kind {
            return self.member_assign(e, op, object, name, value);
        }
        let ExprKind::Ident(name) = &target.kind else {
            return self.bad(
                span,
                "assignment targets other than names are not lowered yet",
            );
        };
        let v = self.expr(value)?;
        let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
        if let Some(slot) = self.lookup(name) {
            if op == AssignOp::Assign {
                let sv = if matches!(self.fslots.get(slot.0 as usize), Some(IrTy::Ptr)) {
                    self.option_wrap(v, &vt, value.span)?
                } else {
                    v
                };
                self.instr(IrInstr::StoreSlot { slot, v: sv });
                return Ok(v);
            }
            let cur = self.load(slot);
            let dst = match self.fslots.get(slot.0 as usize).copied() {
                Some(IrTy::Str) => self.extern_call_t1(
                    "pickle_str_concat",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Str,
                    vec![cur, v],
                )?,
                _ => {
                    let t = self.temp();
                    self.instr(IrInstr::BinOp {
                        dst: t,
                        op: assign_opcode(op),
                        a: cur,
                        b: v,
                    });
                    t
                }
            };
            self.instr(IrInstr::StoreSlot { slot, v: dst });
return Ok(dst);
         }
         // A top-level `var` write goes to the data symbol. Compound forms
         // are read-modify-write against the slot, so `g += 1` is a load then
         // a store, never a blind store of `v`.
         if let Some(&id) = self.global_ids.get(name) {
            let ty = self.module.globals[id].ty;
            let stored = if op == AssignOp::Assign {
               v
            } else {
               let cur = self.temp();
               self.instr(IrInstr::LoadGlobal { dst: cur, id, ty });
               let dst = self.temp();
               self.instr(IrInstr::BinOp {
                  dst,
                  op: assign_opcode(op),
                  a: cur,
                  b: v,
               });
               dst
            };
            self.instr(IrInstr::StoreGlobal {
               id,
               v: stored,
               ty,
            });
            return Ok(stored);
         }
         // Inside an instance method a bare field name assigns `this.field`.
        if let Some(cid) = self.owner {
            if let Some(idx) = self.instance_field_index(cid, name) {
                let field_ty = self.field_at(cid, idx).ty.clone();
                let owned = self.field_at(cid, idx).manual;
                let this = self.this_value(e)?;
                return self.field_assign(e.span, op, this, &field_ty, idx, v, &vt, owned);
            }
            // A bare name may also be a static field of the enclosing class.
            if let Some((dcid, slot, info)) = self.static_field(cid, name) {
                if !info.mutable {
                    return self.bad(
                        span,
                        format!("cannot assign to immutable static field `{name}`"),
                    );
                }
                return self.static_assign(span, op, dcid, slot, &info.ty, v, &vt);
            }
        }
        self.bad(span, format!("cannot assign to `{name}`"))
    }

    /// `obj.field` and `obj.field op= value` on class/struct instances.
    fn member_assign(
        &mut self,
        e: &Expr,
        op: AssignOp,
        object: &Expr,
        name: &str,
        value: &Expr,
    ) -> Result<Temp, ()> {
        // `Type.staticProperty = v` dispatches the receiver-less setter.
        if let ExprKind::Ident(tname) = &object.kind {
            if let Some(&cid) = self.class_by_name.get(tname) {
                if let Some(&fid) = self.static_property_ids.get(&(cid, name.to_string(), true)) {
                    if op != AssignOp::Assign {
                        // `Type.prop op= value`: read through the getter, combine,
                        // call the setter with the result.
                        let Some(&get_fid) =
                            self.static_property_ids
                                .get(&(cid, name.to_string(), false))
                        else {
                            return self
                                .bad(e.span, format!("static property `{name}` has no getter"));
                        };
                        let pty = self.property_ty_of(tname, name);
                        let cur = self.call_method(e, get_fid, &[], None)?;
                        let v = self.expr(value)?;
                        let dst =
                            self.combine_compound(e.span, op, &pty, cur, v, "static property")?;
                        let packed = if matches!(
                            self.module.funcs[fid.0].params.first().map(|p| p.ty),
                            Some(IrTy::Ptr)
                        ) {
                            self.option_wrap(dst, &pty, value.span)?
                        } else {
                            dst
                        };
                        let dsti = self.temp();
                        self.instr(IrInstr::Call {
                            dst: Some(dsti),
                            callee: Callee::Func(fid),
                            args: vec![packed],
                        });
                        // A setter is an ordinary method body, so it can
                        // `throw`. Nothing reads `dsti` here, but the pending
                        // flag still has to be noticed before the assignment
                        // finishes and the caller carries on.
                        self.guard_propagation(fid, e.span)?;
                        return Ok(dst);
                    }
                    let v = self.expr(value)?;
                    let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
                    let packed = if matches!(
                        self.module.funcs[fid.0].params.first().map(|p| p.ty),
                        Some(IrTy::Ptr)
                    ) {
                        self.option_wrap(v, &vt, value.span)?
                    } else {
                        v
                    };
                    let dst = self.temp();
                    self.instr(IrInstr::Call {
                        dst: Some(dst),
                        callee: Callee::Func(fid),
                        args: vec![packed],
                    });
                    // Plain `Type.prop = v`: same as the instance case, the
                    // assignment's value is `v` but the accessor call still
                    // has to be observed for a pending throw.
                    self.guard_propagation(fid, e.span)?;
                    return Ok(v);
                }
                if let Some((dcid, slot, info)) = self.static_field(cid as i64, name) {
                    if !info.mutable {
                        return self.bad(
                            e.span,
                            format!("cannot assign to immutable static field `{name}`"),
                        );
                    }
                    let v = self.expr(value)?;
                    let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
                    return self.static_assign(e.span, op, dcid, slot, &info.ty, v, &vt);
                }
                return self.bad(
                    e.span,
                    format!("`{tname}` has no settable static member `{name}`"),
                );
            }
        }
        let ot = self.ty_of(&object.span);
        let ot = match ot {
            Some(Ty::Ptr(inner)) => Some((*inner).clone()),
            other => other,
        };
        let cid = match ot {
            Some(otv @ (Ty::Class(..) | Ty::Struct(..))) => self.class_id_of(&otv, e.span)?,
            _ => None,
        };
        let Some(cid) = cid else {
            return self.bad(
                e.span,
                "assignment over this member type is not lowered yet",
            );
        };
        let Some(idx) = self.instance_field_index(cid as i64, name) else {
            // `obj.prop = value` dispatches the property's setter.
            if let Some(&fid) = self.property_ids.get(&(cid, name.to_string(), true)) {
                if op != AssignOp::Assign {
                    // `obj.prop op= value`: read through the getter, combine,
                    // call the setter with the result.
                    let Some(&get_fid) = self.property_ids.get(&(cid, name.to_string(), false))
                    else {
                        return self.bad(e.span, format!("property `{name}` has no getter"));
                    };
                    let obj = self.expr(object)?;
                    let pty = self.property_ty_of(&self.class_name_of(cid), name);
                    let cur = self.call_method(e, get_fid, &[], Some(obj))?;
                    let v = self.expr(value)?;
                    let dst = self.combine_compound(e.span, op, &pty, cur, v, "property")?;
                    let packed = if matches!(
                        self.module.funcs[fid.0].params.get(1).map(|p| p.ty),
                        Some(IrTy::Ptr)
                    ) {
                        self.option_wrap(dst, &pty, value.span)?
                    } else {
                        dst
                    };
                    let dsti = self.temp();
                    self.instr(IrInstr::Call {
                        dst: Some(dsti),
                        callee: Callee::Func(fid),
                        args: vec![obj, packed],
                    });
                    // As with the static setter: a throwing accessor has to
                    // be noticed here, or the assignment looks successful
                    // and the flag escapes to some unrelated later call.
                    self.guard_propagation(fid, e.span)?;
                    return Ok(dst);
                }
                let obj = self.expr(object)?;
                let v = self.expr(value)?;
                let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
                let packed = if matches!(
                    self.module.funcs[fid.0].params.get(1).map(|p| p.ty),
                    Some(IrTy::Ptr)
                ) {
                    self.option_wrap(v, &vt, value.span)?
                } else {
                    v
                };
                let dst = self.temp();
                self.instr(IrInstr::Call {
                    dst: Some(dst),
                    callee: Callee::Func(fid),
                    args: vec![obj, packed],
                });
                // Plain `obj.prop = v`. The accessor's own body may `throw`,
                // which comes back as a pending flag on this function's
                // return; the assignment's value is `v`, but the call still
                // has to be observed or the flag leaks into whatever runs next.
                self.guard_propagation(fid, e.span)?;
                return Ok(v);
            }
            return self.bad(
                e.span,
                format!(
                    "`{name}` is not a field or settable property of this `{}`",
                    self.class_name_of(cid)
                ),
            );
        };
        let field_ty = self.field_at(cid as i64, idx).ty.clone();
        let owned = self.field_at(cid as i64, idx).manual;
        let v = self.expr(value)?;
        let vvt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
        let obj = self.expr(object)?;
        self.field_assign(e.span, op, obj, &field_ty, idx, v, &vvt, owned)
    }

    /// Read-modify-write/plain store of one field slot.
    #[allow(clippy::too_many_arguments)]
    fn field_assign(
        &mut self,
        span: Span,
        op: AssignOp,
        obj: Temp,
        field_ty: &Ty,
        slot: usize,
        v: Temp,
        vt: &Ty,
        owned: bool,
    ) -> Result<Temp, ()> {
        let rep = self.elem_rep(field_ty, span)?;
        if op == AssignOp::Assign {
            if owned {
                // Replacing an owned field releases the value it held.
                let idx = self.int_const(slot as i64);
                let old = self.extern_call_t1(
                    "pickle_obj_slot_get",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![obj, idx],
                )?;
                let v = self.extern_call_t1(
                    "pickle_manual_adopt",
                    vec![IrTy::Ptr],
                    IrTy::Ptr,
                    vec![v],
                )?;
                let packed = self.pack_for_pointer_boundary(&rep, v, vt, span)?;
                let boxed = self.box_for_store(&rep, packed, elem_ir(field_ty))?;
                self.field_store(obj, slot as i64, field_ty, boxed);
                self.extern_call_void("pickle_manual_free", vec![IrTy::Ptr], vec![old]);
                return Ok(v);
            }
            let packed = self.pack_for_pointer_boundary(&rep, v, vt, span)?;
            let boxed = self.box_for_store(&rep, packed, elem_ir(field_ty))?;
            self.field_store(obj, slot as i64, field_ty, boxed);
            return Ok(v);
        }
        if matches!(rep, ElemRep::Ptr) {
            if field_ty != &Ty::String {
                return self.bad(
                    span,
                    "compound assignment to a non-scalar field is not lowered yet",
                );
            }
            // String field: read the current string, concatenate, store back.
            let idx = self.int_const(slot as i64);
            let cur = self.extern_call_t1(
                "pickle_obj_slot_get",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![obj, idx],
            )?;
            let dst = self.combine_compound(span, op, field_ty, cur, v, "field")?;
            let boxed = self.box_for_store(&rep, dst, elem_ir(field_ty))?;
            self.field_store(obj, slot as i64, field_ty, boxed);
            return Ok(dst);
        }
        let obj_c = obj;
        // Read the current value (unboxed), combine, write back.
        let idx = self.int_const(slot as i64);
        let raw = self.extern_call_t1(
            "pickle_obj_slot_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![obj_c, idx],
        )?;
        let cur = match rep {
            ElemRep::Scalar(_, unbox, ir) => {
                self.extern_call_t1(unbox, vec![IrTy::Ptr], ir, vec![raw])?
            }
            ElemRep::Ptr => raw,
        };
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: assign_opcode(op),
            a: cur,
            b: v,
        });
        let boxed = self.box_for_store(&rep, dst, elem_ir(field_ty))?;
        self.field_store(obj_c, slot as i64, field_ty, boxed);
        Ok(dst)
    }

    /// Combine the current value `cur` of a compound-assignment target with the
    /// RHS `v` for operator `op`, where the target's declared type is `ty`.
    /// Scalar targets use the arithmetic binary op; `string` targets
    /// concatenate (`pickle_str_concat`), gated on `+=` because the checker
    /// does not validate compound operators, so any other operator on a string
    /// bails instead of being silently interpreted as concatenation. Other
    /// pointer-shape targets (`Option`, `List`, class instances, ...) have no
    /// compound operator and keep bailing. `what` names the target for the
    /// not-lowered diagnostic.
    fn combine_compound(
        &mut self,
        span: Span,
        op: AssignOp,
        ty: &Ty,
        cur: Temp,
        v: Temp,
        what: &str,
    ) -> Result<Temp, ()> {
        match ty {
            Ty::String => {
                if op != AssignOp::Add {
                    return self.bad(
                        span,
                        "compound assignment on this string only supports `+=`",
                    );
                }
                self.extern_call_t1(
                    "pickle_str_concat",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Str,
                    vec![cur, v],
                )
            }
            scalar if Self::scalar_ir(scalar).is_some() => {
                let dst = self.temp();
                self.instr(IrInstr::BinOp {
                    dst,
                    op: assign_opcode(op),
                    a: cur,
                    b: v,
                });
                Ok(dst)
            }
            _ => self.bad(
                span,
                format!("compound assignment to a non-scalar {what} is not lowered yet"),
            ),
        }
    }

    /// Emit `pickle_obj_slot_set(obj, slot, boxed_value)`.
    fn field_store(&mut self, obj: Temp, slot: i64, _field_ty: &Ty, boxed: Temp) {
        let idx = self.int_const(slot);
        self.extern_call_void(
            "pickle_obj_slot_set",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
            vec![obj, idx, boxed],
        );
    }

    /// Declared info of the static field at static slot `idx` of class `cid`.
    fn static_field_at(&self, cid: i64, idx: usize) -> Option<FieldInfo> {
        let plan = self.classes.iter().find(|p| p.class_id as i64 == cid)?;
        plan.table
            .fields
            .iter()
            .filter(|f| f.is_static)
            .nth(idx)
            .cloned()
    }

    /// Static-field read: `pickle_static_get` then unbox scalars.
    fn static_read(
        &mut self,
        span: Span,
        cid: u32,
        slot: usize,
        field_ty: &Ty,
    ) -> Result<Temp, ()> {
        let rep = self.elem_rep(field_ty, span)?;
        let c = self.int_const(cid as i64);
        let s = self.int_const(slot as i64);
        let raw = self.extern_call_t1(
            "pickle_static_get",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![c, s],
        )?;
        match rep {
            ElemRep::Scalar(_, unbox, ir) => {
                self.extern_call_t1(unbox, vec![IrTy::Ptr], ir, vec![raw])
            }
            ElemRep::Ptr => Ok(raw),
        }
    }

    /// Static-field store: box scalars, then `pickle_static_set(cid, slot, v)`.
    fn static_store(&mut self, cid: u32, slot: usize, boxed: Temp) {
        let c = self.int_const(cid as i64);
        let s = self.int_const(slot as i64);
        self.extern_call_void(
            "pickle_static_set",
            vec![IrTy::Int, IrTy::Int, IrTy::Ptr],
            vec![c, s, boxed],
        );
    }

    /// Plain or compound (`op=`) store of one static field cell.
    #[allow(clippy::too_many_arguments)]
    fn static_assign(
        &mut self,
        span: Span,
        op: AssignOp,
        cid: u32,
        slot: usize,
        field_ty: &Ty,
        v: Temp,
        vt: &Ty,
    ) -> Result<Temp, ()> {
        let rep = self.elem_rep(field_ty, span)?;
        if op == AssignOp::Assign {
            let packed = self.pack_for_pointer_boundary(&rep, v, vt, span)?;
            let boxed = self.box_for_store(&rep, packed, elem_ir(field_ty))?;
            self.static_store(cid, slot, boxed);
            return Ok(v);
        }
        if matches!(rep, ElemRep::Ptr) {
            if field_ty != &Ty::String {
                return self.bad(
                    span,
                    "compound assignment to a non-scalar static field is not lowered yet",
                );
            }
            // String static field: read the current string, concatenate, store back.
            let c = self.int_const(cid as i64);
            let s = self.int_const(slot as i64);
            let cur = self.extern_call_t1(
                "pickle_static_get",
                vec![IrTy::Int, IrTy::Int],
                IrTy::Ptr,
                vec![c, s],
            )?;
            let dst = self.combine_compound(span, op, field_ty, cur, v, "static field")?;
            let boxed = self.box_for_store(&rep, dst, elem_ir(field_ty))?;
            self.static_store(cid, slot, boxed);
            return Ok(dst);
        }
        let c = self.int_const(cid as i64);
        let s = self.int_const(slot as i64);
        let raw = self.extern_call_t1(
            "pickle_static_get",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![c, s],
        )?;
        let cur = match rep {
            ElemRep::Scalar(_, unbox, ir) => {
                self.extern_call_t1(unbox, vec![IrTy::Ptr], ir, vec![raw])?
            }
            ElemRep::Ptr => raw,
        };
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: assign_opcode(op),
            a: cur,
            b: v,
        });
        let boxed = self.box_for_store(&rep, dst, elem_ir(field_ty))?;
        self.static_store(cid, slot, boxed);
        Ok(dst)
    }

    /// Store the default (zero scalar / null pointer) into one static cell.
    fn build_static_default(&mut self, cid: u32, slot: usize) -> Result<(), ()> {
        let Some(info) = self.static_field_at(cid as i64, slot) else {
            return Ok(());
        };
        let field_ty = info.ty.clone();
        let rep = self.elem_rep(&field_ty, info.span)?;
        let default = match rep {
            ElemRep::Scalar(..) => self.zero_scalar(elem_ir(&field_ty), info.span)?,
            ElemRep::Ptr => self.null_temp()?,
        };
        let boxed = self.box_for_store(&rep, default, elem_ir(&field_ty))?;
        self.static_store(cid, slot, boxed);
        Ok(())
    }

    /// Evaluate one static-field initializer and store it.
    /// Body of the synthesized `pkl_globals_init`: for each managed top-level
    /// `var`, register the slot as a GC root and then store its initializer.
    ///
    /// All registrations are emitted before any initializer is evaluated. An
    /// initializer is the first code that can allocate (building a string), and
    /// a collection during that allocation must not be able to reach a managed
    /// global that has been written but not yet rooted.
    fn build_globals_init(&mut self, inits: &[(usize, Expr)]) {
let ex = self.module.extern_id(IrExtern {
            // `pickle_gc_root_add(cell: *mut *mut PickleObject) -> *mut RootCell`.
            // Both are raw machine addresses, not managed references, so they
            // cross as `Int`; declaring them `Ptr` would make the JIT treat the
            // slot address as a heap object it must root.
            symbol: "pickle_gc_root_add".to_string(),
            params: vec![IrTy::Int],
            ret: IrTy::Int,
        });
for (id, _) in inits {
            let addr = self.temp();
            self.instr(IrInstr::GlobalAddr { dst: addr, id: *id });
            self.instr(IrInstr::Call {
                dst: None,
                callee: Callee::Extern(ex),
                args: vec![addr],
            });
        }
        for (id, x) in inits {
            let v = match self.expr(x) {
                Ok(v) => v,
                Err(()) => return,
            };
            self.instr(IrInstr::StoreGlobal {
                id: *id,
                v,
                ty: IrTy::Str,
            });
        }
    }

    fn build_static_init_one(&mut self, cid: u32, slot: usize, x: &Expr) -> Result<(), ()> {
        let Some(info) = self.static_field_at(cid as i64, slot) else {
            return Ok(());
        };
        let field_ty = info.ty.clone();
        let v = self.expr(x)?;
        let vt = self.ty_of(&x.span).unwrap_or(Ty::Unknown);
        self.static_assign(x.span, AssignOp::Assign, cid, slot, &field_ty, v, &vt)?;
        Ok(())
    }

    /// `xs[i] = v` and `xs[i] op= v` for `List<T>` targets.
    fn index_assign(
        &mut self,
        e: &Expr,
        op: AssignOp,
        object: &Expr,
        index: &Expr,
        value: &Expr,
    ) -> Result<Temp, ()> {
        let ot = self.ty_of(&object.span);
        if let Some(Ty::Map(k, v)) = &ot {
            return self.map_index_assign(
                op,
                object,
                index,
                value,
                k.as_ref().clone(),
                v.as_ref().clone(),
            );
        }
        if let Some(Ty::Ptr(inner)) = &ot {
            let Some(ty) = Self::scalar_ir(inner) else {
                return self.bad(
                    e.span,
                    "storing through a pointer to a managed value is not lowered yet",
                );
            };
            let base = self.expr(object)?;
            let idx = self.expr(index)?;
            let addr = self.ptr_element_addr(base, idx, ty, e.span)?;
            if op == AssignOp::Assign {
                let v = self.expr(value)?;
                self.instr(IrInstr::StoreRaw { addr, v, ty });
                return Ok(v);
            }
            let cur = self.temp();
            self.instr(IrInstr::LoadRaw { dst: cur, addr, ty });
            let v = self.expr(value)?;
            let dst = self.temp();
            self.instr(IrInstr::BinOp {
                dst,
                op: assign_opcode(op),
                a: cur,
                b: v,
            });
            self.instr(IrInstr::StoreRaw { addr, v: dst, ty });
            return Ok(dst);
        }
        if let Some(Ty::Ref(_)) = &ot {
            return self.bad(e.span, "cannot write through an immutable reference (`&T`)");
        }
        if matches!(ot, Some(Ty::String)) {
            return self.str_index_assign(e, op, object, index, value);
        }
        if !matches!(ot, Some(Ty::List(_))) {
            // Maps and strings are handled above; other types are typed but
            // unlowered.
            return self.bad(e.span, "index assignment over this type is not lowered yet");
        }
        let elem = match &ot {
            Some(Ty::List(inner)) => inner.as_ref().clone(),
            _ => unreachable!(),
        };
        let rep = self.elem_rep(&elem, e.span)?;
        let obj = self.expr(object)?;
        let idx = self.expr(index)?;
        if op == AssignOp::Assign {
            let v = self.expr(value)?;
            self.list_store(obj, idx, &rep, &elem, v)?;
            return Ok(v);
        }
        if matches!(rep, ElemRep::Ptr) {
            if elem != Ty::String {
                return self.bad(
                    e.span,
                    "compound assignment to a non-scalar list element is not lowered yet",
                );
            }
            // String element: read the current string, concatenate, store back.
            let cur = self.extern_call_t1(
                "pickle_list_get",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![obj, idx],
            )?;
            let v = self.expr(value)?;
            let dst = self.combine_compound(e.span, op, &elem, cur, v, "list element")?;
            self.list_store(obj, idx, &rep, &elem, dst)?;
            return Ok(dst);
        }
        let cur = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![obj, idx],
        )?;
        let cur_v = match rep {
            ElemRep::Scalar(_, unbox_sym, ir) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![cur])?
            }
            ElemRep::Ptr => cur,
        };
        let v = self.expr(value)?;
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: assign_opcode(op),
            a: cur_v,
            b: v,
        });
        self.list_store(obj, idx, &rep, &elem, dst)?;
        Ok(dst)
    }

    /// `s[i] = v` / `s[i] op= v` over a string: strings are immutable, so the
    /// current byte is combined in place, a fresh copy with byte `i` replaced
    /// is computed (`pickle_str_set`, copy-on-write), and the new string is
    /// stored back into the *target* the string came from. Supported targets
    /// are names (a local, or a bare instance/static field inside a method)
    /// and `obj.field` members; nested targets (`m[k][j]`, `xs[i][j]`) are not
    /// lowered yet because the copy would not propagate through the container
    /// on the opaque-string ABI. Writes through `&T` references are rejected
    /// up front by `index_assign`.
    fn str_index_assign(
        &mut self,
        e: &Expr,
        op: AssignOp,
        object: &Expr,
        index: &Expr,
        value: &Expr,
    ) -> Result<Temp, ()> {
        match &object.kind {
            ExprKind::Ident(name) => {
                let s = self.expr(object)?;
                let i = self.expr(index)?;
                let v = self.byte_rhs(value)?;
                let byte = self.str_index_byte(op, s, i, v)?;
                let ns = self.extern_call_t1(
                    "pickle_str_set",
                    vec![IrTy::Ptr, IrTy::Int, IrTy::Int],
                    IrTy::Str,
                    vec![s, i, byte],
                )?;
                if let Some(slot) = self.lookup(name) {
                    self.instr(IrInstr::StoreSlot { slot, v: ns });
                    return Ok(ns);
                }
                // Inside an instance method a bare field name assigns
                // `this.field`.
                if let Some(cid) = self.owner {
                    if let Some(idx) = self.instance_field_index(cid, name) {
                        let field_ty = self.field_at(cid, idx).ty.clone();
                        let owned = self.field_at(cid, idx).manual;
                        let this = self.this_value(e)?;
                        let vt = self.ty_of(&object.span).unwrap_or(Ty::String);
                        return self.field_assign(
                            e.span,
                            AssignOp::Assign,
                            this,
                            &field_ty,
                            idx,
                            ns,
                            &vt,
                            owned,
                        );
                    }
                    // A bare name may also be a static field of the enclosing
                    // class.
                    if let Some((dcid, slot, info)) = self.static_field(cid, name) {
                        if !info.mutable {
                            return self.bad(
                                e.span,
                                format!("cannot assign to immutable static field `{name}`"),
                            );
                        }
                        let vt = self.ty_of(&object.span).unwrap_or(Ty::String);
                        return self.static_assign(
                            e.span,
                            AssignOp::Assign,
                            dcid,
                            slot,
                            &info.ty,
                            ns,
                            &vt,
                        );
                    }
                }
                self.bad(e.span, format!("cannot assign to `{name}`"))
            }
            ExprKind::Member { object: mobj, name } => {
                let ot = self.ty_of(&mobj.span);
                let ot = match ot {
                    Some(Ty::Ptr(inner)) => Some((*inner).clone()),
                    other => other,
                };
                let cid = match ot {
                    Some(otv @ (Ty::Class(..) | Ty::Struct(..))) => {
                        self.class_id_of(&otv, e.span)?
                    }
                    _ => None,
                };
                let Some(cid) = cid else {
                    return self.bad(
                        e.span,
                        "string index assignment over this member type is not lowered yet",
                    );
                };
                let Some(idx) = self.instance_field_index(cid as i64, name) else {
                    return self.bad(
                        e.span,
                        format!(
                            "`{name}` is not a field of this `{}`",
                            self.class_name_of(cid)
                        ),
                    );
                };
                let field_ty = self.field_at(cid as i64, idx).ty.clone();
                let owned = self.field_at(cid as i64, idx).manual;
                let mo = self.expr(mobj)?;
                let idxc = self.int_const(idx as i64);
                let raw = self.extern_call_t1(
                    "pickle_obj_slot_get",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![mo, idxc],
                )?;
                let i = self.expr(index)?;
                let v = self.byte_rhs(value)?;
                let byte = self.str_index_byte(op, raw, i, v)?;
                let ns = self.extern_call_t1(
                    "pickle_str_set",
                    vec![IrTy::Ptr, IrTy::Int, IrTy::Int],
                    IrTy::Str,
                    vec![raw, i, byte],
                )?;
                let vt = self.ty_of(&object.span).unwrap_or(Ty::String);
                self.field_assign(e.span, AssignOp::Assign, mo, &field_ty, idx, ns, &vt, owned)
            }
            _ => self.bad(
                e.span,
                "string index assignment over a non-variable target is not lowered yet (strings are copy-on-write)",
            ),
        }
    }

    /// The byte (int ABI) to write into the string: an ASCII `char` literal
    /// lowers directly to its 0..=255 value (the checker admits it into a
    /// `byte` slot), anything else lowers as its own value expression.
    fn byte_rhs(&mut self, value: &Expr) -> Result<Temp, ()> {
        if let ExprKind::Lit(Lit::Char(c)) = &value.kind {
            if *c as u32 <= 0x7F {
                return Ok(self.int_const(*c as u8 as i64));
            }
        }
        self.expr(value)
    }

    /// The byte to write back for `s[i] op= v`: `v` itself for a plain store,
    /// or the current byte combined with `v` for a compound operator.
    fn str_index_byte(&mut self, op: AssignOp, s: Temp, i: Temp, v: Temp) -> Result<Temp, ()> {
        if op == AssignOp::Assign {
            return Ok(v);
        }
        let cur = self.extern_call_t1(
            "pickle_str_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Int,
            vec![s, i],
        )?;
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: assign_opcode(op),
            a: cur,
            b: v,
        });
        Ok(dst)
    }

    /// Byte address of `base[i]` for a scalar pointee (`base + i * stride`).
    /// Strides match the runtime block layout: 8 for int/float, 4 for char,
    /// 1 for bool.
    fn ptr_element_addr(
        &mut self,
        base: Temp,
        idx: Temp,
        elem: IrTy,
        span: Span,
    ) -> Result<Temp, ()> {
        let stride = match elem {
            IrTy::Bool => 1,
            IrTy::Char => 4,
            IrTy::Int | IrTy::Float => 8,
            _ => return self.bad(span, "this pointee type is not indexed yet"),
        };
        let s = self.int_const(stride);
        let off = self.temp();
        self.instr(IrInstr::BinOp {
            dst: off,
            op: IrBinOp::Mul,
            a: idx,
            b: s,
        });
        let addr = self.temp();
        self.instr(IrInstr::BinOp {
            dst: addr,
            op: IrBinOp::Add,
            a: base,
            b: off,
        });
        Ok(addr)
    }

    /// `pickle_list_set(obj, idx, boxed)`: boxes a scalar element first.
    fn list_store(
        &mut self,
        obj: Temp,
        idx: Temp,
        rep: &ElemRep,
        elem: &Ty,
        v: Temp,
    ) -> Result<(), ()> {
        let boxed = match rep {
            ElemRep::Scalar(box_sym, _, _) => {
                self.extern_call_t1(box_sym, vec![elem_ir(elem)], IrTy::Ptr, vec![v])?
            }
            ElemRep::Ptr => v,
        };
        self.extern_call_void(
            "pickle_list_set",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
            vec![obj, idx, boxed],
        );
        Ok(())
    }

    /// `xs[i]` element read for a `List<T>` or `Map<string, V>` (and a clear
    /// error for strings). Indexing through an immutable `&T` borrow reads
    /// through the referent.
    fn index_read(&mut self, e: &Expr, object: &Expr, index: &Expr) -> Result<Temp, ()> {
        if let Some(Ty::Map(k, v)) = self.ty_of(&object.span) {
            return self.map_index_read(e, object, index, k.as_ref().clone(), v.as_ref().clone());
        }
        if let Some(Ty::Ref(inner)) = self.ty_of(&object.span) {
            if let Ty::Map(k, v) = inner.as_ref() {
                return self.map_index_read(
                    e,
                    object,
                    index,
                    k.as_ref().clone(),
                    v.as_ref().clone(),
                );
            }
        }
        let ot = self.ty_of(&object.span);
        let elem = match ot {
            Some(Ty::Ptr(inner)) if Self::scalar_ir(inner.as_ref()).is_some() => {
                let ty = Self::scalar_ir(inner.as_ref()).unwrap();
                let base = self.expr(object)?;
                let idx = self.expr(index)?;
                let addr = self.ptr_element_addr(base, idx, ty, e.span)?;
                let dst = self.temp();
                self.instr(IrInstr::LoadRaw { dst, addr, ty });
                return Ok(dst);
            }
            Some(Ty::Ref(inner)) if Self::scalar_ir(inner.as_ref()).is_some() => {
                let ty = Self::scalar_ir(inner.as_ref()).unwrap();
                let base = self.expr(object)?;
                let idx = self.expr(index)?;
                let addr = self.ptr_element_addr(base, idx, ty, e.span)?;
                let dst = self.temp();
                self.instr(IrInstr::LoadRaw { dst, addr, ty });
                return Ok(dst);
            }
            Some(Ty::List(inner)) => inner.as_ref().clone(),
            Some(Ty::Range(inner)) => inner.as_ref().clone(),
            Some(Ty::Ref(inner)) if matches!(inner.as_ref(), Ty::List(..)) => {
                match inner.as_ref() {
                    Ty::List(e) => e.as_ref().clone(),
                    _ => unreachable!(),
                }
            }
            Some(Ty::String) => {
                let obj = self.expr(object)?;
                let idx = self.expr(index)?;
                return self.extern_call_t1(
                    "pickle_str_get",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Int,
                    vec![obj, idx],
                );
            }
            Some(Ty::Ref(inner)) if matches!(inner.as_ref(), Ty::String) => {
                let obj = self.expr(object)?;
                let idx = self.expr(index)?;
                return self.extern_call_t1(
                    "pickle_str_get",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Int,
                    vec![obj, idx],
                );
            }
            _ => return self.bad(e.span, "indexing this type is not lowered yet"),
        };
        let rep = self.elem_rep(&elem, e.span)?;
        let obj = self.expr(object)?;
        let idx = self.expr(index)?;
        let raw = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![obj, idx],
        )?;
        match rep {
            ElemRep::Scalar(_, unbox_sym, ir) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![raw])
            }
            ElemRep::Ptr => Ok(raw),
        }
    }

    /// `m[k]` read for a `Map<string, V>` / scalar-keyed map. Absent keys
    /// yield the value type's default (`0`/`0.0`/`false`, the empty string, or
    /// null) via the runtime's boxed get, so a scalar is never unboxed from a
    /// null pointer. Scalar keys are boxed before the lookup.
    fn map_index_read(
        &mut self,
        e: &Expr,
        object: &Expr,
        index: &Expr,
        kty: Ty,
        vty: Ty,
    ) -> Result<Temp, ()> {
        let krep = self.key_rep(&kty, index.span)?;
        let vrep = self.elem_rep(&vty, e.span)?;
        let obj = self.expr(object)?;
        let k = self.expr(index)?;
        let k_ty = self.ty_of(&index.span).unwrap_or(kty.clone());
        let key = self.pack_map_key(&krep, k, &k_ty, index.span)?;
        let default: Temp = match vrep {
            ElemRep::Scalar(box_sym, _, ir) => {
                let zero = self.zero_scalar(ir, index.span)?;
                self.extern_call_t1(box_sym, vec![ir], IrTy::Ptr, vec![zero])?
            }
            ElemRep::Ptr => {
                if vty == Ty::String {
                    self.string_literal(&[])?
                } else {
                    let nul = self.null_temp()?;
                    self.extern_call_t1(
                        "pickle_map_get_boxed",
                        vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                        IrTy::Ptr,
                        vec![obj, key, nul],
                    )?
                }
            }
        };
        let raw = self.extern_call_t1(
            "pickle_map_get_boxed",
            vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
            IrTy::Ptr,
            vec![obj, key, default],
        )?;
        match vrep {
            ElemRep::Scalar(_, unbox_sym, ir) => {
                self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![raw])
            }
            ElemRep::Ptr => Ok(raw),
        }
    }

    /// `m[k] = v` and `m[k] op= v` for `Map<K, V>` targets (string or scalar
    /// keys; scalar keys are boxed before the runtime call).
    fn map_index_assign(
        &mut self,
        op: AssignOp,
        object: &Expr,
        index: &Expr,
        value: &Expr,
        kty: Ty,
        vty: Ty,
    ) -> Result<Temp, ()> {
        let krep = self.key_rep(&kty, index.span)?;
        let vrep = self.elem_rep(&vty, object.span)?;
        let obj = self.expr(object)?;
        let k = self.expr(index)?;
        let k_ty = self.ty_of(&index.span).unwrap_or(kty.clone());
        let key = self.pack_map_key(&krep, k, &k_ty, index.span)?;
        if op == AssignOp::Assign {
            let v = self.expr(value)?;
            let v_ty = self.irty(value.span)?;
            let boxed = self.box_for_store(&vrep, v, v_ty)?;
            self.extern_call_void(
                "pickle_map_set",
                vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                vec![obj, key, boxed],
            );
            return Ok(v);
        }
        let (box_sym, unbox_sym, ir) = match vrep {
            ElemRep::Scalar(box_sym, unbox_sym, ir) => (box_sym, unbox_sym, ir),
            ElemRep::Ptr => {
                if vty != Ty::String {
                    return self.bad(
                        object.span,
                        "compound assignment to a non-scalar map value is not lowered yet",
                    );
                }
                // String value: an absent key reads as `""`, then the current
                // string is concatenated with the RHS and stored back.
                let default = self.string_literal(&[])?;
                let cur = self.extern_call_t1(
                    "pickle_map_get_boxed",
                    vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                    IrTy::Ptr,
                    vec![obj, key, default],
                )?;
                let v = self.expr(value)?;
                let dst = self.combine_compound(object.span, op, &vty, cur, v, "map value")?;
                self.extern_call_void(
                    "pickle_map_set",
                    vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                    vec![obj, key, dst],
                );
                return Ok(dst);
            }
        };
        let zero = self.zero_scalar(ir, object.span)?;
        let default = self.extern_call_t1(box_sym, vec![ir], IrTy::Ptr, vec![zero])?;
        let cur_ptr = self.extern_call_t1(
            "pickle_map_get_boxed",
            vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
            IrTy::Ptr,
            vec![obj, key, default],
        )?;
        let cur = self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![cur_ptr])?;
        let v = self.expr(value)?;
        let dst = self.temp();
        self.instr(IrInstr::BinOp {
            dst,
            op: assign_opcode(op),
            a: cur,
            b: v,
        });
        let boxed = self.extern_call_t1(box_sym, vec![ir], IrTy::Ptr, vec![dst])?;
        self.extern_call_void(
            "pickle_map_set",
            vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
            vec![obj, key, boxed],
        );
        Ok(dst)
    }

    /// `(a, b, c)` tuple literal: build a `PTuple` by writing each element
    /// into its slot, boxing scalar elements along the way (matching list
    /// element rules). The tuple's arity is static, so elements are written by
    /// constant index into a freshly allocated object of the right width.
    fn tuple_literal(&mut self, e: &Expr, items: &[Expr]) -> Result<Temp, ()> {
        let tys = match self.ty_of(&e.span) {
            Some(Ty::Tuple(tys)) => tys,
            _ => return self.bad(e.span, "tuple literal does not have a `Tuple` type"),
        };
        if items.is_empty() {
            return self.bad(e.span, "empty tuple values are not lowered yet");
        }
        if tys.len() != items.len() {
            return self.bad(e.span, "tuple literal element/type count mismatch");
        }
        let n = self.temp();
        self.instr(IrInstr::Const {
            dst: n,
            c: IrConst::Int(items.len() as i64),
        });
        let tv = self.extern_call_t1("pickle_tuple_new", vec![IrTy::Int], IrTy::Ptr, vec![n])?;
        for (i, it) in items.iter().enumerate() {
            let v = self.expr(it)?;
            let rep = self.elem_rep(&tys[i], e.span)?;
            let store_v = match rep {
                ElemRep::Scalar(box_sym, _, _) => {
                    let v_ty = self.irty(it.span)?;
                    self.extern_call_t1(box_sym, vec![v_ty], IrTy::Ptr, vec![v])?
                }
                ElemRep::Ptr => v,
            };
            let idx = self.temp();
            self.instr(IrInstr::Const {
                dst: idx,
                c: IrConst::Int(i as i64),
            });
            self.extern_call_void(
                "pickle_tuple_set_field",
                vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
                vec![tv, idx, store_v],
            );
        }
        Ok(tv)
    }

    /// `[a, b, c]` array literal: build a `List` by pushing each element,
    /// boxing scalar elements along the way.
    fn array_literal(&mut self, e: &Expr, items: &[Expr]) -> Result<Temp, ()> {
        let elem = match self.ty_of(&e.span) {
            Some(Ty::List(inner)) => inner.as_ref().clone(),
            _ => return self.bad(e.span, "array literal does not have a `List` type"),
        };
        if items.is_empty() {
            return self.bad(
                e.span,
                "empty array literal cannot be typed; add an element to infer `List<T>`",
            );
        }
        let rep = self.elem_rep(&elem, e.span)?;
        let cap = self.temp();
        self.instr(IrInstr::Const {
            dst: cap,
            c: IrConst::Int(items.len() as i64),
        });
        let list = self.extern_call_t1("pickle_list_new", vec![IrTy::Int], IrTy::Ptr, vec![cap])?;
        for it in items {
            let v = self.expr(it)?;
            let push_v = match rep {
                ElemRep::Scalar(box_sym, _, _) => {
                    let v_ty = self.irty(it.span)?;
                    self.extern_call_t1(box_sym, vec![v_ty], IrTy::Ptr, vec![v])?
                }
                ElemRep::Ptr => v,
            };
            self.extern_call_void(
                "pickle_list_push",
                vec![IrTy::Ptr, IrTy::Ptr],
                vec![list, push_v],
            );
        }
        Ok(list)
    }

    /// `{ "k": v, ... }` map literal: build a `Map` by inserting each entry,
    /// boxing scalar values (and scalar keys) along the way.
    fn map_literal(&mut self, e: &Expr, pairs: &[(Expr, Expr)]) -> Result<Temp, ()> {
        let (kty, vty) = match self.ty_of(&e.span) {
            Some(Ty::Map(k, v)) => (k.as_ref().clone(), v.as_ref().clone()),
            _ => return self.bad(e.span, "map literal does not have a `Map` type"),
        };
        let krep = self.key_rep(&kty, e.span)?;
        let vrep = self.elem_rep(&vty, e.span)?;
        let cap = self.temp();
        self.instr(IrInstr::Const {
            dst: cap,
            c: IrConst::Int(0),
        });
        let map = self.extern_call_t1("pickle_map_new", vec![IrTy::Int], IrTy::Ptr, vec![cap])?;
        for (k, v) in pairs {
            let kt = self.expr(k)?;
            let k_ty = self.ty_of(&k.span).unwrap_or(Ty::Unknown);
            let key = self.pack_map_key(&krep, kt, &k_ty, k.span)?;
            let vt = self.expr(v)?;
            let boxed = match vrep {
                ElemRep::Scalar(box_sym, _, _) => {
                    let v_ty = self.irty(v.span)?;
                    self.extern_call_t1(box_sym, vec![v_ty], IrTy::Ptr, vec![vt])?
                }
                ElemRep::Ptr => vt,
            };
            self.extern_call_void(
                "pickle_map_set",
                vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                vec![map, key, boxed],
            );
        }
        Ok(map)
    }

    /// Box a scalar so it can be stored in a List/Map, or pass a pointer type
    /// through untouched.
    fn box_for_store(&mut self, rep: &ElemRep, v: Temp, v_ty: IrTy) -> Result<Temp, ()> {
        match rep {
            ElemRep::Scalar(box_sym, _, _) => {
                self.extern_call_t1(box_sym, vec![v_ty], IrTy::Ptr, vec![v])
            }
            ElemRep::Ptr => Ok(v),
        }
    }

    /// The `key` operand for a map op. Scalar keys are boxed; `Ptr` keys
    /// (option/tuple/composite/string) pass through as the managed pointer,
    /// except a raw scalar operand that must be lifted to its canonical
    /// `Option<T>` pointer form first (e.g. `m[5]` on a `Map<int?, V>`).
    fn pack_map_key(&mut self, rep: &ElemRep, v: Temp, vt: &Ty, span: Span) -> Result<Temp, ()> {
        match rep {
            ElemRep::Scalar(box_sym, _, _) => {
                let v_ir = self.map_ty(vt, span)?;
                self.extern_call_t1(box_sym, vec![v_ir], IrTy::Ptr, vec![v])
            }
            ElemRep::Ptr => self.option_wrap(v, vt, span),
        }
    }

    /// Lift a scalar value to the managed-pointer representation used for
    /// `Option<T>` (and for any pointer-typed boundary). A `none` or an
    /// already-managed value passes through untouched; scalars are boxed.
    fn option_wrap(&mut self, v: Temp, ty: &Ty, span: Span) -> Result<Temp, ()> {
        match ty {
            Ty::Int => self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![v]),
            Ty::Float => {
                self.extern_call_t1("pickle_box_f64", vec![IrTy::Float], IrTy::Ptr, vec![v])
            }
            Ty::Bool => {
                self.extern_call_t1("pickle_box_bool", vec![IrTy::Bool], IrTy::Ptr, vec![v])
            }
            Ty::Byte => self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![v]),
            Ty::Char => {
                self.extern_call_t1("pickle_box_char", vec![IrTy::Char], IrTy::Ptr, vec![v])
            }
            _ => {
                let _ = span;
                Ok(v)
            }
        }
    }

    /// Mirror of `box_for_store` for boundaries that are not list/map element
    /// slots: when the destination representation is `Ptr`, a scalar source
    /// (e.g. `let m: int? = 5`) must be lifted via `option_wrap` first.
    fn pack_for_pointer_boundary(
        &mut self,
        rep: &ElemRep,
        v: Temp,
        vt: &Ty,
        span: Span,
    ) -> Result<Temp, ()> {
        if matches!(rep, ElemRep::Ptr) {
            self.option_wrap(v, vt, span)
        } else {
            Ok(v)
        }
    }

    /// The zero value of a scalar type, as an IR temp.
    fn zero_scalar(&mut self, ir: IrTy, span: Span) -> Result<Temp, ()> {
        let dst = self.temp();
        let c = match ir {
            IrTy::Int => IrConst::Int(0),
            IrTy::Float => IrConst::Float(0.0f64.to_bits()),
            IrTy::Bool => IrConst::Bool(false),
            IrTy::Char => IrConst::Char(0),
            other => return self.bad(span, format!("no zero value for `{other:?}`")),
        };
        self.instr(IrInstr::Const { dst, c });
        Ok(dst)
    }

    /// A null pointer constant (`IrConst::Null`).
    fn null_temp(&mut self) -> Result<Temp, ()> {
        let dst = self.temp();
        self.instr(IrInstr::Const {
            dst,
            c: IrConst::Null,
        });
        Ok(dst)
    }

    /// Lower `match (scrutinee) { case p -> body ... }` into a chain of
    /// `tag == variant` checks. The scrutinee is kept in a managed slot across
    /// the chain; payload bindings extract and unbox each field like list
    /// elements. A chain that reaches its end without a catch-all arm calls
    /// `pickle_panic_no_match`.
    fn match_expr(&mut self, e: &Expr, scrutinee: &Expr, arms: &[MatchArm]) -> Result<Temp, ()> {
        let st = self.ty_of(&scrutinee.span);
        let Some(st) = st else {
            return self.bad(e.span, "matching over non-enum values is not lowered yet");
        };
        if !matches!(st, Ty::Enum(..)) {
            return self.lower_generic_match(e, scrutinee, arms, &st);
        }
        let Ty::Enum(en_name, _) = &st else {
            unreachable!()
        };
        let enum_name = en_name.clone();
        let table = match self.resolved.types.get(&enum_name) {
            Some(TypeTableEntry::Enum(t)) => t.clone(),
            _ => return self.bad(e.span, format!("unknown enum type `{enum_name}`")),
        };

        // The scrutinee is a managed pointer; keep it in a slot so the
        // collector retains it across any allocation the arms perform.
        let s = self.expr(scrutinee)?;
        let s_slot = self.new_slot(IrTy::Ptr);
        self.instr(IrInstr::StoreSlot { slot: s_slot, v: s });

        let res_ty = self.irty(e.span).ok();
        let res_slot = if !matches!(res_ty, Some(IrTy::Unit) | None) {
            Some(self.new_slot(res_ty.unwrap()))
        } else {
            None
        };

        let sv = self.load(s_slot);
        let tag = self.extern_call_t1("pickle_enum_tag", vec![IrTy::Ptr], IrTy::Int, vec![sv])?;
        let join = self.new_block();

        let mut cur = self.cur;
        let mut chain_live = true;
        for arm in arms {
            let body = self.new_block();
            let guard = arm.guard.as_ref();
            // With a guard, bindings and the guard condition are evaluated in
            // a `guard_in` block that the tag check branches into; on a false
            // guard the chain falls through to the next arm (or the
            // non-exhaustive panic when nothing else remains). A nested payload
            // pattern (e.g. `ExprKind.Lit(Lit.String(parts))`) also contributes
            // match conditions folded after the tag check, so it needs its own
            // `guard_in` block too: otherwise `guard_in` aliases `body` and the
            // arm's `BranchIf` term is overwritten when the body re-terminates
            // the shared block, letting the body run for every matching outer
            // variant.
            let has_payload_cond = match &arm.pattern {
                Pattern::Variant { payloads, .. } => payloads
                    .iter()
                    .any(|p| !matches!(p, Pattern::Wildcard | Pattern::Binding { .. })),
                _ => false,
            };
            let guard_in = if guard.is_some() || has_payload_cond {
                self.new_block()
            } else {
                body
            };
            match &arm.pattern {
                Pattern::Variant { path, payloads } => {
                    let vname = match path.len() {
                        2 => &path[1],
                        1 => &path[0],
                        _ => return self.bad(arm.span, "invalid enum variant pattern"),
                    };
                    let Some(vi) = table.variants.iter().position(|(n, ..)| n == vname) else {
                        return self.bad(
                            arm.span,
                            format!("enum `{enum_name}` has no variant `{vname}`"),
                        );
                    };
                    if chain_live {
                        self.cur = cur;
                        let vi_t = self.temp();
                        self.instr(IrInstr::Const {
                            dst: vi_t,
                            c: IrConst::Int(vi as i64),
                        });
                        let c = self.temp();
                        self.instr(IrInstr::BinOp {
                            dst: c,
                            op: IrBinOp::Eq,
                            a: tag,
                            b: vi_t,
                        });
                        let next = self.new_block();
                        self.term(IrTerm::BranchIf {
                            cond: c,
                            then: guard_in,
                            else_: next,
                        });
                        cur = next;
                    }
                    self.cur = guard_in;
                    self.push_scope();
                    let payload_cond =
                        self.bind_enum_payloads(arm.span, &table, vi, payloads, s_slot)?;
                    let mut cf = payload_cond;
                    if let Some(g) = guard {
                        let cond = self.expr(g)?;
                        cf = match cf {
                            None => Some(cond),
                            Some(prev) => Some(self.combine_cond(true, prev, cond)),
                        };
                    }
                    if let Some(cond) = cf {
                        let fall = if chain_live { cur } else { join };
                        self.term(IrTerm::BranchIf {
                            cond,
                            then: body,
                            else_: fall,
                        });
                        self.cur = body;
                    }
                    self.match_arm_body(&arm.body, res_slot)?;
                    self.pop_scope();
                    self.term(IrTerm::Branch { target: join });
                }
                Pattern::Wildcard | Pattern::Binding { .. } => {
                    if let Some(g) = guard {
                        if chain_live {
                            // A failing guard must keep checking the remaining
                            // arms, so the chain stays live through a fresh
                            // fall-through block.
                            self.cur = cur;
                            self.term(IrTerm::Branch { target: guard_in });
                            let fall = self.new_block();
                            cur = fall;
                            self.cur = guard_in;
                            self.push_scope();
                            if let Pattern::Binding { name, .. } = &arm.pattern {
                                let slot = self.new_slot(IrTy::Ptr);
                                let sv = self.load(s_slot);
                                self.instr(IrInstr::StoreSlot { slot, v: sv });
                                self.declare(name, slot);
                            }
                            let cond = self.expr(g)?;
                            self.term(IrTerm::BranchIf {
                                cond,
                                then: body,
                                else_: fall,
                            });
                            self.cur = body;
                            self.match_arm_body(&arm.body, res_slot)?;
                            self.pop_scope();
                            self.term(IrTerm::Branch { target: join });
                        } else {
                            // Dead guarded catch-all after an unguarded one;
                            // still emit a terminated guard+body for the graph.
                            self.cur = guard_in;
                            self.push_scope();
                            if let Pattern::Binding { name, .. } = &arm.pattern {
                                let slot = self.new_slot(IrTy::Ptr);
                                let sv = self.load(s_slot);
                                self.instr(IrInstr::StoreSlot { slot, v: sv });
                                self.declare(name, slot);
                            }
                            let cond = self.expr(g)?;
                            self.term(IrTerm::BranchIf {
                                cond,
                                then: body,
                                else_: join,
                            });
                            self.cur = body;
                            self.match_arm_body(&arm.body, res_slot)?;
                            self.pop_scope();
                            self.term(IrTerm::Branch { target: join });
                        }
                        continue;
                    }
                    if chain_live {
                        self.cur = cur;
                        self.term(IrTerm::Branch { target: body });
                        chain_live = false;
                    }
                    self.cur = body;
                    self.push_scope();
                    if let Pattern::Binding { name, .. } = &arm.pattern {
                        let slot = self.new_slot(IrTy::Ptr);
                        let sv = self.load(s_slot);
                        self.instr(IrInstr::StoreSlot { slot, v: sv });
                        self.declare(name, slot);
                    }
                    self.match_arm_body(&arm.body, res_slot)?;
                    self.pop_scope();
                    self.term(IrTerm::Branch { target: join });
                }
                Pattern::Or(alts) => {
                    // `Tag(a) | Tag(b)` style alternatives: every alternative
                    // must be a variant of this enum carrying only `_`
                    // payloads (the checker rejects name-binding alternatives),
                    // so entry is decided by one OR-folded tag equality and
                    // nothing is bound.
                    if chain_live {
                        // The fold's `combine_cond` terminates the chain entry
                        // block with its own branch, so `self.cur` must be the
                        // chain block when the fold starts.
                        self.cur = cur;
                    }
                    let mut tag_test: Option<Temp> = None;
                    for alt in alts {
                        let Pattern::Variant { path, payloads } = alt else {
                            return self.bad(
                                arm.span,
                                "enum or-pattern alternatives must be variant patterns",
                            );
                        };
                        for pl in payloads {
                            if !matches!(pl, Pattern::Wildcard) {
                                return self.bad(
                                    arm.span,
                                    "enum or-pattern alternatives may only use `_` payloads",
                                );
                            }
                        }
                        let vname = match path.len() {
                            2 => &path[1],
                            1 => &path[0],
                            _ => return self.bad(arm.span, "invalid enum variant pattern"),
                        };
                        let Some(vi) = table.variants.iter().position(|(n, ..)| n == vname) else {
                            return self.bad(
                                arm.span,
                                format!("enum `{enum_name}` has no variant `{vname}`"),
                            );
                        };
                        let vi_t = self.const_temp(IrConst::Int(vi as i64));
                        let c = self.temp();
                        self.instr(IrInstr::BinOp {
                            dst: c,
                            op: IrBinOp::Eq,
                            a: tag,
                            b: vi_t,
                        });
                        tag_test = match tag_test {
                            None => Some(c),
                            Some(prev) => Some(self.combine_cond(false, prev, c)),
                        };
                    }
                    let cond = tag_test.expect("an or-pattern has at least one alternative");
                    if chain_live {
                        // `self.cur` is the last fold's join, where `cond` is
                        // defined; branch forward to this arm's guard/body or
                        // fall through to the next arm.
                        let next = self.new_block();
                        self.term(IrTerm::BranchIf {
                            cond,
                            then: guard_in,
                            else_: next,
                        });
                        cur = next;
                    }
                    self.cur = guard_in;
                    self.push_scope();
                    if let Some(g) = guard {
                        let cond = self.expr(g)?;
                        let fall = if chain_live { cur } else { join };
                        self.term(IrTerm::BranchIf {
                            cond,
                            then: body,
                            else_: fall,
                        });
                        self.cur = body;
                    }
                    self.match_arm_body(&arm.body, res_slot)?;
                    self.pop_scope();
                    self.term(IrTerm::Branch { target: join });
                }
                Pattern::Literal(_) | Pattern::Tuple(_) => {
                    return self.bad(arm.span, "this match pattern is not lowered yet");
                }
            }
        }
        // An enum match whose chain checked every arm and still fell through
        // is non-exhaustive: the checker does not require exhaustiveness, so
        // this must fail loudly at runtime rather than read garbage.
        if chain_live {
            self.cur = cur;
            self.extern_call_void("pickle_panic_no_match", vec![], vec![]);
            self.term(IrTerm::Branch { target: join });
        }
        self.cur = join;
        match res_slot {
            Some(slot) => Ok(self.load(slot)),
            None => Ok(self.unit_temp()),
        }
    }

    /// Lower `match` over non-enum scrutinees: numeric scalars, `bool`,
    /// `char`, `byte`, `string`, and options (`some(v)`/`none`). Literal
    /// patterns become equality tests against the scrutinee slot; a
    /// binding/wildcard arm catches everything that reaches it. Mirrors the
    /// enum chain shape: options may guard, arms store into `res_slot`, and
    /// a non-exhaustive chain fails loudly at runtime.
    fn lower_generic_match(
        &mut self,
        e: &Expr,
        scrutinee: &Expr,
        arms: &[MatchArm],
        st: &Ty,
    ) -> Result<Temp, ()> {
        let Some(s_ir) = self.match_scrutinee_ir(st) else {
            return self.bad(
                e.span,
                format!("matching over `{st}` values is not lowered yet"),
            );
        };
        // The scrutinee lives in a slot so every arm sees the same value (and
        // a managed one stays rooted across arm allocations).
        let s = self.expr(scrutinee)?;
        let s_slot = self.new_slot(s_ir);
        self.instr(IrInstr::StoreSlot { slot: s_slot, v: s });

        let res_ty = self.irty(e.span).ok();
        let res_slot = if !matches!(res_ty, Some(IrTy::Unit) | None) {
            Some(self.new_slot(res_ty.unwrap()))
        } else {
            None
        };
        let join = self.new_block();
        let mut cur = self.cur;
        let mut chain_live = true;

        for arm in arms {
            let body = self.new_block();
            let guard = arm.guard.as_ref();
            let guard_in = if guard.is_some() {
                self.new_block()
            } else {
                body
            };

            let testable = match &arm.pattern {
                Pattern::Wildcard | Pattern::Binding { .. } => None,
                other => Some(other),
            };

            // A testable arm: the pattern's match condition gates entry.
            if let Some(p) = testable {
                if chain_live {
                    self.cur = cur;
                    let sv = self.load(s_slot);
                    let cond = self.pattern_test_cond(p, st, sv, arm.span)?;
                    let next = self.new_block();
                    self.term(IrTerm::BranchIf {
                        cond,
                        then: guard_in,
                        else_: next,
                    });
                    cur = next;
                }
                self.cur = guard_in;
                self.push_scope();
                let bv = self.load(s_slot);
                self.bind_match_value(&arm.pattern, st, bv, arm.span)?;
                if let Some(g) = guard {
                    let cond = self.expr(g)?;
                    let fall = if chain_live { cur } else { join };
                    self.term(IrTerm::BranchIf {
                        cond,
                        then: body,
                        else_: fall,
                    });
                    self.cur = body;
                }
                self.match_arm_body(&arm.body, res_slot)?;
                self.pop_scope();
                self.term(IrTerm::Branch { target: join });
                continue;
            }

            // Catch-all pattern (always matches when reached).
            if let Some(g) = guard {
                if chain_live {
                    self.cur = cur;
                    self.term(IrTerm::Branch { target: guard_in });
                    cur = self.new_block();
                    self.cur = guard_in;
                    self.push_scope();
                    let bv = self.load(s_slot);
                    self.bind_match_value(&arm.pattern, st, bv, arm.span)?;
                    let cond = self.expr(g)?;
                    self.term(IrTerm::BranchIf {
                        cond,
                        then: body,
                        else_: cur,
                    });
                    self.cur = body;
                    self.match_arm_body(&arm.body, res_slot)?;
                    self.pop_scope();
                    self.term(IrTerm::Branch { target: join });
                } else {
                    self.cur = guard_in;
                    self.push_scope();
                    let bv = self.load(s_slot);
                    self.bind_match_value(&arm.pattern, st, bv, arm.span)?;
                    let cond = self.expr(g)?;
                    self.term(IrTerm::BranchIf {
                        cond,
                        then: body,
                        else_: join,
                    });
                    self.cur = body;
                    self.match_arm_body(&arm.body, res_slot)?;
                    self.pop_scope();
                    self.term(IrTerm::Branch { target: join });
                }
                continue;
            }
            if chain_live {
                self.cur = cur;
                self.term(IrTerm::Branch { target: body });
                chain_live = false;
            }
            self.cur = body;
            self.push_scope();
            let bv = self.load(s_slot);
            self.bind_match_value(&arm.pattern, st, bv, arm.span)?;
            self.match_arm_body(&arm.body, res_slot)?;
            self.pop_scope();
            self.term(IrTerm::Branch { target: join });
        }

        // A chain that checked every arm and still fell through is
        // non-exhaustive; fail loudly at runtime rather than read garbage.
        if chain_live {
            self.cur = cur;
            self.extern_call_void("pickle_panic_no_match", vec![], vec![]);
            self.term(IrTerm::Branch { target: join });
        }
        self.cur = join;
        match res_slot {
            Some(slot) => Ok(self.load(slot)),
            None => Ok(self.unit_temp()),
        }
    }

    /// Produce the branch condition under which `p` matches the scrutinee
    /// value `a` (already loaded). `none` tests absence; `some(payload)`
    /// tests presence (and, for nested payload patterns, recurses); tuple
    /// patterns test each element; `a | b` tests either alternative; literal
    /// patterns test equality. Only call for patterns that can actually gate
    /// a branch; the checker rejects incompatible ones first.
    fn pattern_test_cond(&mut self, p: &Pattern, st: &Ty, a: Temp, span: Span) -> Result<Temp, ()> {
        match p {
            Pattern::Literal(Lit::None) if st.is_option() => {
                let present = self.opt_is_present(a)?;
                let f = self.const_temp(IrConst::Bool(false));
                let dst = self.temp();
                self.instr(IrInstr::BinOp {
                    dst,
                    op: IrBinOp::Eq,
                    a: present,
                    b: f,
                });
                Ok(dst)
            }
            Pattern::Variant { path, payloads } if path == &["some"] && st.is_option() => {
                let inner = st.inner_option().unwrap_or(Ty::Unknown);
                let present = self.opt_is_present(a)?;
                match payloads.first() {
                    None | Some(Pattern::Wildcard) | Some(Pattern::Binding { .. }) => Ok(present),
                    Some(payload) => {
                        // Nested pattern on the inner value: the arm matches
                        // only when the option is present *and* the payload
                        // does too. Reading the payload must be guarded behind
                        // presence so a `none` option is never dereferenced.
                        let res_slot = self.new_slot(IrTy::Bool);
                        let on_present = self.new_block();
                        let on_absent = self.new_block();
                        let join = self.new_block();
                        self.term(IrTerm::BranchIf {
                            cond: present,
                            then: on_present,
                            else_: on_absent,
                        });
                        self.cur = on_present;
                        let resolved = self.opt_resolve(a, &inner, span)?;
                        let inner_cond = self.pattern_test_cond(payload, &inner, resolved, span)?;
                        self.instr(IrInstr::StoreSlot {
                            slot: res_slot,
                            v: inner_cond,
                        });
                        self.term(IrTerm::Branch { target: join });
                        self.cur = on_absent;
                        let f = self.const_temp(IrConst::Bool(false));
                        self.instr(IrInstr::StoreSlot {
                            slot: res_slot,
                            v: f,
                        });
                        self.term(IrTerm::Branch { target: join });
                        self.cur = join;
                        Ok(self.load(res_slot))
                    }
                }
            }
            Pattern::Tuple(parts) => {
                let Ty::Tuple(tys) = st else {
                    return self.bad(span, "a tuple pattern requires a tuple scrutinee");
                };
                if tys.len() != parts.len() {
                    return self.bad(
                        span,
                        "tuple pattern arity does not match the scrutinee type",
                    );
                }
                let mut acc: Option<Temp> = None;
                for (i, part) in parts.iter().enumerate() {
                    let c = self.tuple_part_cond(part, &tys[i], a, i, span)?;
                    if let Some(c) = c {
                        acc = match acc {
                            None => Some(c),
                            Some(prev) => Some(self.combine_cond(true, prev, c)),
                        };
                    }
                }
                Ok(acc.unwrap_or_else(|| self.const_temp(IrConst::Bool(true))))
            }
            Pattern::Or(alts) => {
                let mut acc: Option<Temp> = None;
                for alt in alts {
                    let c = self.pattern_test_cond(alt, st, a, span)?;
                    acc = match acc {
                        None => Some(c),
                        Some(prev) => Some(self.combine_cond(false, prev, c)),
                    };
                }
                Ok(acc.expect("an or-pattern has at least one alternative"))
            }
            // An enum variant pattern: the scrutinee (of enum type `st`) must
            // carry this variant's tag; nested payload patterns add their own
            // conditions, evaluated only once the tag matches.
            Pattern::Variant { .. } if matches!(st, Ty::Enum(..)) => {
                self.enum_variant_cond(p, st, a, span)
            }
            Pattern::Literal(lit) if matches!(st, Ty::String) => {
                let Lit::String(parts) = lit else {
                    return self.bad(span, "this match pattern is not lowered yet");
                };
                let mut text = String::new();
                for part in parts {
                    match part {
                        StrPart::Text(t) => text.push_str(t),
                        StrPart::Expr(_) => {
                            return self.bad(span, "string match patterns may not interpolate");
                        }
                    }
                }
                let b = self.str_const(&text)?;
                let cmp = self.extern_call_t1(
                    "pickle_str_cmp",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Int,
                    vec![a, b],
                )?;
                let zero = self.int_const(0);
                let dst = self.temp();
                self.instr(IrInstr::BinOp {
                    dst,
                    op: IrBinOp::Eq,
                    a: cmp,
                    b: zero,
                });
                Ok(dst)
            }
            Pattern::Literal(lit) => {
                let b = self.pattern_literal_temp(lit, st, span)?;
                let dst = self.temp();
                self.instr(IrInstr::BinOp {
                    dst,
                    op: IrBinOp::Eq,
                    a,
                    b,
                });
                Ok(dst)
            }
            _ => self.bad(span, "this match pattern is not lowered yet"),
        }
    }

    /// The match condition contributed by tuple element `i` (read from the
    /// tuple pointer `a`): bindings and wildcards are always true; a testable
    /// nested pattern is unboxed to the element's IR domain and recursed.
    fn tuple_part_cond(
        &mut self,
        part: &Pattern,
        elem: &Ty,
        a: Temp,
        i: usize,
        span: Span,
    ) -> Result<Option<Temp>, ()> {
        match part {
            Pattern::Binding { .. } | Pattern::Wildcard => Ok(None),
            other => {
                let idx = self.const_temp(IrConst::Int(i as i64));
                if let ElemRep::Scalar(_, unbox_sym, unbox_ir) = self.elem_rep(elem, span)? {
                    let raw = self.extern_call_t1(
                        "pickle_tuple_field",
                        vec![IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                        vec![a, idx],
                    )?;
                    let un =
                        self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], unbox_ir, vec![raw])?;
                    Ok(Some(self.pattern_test_cond(other, elem, un, span)?))
                } else {
                    let raw = self.extern_call_t1(
                        "pickle_tuple_field",
                        vec![IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                        vec![a, idx],
                    )?;
                    Ok(Some(self.pattern_test_cond(other, elem, raw, span)?))
                }
            }
        }
    }

    /// Build a constant in the scrutinee's domain for a literal pattern. A
    /// float scrutinee widens integer literals; a byte scrutinee accepts
    /// ASCII char literals.
    fn pattern_literal_temp(&mut self, lit: &Lit, st: &Ty, span: Span) -> Result<Temp, ()> {
        match (lit, st) {
            (Lit::Int { value }, Ty::Float) => {
                let Ok(v) = i64::try_from(*value) else {
                    return self.bad(span, "integer literal out of range for i64");
                };
                let i = self.const_temp(IrConst::Int(v));
                let d = self.temp();
                self.instr(IrInstr::Itof { dst: d, v: i });
                Ok(d)
            }
            (Lit::Int { value }, _) => {
                let Ok(v) = i64::try_from(*value) else {
                    return self.bad(span, "integer literal out of range for i64");
                };
                Ok(self.const_temp(IrConst::Int(v)))
            }
            (Lit::Float { value }, _) if matches!(st, Ty::Int | Ty::Byte) => {
                if *value == value.trunc() && *value >= i64::MIN as f64 && *value <= i64::MAX as f64
                {
                    Ok(self.const_temp(IrConst::Int(*value as i64)))
                } else {
                    self.bad(span, "float pattern does not represent an integer")
                }
            }
            (Lit::Float { value }, _) => Ok(self.const_temp(IrConst::Float(value.to_bits()))),
            (Lit::Bool(v), Ty::Bool) => Ok(self.const_temp(IrConst::Bool(*v))),
            (Lit::Char(c), Ty::Char) => Ok(self.const_temp(IrConst::Char(*c as u32))),
            (Lit::Char(c), Ty::Byte) => {
                if *c as u32 <= 0x7F {
                    Ok(self.const_temp(IrConst::Int(*c as u8 as i64)))
                } else {
                    self.bad(span, "a non-ASCII char pattern has no `byte` value")
                }
            }
            _ => self.bad(span, "this match pattern is not lowered yet"),
        }
    }

    /// The raw IR domain a non-enum, non-string matched type lives in.
    fn scalar_ir_of(&self, st: &Ty) -> Option<IrTy> {
        match st {
            Ty::Int | Ty::Byte => Some(IrTy::Int),
            Ty::Float => Some(IrTy::Float),
            Ty::Bool => Some(IrTy::Bool),
            Ty::Char => Some(IrTy::Char),
            _ => None,
        }
    }

    /// The IR domain a match scrutinee (`match`/`if let`) is kept in: scalars
    /// as themselves, strings as `Str` foreign pointers, and every composite
    /// (option/list/map/class/struct/enum/interface/tuple/range) as a managed
    /// `Ptr`.
    fn match_scrutinee_ir(&self, st: &Ty) -> Option<IrTy> {
        match st {
            Ty::Int | Ty::Byte | Ty::Float | Ty::Bool | Ty::Char => self.scalar_ir_of(st),
            Ty::String => Some(IrTy::Str),
            Ty::Option(..)
            | Ty::Class(..)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Interface(..)
            | Ty::List(..)
            | Ty::Map(..)
            | Ty::Tuple(..)
            | Ty::Range(..) => Some(IrTy::Ptr),
            _ => None,
        }
    }

    fn const_temp(&mut self, c: IrConst) -> Temp {
        let dst = self.temp();
        self.instr(IrInstr::Const { dst, c });
        dst
    }

    /// Comparison kind for `pickle_list_sort`: 0 = int/byte/char/bool boxed
    /// payloads, 1 = float bits, 2 = string bytes.
    fn sort_kind(&mut self, elem: &Ty, span: Span) -> Result<Temp, ()> {
        let kind = match elem {
            Ty::Int | Ty::Byte | Ty::Char | Ty::Bool => 0i64,
            Ty::Float => 1,
            Ty::String => 2,
            other => {
                return self.bad(
                    span,
                    format!("`sort` is not lowered for `List<{}>`", other.bare_name()),
                )
            }
        };
        Ok(self.const_temp(IrConst::Int(kind)))
    }

    /// Bind the names an arm pattern introduces to the scrutinee value `v`
    /// (already loaded, of type `st`) in the current block. Bindings declare
    /// under `map_ty`; `some(v)` unboxes the present option; tuple patterns
    /// unbox each element; `[a | b]` binds nothing (the checker only admits
    /// or-patterns that introduce no names); `_`/literals bind nothing.
    fn bind_match_value(&mut self, p: &Pattern, st: &Ty, v: Temp, span: Span) -> Result<(), ()> {
        match p {
            Pattern::Wildcard | Pattern::Literal(_) => Ok(()),
            Pattern::Binding { name, .. } => {
                let ir = if matches!(st, Ty::String) {
                    IrTy::Str
                } else {
                    self.map_ty(st, span)?
                };
                self.declare_binding(name, ir, v);
                Ok(())
            }
            Pattern::Variant { path, payloads } => {
                if path == &["some"] && st.is_option() {
                    // The match condition already proved the option present, so
                    // resolving is safe here.
                    let inner = st.inner_option().unwrap_or(Ty::Unknown);
                    let resolved = self.opt_resolve(v, &inner, span)?;
                    match payloads.first() {
                        None => Ok(()),
                        Some(pl) => self.bind_pattern_value(pl, &inner, resolved, span),
                    }
                } else if matches!(st, Ty::Enum(..)) {
                    // Bind the names an enum variant pattern introduces,
                    // recursing into each payload.
                    let Ty::Enum(ename, _) = st else {
                        unreachable!()
                    };
                    let table = match self.resolved.types.get(ename) {
                        Some(TypeTableEntry::Enum(t)) => t.clone(),
                        _ => return self.bad(span, format!("unknown enum type `{ename}`")),
                    };
                    let vname = match path.len() {
                        2 => &path[1],
                        1 => &path[0],
                        _ => return self.bad(span, "invalid enum variant pattern"),
                    };
                    let Some(vi) = table.variants.iter().position(|(n, ..)| n == vname) else {
                        return self.bad(span, format!("enum `{ename}` has no variant `{vname}`"));
                    };
                    let ftypes = table.variants[vi].1.clone();
                    for (pi, p) in payloads.iter().enumerate() {
                        let ft = ftypes.get(pi).cloned().unwrap_or(Ty::Unknown);
                        let idx = self.const_temp(IrConst::Int(pi as i64));
                        let raw = self.extern_call_t1(
                            "pickle_enum_field",
                            vec![IrTy::Ptr, IrTy::Int],
                            IrTy::Ptr,
                            vec![v, idx],
                        )?;
                        let pv = match self.elem_rep(&ft, span)? {
                            ElemRep::Ptr => raw,
                            ElemRep::Scalar(_, unbox_sym, unbox_ir) => self.extern_call_t1(
                                unbox_sym,
                                vec![IrTy::Ptr],
                                unbox_ir,
                                vec![raw],
                            )?,
                        };
                        self.bind_pattern_value(p, &ft, pv, span)?;
                    }
                    Ok(())
                } else {
                    self.bad(span, "this match pattern is not lowered yet")
                }
            }
            Pattern::Tuple(parts) => {
                let Ty::Tuple(tys) = st else {
                    return self.bad(span, "a tuple pattern requires a tuple scrutinee");
                };
                if tys.len() != parts.len() {
                    return self.bad(
                        span,
                        "tuple pattern arity does not match the scrutinee type",
                    );
                }
                for (i, part) in parts.iter().enumerate() {
                    let idx = self.const_temp(IrConst::Int(i as i64));
                    let raw = self.extern_call_t1(
                        "pickle_tuple_field",
                        vec![IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                        vec![v, idx],
                    )?;
                    let elem = &tys[i];
                    let pv = if let ElemRep::Scalar(_, unbox_sym, unbox_ir) =
                        self.elem_rep(elem, span)?
                    {
                        self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], unbox_ir, vec![raw])?
                    } else {
                        raw
                    };
                    self.bind_pattern_value(part, elem, pv, span)?;
                }
                Ok(())
            }
            Pattern::Or(_) => Ok(()),
        }
    }

    /// Bind a nested pattern to a value `v` of type `ty` already in IR form
    /// (a `some` payload or a tuple element).
    fn bind_pattern_value(&mut self, p: &Pattern, ty: &Ty, v: Temp, span: Span) -> Result<(), ()> {
        self.bind_match_value(p, ty, v, span)
    }

    fn declare_binding(&mut self, name: &str, ir: IrTy, v: Temp) {
        let slot = self.new_slot(ir);
        self.instr(IrInstr::StoreSlot { slot, v });
        self.declare(name, slot);
    }
    fn match_arm_body(&mut self, body: &Expr, res_slot: Option<Slot>) -> Result<(), ()> {
        let bt = self.expr(body)?;
        if let Some(slot) = res_slot {
            // A `Ptr` result slot holds an option: scalar arm values must be
            // boxed first, or a later `some(x)`/`?` read dereferences the raw
            // scalar as a pointer.
            let bt = if matches!(self.fslots.get(slot.0 as usize), Some(IrTy::Ptr)) {
                let bt_ty = self.ty_of(&body.span).unwrap_or(Ty::Unknown);
                self.option_wrap(bt, &bt_ty, body.span)?
            } else {
                bt
            };
            self.instr(IrInstr::StoreSlot { slot, v: bt });
        }
        Ok(())
    }

    /// Bind the payload fields of variant `vi` of `table` into fresh slots for
    /// `Color.Rgb(r, g, b)` patterns, and return the folded match conditions
    /// contributed by any nested (non-binding) payload patterns. Fields are
    /// boxed in the object, so each is unboxed to its element IR type; `_`
    /// payloads bind nothing. Nested patterns (e.g. `ExprKind.Lit(Lit.String(
    /// parts))`) bind the names they introduce and contribute a condition.
    fn bind_enum_payloads(
        &mut self,
        span: Span,
        table: &EnumTable,
        vi: usize,
        payloads: &[Pattern],
        s_slot: Slot,
    ) -> Result<Option<Temp>, ()> {
        let ftypes = table.variants[vi].1.clone();
        let mut acc: Option<Temp> = None;
        for (pi, p) in payloads.iter().enumerate() {
            let ft = ftypes.get(pi).cloned().unwrap_or(Ty::Unknown);
            let rep = self.elem_rep(&ft, span)?;
            let idx = self.temp();
            self.instr(IrInstr::Const {
                dst: idx,
                c: IrConst::Int(pi as i64),
            });
            let raw = {
                let sv = self.load(s_slot);
                self.extern_call_t1(
                    "pickle_enum_field",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![sv, idx],
                )?
            };
            let (pv, ir) = match &rep {
                ElemRep::Ptr => (raw, IrTy::Ptr),
                ElemRep::Scalar(_, unbox_sym, unbox_ir) => (
                    self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], *unbox_ir, vec![raw])?,
                    *unbox_ir,
                ),
            };
            match p {
                Pattern::Wildcard => {}
                Pattern::Binding { name, .. } => {
                    let slot = self.new_slot(ir);
                    self.instr(IrInstr::StoreSlot { slot, v: pv });
                    self.declare(name, slot);
                }
                nested => {
                    // Nested payload pattern: bind the names it introduces,
                    // then fold in the condition it adds. Keep the nested
                    // value rooted while it is examined.
                    self.bind_pattern_value(nested, &ft, pv, span)?;
                    let root = self.new_slot(IrTy::Ptr);
                    self.instr(IrInstr::StoreSlot { slot: root, v: pv });
                    let sv = self.load(root);
                    let c = if matches!(ft, Ty::Enum(..))
                        && matches!(nested, Pattern::Variant { .. })
                    {
                        self.enum_variant_cond(nested, &ft, sv, span)?
                    } else {
                        self.pattern_test_cond(nested, &ft, sv, span)?
                    };
                    acc = match acc {
                        None => Some(c),
                        Some(prev) => Some(self.combine_cond(true, prev, c)),
                    };
                }
            }
        }
        Ok(acc)
    }

    /// The match condition for an enum variant pattern: the scrutinee value
    /// `v` (of enum type `st`) must carry the variant's tag, and nested payload
    /// conditions are folded in. Payload fields are read only inside a block
    /// guarded by the tag equality, so a mismatched variant is never
    /// dereferenced.
    fn enum_variant_cond(&mut self, p: &Pattern, st: &Ty, v: Temp, span: Span) -> Result<Temp, ()> {
        let Pattern::Variant { path, payloads } = p else {
            unreachable!()
        };
        let Ty::Enum(ename, _) = st else {
            return self.bad(span, "this match pattern is not lowered yet");
        };
        let table = match self.resolved.types.get(ename) {
            Some(TypeTableEntry::Enum(t)) => t.clone(),
            _ => return self.bad(span, format!("unknown enum type `{ename}`")),
        };
        let vname = match path.len() {
            2 => &path[1],
            1 => &path[0],
            _ => return self.bad(span, "invalid enum variant pattern"),
        };
        let Some(vi) = table.variants.iter().position(|(n, ..)| n == vname) else {
            return self.bad(span, format!("enum `{ename}` has no variant `{vname}`"));
        };
        let tag = self.extern_call_t1("pickle_enum_tag", vec![IrTy::Ptr], IrTy::Int, vec![v])?;
        let vi_t = self.const_temp(IrConst::Int(vi as i64));
        let eq = self.temp();
        self.instr(IrInstr::BinOp {
            dst: eq,
            op: IrBinOp::Eq,
            a: tag,
            b: vi_t,
        });
        let testable = payloads
            .iter()
            .any(|p| !matches!(p, Pattern::Binding { .. } | Pattern::Wildcard));
        if testable {
            let root = self.new_slot(IrTy::Ptr);
            self.instr(IrInstr::StoreSlot { slot: root, v });
            let res_slot = self.new_slot(IrTy::Bool);
            let on_match = self.new_block();
            let on_other = self.new_block();
            let join = self.new_block();
            self.term(IrTerm::BranchIf {
                cond: eq,
                then: on_match,
                else_: on_other,
            });
            self.cur = on_match;
            let inner_conds = self.bind_enum_payload_cond(span, &table, vi, payloads, root)?;
            let val = inner_conds.unwrap_or_else(|| self.const_temp(IrConst::Bool(true)));
            self.instr(IrInstr::StoreSlot {
                slot: res_slot,
                v: val,
            });
            self.term(IrTerm::Branch { target: join });
            self.cur = on_other;
            let f = self.const_temp(IrConst::Bool(false));
            self.instr(IrInstr::StoreSlot {
                slot: res_slot,
                v: f,
            });
            self.term(IrTerm::Branch { target: join });
            self.cur = join;
            Ok(self.load(res_slot))
        } else {
            Ok(eq)
        }
    }

    /// Fold the extra match conditions contributed by the nested (non-binding)
    /// payload patterns of enum variant `vi`, whose object pointer sits in the
    /// rooted slot `v_slot`. Binding names are declared separately by
    /// `bind_enum_payloads`; the returned condition is `None` when every
    /// payload is a name or `_`.
    fn bind_enum_payload_cond(
        &mut self,
        span: Span,
        table: &EnumTable,
        vi: usize,
        payloads: &[Pattern],
        v_slot: Slot,
    ) -> Result<Option<Temp>, ()> {
        let ftypes = table.variants[vi].1.clone();
        let mut acc: Option<Temp> = None;
        for (pi, p) in payloads.iter().enumerate() {
            if matches!(p, Pattern::Binding { .. } | Pattern::Wildcard) {
                continue;
            }
            let ft = ftypes.get(pi).cloned().unwrap_or(Ty::Unknown);
            let idx = self.const_temp(IrConst::Int(pi as i64));
            let raw = {
                let sv = self.load(v_slot);
                self.extern_call_t1(
                    "pickle_enum_field",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![sv, idx],
                )?
            };
            let pv = match self.elem_rep(&ft, span)? {
                ElemRep::Ptr => raw,
                ElemRep::Scalar(_, unbox_sym, unbox_ir) => {
                    self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], unbox_ir, vec![raw])?
                }
            };
            let root = self.new_slot(IrTy::Ptr);
            self.instr(IrInstr::StoreSlot { slot: root, v: pv });
            let sv = self.load(root);
            let c = if matches!(ft, Ty::Enum(..)) && matches!(p, Pattern::Variant { .. }) {
                self.enum_variant_cond(p, &ft, sv, span)?
            } else {
                self.pattern_test_cond(p, &ft, sv, span)?
            };
            acc = match acc {
                None => Some(c),
                Some(prev) => Some(self.combine_cond(true, prev, c)),
            };
        }
        Ok(acc)
    }

    fn if_expr(
        &mut self,
        e: &Expr,
        cond: &IfCond,
        then: &Block,
        else_else: Option<&Expr>,
    ) -> Result<Temp, ()> {
        if let IfCond::Binding { pattern, value } = cond {
            return self.if_let_expr(e, pattern, value, then, else_else);
        }
        let IfCond::Cond(c) = cond else {
            unreachable!()
        };
        let cond_t = self.expr(c)?;
        let res_ty = self.irty(e.span).ok();
        let need_slot = !matches!(res_ty, Some(IrTy::Unit) | None) && else_else.is_some();
        let res_slot = if need_slot {
            Some(self.new_slot(res_ty.unwrap()))
        } else {
            None
        };
        let then_id = self.new_block();
        let else_id = if else_else.is_some() {
            Some(self.new_block())
        } else {
            None
        };
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: cond_t,
            then: then_id,
            else_: else_id.unwrap_or(join),
        });

        self.cur = then_id;
        self.push_scope();
        self.block_into_slot(then, res_slot)?;
        self.pop_scope();
        self.term(IrTerm::Branch { target: join });

        if let Some(e) = else_else {
            self.cur = else_id.unwrap();
            match res_slot {
                Some(slot) => {
                    let t = self.expr(e)?;
                    let t = if matches!(self.fslots.get(slot.0 as usize), Some(IrTy::Ptr)) {
                        let ty = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                        self.option_wrap(t, &ty, e.span)?
                    } else {
                        t
                    };
                    self.instr(IrInstr::StoreSlot { slot, v: t });
                }
                None => {
                    let _ = self.expr(e)?;
                }
            }
            self.term(IrTerm::Branch { target: join });
        }
        self.cur = join;
        match res_slot {
            Some(slot) => Ok(self.load(slot)),
            None => Ok(self.unit_temp()),
        }
    }

    /// Lower `if (let pattern = value) then-block [else ...]`: the value is
    /// evaluated once into a slot, a `pattern_test_cond` gates the then-block
    /// (an always-true binding/wildcard pattern needs no branch), and the
    /// binding is scoped to the then-block only. An `ElseStmt`/value
    /// expression otherwise mirrors `if_expr`.
    fn if_let_expr(
        &mut self,
        e: &Expr,
        pattern: &Pattern,
        value: &Expr,
        then: &Block,
        else_else: Option<&Expr>,
    ) -> Result<Temp, ()> {
        let st = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
        // Composite scrutinees (lists, classes, ...) are kept as `Ptr`, never
        // squeezed into an `Int` slot: a scalar-less type is not `Int`.
        let s_ir = self.match_scrutinee_ir(&st).unwrap_or(IrTy::Ptr);
        let v = self.expr(value)?;
        let s_slot = self.new_slot(s_ir);
        self.instr(IrInstr::StoreSlot { slot: s_slot, v });

        let res_ty = self.irty(e.span).ok();
        let need_slot = !matches!(res_ty, Some(IrTy::Unit) | None) && else_else.is_some();
        let res_slot = if need_slot {
            Some(self.new_slot(res_ty.unwrap()))
        } else {
            None
        };
        let always = matches!(pattern, Pattern::Binding { .. } | Pattern::Wildcard);
        let then_id = self.new_block();
        let else_id = if else_else.is_some() {
            Some(self.new_block())
        } else {
            None
        };
        let join = self.new_block();

        // Bindings are declared in the then-block so a failed `some(v)` test
        // never leaves a stale name visible to the else branch.
        if always {
            self.term(IrTerm::Branch { target: then_id });
            self.cur = then_id;
            self.push_scope();
            let bv = self.load(s_slot);
            self.bind_match_value(pattern, &st, bv, value.span)?;
            self.block_into_slot(then, res_slot)?;
            self.pop_scope();
            self.term(IrTerm::Branch { target: join });
        } else {
            let sv = self.load(s_slot);
            let cond = self.pattern_test_cond(pattern, &st, sv, value.span)?;
            self.term(IrTerm::BranchIf {
                cond,
                then: then_id,
                else_: else_id.unwrap_or(join),
            });
            self.cur = then_id;
            self.push_scope();
            let bv = self.load(s_slot);
            self.bind_match_value(pattern, &st, bv, value.span)?;
            self.block_into_slot(then, res_slot)?;
            self.pop_scope();
            self.term(IrTerm::Branch { target: join });
        }

        if let Some(e) = else_else {
            self.cur = else_id.unwrap();
            match res_slot {
                Some(slot) => {
                    let t = self.expr(e)?;
                    let t = if matches!(self.fslots.get(slot.0 as usize), Some(IrTy::Ptr)) {
                        let ty = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                        self.option_wrap(t, &ty, e.span)?
                    } else {
                        t
                    };
                    self.instr(IrInstr::StoreSlot { slot, v: t });
                }
                None => {
                    let _ = self.expr(e)?;
                }
            }
            self.term(IrTerm::Branch { target: join });
        }
        self.cur = join;
        match res_slot {
            Some(slot) => Ok(self.load(slot)),
            None => Ok(self.unit_temp()),
        }
    }

    /// Emit a block whose tail value (if any) is stored into `res_slot`.
    fn block_into_slot(&mut self, b: &Block, res_slot: Option<Slot>) -> Result<(), ()> {
        if let Some(slot) = res_slot {
            let want = self
                .fslots
                .get(slot.0 as usize)
                .copied()
                .unwrap_or(IrTy::Unit);
            if let Some(try_slot) = self.stmts_with_try_tail(&b.stmts, want)? {
                let t = self.load(try_slot);
                self.instr(IrInstr::StoreSlot { slot, v: t });
                return Ok(());
            }
        } else if self.stmts_with_try_tail(&b.stmts, IrTy::Unit)?.is_some() {
            return Ok(());
        }
        for s in &b.stmts {
            self.stmt(s)?;
        }
        if let Some(e) = &b.expr {
            let t = self.expr(e)?;
            if let Some(slot) = res_slot {
                // Same option-boxing rule as match arms: a scalar tail into a
                // `Ptr` result slot (an option-typed `if`/`if let`) must be
                // lifted before storing.
                let t = if matches!(self.fslots.get(slot.0 as usize), Some(IrTy::Ptr)) {
                    let ty = self.ty_of(&e.span).unwrap_or(Ty::Unknown);
                    self.option_wrap(t, &ty, e.span)?
                } else {
                    t
                };
                self.instr(IrInstr::StoreSlot { slot, v: t });
            }
        }
        Ok(())
    }

    /// Is `callee` a general function-typed expression (as opposed to a plain
    /// named user/ctor/builtin call, which has its own static paths)?
    fn is_fn_dispatchable(&self, callee: &Expr) -> bool {
        match &callee.kind {
            ExprKind::Ident(name) => {
                if self.module.funcs_by_name.contains_key(name) {
                    return false;
                }
                if self.class_by_name.contains_key(name) {
                    return false;
                }
                if matches!(
                    name.as_str(),
                    "print"
                        | "println"
                        | "len"
                        | "alloc"
                        | "free"
                        | "assert"
                        | "expect"
                        | "abs"
                        | "range"
                        | "min"
                        | "max"
                        | "clamp"
                        | "str"
                        | "bytes"
                        | "read_file"
                        | "write_file"
                        | "file_exists"
                        | "delete"
                        | "list_dir"
                        | "mkdir"
                        | "stream_open_read"
                        | "stream_open_write"
                        | "stream_open_append"
                        | "stdout_stream"
                        | "stderr_stream"
                        | "args"
                        | "exit"
                        | "run_cmd"
                        | "run_spawn"
                        | "proc_wait"
                        | "proc_running"
                        | "proc_code"
                        | "proc_kill"
                        | "proc_out_read"
                        | "proc_err_read"
                        | "proc_in_write"
                        | "proc_in_close"
                        | "current_dir"
                        | "captureBegin"
                        | "captureTake"
                        | "gcCollect"
                        | "gcAllocationsSinceGc"
                        | "gcBytesSinceGc"
                        | "gcCollectionCount"
                        | "gcSetThreshold"
                ) {
                    return false;
                }
            }
            ExprKind::GenericCall { .. } => return false,
            _ => {}
        }
        matches!(self.types.get(&callee.span), Some(Ty::Fn(..)))
    }

    /// The value argument of the outermost `expect(...)` in an assertion chain,
    /// plus whether a `.not()` flips the expectation.
    fn peel_expect<'x>(&self, e: &'x Expr) -> Option<(&'x Expr, bool)> {
        match &e.kind {
            ExprKind::Call { callee, args }
                if args.len() == 1 && !args[0].spread && args[0].name.is_none() =>
            {
                match &callee.kind {
                    ExprKind::Ident(n) if n == "expect" => Some((&args[0].value, false)),
                    _ => None,
                }
            }
            ExprKind::Call { callee, args } if args.is_empty() => {
                if let ExprKind::Member { object, name } = &callee.kind {
                    if name == "not" {
                        return self.peel_expect(object).map(|(v, neg)| (v, !neg));
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Lower one `expect(...)` assertion. Only the failure path is real: the
    /// computed condition branches over a recorded `pickle_test_fail_obj(msg)`
    /// (fatal in program mode), mirroring `assert`. Polarity of a `.not()`
    /// chain is folded into the comparison op selection so the condition is
    /// always a raw `BinOp` result (`b1`), never a `UnOp::Not` of one.
    fn expect_assert(
        &mut self,
        e: &Expr,
        value: &Expr,
        method: &str,
        args: &[CallArg],
        negated: bool,
    ) -> Result<Temp, ()> {
        for a in args {
            if a.spread || a.name.is_some() {
                return self.bad(
                    e.span,
                    "expectation methods take plain positional arguments",
                );
            }
        }
        let vt = self.ty_of(&value.span).unwrap_or(Ty::Unknown);
        let it = self.irty(value.span)?;
        let v = self.expr(value)?;

        let (ok, expected_t, received_t): (Temp, Temp, Temp) = match method {
            "toBe" | "toEqual" => {
                let a = self.expect_single_arg(e, method, args)?;
                let at = self.expr(&a.value)?;
                let a_it = self.irty(a.value.span)?;
                let expected = self.expect_str(at, a_it, a.value.span)?;
                let received = self.expect_str(v, it, value.span)?;
                let ok = self.expect_eq_cond(v, it, at, a_it, negated, e.span)?;
                (ok, expected, received)
            }
            "toBeTruthy" | "toBeFalsy" => {
                self.expect_no_args(e, method, args)?;
                let received = self.expect_str(v, it, value.span)?;
                let want_truthy = method == "toBeTruthy";
                let need = want_truthy != negated;
                let ok = self.expect_truthy(v, it, need, e.span)?;
                let expected = self.str_const(if want_truthy { "true" } else { "false" })?;
                (ok, expected, received)
            }
            "toBeNull" => {
                self.expect_no_args(e, method, args)?;
                let received = self.expect_str(v, it, value.span)?;
                let null = self.null_temp()?;
                let res = self.extern_call_t1(
                    "pickle_expect_obj_eq",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    IrTy::Int,
                    vec![v, null],
                )?;
                // null-ness: obj_eq != 0 (flipped by `.not()`).
                let ok = self.cmp_zero(res, IrBinOp::Ne, negated)?;
                let expected = self.str_const("none")?;
                (ok, expected, received)
            }
            "toExist" => {
                self.expect_no_args(e, method, args)?;
                let received = self.expect_str(v, it, value.span)?;
                let null = self.null_temp()?;
                let res = self.extern_call_t1(
                    "pickle_expect_obj_eq",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    IrTy::Int,
                    vec![v, null],
                )?;
                // exists: obj_eq == 0.
                let ok = self.cmp_zero(res, IrBinOp::Eq, negated)?;
                let expected = self.str_const("a non-none value")?;
                (ok, expected, received)
            }
            "toHaveLength" => {
                let a = self.expect_single_arg(e, method, args)?;
                let n = self.expr(&a.value)?;
                let n_it = self.irty(a.value.span)?;
                let len = match &vt {
                    Ty::String => {
                        self.extern_call_t1("pickle_str_len", vec![IrTy::Str], IrTy::Int, vec![v])?
                    }
                    Ty::List(_) => {
                        self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![v])?
                    }
                    Ty::Map(_, _) => {
                        self.extern_call_t1("pickle_map_len", vec![IrTy::Ptr], IrTy::Int, vec![v])?
                    }
                    _ => {
                        return self.bad(
                            value.span,
                            "`.toHaveLength()` needs a string, list, or map value",
                        )
                    }
                };
                let eq = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: eq,
                    op: flip_cmp(IrBinOp::Eq, negated),
                    a: len,
                    b: n,
                });
                let expected = self.expect_str(n, n_it, a.value.span)?;
                let received = self.expect_str(len, IrTy::Int, value.span)?;
                (eq, expected, received)
            }
            "toContain" => {
                let a = self.expect_single_arg(e, method, args)?;
                let needle = self.expr(&a.value)?;
                let needle_it = self.irty(a.value.span)?;
                let res = match &vt {
                    Ty::String => self.extern_call_t1(
                        "pickle_expect_str_contains",
                        vec![IrTy::Ptr, IrTy::Ptr],
                        IrTy::Int,
                        vec![v, needle],
                    )?,
                    Ty::List(_) => {
                        let needle_ty = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                        let needle = self.option_wrap(needle, &needle_ty, a.value.span)?;
                        self.extern_call_t1(
                            "pickle_expect_list_contains",
                            vec![IrTy::Ptr, IrTy::Ptr],
                            IrTy::Int,
                            vec![v, needle],
                        )?
                    }
                    _ => {
                        return self.bad(value.span, "`.toContain()` needs a string or list value")
                    }
                };
                let ok = self.cmp_zero(res, IrBinOp::Ne, negated)?;
                let expected = self.expect_str(needle, needle_it, a.value.span)?;
                let received = self.expect_str(v, it, value.span)?;
                (ok, expected, received)
            }
            "toBeGreaterThan" | "toBeLessThan" => {
                let a = self.expect_single_arg(e, method, args)?;
                let at = self.expr(&a.value)?;
                let a_it = self.irty(a.value.span)?;
                let (vf, af) = self.numeric_unify(v, it, at, a_it, e.span)?;
                let cmp = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: cmp,
                    op: if method == "toBeGreaterThan" {
                        flip_cmp(IrBinOp::Gt, negated)
                    } else {
                        flip_cmp(IrBinOp::Lt, negated)
                    },
                    a: vf,
                    b: af,
                });
                let op_txt = self.str_const(if method == "toBeGreaterThan" {
                    "> "
                } else {
                    "< "
                })?;
                let d = self.expect_str(at, a_it, a.value.span)?;
                let expected = self.concat_strs(&[op_txt, d])?;
                let received = self.expect_str(v, it, value.span)?;
                (cmp, expected, received)
            }
            other => return self.bad(e.span, format!("unknown expectation method `.{other}()`")),
        };

        // "Expected:\n<e>\n\nReceived:\n<r>\n"
        let head = self.str_const("Expected:\n")?;
        let mid = self.str_const("\n\nReceived:\n")?;
        let tail = self.str_const("\n")?;
        let msg = self.concat_strs(&[head, expected_t, mid, received_t, tail])?;

        let cont = self.new_block();
        let fail = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: ok,
            then: cont,
            else_: fail,
        });
        self.cur = fail;
        self.extern_call_void("pickle_test_fail_obj", vec![IrTy::Ptr], vec![msg]);
        self.term(IrTerm::Branch { target: cont });
        self.cur = cont;
        Ok(self.unit_temp())
    }

    /// `(res OP 0)` with OP flipped by `.not()`.
    fn cmp_zero(&mut self, res: Temp, op: IrBinOp, negated: bool) -> Result<Temp, ()> {
        let dst = self.temp();
        let zero = self.int_const(0);
        self.instr(IrInstr::BinOp {
            dst,
            op: flip_cmp(op, negated),
            a: res,
            b: zero,
        });
        Ok(dst)
    }

    /// Structural/low-level equality condition for `toBe`/`toEqual`.
    fn expect_eq_cond(
        &mut self,
        v: Temp,
        it: IrTy,
        w: Temp,
        wit: IrTy,
        negated: bool,
        span: Span,
    ) -> Result<Temp, ()> {
        let managed = Self::managed_ir(it) || Self::managed_ir(wit);
        if managed {
            let res = self.extern_call_t1(
                "pickle_expect_obj_eq",
                vec![IrTy::Ptr, IrTy::Ptr],
                IrTy::Int,
                vec![v, w],
            )?;
            self.cmp_zero(res, IrBinOp::Ne, negated)
        } else {
            let (v, w) = self.numeric_unify(v, it, w, wit, span)?;
            let dst = self.temp();
            self.instr(IrInstr::BinOp {
                dst,
                op: flip_cmp(IrBinOp::Eq, negated),
                a: v,
                b: w,
            });
            Ok(dst)
        }
    }

    /// Bring two numeric operand temps to one shape (int vs float promoted).
    fn numeric_unify(
        &mut self,
        a: Temp,
        aty: IrTy,
        b: Temp,
        bty: IrTy,
        _span: Span,
    ) -> Result<(Temp, Temp), ()> {
        if matches!(aty, IrTy::Float) || matches!(bty, IrTy::Float) {
            let a = if matches!(aty, IrTy::Float) {
                a
            } else {
                self.float_cast(a)?
            };
            let b = if matches!(bty, IrTy::Float) {
                b
            } else {
                self.float_cast(b)?
            };
            Ok((a, b))
        } else {
            Ok((a, b))
        }
    }

    fn float_cast(&mut self, v: Temp) -> Result<Temp, ()> {
        let dst = self.temp();
        self.instr(IrInstr::Itof { dst, v });
        Ok(dst)
    }

    /// Truthiness as a branchable condition of a raw value temp. `need` is
    /// `true` when a truthy value passes (folded through `.not()`).
    fn expect_truthy(&mut self, v: Temp, it: IrTy, need: bool, _span: Span) -> Result<Temp, ()> {
        match it {
            IrTy::Bool => {
                let dst = self.temp();
                let z = self.bool_const(false);
                self.instr(IrInstr::BinOp {
                    dst,
                    op: flip_cmp(IrBinOp::Ne, !need),
                    a: v,
                    b: z,
                });
                Ok(dst)
            }
            IrTy::Char => {
                let dst = self.temp();
                let z = self.char_const(0);
                self.instr(IrInstr::BinOp {
                    dst,
                    op: flip_cmp(IrBinOp::Ne, !need),
                    a: v,
                    b: z,
                });
                Ok(dst)
            }
            IrTy::Int => {
                let dst = self.temp();
                let zero = self.int_const(0);
                self.instr(IrInstr::BinOp {
                    dst,
                    op: flip_cmp(IrBinOp::Ne, !need),
                    a: v,
                    b: zero,
                });
                Ok(dst)
            }
            IrTy::Float => {
                let dst = self.temp();
                let z = self.float_const(0.0);
                self.instr(IrInstr::BinOp {
                    dst,
                    op: flip_cmp(IrBinOp::Ne, !need),
                    a: v,
                    b: z,
                });
                Ok(dst)
            }
            IrTy::Str | IrTy::Ptr => {
                let len =
                    self.extern_call_t1("pickle_str_len", vec![IrTy::Ptr], IrTy::Int, vec![v])?;
                self.cmp_zero(len, IrBinOp::Ne, !need)
            }
            IrTy::Unit => {
                let zero = self.int_const(0);
                self.cmp_zero(zero, IrBinOp::Ne, !need)
            }
        }
    }

    /// Render a value temp as text for an `Expected:/Received:` line.
    fn expect_str(&mut self, t: Temp, it: IrTy, _span: Span) -> Result<Temp, ()> {
        match it {
            IrTy::Int => {
                self.extern_call_t1("pickle_str_from_i64", vec![IrTy::Int], IrTy::Str, vec![t])
            }
            IrTy::Float => {
                self.extern_call_t1("pickle_str_from_f64", vec![IrTy::Float], IrTy::Str, vec![t])
            }
            IrTy::Bool => {
                self.extern_call_t1("pickle_str_from_bool", vec![IrTy::Bool], IrTy::Str, vec![t])
            }
            IrTy::Char => {
                self.extern_call_t1("pickle_str_from_char", vec![IrTy::Char], IrTy::Str, vec![t])
            }
            IrTy::Str | IrTy::Ptr => {
                self.extern_call_t1("pickle_expect_display", vec![IrTy::Ptr], IrTy::Str, vec![t])
            }
            IrTy::Unit => self.str_const("none"),
        }
    }

    fn str_const(&mut self, text: &str) -> Result<Temp, ()> {
        let sid = StrId(self.intern_string(text));
        let t = self.temp();
        self.instr(IrInstr::Const {
            dst: t,
            c: IrConst::Str(sid),
        });
        Ok(t)
    }

    fn concat_strs(&mut self, parts: &[Temp]) -> Result<Temp, ()> {
        if parts.is_empty() {
            return self.str_const("");
        }
        let mut acc = parts[0];
        for p in &parts[1..] {
            acc = self.extern_call_t1(
                "pickle_str_concat",
                vec![IrTy::Str, IrTy::Str],
                IrTy::Str,
                vec![acc, *p],
            )?;
        }
        Ok(acc)
    }

    fn expect_single_arg<'x>(
        &mut self,
        e: &Expr,
        method: &str,
        args: &'x [CallArg],
    ) -> Result<&'x CallArg, ()> {
        if args.len() != 1 {
            return self.bad(
                e.span,
                format!("`.{method}(expected)` takes exactly one argument"),
            );
        }
        if args[0].name.is_some() || args[0].spread {
            return self.bad(
                args[0].span,
                format!("`.{method}()` takes a plain positional argument"),
            );
        }
        Ok(&args[0])
    }

    fn expect_no_args(&mut self, e: &Expr, method: &str, args: &[CallArg]) -> Result<(), ()> {
        if args.is_empty() {
            return Ok(());
        }
        self.bad(e.span, format!("`.{method}()` takes no arguments"))
    }

    fn managed_ir(it: IrTy) -> bool {
        matches!(it, IrTy::Str | IrTy::Ptr | IrTy::Unit)
    }

    /// A closure-valued callee: load its body address out of slot 0 and call it
    /// indirectly. The first argument is the closure object itself, so the
    /// hoisted body can copy its captures out, mirroring the static-call arg
    /// handling for the remaining arguments.
    fn fn_value_call(&mut self, e: &Expr, callee: &Expr, args: &[CallArg]) -> Result<Temp, ()> {
        let Some(Ty::Fn(pty, prt)) = self.ty_of(&callee.span) else {
            return self.bad(callee.span, "function-valued call has no signature");
        };
        let obj = self.expr(callee)?;
        self.fn_value_call_from(e, obj, &Ty::Fn(pty, prt), args)
    }

    /// Core dynamic dispatch for a closure-valued callee whose object was
    /// already evaluated (`obj`) together with its `fn` type.
    fn fn_value_call_from(
        &mut self,
        e: &Expr,
        obj: Temp,
        fnty: &Ty,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let Ty::Fn(pty, prt) = fnty else {
            return self.bad(e.span, "function-valued call has no signature");
        };
        let zero = self.int_const(0);
        let addr_boxed = self.extern_call_t1(
            "pickle_obj_slot_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![obj, zero],
        )?;
        let fn_addr = self.extern_call_t1(
            "pickle_unbox_i64",
            vec![IrTy::Ptr],
            IrTy::Int,
            vec![addr_boxed],
        )?;
        let mut ir_params = vec![IrTy::Ptr];
        let mut iargs = vec![obj];
        for (i, a) in args.iter().enumerate() {
            if a.spread {
                return self.bad(a.span, "spread arguments are not lowered yet");
            }
            let src = pty.get(i).cloned();
            let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
            let t = self.borrow_arg(src.as_ref(), a)?;
            let maps_ptr = match src.as_ref() {
                Some(st) => matches!(self.map_ty(st, a.span), Ok(IrTy::Ptr)),
                None => false,
            };
            let t = if !is_ref && maps_ptr {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                self.option_wrap(t, &vt, a.value.span)?
            } else {
                t
            };
            let ir = match src.as_ref() {
                Some(st) => self.map_ty(st, a.span).unwrap_or(IrTy::Ptr),
                None => IrTy::Ptr,
            };
            ir_params.push(ir);
            iargs.push(t);
        }
        let ir_ret = self.map_ty(prt, e.span).unwrap_or(IrTy::Unit);
        let dst = self.temp();
        self.instr(IrInstr::CallInd {
            dst: Some(dst),
            fn_addr,
            params: ir_params,
            ret: ir_ret,
            args: iargs,
        });
        Ok(dst)
    }

    /// A closure object holding `fid`'s code address (boxed) in slot 0 and the
    /// given captures (boxed like list elements) in slots 1..N. The slot layout
    /// must match the hoisted body's reads in `build_lambda_body`.
    fn closure_obj(&mut self, e: &Expr, fid: FuncId, captures: &[Capture]) -> Result<Temp, ()> {
        self.closure_obj_span(e.span, fid, captures)
    }

    /// Span-taking core of `closure_obj`; used where no source expression is at
    /// hand (an `async fn` wrapper synthesizes the future over its parameters).
    fn closure_obj_span(
        &mut self,
        span: Span,
        fid: FuncId,
        captures: &[Capture],
    ) -> Result<Temp, ()> {
        let cid = self.register_closure_class(captures.len(), span)?;
        let cid_t = self.int_const(cid as i64);
        let n_t = self.int_const((captures.len() + 1) as i64);
        let obj = self.extern_call_t1(
            "pickle_class_new",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![cid_t, n_t],
        )?;
        let addr = self.temp();
        self.instr(IrInstr::Const {
            dst: addr,
            c: IrConst::FuncAddr(fid),
        });
        let boxed =
            self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![addr])?;
        let zero = self.int_const(0);
        self.extern_call_void(
            "pickle_obj_slot_set",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
            vec![obj, zero, boxed],
        );
        for (i, cap) in captures.iter().enumerate() {
            let Some(slot) = self.lookup(&cap.name) else {
                return self.bad(
                    cap.span,
                    format!("cannot capture `{}` (not in scope here)", cap.name),
                );
            };
            let v = self.load(slot);
            let vt = self.ty_of(&cap.span).unwrap_or_else(|| cap.ty.clone());
            let rep = self.elem_rep(&cap.ty, cap.span)?;
            let packed = self.pack_for_pointer_boundary(&rep, v, &vt, cap.span)?;
            let boxed = self.box_for_store(&rep, packed, elem_ir(&cap.ty))?;
            let idx = self.int_const((1 + i) as i64);
            self.extern_call_void(
                "pickle_obj_slot_set",
                vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
                vec![obj, idx, boxed],
            );
        }
        Ok(obj)
    }

    /// Lower a lambda expression to a closure object whose slot 0 holds the
    /// address of its pre-registered hoisted body. A lambda declared inside a
    /// generic function body has no module-scope registration: the first time
    /// an instantiation lowers it, a per-instantiation copy is registered under
    /// that enclosing function id (substituting the instantiation into the
    /// signature, captures, and return type).
    fn lambda_value(&mut self, e: &Expr) -> Result<Temp, ()> {
        let inst_key = self.current_fid.map(|f| (Some(f), e.span));
        let base_key = (None, e.span);
        let mut fid = inst_key
            .as_ref()
            .and_then(|k| self.lambda_fids.get(k).copied())
            .or_else(|| self.lambda_fids.get(&base_key).copied());
        if fid.is_none() {
            // A lambda inside a generic class/struct body is rejected at walk
            // time (E0900); if lowering still reaches it, keep the diagnostic
            // in that spirit instead of an uncoded surprise.
            if let Some(cls) = self.generic_owner() {
                return self.bad(
                    e.span,
                    format!("a lambda inside the generic class/struct `{cls}` is not lowered yet"),
                );
            }
            // Inside an instantiated generic body, the walk stashed the
            // captures (and owner); register the copy under this instantiation.
            let inst = match (self.current_fid, self.current_subst.is_empty()) {
                (Some(i), false) => i,
                _ => {
                    return self.bad(e.span, "this lambda was not registered for lowering");
                }
            };
            let caps = self.lambda_caps.get(&base_key).cloned().unwrap_or_default();
            let owner = self.lambda_owners.get(&e.span).copied().flatten();
            let sub = self.instanton_subst.get(&inst).cloned().unwrap_or_default();
            let Some(le) = self.lambda_exprs.get(&e.span).copied() else {
                return self.bad(e.span, "this lambda was not registered for lowering");
            };
            fid = self.register_lambda_at(le, caps, owner, &sub, Some(inst));
        }
        let Some(fid) = fid else {
            return self.bad(e.span, "this lambda was not registered for lowering");
        };
        let caps = inst_key
            .as_ref()
            .and_then(|k| self.lambda_caps.get(k))
            .or_else(|| self.lambda_caps.get(&base_key))
            .cloned()
            .unwrap_or_default();
        self.closure_obj(e, fid, &caps)
    }

    /// A top-level function referenced as a value: wrap its forwarder trampoline
    /// (not the function itself) in a zero-capture closure object, so the
    /// dynamic-call convention -- closure object first, then the real
    /// arguments -- applies to module functions exactly as to hoisted lambda
    /// bodies.
    fn fn_value_closure(&mut self, e: &Expr, _fid: FuncId) -> Result<Temp, ()> {
        let Some(&tramp) = self.fn_tramp.get(&e.span) else {
            return self.bad(
                e.span,
                "function value was not registered for dynamic dispatch",
            );
        };
        self.closure_obj(e, tramp, &[])
    }

    /// An instance method bound to a receiver, used as a value: a one-capture
    /// closure object whose slot 0 is the method trampoline and whose slot 1 is
    /// the receiver object. `fn_value_call` dispatches through it exactly like
    /// a lambda capture (`closure_obj`), except the captured value is the
    /// receiver rather than a checked lambda capture.
    fn bound_method_value(&mut self, e: &Expr, object: &Expr) -> Result<Temp, ()> {
        let Some(&tramp) = self.fn_tramp.get(&e.span) else {
            return self.bad(
                e.span,
                "bound method was not registered for dynamic dispatch",
            );
        };
        let cid = self.register_closure_class(1, e.span)?;
        let cid_t = self.int_const(cid as i64);
        let n_t = self.int_const(2);
        let obj = self.extern_call_t1(
            "pickle_class_new",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![cid_t, n_t],
        )?;
        // Slot 0: the trampoline's address -- the same data slot 0 of a lambda
        // closure holds, so `fn_value_call`'s load-then-call does exactly the
        // same thing.
        let addr = self.temp();
        self.instr(IrInstr::Const {
            dst: addr,
            c: IrConst::FuncAddr(tramp),
        });
        let boxed =
            self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![addr])?;
        let zero = self.int_const(0);
        self.extern_call_void(
            "pickle_obj_slot_set",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
            vec![obj, zero, boxed],
        );
        // Slot 1: the receiver, stored as its identity pointer (a managed
        // class object; `single_boxed` passes it through).
        let recv = self.expr(object)?;
        let recv_ty = self.ty_of(&object.span).unwrap_or(Ty::Unknown);
        let packed = self.pack_for_pointer_boundary(&ElemRep::Ptr, recv, &recv_ty, e.span)?;
        let one = self.int_const(1);
        self.extern_call_void(
            "pickle_obj_slot_set",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
            vec![obj, one, packed],
        );
        Ok(obj)
    }

    fn call(&mut self, e: &Expr, callee: &Expr, args: &[CallArg]) -> Result<Temp, ()> {
        if let ExprKind::Member { object, name } = &callee.kind {
            // Testing-framework assertions: `expect(v).toBe(w)` and friends,
            // with an optional `.not()` in between.
            if is_expect_method(name) {
                if let Some((value, negated)) = self.peel_expect(object) {
                    return self.expect_assert(e, value, name, args, negated);
                }
            }
            // `.free()` on a `#[manualAlloc]` binding releases the object.
            if name == "free" {
                if let ExprKind::Ident(id) = &object.kind {
                    if self.is_manual(id) {
                        if !args.is_empty() {
                            return self.bad(e.span, "`free()` takes no arguments");
                        }
                        let obj = self.expr(object)?;
                        self.extern_call_void("pickle_manual_free", vec![IrTy::Ptr], vec![obj]);
                        return Ok(self.unit_temp());
                    }
                }
            }
            // `Enum.Variant(...)` constructor call.
            if let ExprKind::Ident(enum_name) = &object.kind {
                if let Some(TypeTableEntry::Enum(t)) = self.resolved.types.get(enum_name) {
                    return self.enum_ctor(e, t, name, args);
                }
            }
            return self.method_call(e, object, name, args);
        }
        // A generic call `name<Type,...>(args)`: monomorphize the callee at this
        // call site and emit a static call to the instantiation.
        if let ExprKind::GenericCall { name, type_args } = &callee.kind {
            return self.generic_call(e, name, type_args, args);
        }
        // A generic function called without explicit type arguments: infer them
        // from the arguments, then monomorphize exactly like the explicit form.
        if let ExprKind::Ident(cname) = &callee.kind {
            if let Some(info) = self
                .resolved
                .fns
                .get(cname)
                .and_then(|infos| {
                    infos
                        .iter()
                        .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
                })
                .cloned()
            {
                if !info.generics.is_empty() {
                    let arg_tys = self.infer_generic_fn_args(e.span, &info, args)?;
                    return self.generic_fn_call(e, &info, arg_tys, args);
                }
            }
        }
        // A bare instance method of the enclosing class: call it on the
        // implicit receiver (`this`), exactly like `this.name(...)`. Statics
        // resolve by the declaring class alone. Overridden methods dispatch
        // on the runtime class id just like an explicit receiver call.
        if let Some(cid) = self.owner {
            if let ExprKind::Ident(mname) = &callee.kind {
                if let Some(&(fid, is_static)) = self.method_ids.get(&(cid as u32, mname.clone())) {
                    if is_static {
                        return self.call_method(e, fid, args, None);
                    }
                    let this = self.this_value(e)?;
                    let virtual_branches = self
                        .virtual_dispatch
                        .get(&(cid as u32, mname.clone()))
                        .cloned();
                    if let Some(branches) = virtual_branches {
                        if !branches.is_empty() {
                            return self.virtual_method_call(e, fid, args, this, &branches);
                        }
                    }
                    return self.call_method(e, fid, args, Some(this));
                }
            }
        }
        // A function-valued callee that is not a plain named call (a closure
        // variable, a function-typed parameter, a call result, ...) dispatches
        // dynamically through the closure's stored address. Member callees and
        // statically-resolvable name calls were handled above, so this branch
        // only ever sees values of `fn` type.
        if self.is_fn_dispatchable(callee) {
            return self.fn_value_call(e, callee, args);
        }
        let ExprKind::Ident(name) = &callee.kind else {
            return self.bad(e.span, "only plain function calls are lowered yet");
        };
        // User function?
        if let Some(&fid) = self.module.funcs_by_name.get(name) {
            if args.iter().any(|a| a.spread) {
                let pty = self.src_param_tys.get(&fid).cloned().unwrap_or_default();
                let ret = self
                    .finfo
                    .get(&fid)
                    .map(|i| i.ret.clone())
                    .unwrap_or(Ty::Unknown);
                return self.spread_static_call(
                    e,
                    SpreadCall {
                        span: e.span,
                        fid,
                        pty: &pty,
                        ret: &ret,
                        receiver: None,
                        args,
                    },
                );
            }
            let fparams = self.module.funcs[fid.0].params.clone();
            let mut arg_temps = Vec::new();
            for (i, a) in args.iter().enumerate() {
                let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
                let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
                let t = self.borrow_arg(src.as_ref(), a)?;
                let t = if !is_ref && matches!(fparams.get(i).map(|p| p.ty), Some(IrTy::Ptr)) {
                    let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                    self.option_wrap(t, &vt, a.value.span)?
                } else {
                    t
                };
                arg_temps.push(t);
            }
            let dst = self.temp();
            self.instr(IrInstr::Call {
                dst: Some(dst),
                callee: Callee::Func(fid),
                args: arg_temps,
            });
            self.guard_propagation(fid, e.span)?;
            return Ok(dst);
        }
        // Class/struct constructor call: `TypeName(arg...)`.
        if let Some(&cid) = self.class_by_name.get(name) {
            let Some(&fid) = self.ctor_ids.get(&cid) else {
                return self.bad(e.span, format!("`{name}` has no constructor"));
            };
            if args.iter().any(|a| a.spread) {
                let pty = self.src_param_tys.get(&fid).cloned().unwrap_or_default();
                return self.spread_static_call(
                    e,
                    SpreadCall {
                        span: e.span,
                        fid,
                        pty: &pty,
                        ret: &Ty::Class(name.to_string(), Vec::new()),
                        receiver: None,
                        args,
                    },
                );
            }
            let fparams = self.module.funcs[fid.0].params.clone();
            let mut arg_temps = Vec::new();
            for (i, a) in args.iter().enumerate() {
                let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
                let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
                let t = self.borrow_arg(src.as_ref(), a)?;
                let t = if !is_ref && matches!(fparams.get(i).map(|p| p.ty), Some(IrTy::Ptr)) {
                    let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                    self.option_wrap(t, &vt, a.value.span)?
                } else {
                    t
                };
                arg_temps.push(t);
            }
            let dst = self.temp();
            self.instr(IrInstr::Call {
                dst: Some(dst),
                callee: Callee::Func(fid),
                args: arg_temps,
            });
            self.guard_propagation(fid, e.span)?;
            return Ok(dst);
        }
        // Builtins.
        match name.as_str() {
            "assert" => {
                if args.is_empty() || args.len() > 2 {
                    return self.bad(
                        e.span,
                        "`assert(cond, msg?)` takes a condition and an optional message",
                    );
                }
                let cond = self.expr(&args[0].value)?;
                // `if !cond { pickle_test_fail_obj(msg); }` -- the failure is
                // recorded (test mode) or fatal (program mode), never unwound
                // across the JIT frame.
                let cont = self.new_block();
                let fail = self.new_block();
                self.term(IrTerm::BranchIf {
                    cond,
                    then: cont,
                    else_: fail,
                });
                self.cur = fail;
                let msg = match args.get(1) {
                    Some(arg) => self.expr(&arg.value)?,
                    None => {
                        let t = self.temp();
                        self.instr(IrInstr::Const {
                            dst: t,
                            c: IrConst::Null,
                        });
                        t
                    }
                };
                self.extern_call_void("pickle_test_fail_obj", vec![IrTy::Ptr], vec![msg]);
                self.term(IrTerm::Branch { target: cont });
                self.cur = cont;
                Ok(self.unit_temp())
            }
            "expect" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`expect(value)` takes exactly one value");
                }
                self.expr(&args[0].value)
            }
            "print" | "println" => {
                let newline = name == "println";
                for a in args {
                    if a.spread {
                        let a_ty = self.ty_of(&a.value.span);
                        let Some(Ty::List(inner)) = a_ty else {
                            return self
                                .bad(a.value.span, "spread argument in `print` must be a `List`");
                        };
                        let sp_t = self.expr(&a.value)?;
                        self.print_spread_list(a.value.span, sp_t, &inner)?;
                        continue;
                    }
                    let t = self.expr(&a.value)?;
                    let a_ty = self.ty_of(&a.value.span);
                    let sym: &str = match a_ty {
                        Some(Ty::Int) => "pickle_print_i64",
                        Some(Ty::Float) => "pickle_print_f64",
                        Some(Ty::Bool) => "pickle_print_bool",
                        Some(Ty::Byte) => "pickle_print_byte",
                        Some(Ty::Char) => "pickle_print_char",
                        Some(Ty::String) => "pickle_print_obj",
                        Some(Ty::List(_)) | Some(Ty::Map(_, _)) | Some(Ty::Enum(..))
                        | Some(Ty::Class(..)) | Some(Ty::Struct(..)) => "pickle_print_obj",
                        _ => return self.bad(a.value.span, "unsupported `print` argument type"),
                    };
                    let pty = match a_ty {
                        Some(Ty::Int) => IrTy::Int,
                        Some(Ty::Float) => IrTy::Float,
                        Some(Ty::Bool) => IrTy::Bool,
                        Some(Ty::Byte) => IrTy::Int,
                        Some(Ty::Char) => IrTy::Char,
                        Some(Ty::String) | Some(Ty::List(_)) | Some(Ty::Map(_, _))
                        | Some(Ty::Enum(..)) | Some(Ty::Class(..)) | Some(Ty::Struct(..)) => {
                            IrTy::Ptr
                        }
                        _ => IrTy::Ptr,
                    };
                    self.extern_call_void(sym, vec![pty], vec![t]);
                }
                if newline {
                    self.extern_call_void("pickle_print_newline", vec![], vec![]);
                }
                Ok(self.unit_temp())
            }
            "alloc" => {
                if args.len() != 2 {
                    return self.bad(e.span, "`alloc(T, count)` takes two arguments");
                }
                let stride = match self.ty_of(&args[0].value.span) {
                    Some(Ty::Int) | Some(Ty::Float) => 8,
                    Some(Ty::Char) => 4,
                    Some(Ty::Bool) => 1,
                    _ => {
                        return self.bad(
                            args[0].span,
                            "`alloc` currently only supports scalar element types (int, float, bool, char)",
                        )
                    }
                };
                let count = self.expr(&args[1].value)?;
                let s = self.int_const(stride);
                let size = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: size,
                    op: IrBinOp::Mul,
                    a: count,
                    b: s,
                });
                self.extern_call_t1("pickle_raw_alloc", vec![IrTy::Int], IrTy::Int, vec![size])
            }
            "free" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`free(p)` takes one argument");
                }
                let p = self.expr(&args[0].value)?;
                self.extern_call_void("pickle_raw_free", vec![IrTy::Int], vec![p]);
                Ok(self.unit_temp())
            }
            "len" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`len` takes one argument");
                }
                let t = self.expr(&args[0].value)?;
                let a_ty = self.ty_of(&args[0].value.span);
                match a_ty {
                    Some(Ty::String) => {
                        self.extern_call_t1("pickle_str_len", vec![IrTy::Str], IrTy::Int, vec![t])
                    }
                    Some(Ty::List(_) | Ty::Range(_)) => {
                        self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![t])
                    }
                    Some(Ty::Map(_, _)) => {
                        self.extern_call_t1("pickle_map_len", vec![IrTy::Ptr], IrTy::Int, vec![t])
                    }
                    _ => self.bad(e.span, "`len` over this type is not lowered yet"),
                }
            }
            "abs" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`abs(x)` takes exactly one argument");
                }
                let v = self.expr(&args[0].value)?;
                let vit = self.irty(args[0].value.span)?;
                let zero = match vit {
                    IrTy::Int => {
                        let t = self.temp();
                        self.instr(IrInstr::Const {
                            dst: t,
                            c: IrConst::Int(0),
                        });
                        t
                    }
                    IrTy::Float => {
                        let t = self.temp();
                        self.instr(IrInstr::Const {
                            dst: t,
                            c: IrConst::Float(0.0f64.to_bits()),
                        });
                        t
                    }
                    _ => {
                        return self.bad(
                            args[0].value.span,
                            "`abs` requires an `int` or `float` argument",
                        )
                    }
                };
                let is_neg = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: is_neg,
                    op: IrBinOp::Lt,
                    a: v,
                    b: zero,
                });
                let neg_v = self.temp();
                self.instr(IrInstr::UnOp {
                    dst: neg_v,
                    op: IrUnOp::Neg,
                    v,
                });
                // `is_neg ? -v : v`
                let res_slot = self.new_slot(vit);
                let neg_b = self.new_block();
                let pos_b = self.new_block();
                let join = self.new_block();
                self.term(IrTerm::BranchIf {
                    cond: is_neg,
                    then: neg_b,
                    else_: pos_b,
                });
                self.cur = neg_b;
                self.instr(IrInstr::StoreSlot {
                    slot: res_slot,
                    v: neg_v,
                });
                self.term(IrTerm::Branch { target: join });
                self.cur = pos_b;
                self.instr(IrInstr::StoreSlot { slot: res_slot, v });
                self.term(IrTerm::Branch { target: join });
                self.cur = join;
                Ok(self.load(res_slot))
            }
            "min" | "max" => {
                if args.len() != 2
                    || args[0].name.is_some()
                    || args[0].spread
                    || args[1].name.is_some()
                    || args[1].spread
                {
                    return self.bad(
                        e.span,
                        format!("`{name}(a, b)` takes exactly two arguments"),
                    );
                }
                let it = self.irty(args[0].value.span)?;
                if !matches!(it, IrTy::Int | IrTy::Float) {
                    return self.bad(e.span, "`min`/`max` requires `int` or `float` arguments");
                }
                let a = self.expr(&args[0].value)?;
                let b = self.expr(&args[1].value)?;
                let pick_a = self.temp();
                let op = if name == "min" {
                    IrBinOp::Lt
                } else {
                    IrBinOp::Gt
                };
                self.instr(IrInstr::BinOp {
                    dst: pick_a,
                    op,
                    a,
                    b,
                });
                // `pick_a ? a : b`
                let res_slot = self.new_slot(it);
                let a_b = self.new_block();
                let b_b = self.new_block();
                let join = self.new_block();
                self.term(IrTerm::BranchIf {
                    cond: pick_a,
                    then: a_b,
                    else_: b_b,
                });
                self.cur = a_b;
                self.instr(IrInstr::StoreSlot {
                    slot: res_slot,
                    v: a,
                });
                self.term(IrTerm::Branch { target: join });
                self.cur = b_b;
                self.instr(IrInstr::StoreSlot {
                    slot: res_slot,
                    v: b,
                });
                self.term(IrTerm::Branch { target: join });
                self.cur = join;
                Ok(self.load(res_slot))
            }
            "clamp" => {
                if args.len() != 3 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(e.span, "`clamp(x, lo, hi)` takes exactly three arguments");
                }
                let it = self.irty(args[0].value.span)?;
                if !matches!(it, IrTy::Int | IrTy::Float) {
                    return self.bad(e.span, "`clamp` requires `int` or `float` arguments");
                }
                let x = self.expr(&args[0].value)?;
                let lo = self.expr(&args[1].value)?;
                let hi = self.expr(&args[2].value)?;
                // clamp(x, lo, hi) = min(max(x, lo), hi)
                let max_cond = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: max_cond,
                    op: IrBinOp::Gt,
                    a: x,
                    b: lo,
                });
                let mid = self.new_slot(it);
                let take_x = self.new_block();
                let take_lo = self.new_block();
                let max_join = self.new_block();
                self.term(IrTerm::BranchIf {
                    cond: max_cond,
                    then: take_x,
                    else_: take_lo,
                });
                self.cur = take_x;
                self.instr(IrInstr::StoreSlot { slot: mid, v: x });
                self.term(IrTerm::Branch { target: max_join });
                self.cur = take_lo;
                self.instr(IrInstr::StoreSlot { slot: mid, v: lo });
                self.term(IrTerm::Branch { target: max_join });
                self.cur = max_join;
                let maxv = self.load(mid);
                let min_cond = self.temp();
                self.instr(IrInstr::BinOp {
                    dst: min_cond,
                    op: IrBinOp::Lt,
                    a: maxv,
                    b: hi,
                });
                let res_slot = self.new_slot(it);
                let take_max = self.new_block();
                let take_hi = self.new_block();
                let join = self.new_block();
                self.term(IrTerm::BranchIf {
                    cond: min_cond,
                    then: take_max,
                    else_: take_hi,
                });
                self.cur = take_max;
                self.instr(IrInstr::StoreSlot {
                    slot: res_slot,
                    v: maxv,
                });
                self.term(IrTerm::Branch { target: join });
                self.cur = take_hi;
                self.instr(IrInstr::StoreSlot {
                    slot: res_slot,
                    v: hi,
                });
                self.term(IrTerm::Branch { target: join });
                self.cur = join;
                Ok(self.load(res_slot))
            }
            "str" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`str(x)` takes exactly one argument");
                }
                let v = self.expr(&args[0].value)?;
                if matches!(self.ty_of(&args[0].value.span), Some(Ty::List(inner)) if matches!(&*inner, Ty::Byte))
                {
                    return self.extern_call_t1(
                        "pickle_str_from_list",
                        vec![IrTy::Ptr],
                        IrTy::Str,
                        vec![v],
                    );
                }
                match self.irty(args[0].value.span)? {
                    IrTy::Int => self.extern_call_t1(
                        "pickle_str_from_i64",
                        vec![IrTy::Int],
                        IrTy::Str,
                        vec![v],
                    ),
                    IrTy::Float => self.extern_call_t1(
                        "pickle_str_from_f64",
                        vec![IrTy::Float],
                        IrTy::Str,
                        vec![v],
                    ),
                    IrTy::Bool => self.extern_call_t1(
                        "pickle_str_from_bool",
                        vec![IrTy::Bool],
                        IrTy::Str,
                        vec![v],
                    ),
                    IrTy::Char => self.extern_call_t1(
                        "pickle_str_from_char",
                        vec![IrTy::Char],
                        IrTy::Str,
                        vec![v],
                    ),
                    _ => self.bad(
                        e.span,
                        "`str` requires an `int`, `float`, `bool`, `char`, or `List<byte>`",
                    ),
                }
            }
            "bytes" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`bytes(s)` takes exactly one argument");
                }
                let v = self.expr(&args[0].value)?;
                self.extern_call_t1("pickle_str_to_bytes", vec![IrTy::Str], IrTy::Ptr, vec![v])
            }
            "range" => {
                if args.is_empty() || args.len() > 3 {
                    return self.bad(
                        e.span,
                        "`range(end)`, `range(start, end)`, or `range(start, end, step)` expected",
                    );
                }
                for a in args {
                    if a.name.is_some() || a.spread {
                        return self.bad(a.span, "`range` takes only plain positional arguments");
                    }
                }
                let int0 = self.int_const(0);
                let int1 = self.int_const(1);
                let (start, end, step) = match args.len() {
                    1 => (int0, self.expr(&args[0].value)?, int1),
                    2 => (self.expr(&args[0].value)?, self.expr(&args[1].value)?, int1),
                    _ => (
                        self.expr(&args[0].value)?,
                        self.expr(&args[1].value)?,
                        self.expr(&args[2].value)?,
                    ),
                };
                self.extern_call_t1(
                    "pickle_range",
                    vec![IrTy::Int, IrTy::Int, IrTy::Int],
                    IrTy::Ptr,
                    vec![start, end, step],
                )
            }
            "read_file" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`read_file(path)` takes exactly one argument");
                }
                let p = self.expr(&args[0].value)?;
                // Returns the whole file as a `string` object, or `null`
                // (`none`) when the file cannot be read.
                self.extern_call_t1("pickle_read_file", vec![IrTy::Str], IrTy::Ptr, vec![p])
            }
            "write_file" => {
                if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(
                        e.span,
                        "`write_file(path, text)` takes exactly two arguments",
                    );
                }
                let p = self.expr(&args[0].value)?;
                let t = self.expr(&args[1].value)?;
                self.extern_call_t1(
                    "pickle_write_file",
                    vec![IrTy::Str, IrTy::Str],
                    IrTy::Bool,
                    vec![p, t],
                )
            }
            "file_exists" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`file_exists(path)` takes exactly one argument");
                }
                let p = self.expr(&args[0].value)?;
                self.extern_call_t1("pickle_file_exists", vec![IrTy::Str], IrTy::Bool, vec![p])
            }
            "delete" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`delete(path)` takes exactly one argument");
                }
                let p = self.expr(&args[0].value)?;
                self.extern_call_t1("pickle_delete", vec![IrTy::Str], IrTy::Bool, vec![p])
            }
            "mkdir" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`mkdir(path)` takes exactly one argument");
                }
                let p = self.expr(&args[0].value)?;
                self.extern_call_t1("pickle_mkdir", vec![IrTy::Str], IrTy::Bool, vec![p])
            }
            "list_dir" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`list_dir(path)` takes exactly one argument");
                }
                let p = self.expr(&args[0].value)?;
                // Returns a `List<string>` of child paths, or `null` (`none`)
                // when the directory cannot be read.
                self.extern_call_t1("pickle_list_dir", vec![IrTy::Str], IrTy::Ptr, vec![p])
            }
            "stream_open_read" | "stream_open_write" | "stream_open_append" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, format!("`{name}(path)` takes exactly one argument"));
                }
                let p = self.expr(&args[0].value)?;
                // Returns a `Stream` object pointer, or `null` (`none`) when
                // the path cannot be opened.
                let sym = match name.as_str() {
                    "stream_open_read" => "pickle_stream_open_read",
                    "stream_open_write" => "pickle_stream_open_write",
                    _ => "pickle_stream_open_append",
                };
                self.extern_call_t1(sym, vec![IrTy::Str], IrTy::Ptr, vec![p])
            }
            "stdout_stream" | "stderr_stream" => {
                if !args.is_empty() {
                    return self.bad(e.span, format!("`{name}()` takes no arguments"));
                }
                let sym = if name == "stdout_stream" {
                    "pickle_stdout_stream"
                } else {
                    "pickle_stderr_stream"
                };
                self.extern_call_t1(sym, vec![], IrTy::Ptr, vec![])
            }
            "args" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`args()` takes no arguments");
                }
                // Returns a `List<string>` of the program's command-line
                // arguments, excluding the program name.
                self.extern_call_t1("pickle_args", vec![], IrTy::Ptr, vec![])
            }
            "exit" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(e.span, "`exit(code)` takes exactly one argument");
                }
                let code = self.expr(&args[0].value)?;
                self.extern_call_void("pickle_exit", vec![IrTy::Int], vec![code]);
                Ok(self.unit_temp())
            }
            "run_cmd" => {
                if args.len() != 8 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(
                        e.span,
                        "`run_cmd(argv, cwd, envs, env_removes, stdin_bytes, max_out, timeout_ms, inherit_io)` takes exactly eight arguments",
                    );
                }
                let mut temps = Vec::with_capacity(8);
                for a in args {
                    let t = self.expr(&a.value)?;
                    // Optional `int` operands (max_out, timeout_ms) are passed
                    // as their boxed pointer; `none` already lowers to null.
                    let t = if matches!(self.ty_of(&a.value.span), Some(Ty::Int)) {
                        self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![t])?
                    } else {
                        t
                    };
                    temps.push(t);
                }
                // Returns `(int?, string, string, bool)` as a tuple object, or
                // `null` (`none`) when the program cannot be spawned.
                self.extern_call_t1(
                    "pickle_run_cmd",
                    vec![
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Ptr,
                        IrTy::Bool,
                    ],
                    IrTy::Ptr,
                    temps,
                )
            }
            "run_spawn" => {
                if args.len() != 5 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(
                        e.span,
                        "`run_spawn(argv, cwd, envs, env_removes, max_out)` takes exactly five arguments",
                    );
                }
                let mut temps = Vec::with_capacity(5);
                for a in args {
                    let t = self.expr(&a.value)?;
                    // Optional `int` operand (max_out) is passed boxed; `none`
                    // already lowers to null.
                    let t = if matches!(self.ty_of(&a.value.span), Some(Ty::Int)) {
                        self.extern_call_t1("pickle_box_i64", vec![IrTy::Int], IrTy::Ptr, vec![t])?
                    } else {
                        t
                    };
                    temps.push(t);
                }
                // Returns a `Stream` handle object, or `null` (`none`) when
                // the program cannot be spawned.
                self.extern_call_t1(
                    "pickle_run_spawn",
                    vec![IrTy::Ptr, IrTy::Ptr, IrTy::Ptr, IrTy::Ptr, IrTy::Ptr],
                    IrTy::Ptr,
                    temps,
                )
            }
            "proc_wait" | "proc_running" | "proc_code" | "proc_kill" | "proc_in_close" => {
                if args.len() != 1 || args[0].name.is_some() || args[0].spread {
                    return self.bad(
                        e.span,
                        format!("`{name}(handle)` takes exactly one argument"),
                    );
                }
                let h = self.expr(&args[0].value)?;
                // `proc_code` returns an `int` box or `null` (`none`); the
                // rest return a `bool` (or `true`/`false` semantics).
                let sym = match name.as_str() {
                    "proc_wait" => "pickle_proc_wait",
                    "proc_running" => "pickle_proc_running",
                    "proc_kill" => "pickle_proc_kill",
                    "proc_in_close" => "pickle_proc_in_close",
                    _ => "pickle_proc_code",
                };
                let ret = if name == "proc_code" {
                    IrTy::Ptr
                } else {
                    IrTy::Bool
                };
                self.extern_call_t1(sym, vec![IrTy::Ptr], ret, vec![h])
            }
            "proc_out_read" | "proc_err_read" => {
                if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(
                        e.span,
                        format!("`{name}(handle, n)` takes exactly two arguments"),
                    );
                }
                let h = self.expr(&args[0].value)?;
                let n = self.expr(&args[1].value)?;
                let sym = if name == "proc_out_read" {
                    "pickle_proc_out_read"
                } else {
                    "pickle_proc_err_read"
                };
                // Returns a `List<byte>` object, or `null` (`none`) at EOF.
                self.extern_call_t1(sym, vec![IrTy::Ptr, IrTy::Int], IrTy::Ptr, vec![h, n])
            }
            "proc_in_write" => {
                if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(
                        e.span,
                        "`proc_in_write(handle, b)` takes exactly two arguments",
                    );
                }
                let h = self.expr(&args[0].value)?;
                let b = self.expr(&args[1].value)?;
                self.extern_call_t1(
                    "pickle_proc_in_write",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    IrTy::Int,
                    vec![h, b],
                )
            }
            "current_dir" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`current_dir()` takes no arguments");
                }
                // Returns the working directory as a `string` object, or
                // `null` (`none`) when it cannot be determined.
                self.extern_call_t1("pickle_current_dir", vec![], IrTy::Ptr, vec![])
            }
            _ if self.flat_builtin(name).is_some() => {
                // Tasks and synchronisation: fixed-arity calls with no special
                // lowering, described by a table rather than two dozen
                // near-identical arms.
                let (sym, params, ret, names) = self.flat_builtin(name).unwrap();
                let args = self.positional_args(e, name, args, names)?;
                match ret {
                    Some(ret) => self.extern_call_t1(sym, params, ret, args),
                    None => {
                        self.extern_call_void(sym, params, args);
                        Ok(self.unit_temp())
                    }
                }
            }
            "captureBegin" => {
                if !args.is_empty() || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(e.span, "`captureBegin()` takes no arguments");
                }
                self.extern_call_void("pickle_test_capture_begin", vec![], vec![]);
                Ok(self.unit_temp())
            }
            "captureTake" => {
                if !args.is_empty() || args.iter().any(|a| a.name.is_some() || a.spread) {
                    return self.bad(e.span, "`captureTake()` takes no arguments");
                }
                // Returns the captured failure message as a `string` object,
                // or `null` (`none`) when there is none.
                self.extern_call_t1("pickle_test_capture_take", vec![], IrTy::Ptr, vec![])
            }
            _ if self.generic_user_fn(name) => self.bad(
                e.span,
                format!(
                    "calls to the generic function `{name}` must specify its type arguments \
                     (`{name}<T,...>(...)`); argument type inference is not lowered yet"
                ),
            ),
            _ => self.bad(e.span, format!("`{name}` is not lowered yet")),
        }
    }

    /// Whether `name` resolves to a top-level generic user function (as opposed
    /// to a builtin or a non-generic overridden callable).
    fn generic_user_fn(&self, name: &str) -> bool {
        self.resolved
            .fns
            .get(name)
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
            })
            .is_some_and(|c| !c.generics.is_empty())
    }

    /// The tasks and synchronisation builtins, as
    /// `(runtime symbol, argument IR types, result IR type, argument names)`.
    /// A `None` result means the call returns nothing.
    ///
    /// Every handle here is an `int` naming a row in a runtime table (a task, a
    /// mutex, a condition variable, ...), and a task body or `once` body is a
    /// zero-argument `fn` value — a closure object the runtime calls with the
    /// object as its hidden environment argument. Nothing in this table needs a
    /// closure conversion or a boxing step, which is why it can be a table: the
    /// work is all in the runtime.
    fn flat_builtin(&self, name: &str) -> Option<FlatBuiltin> {
        use IrTy::{Bool, Float, Int, Ptr};
        const INT: &[IrTy] = &[Int];
        const NONE: Option<IrTy> = None;
        Some(match name {
            // Tasks. `thread_spawn` takes the closure object itself, not a call
            // to it: the new OS thread runs it, so the spawn cannot lower it.
            "thread_spawn" => ("pickle_thread_spawn", vec![Ptr], Some(Int), &["body"]),
            "thread_join" => ("pickle_thread_join", INT.to_vec(), Some(Bool), &["task"]),
            "thread_finished" => (
                "pickle_thread_finished",
                INT.to_vec(),
                Some(Bool),
                &["task"],
            ),
            // A `string?` message: `null` when the task is running or was clean.
            "thread_message" => (
                "pickle_thread_panic_message",
                INT.to_vec(),
                Some(Ptr),
                &["task"],
            ),
            "thread_count" => ("pickle_thread_count", vec![], Some(Int), &[]),
            "thread_current" => ("pickle_thread_current", vec![], Some(Int), &[]),
            "sleep_ms" => ("pickle_thread_sleep_ms", INT.to_vec(), NONE, &["ms"]),
            "thread_yield" => ("pickle_thread_yield", vec![], NONE, &[]),
            // Synchronisation.
            "mutex_new" => ("pickle_mutex_new", vec![], Some(Int), &[]),
            "mutex_lock" => ("pickle_mutex_lock", INT.to_vec(), NONE, &["m"]),
            "mutex_trylock" => ("pickle_mutex_trylock", INT.to_vec(), Some(Bool), &["m"]),
            "mutex_unlock" => ("pickle_mutex_unlock", INT.to_vec(), NONE, &["m"]),
            "cond_new" => ("pickle_cond_new", vec![], Some(Int), &[]),
            "cond_signal" => ("pickle_cond_signal", INT.to_vec(), NONE, &["c"]),
            "cond_broadcast" => ("pickle_cond_broadcast", INT.to_vec(), NONE, &["c"]),
            "cond_wait" => ("pickle_cond_wait", vec![Int, Int], NONE, &["c", "m"]),
            "sem_new" => ("pickle_sem_new", INT.to_vec(), Some(Int), &["permits"]),
            "sem_acquire" => ("pickle_sem_acquire", vec![Int, Int], NONE, &["s", "n"]),
            "sem_try_acquire" => (
                "pickle_sem_try_acquire",
                vec![Int, Int],
                Some(Bool),
                &["s", "n"],
            ),
            "sem_release" => ("pickle_sem_release", vec![Int, Int], NONE, &["s", "n"]),
            "atomic_new" => ("pickle_atomic_new", INT.to_vec(), Some(Int), &["value"]),
            "atomic_load" => ("pickle_atomic_load", INT.to_vec(), Some(Int), &["a"]),
            "atomic_store" => ("pickle_atomic_store", vec![Int, Int], NONE, &["a", "value"]),
            "atomic_add" => (
                "pickle_atomic_add",
                vec![Int, Int],
                Some(Int),
                &["a", "delta"],
            ),
            "atomic_cas" => (
                "pickle_atomic_cas",
                vec![Int, Int, Int],
                Some(Bool),
                &["a", "expected", "value"],
            ),
            "once_new" => ("pickle_once_new", vec![], Some(Int), &[]),
            "once_done" => ("pickle_once_done", INT.to_vec(), Some(Bool), &["o"]),
            "once_run" => (
                "pickle_once_run",
                vec![Int, Ptr],
                Some(Bool),
                &["o", "body"],
            ),
            // Channels. Values cross the ABI as raw integers, boxed only by the
            // runtime when a receiver takes one out; a `try_recv` may come back
            // null (`none`).
            "channel_new" => ("pickle_channel_new", INT.to_vec(), Some(Int), &["cap"]),
            "channel_cap" => ("pickle_channel_cap", INT.to_vec(), Some(Int), &["ch"]),
            "channel_len" => ("pickle_channel_len", INT.to_vec(), Some(Int), &["ch"]),
            "channel_send" => (
                "pickle_channel_send",
                vec![Int, Int],
                NONE,
                &["ch", "value"],
            ),
            "channel_try_send" => (
                "pickle_channel_try_send",
                vec![Int, Int],
                Some(Bool),
                &["ch", "value"],
            ),
            "channel_recv" => ("pickle_channel_recv", INT.to_vec(), Some(Int), &["ch"]),
            "channel_try_recv" => ("pickle_channel_try_recv", INT.to_vec(), Some(Ptr), &["ch"]),
            "channel_close" => ("pickle_channel_close", INT.to_vec(), NONE, &["ch"]),
            "channel_closed" => ("pickle_channel_closed", INT.to_vec(), Some(Bool), &["ch"]),
                // Bit-reinterpret and class-name primitives. Both exist for the
                // `pickle_print_obj` port in `pickle/runtime/abi_console.pkl`.
                //
                // `f64FromBits` has no source-level spelling: `x as float` is a
                // *numeric* conversion, not a bitcast (verified - `1072693248 as
                // float` yields `1072693248`, not `0.5`). It is the exact inverse
                // of the boxing side, which stores `v.to_bits()`.
                //
                // `className` is the user-class fallback arm's lookup: the
                // collector holds the descriptor name table, and without a symbol
                // the arm had nothing to call. Returns a `string` object or
                // `none`.
                "f64FromBits" => ("pickle_f64_from_bits", INT.to_vec(), Some(Float), &["bits"]),
                "className" => ("pickle_class_name", INT.to_vec(), Some(Ptr), &["classId"]),
            // Garbage collection. `bool` crosses the ABI as the C `bool` the
            // runtime extern takes, so it needs no boxing.
            "gcCollect" => ("pickle_gc_collect", vec![Bool], NONE, &["force"]),
            // Observable GC contract: the counters/threshold that make the
            // auto-collect decision drivable from Pickle (gc.rs:714/720/727).
            // All three return/take `u32` across the ABI, so `Int` unboxed.
            "gcAllocationsSinceGc" => ("pickle_gc_allocations_since_gc", vec![], Some(Int), &[]),
            "gcBytesSinceGc" => ("pickle_gc_bytes_since_gc", vec![], Some(Int), &[]),
            "gcCollectionCount" => ("pickle_gc_collection_count", vec![], Some(Int), &[]),
            "gcSetThreshold" => ("pickle_gc_set_threshold", vec![Int], NONE, &["bytes"]),
            _ => return None,
        })
    }

    /// Check a fixed-arity builtin's arguments and evaluate them left to right.
    fn positional_args(
        &mut self,
        e: &Expr,
        name: &str,
        args: &[CallArg],
        params: &[&str],
    ) -> Result<Vec<Temp>, ()> {
        if args.len() != params.len() || args.iter().any(|a| a.name.is_some() || a.spread) {
            let word = if params.len() == 1 {
                "argument"
            } else {
                "arguments"
            };
            return self.bad(
                e.span,
                format!(
                    "`{name}({})` takes exactly {} {word}",
                    params.join(", "),
                    params.len()
                ),
            );
        }
        let mut temps = Vec::with_capacity(args.len());
        for a in args {
            temps.push(self.expr(&a.value)?);
        }
        Ok(temps)
    }

    /// Lower `name<Type,...>(args)`. The call is monomorphized: the generic
    /// callee is instantiated over the resolved type arguments (substituted
    /// under the enclosing instantiation, if any), and the call is emitted as
    /// a static call to that concrete function.
    fn generic_call(
        &mut self,
        e: &Expr,
        name: &str,
        type_args: &[TypeExpr],
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        // `Box<int>(...)`: a generic class/struct constructor call. The name
        // shares the generic-call surface with generic functions, but
        // resolves through the type table.
        if let Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) =
            self.resolved.types.get(name)
        {
            return self.generic_class_call(e, name, t, type_args, args);
        }
        let Some(info) = self
            .resolved
            .fns
            .get(name)
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
            })
            .cloned()
        else {
            return self.bad(e.span, format!("unknown generic function `{name}`"));
        };
        let arg_tys: Vec<Ty> = type_args
            .iter()
            .map(|te| {
                let t = self.resolved.resolve_ty(te, &self.fn_generics, self.diags);
                self.subst_ty(t)
            })
            .collect();
        if arg_tys.len() != info.generics.len() {
            return self.bad(
                e.span,
                format!(
                    "`{name}` takes {} type argument(s), found {}",
                    info.generics.len(),
                    arg_tys.len()
                ),
            );
        }
        self.generic_fn_call(e, &info, arg_tys, args)
    }

    /// Core of a monomorphized generic-function call, shared by the explicit
    /// `name<T,...>(args)` form and the argument-driven `name(args)` form.
    fn generic_fn_call(
        &mut self,
        e: &Expr,
        info: &CallableInfo,
        arg_tys: Vec<Ty>,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let name = info.name.clone();
        if arg_tys.iter().any(|t| t.has_var()) {
            return self.bad(
                e.span,
                format!(
                    "cannot instantiate `{name}` with unresolved type arguments; \
                     specify them explicitly like `{name}<...>(...)`"
                ),
            );
        }
        let map: HashMap<String, Ty> = info
            .generics
            .iter()
            .cloned()
            .zip(arg_tys.iter().cloned())
            .collect();
        let key = (name.clone(), arg_tys);
        let fid = match self.instantiations.get(&key) {
            Some(&fid) => fid,
            None => self.instantiate_generic_fn(e.span, info, &key, &map)?,
        };
        // Mirror the plain user-function call: borrow `&T` reference params,
        // wrap optional params, and store the result.
        if args.iter().any(|a| a.spread) {
            let pty = self.src_param_tys.get(&fid).cloned().unwrap_or_default();
            let ret = self
                .finfo
                .get(&fid)
                .map(|i| i.ret.clone())
                .unwrap_or(Ty::Unknown);
            return self.spread_static_call(
                e,
                SpreadCall {
                    span: e.span,
                    fid,
                    pty: &pty,
                    ret: &ret,
                    receiver: None,
                    args,
                },
            );
        }
        let mut arg_temps = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
            let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
            let is_opt = matches!(src.as_ref(), Some(Ty::Option(_) | Ty::None));
            let t = self.borrow_arg(src.as_ref(), a)?;
            let t = if !is_ref && is_opt {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                self.option_wrap(t, &vt, a.value.span)?
            } else {
                t
            };
            arg_temps.push(t);
        }
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Func(fid),
            args: arg_temps,
        });
        self.guard_propagation(fid, e.span)?;
        Ok(dst)
    }

    /// Lower `name<Type,...>` used as a VALUE (not called): the explicit type
    /// arguments pick the concrete instantiation, which is wrapped in the same
    /// zero-capture closure a plain function value gets, so it can be stored,
    /// passed around, and called through the regular dynamic call convention.
    fn generic_fn_value(
        &mut self,
        e: &Expr,
        name: &str,
        type_args: &[TypeExpr],
    ) -> Result<Temp, ()> {
        let Some(info) = self
            .resolved
            .fns
            .get(name)
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
            })
            .cloned()
        else {
            return self.bad(e.span, format!("unknown generic function `{name}`"));
        };
        let arg_tys: Vec<Ty> = type_args
            .iter()
            .map(|te| {
                let t = self.resolved.resolve_ty(te, &self.fn_generics, self.diags);
                self.subst_ty(t)
            })
            .collect();
        if arg_tys.len() != info.generics.len() {
            return self.bad(
                e.span,
                format!(
                    "`{name}` takes {} type argument(s), found {}",
                    info.generics.len(),
                    arg_tys.len()
                ),
            );
        }
        self.generic_fn_value_for(e, &info, arg_tys)
    }

    /// Materialize a generic function as a value over concrete `arg_tys`:
    /// create (or reuse) the instantiation, wrap it in a zero-capture closure
    /// whose slot 0 holds a forwarder trampoline's address, and return the
    /// closure object.
    fn generic_fn_value_for(
        &mut self,
        e: &Expr,
        info: &CallableInfo,
        arg_tys: Vec<Ty>,
    ) -> Result<Temp, ()> {
        if arg_tys.iter().any(|t| t.has_var()) {
            return self.bad(
                e.span,
                format!(
                    "cannot use the generic function `{}` as a value with unresolved type arguments",
                    info.name
                ),
            );
        }
        let name = info.name.clone();
        let map: HashMap<String, Ty> = info
            .generics
            .iter()
            .cloned()
            .zip(arg_tys.iter().cloned())
            .collect();
        let key = (name.clone(), arg_tys);
        let fid = match self.instantiations.get(&key) {
            Some(&fid) => fid,
            None => self.instantiate_generic_fn(e.span, info, &key, &map)?,
        };
        let tramp = self.value_trampoline(e.span, fid)?;
        self.closure_obj(e, tramp, &[])
    }

    /// The forwarder trampoline a generic instantiation value is wrapped in:
    /// one per target instantiation, always forwarding the concrete
    /// substituted signature recorded in `finfo`. Mirrors `register_trampoline`
    /// but targets a lazily-materialized instantiation instead of a plain
    /// registered function.
    fn value_trampoline(&mut self, span: Span, target: FuncId) -> Result<FuncId, ()> {
        if let Some(&t) = self.tramp_fids.get(&target) {
            return Ok(t);
        }
        let Some(info) = self.finfo.get(&target).cloned() else {
            return self.bad(
                span,
                format!("no signature recorded for instantiated function {target:?}"),
            );
        };
        let pty: Vec<Ty> = info.params.iter().map(|p| p.ty.clone()).collect();
        let ret = info.ret.clone();
        if pty.iter().any(|t| self.map_ty(t, span).is_err()) {
            return self.bad(
                span,
                "cannot map an instantiated function value's parameters",
            );
        }
        if self.map_ty(&ret, span).is_err() {
            return self.bad(
                span,
                "cannot map an instantiated function value's return type",
            );
        }
        let t = self.push_class_func(
            "fn.value",
            &format!("pkl_tramp_{}", target.0),
            FnSource::Trampoline {
                span,
                target,
                pty,
                ret,
            },
        );
        self.tramp_fids.insert(target, t);
        Ok(t)
    }

    /// When a call argument is a *generic user function* given bare (no type
    /// arguments) and the expected parameter type is `fn`, infer the function's
    /// type arguments from that fn type and materialize the concrete
    /// instantiation as a value. `Some(v)` means materialized; `None` lets the
    /// regular argument path handle it (a `funcs_by_name` member, or the
    /// clean "as a value is not lowered yet" diagnostic).
    fn generic_fn_arg_value(&mut self, v: &Expr, hint: &Ty) -> Result<Option<Temp>, ()> {
        let ExprKind::Ident(name) = &v.kind else {
            return Ok(None);
        };
        let Some(info) = self
            .resolved
            .fns
            .get(name)
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
            })
            .cloned()
        else {
            return Ok(None);
        };
        if info.generics.is_empty() {
            return Ok(None);
        }
        let sig = Ty::Fn(
            info.params.iter().map(|p| p.ty.clone()).collect(),
            Box::new(info.ret.clone()),
        );
        let mut map: HashMap<String, Ty> = HashMap::new();
        crate::ty::infer_from(&sig, hint, &mut map);
        let arg_tys: Vec<Ty> = info
            .generics
            .iter()
            .map(|g| map.get(g).cloned().unwrap_or(Ty::Unknown))
            .collect();
        if arg_tys
            .iter()
            .any(|t| matches!(t, Ty::Unknown | Ty::Var(_)) || t.has_var())
        {
            return Ok(None);
        }
        Ok(Some(self.generic_fn_value_for(v, &info, arg_tys)?))
    }

    /// Infer the type arguments of a generic user function `name(args)` from
    /// the argument expressions' recorded types, exactly as the checker does
    /// (`ty::infer_from`). Every generic parameter must be pinned by at least
    /// one argument; an unpinned one bails with a message mirroring the
    /// checker's. Clean programs never reach this failure (the checker
    /// reports it), so the message is a defensive fallback.
    fn infer_generic_fn_args(
        &mut self,
        span: Span,
        info: &CallableInfo,
        args: &[CallArg],
    ) -> Result<Vec<Ty>, ()> {
        let mut map: HashMap<String, Ty> = HashMap::new();
        for (i, a) in args.iter().enumerate() {
            if a.spread {
                return self.bad(a.span, "spread arguments are not lowered yet");
            }
            if a.name.is_none() {
                if let Some(want) = info.params.get(i) {
                    let got = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                    crate::ty::infer_from(&want.ty, &got, &mut map);
                }
            }
        }
        let missing: Vec<&String> = info
            .generics
            .iter()
            .filter(|g| !map.contains_key(*g))
            .collect();
        if !missing.is_empty() {
            let plural = if missing.len() == 1 { "" } else { "s" };
            let names = missing
                .iter()
                .map(|g| format!("`{g}`"))
                .collect::<Vec<_>>()
                .join(", ");
            return self.bad(
                span,
                format!(
                    "cannot infer the type argument{plural} {names} for `{}`; \
                     specify them explicitly like `{}<...>(...)`",
                    info.name, info.name
                ),
            );
        }
        Ok(info
            .generics
            .iter()
            .map(|g| map.get(g).cloned().unwrap_or(Ty::Unknown))
            .collect())
    }

    /// A generic class/struct constructor call `Box<int>(arg, ...)`. Resolves
    /// the concrete type arguments, materializes the instantiation plan, and
    /// calls its implicit constructor exactly like a plain `TypeName(arg)`
    /// call.
    fn generic_class_call(
        &mut self,
        e: &Expr,
        name: &str,
        table: &ClassTable,
        type_args: &[TypeExpr],
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let arg_tys: Vec<Ty> = type_args
            .iter()
            .map(|te| {
                let t = self.resolved.resolve_ty(te, &self.fn_generics, self.diags);
                self.subst_ty(t)
            })
            .collect();
        if arg_tys.len() != table.generics.len() {
            return self.bad(
                e.span,
                format!(
                    "`{name}` takes {} type argument(s), found {}",
                    table.generics.len(),
                    arg_tys.len()
                ),
            );
        }
        let key = (name.to_string(), arg_tys.clone());
        let cid = match self.class_inst_by_args.get(&key) {
            Some(&cid) => cid,
            None => self.register_class_instantiation(name, &arg_tys, e.span)?,
        };
        let fid = *self.ctor_ids.get(&cid).ok_or(())?;
        // Mirror the plain class-constructor call: borrow `&T` reference
        // parameters, wrap optional parameters, and store the result.
        let fparams = self.module.funcs[fid.0].params.clone();
        let mut arg_temps = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if a.spread {
                return self.bad(a.span, "spread arguments are not lowered yet");
            }
            let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
            let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
            let t = self.borrow_arg(src.as_ref(), a)?;
            let t = if !is_ref && matches!(fparams.get(i).map(|p| p.ty), Some(IrTy::Ptr)) {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                self.option_wrap(t, &vt, a.value.span)?
            } else {
                t
            };
            arg_temps.push(t);
        }
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Func(fid),
            args: arg_temps,
        });
        self.guard_propagation(fid, e.span)?;
        Ok(dst)
    }

    /// Create the concrete instantiation of a generic function over `map`
    /// (generic parameter -> concrete type), registering its signature under
    /// `key`, and return its function id. Later calls with the same key reuse
    /// the existing function.
    /// the same instantiation.
    fn instantiate_generic_fn(
        &mut self,
        span: Span,
        info: &CallableInfo,
        key: &(String, Vec<Ty>),
        map: &HashMap<String, Ty>,
    ) -> Result<FuncId, ()> {
        let name = &info.name;
        let Some(f) = self.fn_decls.get(name).copied() else {
            return self.bad(
                span,
                format!("no declaration for generic function `{name}`"),
            );
        };
        if f.params.iter().any(|p| p.default.is_some() || p.rest) {
            return self.bad(
                span,
                format!(
                    "generic function `{name}` with default or rest parameters is not lowered yet"
                ),
            );
        }
        let params: Vec<ParamInfo> = info
            .params
            .iter()
            .map(|p| ParamInfo {
                ty: p.ty.subst(map),
                ..p.clone()
            })
            .collect();
        let ret = info.ret.subst(map);
        let mut symbol = format!(
            "pkl_{name}__{}",
            key.1
                .iter()
                .map(|t| sanitize_symbol(&t.bare_name()))
                .collect::<Vec<_>>()
                .join("_")
        );
        let mut n = 0;
        while self.module.funcs.iter().any(|f| f.symbol == symbol) {
            n += 1;
            symbol = format!("{symbol}_v{n}");
        }
        if f.is_async {
            // The instantiation is the wrapper; a hidden future body (with the
            // lambda ABI) is registered first so the wrapper can name it. Both
            // carry the substitution, mirroring the non-generic pair.
            let hidden_symbol = format!("{symbol}__future");
            let hidden = FuncId(self.module.funcs.len());
            self.module.funcs.push(IrFunc {
                name: format!("{}__future", name),
                symbol: hidden_symbol,
                params: Vec::new(),
                ret: IrTy::Unit,
                slots: Vec::new(),
                entry: BlockId(0),
                blocks: Vec::new(),
                is_main: false,
                is_test: false,
                tags: Vec::new(),
            });
            self.fid_list.push(hidden);
            self.fsource.insert(
                hidden,
                FnSource::AsyncBody {
                    f,
                    pty: params.iter().map(|p| p.ty.clone()).collect(),
                    ret: ret.clone(),
                },
            );
            let mut hidden_info = info.clone();
            hidden_info.params = Vec::new();
            self.finfo.insert(hidden, hidden_info);
            self.instanton_subst.insert(hidden, map.clone());
            self.fid_generics
                .insert(hidden, f.generics.iter().map(|g| g.name.clone()).collect());

            let wfid = FuncId(self.module.funcs.len());
            self.module.funcs.push(IrFunc {
                name: name.clone(),
                symbol,
                params: Vec::new(),
                ret: IrTy::Unit,
                slots: Vec::new(),
                entry: BlockId(0),
                blocks: Vec::new(),
                is_main: false,
                is_test: false,
                tags: Vec::new(),
            });
            self.fid_list.push(wfid);
            self.fsource.insert(wfid, FnSource::AsyncFn { f, hidden });
            self.finfo.insert(
                wfid,
                CallableInfo {
                    params: params.clone(),
                    ret: ret.clone(),
                    ..info.clone()
                },
            );
            self.src_param_tys
                .insert(wfid, params.iter().map(|p| p.ty.clone()).collect());
            self.instanton_subst.insert(wfid, map.clone());
            self.fid_generics
                .insert(wfid, f.generics.iter().map(|g| g.name.clone()).collect());
            self.instantiations.insert(key.clone(), wfid);
            return Ok(wfid);
        }
        let fid = FuncId(self.module.funcs.len());
        self.module.funcs.push(IrFunc {
            name: name.clone(),
            symbol,
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            is_main: false,
            is_test: false,
            tags: Vec::new(),
        });
        self.fid_list.push(fid);
        self.fsource.insert(fid, FnSource::TopLevel(f));
        self.finfo.insert(
            fid,
            CallableInfo {
                params,
                ret,
                ..info.clone()
            },
        );
        self.src_param_tys
            .insert(fid, info.params.iter().map(|p| p.ty.subst(map)).collect());
        self.instanton_subst.insert(fid, map.clone());
        self.instantiations.insert(key.clone(), fid);
        Ok(fid)
    }

    /// Build an enum value via `Enum.Variant(arg...)`. Payload scalars are
    /// boxed exactly like list elements; the value is a managed object whose
    /// tag is the variant index and whose fields are the boxed args.
    fn enum_ctor(
        &mut self,
        e: &Expr,
        table: &EnumTable,
        variant_name: &str,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        if args.iter().any(|a| a.spread) {
            return self.bad(e.span, "spread arguments are not lowered yet");
        }
        let Some((vi, ftypes)) = table
            .variants
            .iter()
            .enumerate()
            .find(|(_, (n, ..))| n == variant_name)
            .map(|(i, (_, ft, _))| (i as i64, ft.clone()))
        else {
            return self.bad(
                e.span,
                format!("enum `{}` has no variant `{variant_name}`", table.name),
            );
        };
        let n = ftypes.len();
        if args.len() != n {
            return self.bad(
                e.span,
                format!("`{variant_name}` takes {n} argument(s), got {}", args.len()),
            );
        }
        let tag = self.temp();
        self.instr(IrInstr::Const {
            dst: tag,
            c: IrConst::Int(vi),
        });
        let count = self.temp();
        self.instr(IrInstr::Const {
            dst: count,
            c: IrConst::Int(n as i64),
        });
        let obj = self.extern_call_t1(
            "pickle_enum_new",
            vec![IrTy::Int, IrTy::Int],
            IrTy::Ptr,
            vec![tag, count],
        )?;
        for (i, a) in args.iter().enumerate() {
            let ft = &ftypes[i];
            let rep = self.elem_rep(ft, e.span)?;
            let v = self.expr(&a.value)?;
            let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
            let packed = self.pack_for_pointer_boundary(&rep, v, &vt, e.span)?;
            let boxed = self.box_for_store(&rep, packed, elem_ir(ft))?;
            let idx = self.temp();
            self.instr(IrInstr::Const {
                dst: idx,
                c: IrConst::Int(i as i64),
            });
            self.extern_call_void(
                "pickle_enum_set_field",
                vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
                vec![obj, idx, boxed],
            );
        }
        Ok(obj)
    }

    /// A bare `Enum.Variant(...)` access with no parentheses. Unit variants
    /// (no payload) lower to a tag-only enum via `pickle_enum_new`.
    fn member_value(&mut self, e: &Expr, object: &Expr, name: &str) -> Result<Temp, ()> {
        if let ExprKind::Ident(enum_name) = &object.kind {
            if let Some(TypeTableEntry::Enum(t)) = self.resolved.types.get(enum_name) {
                let Some((vi, fields)) = t
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, (n, _, _))| n == name)
                    .map(|(i, (_, ft, _))| (i as i64, ft))
                else {
                    return self.bad(e.span, format!("enum `{}` has no variant `{name}`", t.name));
                };
                if !fields.is_empty() {
                    return self.bad(
                        e.span,
                        format!(
                            "variant `{name}` carries {} payload field(s) and needs `{name}(...)`",
                            fields.len()
                        ),
                    );
                }
                let tag = self.temp();
                self.instr(IrInstr::Const {
                    dst: tag,
                    c: IrConst::Int(vi),
                });
                let zero = self.temp();
                self.instr(IrInstr::Const {
                    dst: zero,
                    c: IrConst::Int(0),
                });
                return self.extern_call_t1(
                    "pickle_enum_new",
                    vec![IrTy::Int, IrTy::Int],
                    IrTy::Ptr,
                    vec![tag, zero],
                );
            }
        }
        // `object.field` on a class/struct instance, also through an immutable
        // `&T` borrow (which reads through the referent).
        let ot = self.ty_of(&object.span);
        let ot = match ot {
            Some(Ty::Ptr(inner)) => Some((*inner).clone()),
            Some(Ty::Ref(inner)) => Some((*inner).clone()),
            other => other,
        };
        let cid = match ot {
            Some(otv @ (Ty::Class(..) | Ty::Struct(..))) => self.class_id_of(&otv, e.span)?,
            _ => None,
        };
        if let Some(cid) = cid {
            if let Some(slot) = self.instance_field_index(cid as i64, name) {
                let field_ty = self.field_at(cid as i64, slot).ty.clone();
                let obj = self.expr(object)?;
                return self.field_read(e.span, obj, &field_ty, slot);
            }
            if let Some(&fid) = self.property_ids.get(&(cid, name.to_string(), false)) {
                let obj = self.expr(object)?;
                return self.call_method(e, fid, &[], Some(obj));
            }
            // An instance method used as a bound value: build a one-capture
            // closure object whose slot 0 is its method trampoline and whose
            // slot 1 is the receiver. `fn_value_call` dispatches through it
            // exactly like a lambda capture.
            if let Some(&(_, is_static)) = self.method_ids.get(&(cid, name.to_string())) {
                if is_static {
                    return self.bad(
                        e.span,
                        format!("static method `{name}` cannot be used as a value"),
                    );
                }
                return self.bound_method_value(e, object);
            }
            return self.bad(
                e.span,
                format!(
                    "method `{name}` of `{}` cannot be used as a value",
                    self.class_name_of(cid)
                ),
            );
        }
        // `Type.staticProperty` reads a receiver-less accessor; `Type.field`
        // reads a static field cell.
        if let ExprKind::Ident(tname) = &object.kind {
            if let Some(&cid) = self.class_by_name.get(tname) {
                if let Some(&fid) = self
                    .static_property_ids
                    .get(&(cid, name.to_string(), false))
                {
                    return self.call_method(e, fid, &[], None);
                }
                if let Some((dcid, slot, info)) = self.static_field(cid as i64, name) {
                    return self.static_read(e.span, dcid, slot, &info.ty);
                }
                if let Some(r) = self.read_class_const(e.span, cid as i64, name) {
                    return r;
                }
                return self.bad(
                    e.span,
                    format!("static member `{name}` on `{tname}` is not lowered yet"),
                );
            }
        }
        self.bad(e.span, "member access is not lowered yet")
    }

    /// The runtime slot of instance field `name` on class `cid`, following the
    /// parent-first layout, when registered.
    fn instance_field_index(&self, cid: i64, name: &str) -> Option<usize> {
        let plan = self.classes.iter().find(|p| p.class_id as i64 == cid)?;
        self.abs_instance_field_slot(&plan.name, name)
    }

    /// The static slot of static field `name` on class `cid`, with its declared
    /// type and the *declaring* class's id, when the class is registered and
    /// the field exists. Static fields live in the declaring class's cells, so
    /// a subclass reference resolves the ancestor that declares the field
    /// (own declarations win, then up the ancestry).
    fn static_field(&self, cid: i64, name: &str) -> Option<(u32, usize, FieldInfo)> {
        let plan = self.classes.iter().find(|p| p.class_id as i64 == cid)?;
        let ancestry = self.ancestry(&plan.name);
        for cn in ancestry.iter().rev() {
            let Some(table) = self.table_of(cn) else {
                continue;
            };
            let Some(slot) = static_field_slot(&table, name) else {
                continue;
            };
            let Some(info) = static_field_info(&table, name) else {
                continue;
            };
            let Some(&dcid) = self.class_by_name.get(cn) else {
                continue;
            };
            return Some((dcid, slot, info));
        }
        None
    }

    /// Whether `name` is a `const` declared on `cid` or any ancestor (own
    /// declarations win, then up the ancestry).
    fn class_const_defined(&self, cid: i64, name: &str) -> bool {
        let Some(plan) = self.classes.iter().find(|p| p.class_id as i64 == cid) else {
            return false;
        };
        let ancestry = self.ancestry(&plan.name);
        ancestry.iter().rev().any(|cn| {
            self.class_by_name
                .get(cn)
                .and_then(|dcid| self.class_consts.get(&(*dcid, name.to_string())))
                .is_some()
        })
    }

    /// Inline a class constant's initializer at a use site. Returns `None` when
    /// `name` is not a constant of class `cid` (or an ancestor); `Some(Err)` on
    /// a cyclic constant.
    fn read_class_const(&mut self, span: Span, cid: i64, name: &str) -> Option<Result<Temp, ()>> {
        let plan = self.classes.iter().find(|p| p.class_id as i64 == cid)?;
        let ancestry = self.ancestry(&plan.name);
        for cn in ancestry.iter().rev() {
            let Some(&dcid) = self.class_by_name.get(cn) else {
                continue;
            };
            let Some((_, value)) = self.class_consts.get(&(dcid, name.to_string())).cloned() else {
                continue;
            };
            let key = format!("{}.{}", self.class_name_of(dcid), name);
            if self.const_inlining.iter().any(|n| n == &key) {
                return Some(self.bad(span, format!("cyclic `const` initialization of `{key}`")));
            }
            self.const_inlining.push(key);
            // Constant initializers are evaluated in the declaring class's
            // scope so a constant can reference another constant of the same
            // class by name.
            let saved_owner = self.owner;
            self.owner = Some(dcid as i64);
            let t = self.expr(value);
            self.owner = saved_owner;
            self.const_inlining.pop();
            return Some(t);
        }
        None
    }

    fn field_at(&self, cid: i64, idx: usize) -> FieldInfo {
        let plan = self
            .classes
            .iter()
            .find(|p| p.class_id as i64 == cid)
            .unwrap_or_else(|| unreachable!("field index on an unregistered class"));
        self.all_instance_fields(&plan.name)
            .into_iter()
            .nth(idx)
            .unwrap_or_else(|| unreachable!("field index out of range"))
    }

    fn class_name_of(&self, cid: u32) -> String {
        self.classes
            .iter()
            .find(|p| p.class_id == cid)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("class#{cid}"))
    }

    /// The class whose instantiated member body is currently being built,
    /// when it is a generic class/struct instantiation (plan names of
    /// materialized generics contain the `<` type-argument separator).
    fn generic_owner(&self) -> Option<String> {
        let cid = self.owner? as u32;
        let name = self.class_name_of(cid);
        (name.contains('<')).then_some(name)
    }

    /// `obj.slot` field read: `pickle_obj_slot_get` then unbox scalars.
    fn field_read(
        &mut self,
        span: Span,
        obj: Temp,
        field_ty: &Ty,
        slot: usize,
    ) -> Result<Temp, ()> {
        let rep = self.elem_rep(field_ty, span)?;
        let idx = self.int_const(slot as i64);
        let raw = self.extern_call_t1(
            "pickle_obj_slot_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![obj, idx],
        )?;
        match rep {
            ElemRep::Scalar(_, unbox, ir) => {
                self.extern_call_t1(unbox, vec![IrTy::Ptr], ir, vec![raw])
            }
            ElemRep::Ptr => Ok(raw),
        }
    }

    /// `xs.push(v)` and `xs.pop()` for `List<T>` receivers.
    fn method_call(
        &mut self,
        e: &Expr,
        object: &Expr,
        name: &str,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let ot = self.ty_of(&object.span);

        // `Type.staticMethod(...)`.
        if let ExprKind::Ident(tname) = &object.kind {
            if let Some(&cid) = self.class_by_name.get(tname) {
                if let Some(&fid) = self.named_ctor_ids.get(&(cid, name.to_string())) {
                    if args.iter().any(|a| a.spread) {
                        return self.bad(
                            e.span,
                            "spreading into a named constructor is not lowered yet",
                        );
                    }
                    return self.call_method(e, fid, args, None);
                }
                if let Some(&(fid, is_static)) = self.method_ids.get(&(cid, name.to_string())) {
                    if !is_static {
                        return self.bad(
                            e.span,
                            format!("instance method `{name}` must be called on an instance of `{tname}`"),
                        );
                    }
                    if args.iter().any(|a| a.spread) {
                        let pty = self.src_param_tys.get(&fid).cloned().unwrap_or_default();
                        let ret = self
                            .finfo
                            .get(&fid)
                            .map(|i| i.ret.clone())
                            .unwrap_or(Ty::Unknown);
                        return self.spread_static_call(
                            e,
                            SpreadCall {
                                span: e.span,
                                fid,
                                pty: &pty,
                                ret: &ret,
                                receiver: None,
                                args,
                            },
                        );
                    }
                    return self.call_method(e, fid, args, None);
                }
                return self.bad(e.span, format!("`{tname}` has no static method `{name}`"));
            }
        }

        // `instance.method(...)` on a class/struct instance, also through an
        // immutable `&T` borrow (which borrows the receiver).
        let cid = match ot {
            Some(ref otv @ (Ty::Class(..) | Ty::Struct(..))) => self.class_id_of(otv, e.span)?,
            Some(Ty::Ref(ref inner)) => match inner.as_ref() {
                Ty::Class(..) | Ty::Struct(..) => self.class_id_of(inner.as_ref(), e.span)?,
                _ => None,
            },
            _ => None,
        };
        if let Some(cid) = cid {
            if let Some(&(fid, is_static)) = self.method_ids.get(&(cid, name.to_string())) {
                if is_static {
                    return self.bad(
                        e.span,
                        format!(
                            "static method `{name}` must be called on the type, not an instance"
                        ),
                    );
                }
                let receiver = self.expr(object)?;
                // An instance method that is overridden somewhere in the program
                // dispatches on the receiver's runtime class id — unless the
                // receiver expression is `super`, which must stay statically
                // bound to this class's own implementation.
                let virtual_branches = if !matches!(&object.kind, ExprKind::Super) {
                    self.virtual_dispatch.get(&(cid, name.to_string())).cloned()
                } else {
                    None
                };
                if let Some(branches) = virtual_branches {
                    if !branches.is_empty() {
                        return self.virtual_method_call(e, fid, args, receiver, &branches);
                    }
                }
                return self.call_method(e, fid, args, Some(receiver));
            }
            // A fn-typed field (`h.f(...)` where `f` is a function field) loads
            // the field value and dispatches dynamically, like any other
            // closure-valued callee.
            if let Some(slot) = self.instance_field_index(cid as i64, name) {
                let field_ty = self.field_at(cid as i64, slot).ty.clone();
                if matches!(field_ty, Ty::Fn(..)) {
                    let owner = self.expr(object)?;
                    let fv = self.field_read(e.span, owner, &field_ty, slot)?;
                    return self.fn_value_call_from(e, fv, &field_ty, args);
                }
                return self.bad(
                    e.span,
                    format!(
                        "field `{name}` of `{}` is not callable",
                        self.class_name_of(cid)
                    ),
                );
            }
            return self.bad(
                e.span,
                format!("`{}` has no method `{name}`", self.class_name_of(cid)),
            );
        }

        // Receive-receiver (interface/list/map/stream/…) methods reject spreads:
        // only class/struct methods are wired through `spread_static_call`.
        if args.iter().any(|a| a.spread) {
            return self.bad(e.span, "spread arguments are not lowered for this receiver");
        }
        match ot {
            Some(ref otv) if self.iface_of(otv).is_some() => {
                self.interface_method_call(e, otv, object, name, args)
            }
            Some(Ty::Map(k, v)) => {
                let ktype = k.clone();
                let krep = self.key_rep(&ktype, object.span)?;
                let obj = self.expr(object)?;
                let vrep = self.elem_rep(&v, e.span)?;
                match name {
                    "has" => {
                        if args.len() != 1 {
                            return self.bad(e.span, "`has` takes one argument");
                        }
                        let kt = self.expr(&args[0].value)?;
                        let a_ty = self
                            .ty_of(&args[0].value.span)
                            .unwrap_or_else(|| (*ktype).clone());
                        let key = self.pack_map_key(&krep, kt, &a_ty, args[0].value.span)?;
                        self.extern_call_t1(
                            "pickle_map_has",
                            vec![IrTy::Ptr, IrTy::Ptr],
                            IrTy::Bool,
                            vec![obj, key],
                        )
                    }
                    "get" => {
                        if args.len() != 1 {
                            return self.bad(e.span, "`get` takes one argument (a key)");
                        }
                        let k = self.expr(&args[0].value)?;
                        let a_ty = self
                            .ty_of(&args[0].value.span)
                            .unwrap_or_else(|| (*ktype).clone());
                        let key = self.pack_map_key(&krep, k, &a_ty, args[0].value.span)?;
                        // Null when absent, else the stored boxed-scalar or
                        // managed pointer — exactly the `T?` representation,
                        // so the raw result is the option.
                        self.extern_call_t1(
                            "pickle_map_get",
                            vec![IrTy::Ptr, IrTy::Ptr],
                            IrTy::Ptr,
                            vec![obj, key],
                        )
                    }
                    "remove" => {
                        if args.len() != 1 {
                            return self.bad(e.span, "`remove` takes one argument (a key)");
                        }
                        let k = self.expr(&args[0].value)?;
                        let a_ty = self
                            .ty_of(&args[0].value.span)
                            .unwrap_or_else(|| (*ktype).clone());
                        let key = self.pack_map_key(&krep, k, &a_ty, args[0].value.span)?;
                        // The runtime returns the stored pointer (a boxed
                        // scalar or managed value) or null — exactly the `T?`
                        // representation, so the raw result is the option.
                        self.extern_call_t1(
                            "pickle_map_remove",
                            vec![IrTy::Ptr, IrTy::Ptr],
                            IrTy::Ptr,
                            vec![obj, key],
                        )
                    }
                    "keys" => {
                        if !args.is_empty() {
                            return self.bad(e.span, "`keys` takes no arguments");
                        }
                        self.extern_call_t1(
                            "pickle_map_keys",
                            vec![IrTy::Ptr],
                            IrTy::Ptr,
                            vec![obj],
                        )
                    }
                    "values" => {
                        if !args.is_empty() {
                            return self.bad(e.span, "`values` takes no arguments");
                        }
                        self.extern_call_t1(
                            "pickle_map_values",
                            vec![IrTy::Ptr],
                            IrTy::Ptr,
                            vec![obj],
                        )
                    }
                    other => {
                        let _ = vrep;
                        self.bad(
                            e.span,
                            format!("`{other}` method on `Map` is not lowered yet"),
                        )
                    }
                }
            }
            Some(Ty::List(_)) => self.list_method_call(e, object, name, args),
            Some(Ty::Stream) => self.stream_method_call(e, object, name, args),
            _ => self.bad(e.span, "method calls on this type are not lowered yet"),
        }
    }

    /// Emit a call to a class method function, passing the receiver first for
    /// instance methods.
    fn call_method(
        &mut self,
        e: &Expr,
        fid: FuncId,
        args: &[CallArg],
        receiver: Option<Temp>,
    ) -> Result<Temp, ()> {
        if args.iter().any(|a| a.spread) {
            let pty = self.src_param_tys.get(&fid).cloned().unwrap_or_default();
            let ret = self
                .finfo
                .get(&fid)
                .map(|i| i.ret.clone())
                .unwrap_or(Ty::Unknown);
            return self.spread_static_call(
                e,
                SpreadCall {
                    span: e.span,
                    fid,
                    pty: &pty,
                    ret: &ret,
                    receiver,
                    args,
                },
            );
        }
        let call_args = self.marshal_method_args(e, fid, args, receiver)?;
        self.emit_call_to(fid, call_args, e)
    }

    /// Resolve the IR parameter type at `fparam_idx` (which counts the
    /// implicit receiver slot, when the call has one) for function `fid`.
    /// The callee's `IrFunc.params` are only populated once its body has been
    /// emitted, so a caller that precedes the callee (functions are lowered in
    /// definition order) would otherwise see an empty param list and skip
    /// value marshaling. In that case the checker's source signature
    /// (`src_param_tys`, indexed by the explicit argument position) is mapped
    /// to its IR type instead.
    fn callee_param_ir(
        &mut self,
        fid: FuncId,
        fparam_idx: usize,
        arg_idx: usize,
        span: Span,
    ) -> Option<IrTy> {
        if let Some(t) = self.module.funcs[fid.0]
            .params
            .get(fparam_idx)
            .map(|p| p.ty)
        {
            return Some(t);
        }
        let src = self
            .src_param_tys
            .get(&fid)
            .and_then(|v| v.get(arg_idx))
            .cloned()?;
        self.map_ty(&src, span).ok()
    }

    /// Marshal a method call's receiver (first for instance methods) and
    /// argument values into the callee's calling-convention temp list.
    fn marshal_method_args(
        &mut self,
        _e: &Expr,
        fid: FuncId,
        args: &[CallArg],
        receiver: Option<Temp>,
    ) -> Result<Vec<Temp>, ()> {
        let mut call_args = match receiver {
            Some(r) => vec![r],
            None => Vec::new(),
        };
        let base = call_args.len();
        for (i, a) in args.iter().enumerate() {
            let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
            let is_ref = matches!(src.as_ref(), Some(Ty::Ref(_)));
            let t = self.borrow_arg(src.as_ref(), a)?;
            let fparam = self.callee_param_ir(fid, base + i, i, a.value.span);
            let t = if !is_ref && matches!(fparam, Some(IrTy::Ptr)) {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                self.option_wrap(t, &vt, a.value.span)?
            } else {
                t
            };
            call_args.push(t);
        }
        Ok(call_args)
    }

    /// Emit the call instruction for an already-marshaled method call.
    fn emit_call_to(&mut self, fid: FuncId, call_args: Vec<Temp>, e: &Expr) -> Result<Temp, ()> {
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Func(fid),
            args: call_args,
        });
        // A Pickle-level `throw` is cooperative: the callee returns normally
        // with the pending flag set instead of unwinding, so the call site has
        // to notice and leave before anyone reads `dst`.
        self.guard_propagation(fid, e.span)?;
        Ok(dst)
    }

    /// Emit the pending check after a call to `fid`, if that call can propagate.
    /// After a call that may leave a throw pending, either record the edge (on
    /// the analysis pass) or emit the check (on the real one).
    ///
    /// Recording happens even when the enclosing function will turn out not to
    /// need a check at all: the call really is a call, so it is really an edge.
    /// Keeping every edge is what makes the fixed point safe, since the answer
    /// is only ever used to *remove* a check.
    fn guard_propagation(&mut self, fid: FuncId, span: Span) -> Result<(), ()> {
        if self.analyzing {
            if let Some(caller) = self.current_fid {
                self.call_edges.entry(caller).or_default().push(fid);
            }
            return Ok(());
        }
        if self.check_propagates(fid, span) {
            self.check_pending()?;
        }
        Ok(())
    }

    /// Can a call to `fid` leave a throw pending?
    ///
    /// A program with no `throw` anywhere cannot propagate one, so it skips
    /// every check. Otherwise this is the fixed point from
    /// [`Emitter::solve_may_throw`], and anything it does not know — a function
    /// the analysis pass could not reach — is assumed to propagate. That
    /// direction only ever costs a predictable branch, whereas guessing "no"
    /// would lose a throw.
    fn check_propagates(&self, fid: FuncId, _span: Span) -> bool {
        if !self.prog_may_throw {
            return false;
        }
        // A `finally` running on a throw path already has the flag set; a check
        // there would see the throw we are transferring on, not a new one.
        if self.in_throw_transfer {
            return false;
        }
        self.may_throw.get(&fid).copied().unwrap_or(true)
    }

    /// After a call that may have thrown, branch to the innermost `catch` in
    /// this function or out of it. The flag stays set until someone consumes it.
    fn check_pending(&mut self) -> Result<(), ()> {
        let pending = self.extern_call_t1(exc::PENDING, vec![], IrTy::Bool, vec![])?;
        let thrown = self.new_block();
        let resume = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: pending,
            then: thrown,
            else_: resume,
        });
        self.cur = thrown;
        self.transfer_to_handler_or_propagate()?;
        self.cur = resume;
        Ok(())
    }

    /// Lower an instance call to a method that is overridden somewhere in the
    /// program: `pickle_class_is(receiver, D_cid)` checks deepest-derived-first,
    /// each branching to that descendant's own implementation and joining on
    /// a result slot; a receiver that is an instance of no overriding
    /// descendant falls through to the statically-resolved `fallback_fid`. The
    /// receiver is borrowed once up front, so the checks and every branch
    /// share it.
    fn virtual_method_call(
        &mut self,
        e: &Expr,
        fallback_fid: FuncId,
        args: &[CallArg],
        receiver: Temp,
        branches: &[(u32, FuncId)],
    ) -> Result<Temp, ()> {
        if args.iter().any(|a| a.spread) {
            return self.bad(
                e.span,
                "spread arguments cannot dispatch virtually (the method is overridden)",
            );
        }
        let ret_ty = self.irty(e.span)?;
        let ret_slot = self.new_slot(ret_ty);
        let join = self.new_block();
        for &(dcid, dfid) in branches {
            let cb = self.new_block();
            let next = self.new_block();

            let dc = self.int_const(dcid as i64);
            let found = self.extern_call_t1(
                "pickle_class_is",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Bool,
                vec![receiver, dc],
            )?;
            self.term(IrTerm::BranchIf {
                cond: found,
                then: cb,
                else_: next,
            });

            self.cur = cb;
            let call_args = self.marshal_method_args(e, dfid, args, Some(receiver))?;
            let val = self.emit_call_to(dfid, call_args, e)?;
            self.instr(IrInstr::StoreSlot {
                slot: ret_slot,
                v: val,
            });
            self.term(IrTerm::Branch { target: join });

            self.cur = next;
        }
        // Fall back to the receiver's own statically-resolved implementation.
        let call_args = self.marshal_method_args(e, fallback_fid, args, Some(receiver))?;
        let val = self.emit_call_to(fallback_fid, call_args, e)?;
        self.instr(IrInstr::StoreSlot {
            slot: ret_slot,
            v: val,
        });
        self.term(IrTerm::Branch { target: join });

        self.cur = join;
        let dst = self.temp();
        self.instr(IrInstr::LoadSlot {
            dst,
            slot: ret_slot,
        });
        Ok(dst)
    }

    /// Lower a call whose final argument is a spread (`...list`) into a fixed
    /// target function. Every argument — the explicitly-supplied prefix plus
    /// the spread elements — is boxed into one `List`; the target's per-target
    /// `FnSource::SpreadTrampoline` (registered on first use and shared by
    /// every caller) unboxes them back into the ordinary call ABI. A runtime
    /// arity guard panics (`pickle_panic_arity_diff`) when the spread list's
    /// length does not exactly fill the remaining parameters, so the
    /// trampoline may assume its boxed list has exactly `c.pty.len()` entries.
    fn spread_static_call(&mut self, _e: &Expr, c: SpreadCall<'_>) -> Result<Temp, ()> {
        let span = c.span;
        let fid = c.fid;
        let pty = c.pty;
        let ret = c.ret;
        let receiver = c.receiver;
        let args = c.args;
        // The checker validates shape and coverage; the emitter re-checks the
        // lowering prerequisites it depends on before registering anything.
        if pty.iter().any(|t| matches!(t, Ty::Ref(_))) {
            return self.bad(span, "spreading into a `&T` parameter is not lowered yet");
        }
        if let Some(FnSource::TopLevel(f)) = self.fsource.get(&fid) {
            if f.params.iter().any(|p| p.default.is_some()) {
                return self.bad(
                    span,
                    "spreading into a parameter with a default value is not lowered yet",
                );
            }
        }
        if let Some(info) = self.finfo.get(&fid) {
            if info.params.iter().any(|p| p.rest) {
                return self.bad(
                    span,
                    "spreading into a rest parameter is only lowered for `println`",
                );
            }
        }
        let m = args.iter().take_while(|a| !a.spread).count();
        if m > pty.len() {
            return self.bad(span, "too many arguments for this call");
        }
        let Some((sp_arg, true)) = args.get(m).map(|a| (a, a.spread)) else {
            return self.bad(span, "internal error: missing spread argument");
        };
        let elem_ty = match self.ty_of(&sp_arg.value.span) {
            Some(Ty::List(inner)) => (*inner).clone(),
            _ => return self.bad(sp_arg.span, "a spread argument must be a `List`"),
        };
        // Every parameter the spread covers must round-trip the element
        // representation exactly (an `int` into `int, byte` still shares the
        // i64 box, but nothing may cross scalar/managed families).
        let erep = self.elem_rep(&elem_ty, sp_arg.span)?;
        for pt in pty.iter().skip(m) {
            if self.elem_rep(pt, sp_arg.span)? != erep {
                return self.bad(
                    span,
                    "spreading over parameters with mixed element representations is not lowered yet",
                );
            }
        }
        if self.map_ty(ret, span).is_err() {
            return self.bad(span, "spread call return type is not lowered yet");
        }
        let tramp = match self.spread_tramps.get(&fid) {
            Some(&t) => t,
            None => {
                let t = self.push_class_func(
                    "fn.spread",
                    &format!("pkl_sptramp_{}", fid.0),
                    FnSource::SpreadTrampoline {
                        span,
                        target: fid,
                        pty: pty.to_vec(),
                        ret: ret.clone(),
                        receiver: receiver.is_some(),
                    },
                );
                self.spread_tramps.insert(fid, t);
                t
            }
        };
        let ret_ir = self.map_ty(ret, span).unwrap_or(IrTy::Unit);
        let res_slot = self.new_slot(ret_ir);

        // Build the boxed argument list: the fixed prefix boxed by each
        // parameter's own representation, then the spread elements verbatim
        // (they already carry the element representation).
        let cap = self.int_const(pty.len() as i64);
        let xs = self.extern_call_t1("pickle_list_new", vec![IrTy::Int], IrTy::Ptr, vec![cap])?;
        let base = receiver.map_or(0, |_| 1);
        for (i, a) in args.iter().enumerate().take(m) {
            let src = self.src_param_tys.get(&fid).and_then(|v| v.get(i)).cloned();
            let mut t = self.borrow_arg(src.as_ref(), a)?;
            let fparam = self.callee_param_ir(fid, base + i, i, a.value.span);
            if !matches!(src.as_ref(), Some(Ty::Ref(_))) && matches!(fparam, Some(IrTy::Ptr)) {
                let vt = self.ty_of(&a.value.span).unwrap_or(Ty::Unknown);
                t = self.option_wrap(t, &vt, a.value.span)?;
            }
            let rep = self.elem_rep(&pty[i], a.value.span)?;
            let bx = match rep {
                ElemRep::Ptr => t,
                ElemRep::Scalar(box_sym, _, ir) => {
                    self.extern_call_t1(box_sym, vec![ir], IrTy::Ptr, vec![t])?
                }
            };
            self.extern_call_void("pickle_list_push", vec![IrTy::Ptr, IrTy::Ptr], vec![xs, bx]);
        }
        let sp = self.expr(&sp_arg.value)?;
        let n = self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![sp])?;
        let r = self.int_const((pty.len() - m) as i64);
        let ok = self.temp();
        self.instr(IrInstr::BinOp {
            dst: ok,
            op: IrBinOp::Eq,
            a: n,
            b: r,
        });
        let r_ok = self.new_block();
        let r_panic = self.new_block();
        let join = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: ok,
            then: r_ok,
            else_: r_panic,
        });
        self.cur = r_panic;
        self.extern_call_void("pickle_panic_arity_diff", vec![], vec![]);
        self.term(IrTerm::Branch { target: join });
        self.cur = r_ok;
        let r_missing = pty.len() - m;
        for k in 0..r_missing {
            let idx = self.int_const(k as i64);
            let raw = self.extern_call_t1(
                "pickle_list_get",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Ptr,
                vec![sp, idx],
            )?;
            self.extern_call_void(
                "pickle_list_push",
                vec![IrTy::Ptr, IrTy::Ptr],
                vec![xs, raw],
            );
        }
        let mut call_args = Vec::new();
        if let Some(recv) = receiver {
            call_args.push(recv);
        }
        call_args.push(xs);
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Func(tramp),
            args: call_args,
        });
        // The trampoline tail-calls `fid`, so a throw comes back with the flag
        // set and `dst` never written. Check before the store, or the join block
        // would load an unwritten slot and the throw would surface much later.
        self.guard_propagation(fid, span)?;
        self.instr(IrInstr::StoreSlot {
            slot: res_slot,
            v: dst,
        });
        self.term(IrTerm::Branch { target: join });
        self.cur = join;
        Ok(self.load(res_slot))
    }

    /// `...xs` as a `print`/`println` operand: emit a runtime loop that
    /// prints every element individually. Scalar element reps are unboxed and
    /// printed with their scalar printer; managed elements (string, class,
    /// nested list, ...) go straight to `pickle_print_obj`. An empty spread
    /// prints nothing.
    fn print_spread_list(&mut self, span: Span, sp: Temp, elem: &Ty) -> Result<(), ()> {
        let (sym, print_ir) = match elem {
            Ty::Int => ("pickle_print_i64", IrTy::Int),
            Ty::Float => ("pickle_print_f64", IrTy::Float),
            Ty::Bool => ("pickle_print_bool", IrTy::Bool),
            Ty::Byte => ("pickle_print_byte", IrTy::Int),
            Ty::Char => ("pickle_print_char", IrTy::Char),
            _ => ("pickle_print_obj", IrTy::Ptr),
        };
        let rep = self.elem_rep(elem, span)?;
        let seq_slot = self.new_slot(IrTy::Ptr);
        let idx_slot = self.new_slot(IrTy::Int);
        let len_slot = self.new_slot(IrTy::Int);
        self.instr(IrInstr::StoreSlot {
            slot: seq_slot,
            v: sp,
        });
        let zero = self.temp();
        self.instr(IrInstr::Const {
            dst: zero,
            c: IrConst::Int(0),
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: zero,
        });
        let seq_l = self.load(seq_slot);
        let len_t =
            self.extern_call_t1("pickle_list_len", vec![IrTy::Ptr], IrTy::Int, vec![seq_l])?;
        self.instr(IrInstr::StoreSlot {
            slot: len_slot,
            v: len_t,
        });
        let cond_id = self.new_block();
        let body_id = self.new_block();
        let next_id = self.new_block();
        let end_id = self.new_block();
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = cond_id;
        let cur_t = self.load(idx_slot);
        let end_l = self.load(len_slot);
        let cmp = self.temp();
        self.instr(IrInstr::BinOp {
            dst: cmp,
            op: IrBinOp::Lt,
            a: cur_t,
            b: end_l,
        });
        self.term(IrTerm::BranchIf {
            cond: cmp,
            then: body_id,
            else_: end_id,
        });
        self.cur = body_id;
        let cur_i = self.load(idx_slot);
        let seq_l = self.load(seq_slot);
        let raw = self.extern_call_t1(
            "pickle_list_get",
            vec![IrTy::Ptr, IrTy::Int],
            IrTy::Ptr,
            vec![seq_l, cur_i],
        )?;
        let v = match rep {
            ElemRep::Scalar(_, unbox, el_ir)
                if matches!(elem, Ty::Int | Ty::Float | Ty::Bool | Ty::Byte | Ty::Char) =>
            {
                self.extern_call_t1(unbox, vec![IrTy::Ptr], el_ir, vec![raw])?
            }
            _ => raw,
        };
        self.extern_call_void(sym, vec![print_ir], vec![v]);
        self.term(IrTerm::Branch { target: next_id });
        self.cur = next_id;
        let c = self.load(idx_slot);
        let one = self.temp();
        self.instr(IrInstr::Const {
            dst: one,
            c: IrConst::Int(1),
        });
        let nxt = self.temp();
        self.instr(IrInstr::BinOp {
            dst: nxt,
            op: IrBinOp::Add,
            a: c,
            b: one,
        });
        self.instr(IrInstr::StoreSlot {
            slot: idx_slot,
            v: nxt,
        });
        self.term(IrTerm::Branch { target: cond_id });
        self.cur = end_id;
        Ok(())
    }
    /// * a compile-time chain of `pickle_class_is(receiver, D_cid)` checks over
    ///   every registered class that (transitively) implements the interface,
    ///   deepest-derived-first, each branching to that class's own resolved
    ///   implementation and joining on a result slot; and
    /// * a runtime fallback `pickle_iface_method(receiver, iface_id, m_idx)`
    ///   that reads the receiver's own class's method-table entry and
    ///   indirect-calls it (a zero guard panics loudly instead of calling
    ///   through null — reachable only through an unregistered/foreign class,
    ///   which is out of contract).
    fn interface_method_call(
        &mut self,
        e: &Expr,
        ift: &Ty,
        object: &Expr,
        name: &str,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let receiver = self.expr(object)?;
        let ret_ty = self.irty(e.span)?;
        self.interface_dispatch(e, e.span, ift, receiver, name, args, ret_ty)
    }

    /// Shared core of an interface-dispatched `.name(args)` call against an
    /// already-lowered receiver. Used by `interface_method_call` for checked
    /// call sites and by the compiler-internal `Iterable`/`Iterator` protocol
    /// calls (`interface_zero_arg_call`) that `for in` lowers. `ret_ty` is the
    /// runtime type of the member's return value.
    #[allow(clippy::too_many_arguments)]
    fn interface_dispatch(
        &mut self,
        e: &Expr,
        span: Span,
        ift: &Ty,
        receiver: Temp,
        name: &str,
        args: &[CallArg],
        ret_ty: IrTy,
    ) -> Result<Temp, ()> {
        let Some((iface_name, iargs)) = self.iface_of(ift) else {
            return self.bad(span, "receiver type is not an interface");
        };
        let iface_id = self.iface_id_for(&iface_name, &iargs, span)?;
        let m_idx = match self
            .iface_members
            .get(&iface_id)
            .and_then(|members| members.iter().find(|(n, _)| n == name).map(|(_, i)| *i))
        {
            Some(i) => i,
            None => return self.bad(span, format!("`{iface_name}` has no method `{name}`")),
        };
        // Static dispatch chain: every registered class that transitively
        // declares `implements` for this interface id, ordered
        // deepest-derived-first so a subclass's override wins.
        let mut branches: Vec<(usize, u32, FuncId)> = Vec::new();
        for p in &self.classes {
            if !self.class_implements_iface(p.class_id, iface_id) {
                continue;
            }
            let Some(&(fid, is_static)) = self.method_ids.get(&(p.class_id, name.to_string()))
            else {
                continue;
            };
            if is_static {
                let _: Result<(), ()> = self.bad(
                    span,
                    format!("interface method `{name}` is static on `{}`", p.name),
                );
                return Err(());
            }
            let depth = self.ancestry(&p.name).len();
            branches.push((depth, p.class_id, fid));
        }
        if branches.is_empty() {
            let _: Result<(), ()> = self.bad(
                span,
                format!("`{iface_name}.{name}` is implemented by no class in this program"),
            );
            return Err(());
        }
        branches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let ret_slot = self.new_slot(ret_ty);
        let join = self.new_block();
        for &(_, dcid, dfid) in &branches {
            let cb = self.new_block();
            let next = self.new_block();

            let dc = self.int_const(dcid as i64);
            let found = self.extern_call_t1(
                "pickle_class_is",
                vec![IrTy::Ptr, IrTy::Int],
                IrTy::Bool,
                vec![receiver, dc],
            )?;
            self.term(IrTerm::BranchIf {
                cond: found,
                then: cb,
                else_: next,
            });

            self.cur = cb;
            let call_args = self.marshal_method_args(e, dfid, args, Some(receiver))?;
            let val = self.emit_call_to(dfid, call_args, e)?;
            self.instr(IrInstr::StoreSlot {
                slot: ret_slot,
                v: val,
            });
            self.term(IrTerm::Branch { target: join });

            self.cur = next;
        }
        // Runtime fallback: the receiver's own class's table (a superclass arm
        // already caught in-branch classes; this reads the concrete bucket of
        // whichever class the receiver actually is).
        let iid = self.int_const(iface_id as i64);
        let midx = self.int_const(m_idx as i64);
        let fp = self.extern_call_t1(
            "pickle_iface_method",
            vec![IrTy::Ptr, IrTy::Int, IrTy::Int],
            IrTy::Int,
            vec![receiver, iid, midx],
        )?;
        let is_zero = self.cmp_zero(fp, IrBinOp::Eq, false)?;
        let ok = self.new_block();
        let guard = self.new_block();
        self.term(IrTerm::BranchIf {
            cond: is_zero,
            then: guard,
            else_: ok,
        });

        self.cur = guard;
        self.extern_call_void("pickle_panic_no_iface_method", vec![], vec![]);
        self.term(IrTerm::Branch { target: join });

        self.cur = ok;
        // Every implementer's method has the same declared shape, so the first
        // branch's exact signature prototypes the indirect call.
        let prototype = branches[0].2;
        let call_args = self.marshal_method_args(e, prototype, args, Some(receiver))?;
        // The prototype method's body may not be built yet when this call is
        // lowered (generic instantiations build after emission), so derive the
        // indirect-call signature from source types: a pointer `this` receiver
        // plus the declared method parameters' IR types (every implementer
        // shares the same declared shape, so any branch's signature works).
        let mut params: Vec<IrTy> = vec![IrTy::Ptr];
        if let Some(src) = self.src_param_tys.get(&prototype) {
            for t in src.clone() {
                params.push(self.map_ty(&t, span).unwrap_or(IrTy::Ptr));
            }
        }
        let ret = ret_ty;
        let dst = self.temp();
        self.instr(IrInstr::CallInd {
            dst: Some(dst),
            fn_addr: fp,
            params,
            ret,
            args: call_args,
        });
        self.instr(IrInstr::StoreSlot {
            slot: ret_slot,
            v: dst,
        });
        self.term(IrTerm::Branch { target: join });

        self.cur = join;
        let out = self.temp();
        self.instr(IrInstr::LoadSlot {
            dst: out,
            slot: ret_slot,
        });
        Ok(out)
    }

    /// A zero-argument interface call against an already-lowered receiver,
    /// used by the `Iterable`/`Iterator` protocol surface in `for_in_protocol`.
    /// `ift` is the static interface view the call dispatches under
    /// (`Iterable<elem>` / `Iterator<elem>`), `rec` the receiver temp, and
    /// `ret_ty` the member's runtime type — `Ptr` for `iterator() ->
    /// Iterator<T>` and for `next() -> T?`. The synthetic `e` is only ever
    /// seen by the zero-arg marshalling path, which never touches it.
    fn interface_zero_arg_call(
        &mut self,
        span: Span,
        ift: &Ty,
        rec: Temp,
        name: &str,
        ret_ty: IrTy,
    ) -> Result<Temp, ()> {
        let dummy = Expr {
            span,
            kind: ExprKind::Ident(String::new()),
        };
        self.interface_dispatch(&dummy, span, ift, rec, name, &[], ret_ty)
    }

    /// `xs.push(v)` and `xs.pop()` for `List<T>` receivers.
    fn list_method_call(
        &mut self,
        e: &Expr,
        object: &Expr,
        name: &str,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let ot = self.ty_of(&object.span);
        let elem = match &ot {
            Some(Ty::List(inner)) => inner.as_ref().clone(),
            _ => return self.bad(e.span, "receiver is not a `List`"),
        };
        let obj = self.expr(object)?;
        match name {
            "push" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`push` takes one argument");
                }
                let rep = self.elem_rep(&elem, e.span)?;
                let v = self.expr(&args[0].value)?;
                let push_v = match rep {
                    ElemRep::Scalar(box_sym, _, _) => {
                        let vt = self.irty(args[0].value.span)?;
                        self.extern_call_t1(box_sym, vec![vt], IrTy::Ptr, vec![v])?
                    }
                    ElemRep::Ptr => v,
                };
                self.extern_call_void(
                    "pickle_list_push",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    vec![obj, push_v],
                );
                Ok(self.unit_temp())
            }
            "pop" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`pop` takes no arguments");
                }
                let rep = self.elem_rep(&elem, e.span)?;
                let raw =
                    self.extern_call_t1("pickle_list_pop", vec![IrTy::Ptr], IrTy::Ptr, vec![obj])?;
                match rep {
                    ElemRep::Scalar(_, unbox_sym, ir) => {
                        self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![raw])
                    }
                    ElemRep::Ptr => Ok(raw),
                }
            }
            "remove" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`remove` takes one argument (an index)");
                }
                let rep = self.elem_rep(&elem, e.span)?;
                let index = self.expr(&args[0].value)?;
                let raw = self.extern_call_t1(
                    "pickle_list_remove",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![obj, index],
                )?;
                match rep {
                    ElemRep::Scalar(_, unbox_sym, ir) => {
                        self.extern_call_t1(unbox_sym, vec![IrTy::Ptr], ir, vec![raw])
                    }
                    ElemRep::Ptr => Ok(raw),
                }
            }
            "insert" => {
                if args.len() != 2 {
                    return self.bad(e.span, "`insert` takes an index and a value");
                }
                let rep = self.elem_rep(&elem, e.span)?;
                let index = self.expr(&args[0].value)?;
                let v = self.expr(&args[1].value)?;
                let ins_v = match rep {
                    ElemRep::Scalar(box_sym, _, _) => {
                        let vt = self.irty(args[1].value.span)?;
                        self.extern_call_t1(box_sym, vec![vt], IrTy::Ptr, vec![v])?
                    }
                    ElemRep::Ptr => v,
                };
                self.extern_call_void(
                    "pickle_list_insert",
                    vec![IrTy::Ptr, IrTy::Int, IrTy::Ptr],
                    vec![obj, index, ins_v],
                );
                Ok(self.unit_temp())
            }
            "sort" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`sort` takes no arguments");
                }
                let kind = self.sort_kind(&elem, e.span)?;
                self.extern_call_void(
                    "pickle_list_sort",
                    vec![IrTy::Ptr, IrTy::Int],
                    vec![obj, kind],
                );
                Ok(self.unit_temp())
            }
            other => self.bad(
                e.span,
                format!("`{other}` method on `List` is not lowered yet"),
            ),
        }
    }

    /// Lower a `Stream` method call. `Stream` is carried at the ABI as a raw
    /// object pointer; every method takes that pointer as its receiver and its
    /// result maps directly onto the IR boundary type (`List<byte>?` = pointer,
    /// `int`/`bool` = the scalar itself).
    fn stream_method_call(
        &mut self,
        e: &Expr,
        object: &Expr,
        name: &str,
        args: &[CallArg],
    ) -> Result<Temp, ()> {
        let obj = self.expr(object)?;
        match name {
            "read" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`read` takes one argument (a byte count)");
                }
                let n = self.expr(&args[0].value)?;
                // Returns up to `n` bytes as a `List<byte>` (pointer), or the
                // null pointer (`none`) at end of stream.
                self.extern_call_t1(
                    "pickle_stream_read",
                    vec![IrTy::Ptr, IrTy::Int],
                    IrTy::Ptr,
                    vec![obj, n],
                )
            }
            "write" => {
                if args.len() != 1 {
                    return self.bad(e.span, "`write` takes one argument (a `List<byte>`)");
                }
                let bytes = self.expr(&args[0].value)?;
                // The `List<byte>` object is its own boundary representation.
                self.extern_call_t1(
                    "pickle_stream_write",
                    vec![IrTy::Ptr, IrTy::Ptr],
                    IrTy::Int,
                    vec![obj, bytes],
                )
            }
            "flush" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`flush` takes no arguments");
                }
                self.extern_call_t1(
                    "pickle_stream_flush",
                    vec![IrTy::Ptr],
                    IrTy::Bool,
                    vec![obj],
                )
            }
            "close" => {
                if !args.is_empty() {
                    return self.bad(e.span, "`close` takes no arguments");
                }
                self.extern_call_t1(
                    "pickle_stream_close",
                    vec![IrTy::Ptr],
                    IrTy::Bool,
                    vec![obj],
                )
            }
            other => self.bad(
                e.span,
                format!("`{other}` method on `Stream` is not lowered yet"),
            ),
        }
    }

    // ---- primitive ops ----

    /// How a `List<T>` element is represented at the runtime boundary.
    fn elem_rep(&mut self, elem: &Ty, span: Span) -> Result<ElemRep, ()> {
        use ElemRep::*;
        match elem {
            Ty::Int => Ok(Scalar("pickle_box_i64", "pickle_unbox_i64", IrTy::Int)),
            Ty::Float => Ok(Scalar("pickle_box_f64", "pickle_unbox_f64", IrTy::Float)),
            Ty::Bool => Ok(Scalar("pickle_box_bool", "pickle_unbox_bool", IrTy::Bool)),
            Ty::Byte => Ok(Scalar("pickle_box_i64", "pickle_unbox_i64", IrTy::Int)),
            Ty::Char => Ok(Scalar("pickle_box_char", "pickle_unbox_char", IrTy::Char)),
            Ty::String
            | Ty::Option(..)
            | Ty::Class(..)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Interface(..)
            | Ty::List(..)
            | Ty::Map(..)
            | Ty::Tuple(..)
            | Ty::Range(..)
            | Ty::Stream
            | Ty::Ptr(..)
            | Ty::Fn(..) => Ok(Ptr),
            // A `&T` element carries a reference *value*: for a scalar referent
            // that is the address (an `i64`), boxed as an integer; for a
            // managed referent the identity `Ptr` passes through uncoated.
            Ty::Ref(inner) => {
                if Self::scalar_ir(inner.as_ref()).is_some() {
                    Ok(Scalar("pickle_box_i64", "pickle_unbox_i64", IrTy::Int))
                } else {
                    Ok(Ptr)
                }
            }
            Ty::None | Ty::Empty => {
                self.bad(span, "a list of `none` has no element representation")
            }
            Ty::Unknown => self.bad(span, "list element type is not statically known"),
            Ty::Var(_) => self.bad(span, "generic element types are not lowered yet"),
        }
    }

    /// How a `Map<K, ...>` key is represented at the runtime boundary: keys
    /// are managed objects — a string object, a boxed scalar, or a composite
    /// object (list, map, enum, class/struct instance). `Ptr` means a key
    /// passes through as a pointer (its runtime boundary is the object itself,
    /// hashed/compared structurally by `runtime/src/map.rs`); `Scalar` means a
    /// scalar key is boxed for storage (and unboxed on iteration).
    fn key_rep(&mut self, kty: &Ty, span: Span) -> Result<ElemRep, ()> {
        use ElemRep::*;
        match kty {
            Ty::String => Ok(Ptr),
            Ty::Int => Ok(Scalar("pickle_box_i64", "pickle_unbox_i64", IrTy::Int)),
            Ty::Float => Ok(Scalar("pickle_box_f64", "pickle_unbox_f64", IrTy::Float)),
            Ty::Bool => Ok(Scalar("pickle_box_bool", "pickle_unbox_bool", IrTy::Bool)),
            Ty::Byte => Ok(Scalar("pickle_box_i64", "pickle_unbox_i64", IrTy::Int)),
            Ty::Char => Ok(Scalar("pickle_box_char", "pickle_unbox_char", IrTy::Char)),
            // Composite keys are their own managed objects: the runtime hashes
            // and compares them structurally by class id + payload. `Option`
            // passes through as-is (`none` = null, normalized to the canonical
            // sentinel inside the runtime; `some` = the boxed/object pointer);
            // `Tuple` is a single managed object whose fields are boxed.
            Ty::List(..)
            | Ty::Map(..)
            | Ty::Class(..)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Interface(..)
            | Ty::Range(..)
            | Ty::Option(..)
            | Ty::Tuple(..) => Ok(Ptr),
            Ty::Stream => self.bad(span, "`Stream` map keys are not lowered yet"),
            Ty::Ref(..) => self.bad(span, "`&T` map keys are not lowered yet"),
            Ty::None | Ty::Empty => self.bad(span, "a map key of type `none` is impossible"),
            Ty::Unknown => self.bad(span, "map key type is not statically known"),
            Ty::Var(_) => self.bad(span, "generic map key types are not lowered yet"),
            Ty::Ptr(..) => self.bad(span, "`T*` map keys are not lowered yet"),
            Ty::Fn(..) => self.bad(span, "function map keys are not lowered yet"),
        }
    }

    fn extern_call_void(&mut self, symbol: &str, params: Vec<IrTy>, args: Vec<Temp>) {
        let ex = self.module.extern_id(IrExtern {
            symbol: symbol.to_string(),
            params,
            ret: IrTy::Unit,
        });
        self.instr(IrInstr::Call {
            dst: None,
            callee: Callee::Extern(ex),
            args,
        });
    }

    fn extern_call_t1(
        &mut self,
        symbol: &str,
        params: Vec<IrTy>,
        ret: IrTy,
        args: Vec<Temp>,
    ) -> Result<Temp, ()> {
        let ex = self.module.extern_id(IrExtern {
            symbol: symbol.to_string(),
            params,
            ret,
        });
        let dst = self.temp();
        self.instr(IrInstr::Call {
            dst: Some(dst),
            callee: Callee::Extern(ex),
            args,
        });
        Ok(dst)
    }

    // ---- types & helpers ----

    fn ty_of(&self, span: &Span) -> Option<Ty> {
        self.types.get(span).cloned().map(|t| self.subst_ty(t))
    }

    /// Apply the active generic-parameter substitution to an inferred type.
    /// No-op outside instantiated generic bodies, where `current_subst` is empty.
    fn subst_ty(&self, ty: Ty) -> Ty {
        if self.current_subst.is_empty() {
            ty
        } else {
            ty.subst(&self.current_subst)
        }
    }

    /// Lower one call argument, honoring `&T` reference parameters. Passing a
    /// `T` value to a `&T` parameter borrows it implicitly: a scalar local is
    /// passed by address (`LocalAddr`, so the callee reads the same slot and
    /// no copy is made), a managed value by its identity (shared, non-owning).
    /// The argument is never moved or freed by the callee. An explicit `&x`
    /// (already a `*T` address/identity) and a subtype argument are passed
    /// directly.
    fn borrow_arg(&mut self, src: Option<&Ty>, a: &CallArg) -> Result<Temp, ()> {
        if let Some(hint) = src {
            if matches!(hint, Ty::Fn(..)) {
                if let Some(v) = self.generic_fn_arg_value(&a.value, hint)? {
                    return Ok(v);
                }
            }
        }
        if let Some(Ty::Ref(inner)) = src {
            let at = self.ty_of(&a.value.span);
            if let Some(at) = at {
                if at == **inner {
                    if Self::scalar_ir(inner).is_none() {
                        // Managed referent: share its identity.
                        return self.expr(&a.value);
                    }
                    // Scalar referent: pass the local's address.
                    let ExprKind::Ident(name) = &a.value.kind else {
                        return self.bad(
                            a.value.span,
                            "implicitly borrowing a scalar needs a plain local variable",
                        );
                    };
                    let Some(slot) = self.lookup(name) else {
                        return self.bad(a.value.span, format!("cannot borrow unknown `{name}`"));
                    };
                    let dst = self.temp();
                    self.instr(IrInstr::LocalAddr { dst, slot });
                    return Ok(dst);
                }
            }
        }
        self.expr(&a.value)
    }

    /// The IR scalar type behind a language scalar type, if any.
    fn scalar_ir(t: &Ty) -> Option<IrTy> {
        match t {
            Ty::Int => Some(IrTy::Int),
            Ty::Float => Some(IrTy::Float),
            Ty::Bool => Some(IrTy::Bool),
            Ty::Char => Some(IrTy::Char),
            Ty::Byte => Some(IrTy::Int),
            _ => None,
        }
    }

    fn irty(&mut self, span: Span) -> Result<IrTy, ()> {
        let Some(t) = self.ty_of(&span) else {
            return self.bad(span, "no inferred type available for this expression");
        };
        self.map_ty(&t, span)
    }

    fn map_ty(&mut self, t: &Ty, span: Span) -> Result<IrTy, ()> {
        match t {
            Ty::Bool => Ok(IrTy::Bool),
            Ty::Char => Ok(IrTy::Char),
            Ty::Byte => Ok(IrTy::Int),
            Ty::Int => Ok(IrTy::Int),
            Ty::Float => Ok(IrTy::Float),
            Ty::String => Ok(IrTy::Str),
            Ty::None | Ty::Empty => Ok(IrTy::Unit),
            // A pointer is always carried as a 64-bit address: `Int` for a
            // raw scalar pointee (never GC-tracked, like `StrAddr`/`FuncAddr`)
            // and `Ptr` for a managed pointee, which is the object pointer
            // itself. An immutable `&T` reference occupies the same bit
            // layout: a scalar referent is passed as its address (`Int`), a
            // managed referent as its identity (`Ptr`).
            Ty::Ptr(inner) | Ty::Ref(inner) => match inner.as_ref() {
                Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Byte => Ok(IrTy::Int),
                _ => Ok(IrTy::Ptr),
            },
            Ty::Option(..)
            | Ty::Class(..)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Interface(..)
            | Ty::List(..)
            | Ty::Map(..)
            | Ty::Tuple(..)
            | Ty::Range(..)
            | Ty::Stream => {
                let _ = span;
                Ok(IrTy::Ptr)
            }
            Ty::Fn(..) => {
                let _ = span;
                Ok(IrTy::Ptr)
            }
            Ty::Unknown => self.bad(span, "untyped expression cannot be lowered"),
            Ty::Var(_) => self.bad(span, "generic functions are not lowered yet"),
        }
    }

    fn annot_ty(&mut self, ty: &Option<TypeExpr>, span: Span) -> Result<IrTy, ()> {
        let Some(te) = ty else {
            return self.bad(span, "`let` requires an initializer or a type annotation");
        };
        match &te.kind {
            TypeExprKind::Path(parts) if parts.len() == 1 => match parts[0].as_str() {
                "Int" => Ok(IrTy::Int),
                "Float" => Ok(IrTy::Float),
                "Bool" => Ok(IrTy::Bool),
                "Char" => Ok(IrTy::Char),
                "String" => Ok(IrTy::Str),
                _ => Ok(IrTy::Ptr),
            },
            TypeExprKind::Pointer(inner) | TypeExprKind::Ref(inner) => {
                match self.annot_ty(&Some((**inner).clone()), span)? {
                    IrTy::Int | IrTy::Float | IrTy::Bool | IrTy::Char => Ok(IrTy::Int),
                    _ => Ok(IrTy::Ptr),
                }
            }
            _ => self.bad(span, "this type annotation is not lowered yet"),
        }
    }

    fn ensure_int(&mut self, e: &Expr) -> Result<(), ()> {
        match self.ty_of(&e.span) {
            Some(Ty::Int) => Ok(()),
            _ => self.bad(e.span, "range bounds must be `int`"),
        }
    }

    fn bad<T>(&mut self, span: Span, msg: impl Into<String>) -> Result<T, ()> {
        self.failed = true;
        let msg = msg.into();
        let mut d = Diagnostic::error_at(span, format!("codegen: {msg}"));
        // The overwhelming majority of `bad` messages describe constructs the
        // code generator has not lowered yet. Only those get E0900; genuine
        // codegen constraints that say something different stay uncoded.
        if msg.contains("not lowered yet") {
            d.code = Some(crate::error::ErrorCode::NotLowered);
        }
        if self.analyzing {
            // The real pass reports this too; holding it here keeps a program
            // that fails to lower from reporting every diagnostic twice.
            self.suppressed.push(d);
        } else {
            self.diags.emit(d);
        }
        Err(())
    }

    fn bad_span_note<T>(&mut self, msg: &str) -> Result<T, ()> {
        self.failed = true;
        let mut d = Diagnostic::error_at(
            Span::new(crate::diag::FileId(0), 0, 0),
            format!("codegen: {msg}"),
        );
        if msg.contains("not lowered yet") {
            d.code = Some(crate::error::ErrorCode::NotLowered);
        }
        if self.analyzing {
            self.suppressed.push(d);
        } else {
            self.diags.emit(d);
        }
        Err(())
    }

    // ---- allocation helpers ----

    fn temp(&mut self) -> Temp {
        let t = Temp(self.next_temp);
        self.next_temp += 1;
        t
    }

    fn unit_temp(&mut self) -> Temp {
        let t = self.temp();
        self.instr(IrInstr::Const {
            dst: t,
            c: IrConst::Int(0),
        });
        t
    }

    fn new_slot(&mut self, ty: IrTy) -> Slot {
        let s = Slot(self.fslots.len() as u32);
        self.fslots.push(ty);
        s
    }

    fn load(&mut self, slot: Slot) -> Temp {
        let d = self.temp();
        self.instr(IrInstr::LoadSlot { dst: d, slot });
        d
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        self.blocks.push(IrBlock {
            id,
            instrs: Vec::new(),
            term: IrTerm::Unreachable,
        });
        id
    }

    fn instr(&mut self, i: IrInstr) {
        let block = &mut self.blocks[self.cur.0 as usize];
        block.instrs.push(i);
    }

    fn term(&mut self, t: IrTerm) {
        let block = &mut self.blocks[self.cur.0 as usize];
        block.term = t;
    }

    fn push_scope(&mut self) {
        self.env.push(HashMap::new());
        self.manual_env.push(HashSet::new());
    }

    fn pop_scope(&mut self) {
        self.env.pop();
        self.manual_env.pop();
    }

    /// Adopt every `#[manualAlloc]` parameter at function entry so the callee
    /// owns (and is responsible for freeing) the incoming allocation.
    fn adopt_manual_params(&mut self, params: &'a [Param]) {
        for p in params {
            if !p.attrs.iter().any(|a| a.name == "manualAlloc") {
                continue;
            }
            let Some(slot) = self.lookup(&p.name) else {
                continue;
            };
            let v = self.load(slot);
            if let Ok(a) =
                self.extern_call_t1("pickle_manual_adopt", vec![IrTy::Ptr], IrTy::Ptr, vec![v])
            {
                self.instr(IrInstr::StoreSlot { slot, v: a });
            }
            self.declare_manual(&p.name);
        }
    }

    fn declare(&mut self, name: &str, slot: Slot) {
        if let Some(scope) = self.env.last_mut() {
            scope.insert(name.to_string(), slot);
        }
    }

    /// Record that `name` is a `#[manualAlloc]` binding in the current scope.
    fn declare_manual(&mut self, name: &str) {
        if let Some(scope) = self.manual_env.last_mut() {
            scope.insert(name.to_string());
        }
    }

    fn is_manual(&self, name: &str) -> bool {
        self.manual_env.iter().rev().any(|s| s.contains(name))
    }

    fn lookup(&self, name: &str) -> Option<Slot> {
        for scope in self.env.iter().rev() {
            if let Some(slot) = scope.get(name) {
                return Some(*slot);
            }
        }
        None
    }

    fn intern_string(&mut self, s: &str) -> usize {
        for (i, existing) in self.module.strings.iter().enumerate() {
            if existing == s.as_bytes() {
                return i;
            }
        }
        self.module.strings.push(s.as_bytes().to_vec());
        self.module.strings.len() - 1
    }
}

fn binary_opcode(op: AstBinOp) -> IrBinOp {
    match op {
        AstBinOp::Add => IrBinOp::Add,
        AstBinOp::Sub => IrBinOp::Sub,
        AstBinOp::Mul => IrBinOp::Mul,
        AstBinOp::Div => IrBinOp::Div,
        AstBinOp::Mod => IrBinOp::Mod,
        AstBinOp::Shl => IrBinOp::Shl,
        AstBinOp::Shr => IrBinOp::Shr,
        AstBinOp::BitAnd => IrBinOp::BitAnd,
        AstBinOp::BitOr => IrBinOp::BitOr,
        AstBinOp::BitXor => IrBinOp::BitXor,
        AstBinOp::Lt => IrBinOp::Lt,
        AstBinOp::Le => IrBinOp::Le,
        AstBinOp::Gt => IrBinOp::Gt,
        AstBinOp::Ge => IrBinOp::Ge,
        AstBinOp::Eq => IrBinOp::Eq,
        AstBinOp::Ne => IrBinOp::Ne,
        _ => IrBinOp::Add,
    }
}

/// Can two comparison operands share one `icmp`/`fcmp`? The scalar mix
/// `int`/`float` is handled by the `Itof` promotion that slices float limbs
/// onto either side, so it counts as compatible; everything else must be the
/// same IR type (both `int`, both `bool`, both pointers, ...). Unknown or
/// unresolved types are treated as compatible so substitution-time programs
/// keep flowing to their concrete form.
fn cmp_types_compatible(a: Option<IrTy>, b: Option<IrTy>) -> bool {
    match (a, b) {
        (Some(va), Some(vb)) => {
            if va == IrTy::Float || vb == IrTy::Float {
                true
            } else {
                va == vb
            }
        }
        _ => true,
    }
}

fn assign_opcode(op: AssignOp) -> IrBinOp {
    match op {
        AssignOp::Add => IrBinOp::Add,
        AssignOp::Sub => IrBinOp::Sub,
        AssignOp::Mul => IrBinOp::Mul,
        AssignOp::Div => IrBinOp::Div,
        AssignOp::Mod => IrBinOp::Mod,
        AssignOp::Shl => IrBinOp::Shl,
        AssignOp::Shr => IrBinOp::Shr,
        AssignOp::BitAnd => IrBinOp::BitAnd,
        AssignOp::BitOr => IrBinOp::BitOr,
        AssignOp::BitXor => IrBinOp::BitXor,
        AssignOp::Assign => IrBinOp::Add,
    }
}

/// Flip a comparison op under `.not()`. Note `!EQ -> NE`, `!GT -> LE`,
/// `!LT -> GE` (the non-strict inverse), which is what a "not greater" pass
/// really means.
fn flip_cmp(op: IrBinOp, negated: bool) -> IrBinOp {
    if !negated {
        return op;
    }
    match op {
        IrBinOp::Eq => IrBinOp::Ne,
        IrBinOp::Ne => IrBinOp::Eq,
        IrBinOp::Gt => IrBinOp::Le,
        IrBinOp::Ge => IrBinOp::Lt,
        IrBinOp::Lt => IrBinOp::Ge,
        IrBinOp::Le => IrBinOp::Gt,
        other => other,
    }
}
