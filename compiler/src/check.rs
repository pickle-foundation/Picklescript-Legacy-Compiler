use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::diag::{Diagnostic, DiagnosticSink, Span};
use crate::resolve::{CallableInfo, ClassTable, CtorInfo, ResolvedProgram, TypeTableEntry};
use crate::ty::Ty;

/// Map a checker diagnostic message to its stable error code.
///
/// The checker emits ~150 diagnostics and each one would describe the same
/// semantic condition thousands of messages later, so codes are NOT assigned
/// per call site. Instead every message here is matched against the exact
/// conditions its code documents, most specific first. A message is never
/// guessed at: unlisted messages render without a code rather than with a
/// wrong one. The `codes_smoke` test pins representative messages to their
/// code so the mapping cannot quietly drift.
pub fn classify_message(msg: &str) -> Option<crate::error::ErrorCode> {
    use crate::error::ErrorCode::*;
    const TABLE: &[(&str, crate::error::ErrorCode)] = &[
        // Codegen limitation (message text is authoritative for check.rs too).
        ("not lowered yet", NotLowered),
        // E05xx ownership and memory.
        ("after `free()`", UseAfterFree),
        ("after it was moved", UseAfterFree),
        ("was already freed", DoubleFree),
        (
            "`free` is only available on a `#[manualAlloc]` binding",
            FreeOnNonManual,
        ),
        ("cannot store `#[manualAlloc]`", OwnedPosition),
        ("cannot assign `#[manualAlloc]`", OwnedPosition),
        ("cannot bind `#[manualAlloc]`", OwnedPosition),
        ("takes ownership of its argument", OwnedPosition),
        ("cannot overwrite `#[manualAlloc]` binding", OverwriteManual),
        ("cannot return `#[manualAlloc]` value", OverwriteManual),
        // E04xx ownership is only through `#[manualAlloc]`; attributes.
        ("`#[manualAlloc]` takes no arguments", ManualAlloc),
        ("`#[manualAlloc]` attribute", ManualAlloc),
        ("is not supported on static fields", ManualAlloc),
        ("is not supported on constants", ManualAlloc),
        ("requires an allocation initializer", ManualAlloc),
        ("requires a class or struct type", ManualAlloc),
        ("requires a class or struct return type", ManualAlloc),
        ("must be a class or struct type", ManualAlloc),
        ("owned field `", ManualAlloc),
        ("unknown attribute `#[", Attribute),
        ("`#[tag", Attribute),
        ("attempt to call a non-function", CallNonFunction),
        ("unknown generic function `", CallArity),
        (" argument(s), found ", CallArity),
        ("missing argument", CallArity),
        ("too many arguments in call", CallArity),
        ("named argument `", CallArity),
        ("cannot access member `", MemberOnNonClass),
        ("left side of `??` is not", Operator),
        ("`?.` on a non-option value", Operator),
        ("cannot unwrap non-option value", Operator),
        ("requires numeric or string operands", Operator),
        ("bitwise operators require", Operator),
        ("unary `", Operator),
        ("requires a numeric operand", Operator),
        ("requires a `bool` operand", Operator),
        ("requires an `int` operand", Operator),
        ("of a scalar requires a local variable", Operator),
        ("currently only supports class, struct, pointer", Operator),
        ("cannot dereference a value of type", Operator),
        ("`alloc(T, count)`", RawBuffer),
        ("`alloc` element type", RawBuffer),
        ("`alloc` count must be an `int`", RawBuffer),
        (
            "`alloc` currently only supports scalar element types",
            RawBuffer,
        ),
        ("`free(p)` takes one argument", RawBuffer),
        ("`free` expects a pointer argument", RawBuffer),
        ("`free()` takes no arguments", RawBuffer),
        ("may only be used inside an `unsafe` block", UnsafeRequired),
        // E03xx type checking.
        ("type mismatch in ", TypeMismatch),
        (
            "cannot return a value from a function with no return type",
            ReturnValue,
        ),
        ("`if` branches have mismatched types", ReturnValue),
        ("match arms produce inconsistent types", ReturnValue),
        ("condition must be a `bool`", ConditionNotBool),
        ("used outside of a loop", BreakOutsideLoop),
        ("`for (x in ...)` requires a sequence", ForSequence),
        ("map keys must be `string` values", MapKeyString),
        ("map keys must be `", MapKeyString),
        ("index must be an `int`", Index),
        ("map index must be a `string`", Index),
        ("map index must be a `", Index),
        ("string index must be an `int`", Index),
        ("pointer index must be an `int`", Index),
        ("cannot index a value of type", Index),
        ("index assignment target must be a List", Index),
        ("indexing a pointer to a", Index),
        ("storing through a pointer to a", Index),
        ("use of undeclared name `", UndeclaredName),
        ("cannot assign to undeclared name `", UndeclaredName),
        ("no member `", NoMember),
        ("no assignable member `", NoMember),
        ("has no variant", EnumVariant),
        ("payload field(s); use `", EnumVariant),
        (
            "enum variant pattern requires a value of enum type",
            EnumVariant,
        ),
        ("cannot be constructed directly", CannotConstruct),
        (
            "cannot assign to a member of a non-class value",
            NonClassMemberAssign,
        ),
        ("declares `implements", InterfaceConformance),
        ("`this` used outside of a class body", ThisSuper),
        ("`super` used outside of a class body", ThisSuper),
        ("no parent to call `super`", ThisSuper),
        (
            "can only be used as a named constructor's delegation",
            ThisSuper,
        ),
        ("may only delegate to `this(...)`", ThisSuper),
        ("cannot assign to immutable", Assignment),
        ("must be assigned on an instance", Assignment),
        ("must be assigned on the type", Assignment),
        ("has no setter", Assignment),
        (
            "assignment target must be a variable, member, or index",
            Assignment,
        ),
        ("cannot write through an immutable reference", Assignment),
        ("cannot write to a field", Assignment),
        ("can never succeed", Cast),
        ("cannot cast `", Cast),
        // E02xx-adjacent reference rules surfaced by the checker.
        (
            "`&T` references may only be formed from an existing `&T` value or a managed referent",
            StoredRef,
        ),
        // E03xx pattern errors without a dedicated code stay unlisted on
        // purpose: `tuple pattern does not match a tuple value` has no bucket
        // that says exactly what happened, so it renders uncoded.
    ];
    for (sub, code) in TABLE {
        if msg.contains(sub) {
            return Some(*code);
        }
    }
    None
}

/// A bound local variable inside a function body.
#[derive(Debug, Clone)]
struct Local {
    ty: Ty,
    mutable: bool,
    /// Declared with `#[manualAlloc]`: the binding owns its allocation and
    /// must be released with `.free()`.
    manual: bool,
    /// Already freed via `.free()` in this scope (statically provable
    /// use-after-free).
    freed: bool,
    /// Ownership was moved out of this binding (`#[manualAlloc] let y = x`,
    /// passing to an owned parameter, or returning it). A moved binding may
    /// not be read or freed again.
    moved: bool,
}

pub fn check_program(prog: &Program, resolved: &ResolvedProgram, diags: &DiagnosticSink) {
    let mut ck = Checker::new(prog, resolved, diags);
    ck.check();
}

/// Type-check and return the inferred type of every expression, keyed by its
/// `Span`. Used by codegen (`emit`) so IR emission reuses the checker's
/// inference instead of re-deriving types.
pub fn collect_expr_types(
    prog: &Program,
    resolved: &ResolvedProgram,
    diags: &DiagnosticSink,
) -> HashMap<Span, Ty> {
    let mut ck = Checker::new(prog, resolved, diags);
    ck.check();
    std::mem::take(&mut ck.types)
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

/// Free functions annotated `#[manualAlloc]` (result owned by the caller) and,
/// for each, which positional parameters are `#[manualAlloc]` (owned).
/// Which `List<T>` element types `sort` can order. It compares raw boxed
/// payloads (int/byte/char/bool), float bits, or string bytes.
fn is_list_sortable(t: &Ty) -> bool {
    matches!(
        t,
        Ty::Int | Ty::Float | Ty::Byte | Ty::Char | Ty::Bool | Ty::String
    )
}

/// Whether `s` is a plain C identifier, i.e. usable verbatim as a linkable symbol
/// name by `#[symbol("...")]`. Guards against a Pickle-level override that would
/// either never resolve at link time or collide with the COFF symbol table.
fn is_abi_symbol(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0].is_ascii_digit() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn collect_manual_fns(prog: &Program) -> (HashSet<String>, HashMap<String, Vec<bool>>) {
    let mut rets = HashSet::new();
    let mut params = HashMap::new();
    for item in &prog.items {
        let f = match &item.kind {
            ItemKind::Fn(f) | ItemKind::Test(f) => f,
            _ => continue,
        };
        let has = |attrs: &[Attribute]| attrs.iter().any(|a| a.name == "manualAlloc");
        if has(&item.attrs) {
            rets.insert(f.name.clone());
        }
        let flags: Vec<bool> = f.params.iter().map(|p| has(&p.attrs)).collect();
        if flags.iter().any(|b| *b) {
            params.insert(f.name.clone(), flags);
        }
    }
    (rets, params)
}

struct Checker<'a> {
    prog: &'a Program,
    resolved: &'a ResolvedProgram,
    diags: &'a DiagnosticSink,
    /// Lexical scopes; bottom (last) is innermost.
    scopes: Vec<HashMap<String, Local>>,
    /// Inferred type of each checked expression (codegen side table).
    types: HashMap<Span, Ty>,
    /// Current enclosing class name and its instantiated type args.
    self_ty: Option<Ty>,
    /// Type args for the current class's generic params (name -> Ty).
    self_args: HashMap<String, Ty>,
    /// Current function/method return type.
    ret_ty: Ty,
    /// Current function's generic params (name only), for Var resolution.
    fn_generics: Vec<String>,
    /// While checking a named constructor body: the primary constructor's
    /// parameter types that the mandatory `this(...)` delegation must match.
    named_ctor_params: Option<Vec<Ty>>,
    /// Whether the current named constructor body has delegated via `this`.
    named_ctor_delegated: bool,
    /// While checking a primary constructor body: the parent class's
    /// constructor parameter types that a `super(...)` delegation must match.
    /// `None` outside a primary constructor body (or when the class has no
    /// parent).
    super_ctor_params: Option<Vec<Ty>>,
    /// Loop depth for `break`/`continue` validation.
    loop_depth: usize,
    /// Whether the current function is `#[manualAlloc]`, i.e. returns ownership
    /// of its result to the caller.
    ret_manual: bool,
    /// Free functions marked `#[manualAlloc]`: their result is owned by the
    /// caller and must be consumed into an owned position.
    manual_ret_fns: HashSet<String>,
    /// For each free function, which positional parameters are `#[manualAlloc]`.
    manual_param_fns: HashMap<String, Vec<bool>>,
    /// Spans of `#[manualAlloc]`-returning calls whose result has not yet been
    /// consumed in the current statement.
    pending_owned_calls: HashSet<Span>,
    /// Nesting depth of `unsafe { }` blocks; raw pointer operations require it.
    unsafe_depth: u32,
}

impl<'a> Checker<'a> {
    fn new(
        prog: &'a Program,
        resolved: &'a ResolvedProgram,
        diags: &'a DiagnosticSink,
    ) -> Checker<'a> {
        let scopes: Vec<HashMap<String, Local>> = vec![HashMap::new()];
        let (manual_ret_fns, manual_param_fns) = collect_manual_fns(prog);
        Checker {
            prog,
            resolved,
            diags,
            scopes,
            types: HashMap::new(),
            self_ty: None,
            self_args: HashMap::new(),
            ret_ty: Ty::Empty,
            fn_generics: Vec::new(),
            named_ctor_params: None,
            named_ctor_delegated: false,
            super_ctor_params: None,
            loop_depth: 0,
            ret_manual: false,
            manual_ret_fns,
            manual_param_fns,
            pending_owned_calls: HashSet::new(),
            unsafe_depth: 0,
        }
    }

    fn check(&mut self) {
        // Item bodies: fn / test / const values.
        for item in &self.prog.items {
            let is_fn = matches!(item.kind, ItemKind::Fn(_) | ItemKind::Test(_));
            let is_test = matches!(item.kind, ItemKind::Test(_));
            for a in &item.attrs {
                match a.name.as_str() {
                    "manualAlloc" if is_fn => {
                        if !a.args.is_empty() {
                            self.err(a.span, "`#[manualAlloc]` takes no arguments");
                        }
                    }
                    // Exact C ABI symbol name for a top-level fn. This is how the
                    // ported runtime defines `pickle_*` ABI functions directly in
                    // Pickle instead of linking the Rust archive.
                    "symbol" if is_fn => {
                        if a.args.len() != 1 {
                            self.err(a.span, "`#[symbol(...)]` takes exactly one string literal");
                        } else {
                            match &a.args[0].kind {
                                ExprKind::Lit(Lit::String(parts)) => {
                                    let is_plain = parts
                                        .iter()
                                        .all(|p| matches!(p, crate::ast::StrPart::Text(_)));
                                    let text: String = parts
                                        .iter()
                                        .map(|p| match p {
                                            crate::ast::StrPart::Text(t) => t.clone(),
                                            crate::ast::StrPart::Expr(_) => String::new(),
                                        })
                                        .collect();
                                    if !is_plain || text.is_empty() {
                                        self.err(
                                            a.args[0].span,
                                            "`#[symbol(...)]` requires a non-empty string literal",
                                        );
                                    } else if !is_abi_symbol(&text) {
                                        self.err(
                                            a.args[0].span,
                                            "`#[symbol(...)]` must be a C identifier \
                                             (letters, digits, underscore; not starting with a digit)",
                                        );
                                    }
                                }
                                _ => {
                                    self.err(
                                        a.args[0].span,
                                        "`#[symbol(...)]` requires a non-empty string literal",
                                    );
                                }
                            }
                        }
                    }
                    "symbol" => {
                        self.err(a.span, "`#[symbol(...)]` is only allowed on functions");
                    }
                    "tag" if is_test => {
                        if a.args.is_empty() {
                            self.err(a.span, "`#[tag(...)]` needs at least one tag");
                        }
                        for arg in &a.args {
                            if let ExprKind::Lit(Lit::String(parts)) = &arg.kind {
                                let is_plain = parts
                                    .iter()
                                    .all(|p| matches!(p, crate::ast::StrPart::Text(_)));
                                let text: String = parts
                                    .iter()
                                    .map(|p| match p {
                                        crate::ast::StrPart::Text(t) => t.clone(),
                                        crate::ast::StrPart::Expr(_) => String::new(),
                                    })
                                    .collect();
                                if !is_plain || text.is_empty() {
                                    self.err(
                                        arg.span,
                                        "`#[tag(...)]` arguments must be non-empty string literals",
                                    );
                                }
                            } else {
                                self.err(
                                    arg.span,
                                    "`#[tag(...)]` arguments must be string literals",
                                );
                            }
                        }
                    }
                    "tag" => {
                        self.err(a.span, "`#[tag(...)]` is only allowed on test items");
                    }
                    _ => {
                        self.err(
                            a.span,
                            format!(
                                "attributes on declarations are not lowered yet (`#[{}]`)",
                                a.name
                            ),
                        );
                    }
                }
            }
            match &item.kind {
                ItemKind::Fn(f) => self.check_fn_signature_bodies(f, &item.attrs),
                ItemKind::Test(f) => self.check_fn_signature_bodies(f, &item.attrs),
                ItemKind::Const(c) => {
                    let ty = self
                        .resolved
                        .consts
                        .iter()
                        .find(|f| f.name == c.name)
                        .map(|f| f.ty.clone())
                        .unwrap_or(Ty::Unknown);
                    let got = self.check_expr(&c.value);
                    self.check_assignable(&ty, &got, c.span, "const initializer");
                    self.guard_stored_ref(&ty, &got, c.span, "a `const` binding");
                }
                ItemKind::Class(c) => self.check_class_bodies(c),
                ItemKind::Struct(s) => self.check_struct_bodies(s),
                ItemKind::Enum(_) | ItemKind::Interface(_) => {}
                ItemKind::Global(g) => {
                    let ty = self
                        .resolved
                        .globals
                        .iter()
                        .find(|f| f.name == g.name)
                        .map(|f| f.ty.clone())
                        .unwrap_or(Ty::Unknown);
                    let got = self.check_expr(&g.value);
                    self.check_assignable(&ty, &got, g.span, "global initializer");
                    self.guard_stored_ref(&ty, &got, g.span, "a `var` binding");
                }
            }
        }
        self.check_interface_conformance();
    }

    // ---- scope helpers -----------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str, ty: Ty, mutable: bool) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(
                name.to_string(),
                Local {
                    ty,
                    mutable,
                    manual: false,
                    freed: false,
                    moved: false,
                },
            );
        }
    }

    /// Validate `#[...]` attributes on a `let`/`var` binding and report
    /// whether the binding is `#[manualAlloc]`.
    fn check_let_attributes(&mut self, attrs: &[Attribute]) -> bool {
        let mut manual = false;
        for a in attrs {
            match a.name.as_str() {
                "manualAlloc" => {
                    if !a.args.is_empty() {
                        self.err(a.span, "`#[manualAlloc]` takes no arguments");
                    }
                    if manual {
                        self.err(a.span, "duplicate `#[manualAlloc]` attribute");
                    }
                    manual = true;
                }
                other => self.err(a.span, format!("unknown attribute `#[{other}]`")),
            }
        }
        manual
    }

    /// Mark a local as `#[manualAlloc]` after `bind_pattern` inserted it.
    fn mark_manual(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(l) = scope.get_mut(name) {
                l.manual = true;
                return;
            }
        }
    }

    fn is_manual(&self, name: &str) -> bool {
        self.lookup(name).map(|l| l.manual).unwrap_or(false)
    }

    fn is_freed(&self, name: &str) -> bool {
        self.lookup(name).map(|l| l.freed).unwrap_or(false)
    }

    fn mark_moved(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(l) = scope.get_mut(name) {
                l.moved = true;
                return;
            }
        }
    }

    /// If `e` is a bare identifier naming a live `#[manualAlloc]` local, return
    /// its name. Naming it in an owning position (another manual binding, an
    /// owned parameter, or an owned return) hands over ownership.
    fn manual_move_source(&self, e: &Expr) -> Option<String> {
        if let ExprKind::Ident(name) = &e.kind {
            if let Some(l) = self.lookup(name) {
                if l.manual && !l.moved && !l.freed {
                    return Some(name.clone());
                }
            }
        }
        None
    }

    /// Mark a pending `#[manualAlloc]`-returning call as consumed by the
    /// enclosing owned position.
    fn consume_owned(&mut self, e: &Expr) {
        self.pending_owned_calls.remove(&e.span);
    }

    /// Is `e` a fresh allocation that an owned position may take directly:
    /// a class/struct constructor call, or a call to a `#[manualAlloc]`
    /// function.
    fn is_fresh_allocation(&self, e: &Expr) -> bool {
        if self.pending_owned_calls.contains(&e.span) {
            return true;
        }
        if let ExprKind::Call { callee, .. } = &e.kind {
            if let ExprKind::Ident(name) = &callee.kind {
                if self.resolved.types.contains_key(name) {
                    return matches!(
                        self.types.get(&e.span),
                        Some(Ty::Class(..) | Ty::Struct(..))
                    );
                }
            }
        }
        false
    }

    /// Check the arguments at an owned-parameter boundary: each owned parameter
    /// receives a `#[manualAlloc]` binding (moved) or a fresh allocation.
    fn check_manual_args(&mut self, flags: &[bool], args: &[CallArg]) {
        for (i, a) in args.iter().enumerate() {
            if !flags.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Some(name) = self.manual_move_source(&a.value) {
                self.mark_moved(&name);
            } else if self.is_fresh_allocation(&a.value) {
                self.consume_owned(&a.value);
            } else {
                self.err_note(
                    a.span,
                    format!(
                        "parameter #{} is `#[manualAlloc]` and takes ownership of its argument",
                        i + 1
                    ),
                    "pass a `#[manualAlloc]` binding or a fresh allocation",
                );
            }
        }
    }

    /// Snapshot the consumption state (`moved`, `freed`) of every live local,
    /// so a branch can be analysed from the state at the join point and merged
    /// afterwards.
    fn capture_moved(&self) -> Vec<HashMap<String, (bool, bool)>> {
        self.scopes
            .iter()
            .map(|s| {
                s.iter()
                    .map(|(k, v)| (k.clone(), (v.moved, v.freed)))
                    .collect()
            })
            .collect()
    }

    fn set_moved(&mut self, state: &[HashMap<String, (bool, bool)>]) {
        for (scope, snapshot) in self.scopes.iter_mut().zip(state.iter()) {
            for (name, local) in scope.iter_mut() {
                if let Some((moved, freed)) = snapshot.get(name) {
                    local.moved = *moved;
                    local.freed = *freed;
                }
            }
        }
    }

    /// Union the consumption bits of `b` into `a` (a value consumed on *either*
    /// path must be treated as consumed after the join).
    fn union_moved(a: &mut [HashMap<String, (bool, bool)>], b: &[HashMap<String, (bool, bool)>]) {
        for (sa, sb) in a.iter_mut().zip(b.iter()) {
            for (name, (moved, freed)) in sb {
                let slot = sa.entry(name.clone()).or_insert((false, false));
                slot.0 = slot.0 || *moved;
                slot.1 = slot.1 || *freed;
            }
        }
    }

    fn mark_freed(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(l) = scope.get_mut(name) {
                l.freed = true;
                return;
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        for scope in self.scopes.iter().rev() {
            if let Some(l) = scope.get(name) {
                return Some(l);
            }
        }
        None
    }

    fn err(&self, span: Span, msg: impl Into<String>) {
        let msg = msg.into();
        let mut d = Diagnostic::error_at(span, msg.clone());
        if let Some(code) = classify_message(&msg) {
            d.code = Some(code);
        }
        self.diags.emit(d);
    }

    fn err_note(&self, span: Span, msg: impl Into<String>, note: impl Into<String>) {
        let msg = msg.into();
        let mut d = Diagnostic::error_at(span, msg.clone()).note(note);
        if let Some(code) = classify_message(&msg) {
            d.code = Some(code);
        }
        self.diags.emit(d);
    }

    // ---- assignability -----------------------------------------------------

    /// Is `got` acceptable where `want` is declared? Handles option/`none`,
    /// `Var` params, interface conformance, `Unknown` recovery.
    fn ok_types(&mut self, want: &Ty, got: &Ty) -> bool {
        if *got == Ty::Unknown || *want == Ty::Unknown {
            return true;
        }
        if let Ty::Var(n) = want {
            // A generic param accepts any single type (unified later).
            let _ = n;
            return true;
        }
        if got == want {
            return true;
        }
        // An `&T` reference parameter accepts the bare value `T` by implicit
        // borrow: `f(x)` for `f(p: &T)`. Borrowing never moves or owns the
        // value, so a `#[manualAlloc]` caller keeps ownership. A raw `*T`
        // argument is NOT accepted (casting between pointer kinds is explicit).
        if let Ty::Ref(inner) = want {
            return self.ok_types(inner, got);
        }
        // none -> T?
        if got.is_none() && want.is_option() {
            return true;
        }
        // T? expression can satisfy T?; T satisfies T? via implicit option.
        if want.is_option() {
            if got.is_option() {
                let (wi, gi) = (want.inner_option().unwrap(), got.inner_option().unwrap());
                return self.ok_types(&wi, &gi);
            }
            let inner = want.inner_option().unwrap();
            return self.ok_types(&inner, got);
        }
        // A `Map` widens when its key or value types widen through the same
        // option rules (e.g. a `Map<int, V>` literal satisfies a
        // `Map<int?, V>` binding). Container covariance is otherwise rejected.
        if let (Ty::Map(kw, vw), Ty::Map(kg, vg)) = (want, got) {
            let key_opt =
                matches!(kw.as_ref(), Ty::Option(_)) || matches!(kg.as_ref(), Ty::Option(_));
            let val_opt =
                matches!(vw.as_ref(), Ty::Option(_)) || matches!(vg.as_ref(), Ty::Option(_));
            if (key_opt || val_opt) && self.ok_types(kw, kg) && self.ok_types(vw, vg) {
                return true;
            }
        }
        // class implements interface?
        if let (Some(gotn), Some(wantn)) = (got.named(), want.named()) {
            if gotn != wantn {
                if self.implements_interface(gotn, wantn) {
                    return true;
                }
                if self.conforms_to(got, want) {
                    return true;
                }
            }
            // A subclass/descendant `got` is assignable to an ancestor `want`.
            if matches!(want, Ty::Class(..) | Ty::Struct(..)) && self.is_ancestor(wantn, gotn) {
                return true;
            }
        }
        // Function types: parameters are contravariant, result covariant, and
        // unknown (`?`) types on either side widen to accept.
        if let (Ty::Fn(gp, gr), Ty::Fn(wp, wr)) = (got, want) {
            if gp.len() != wp.len() {
                return false;
            }
            for (g, w) in gp.iter().zip(wp.iter()) {
                if *g == Ty::Unknown || *w == Ty::Unknown {
                    continue;
                }
                // contravariance: callee params may accept a wider type
                if !self.ok_types(g, w) {
                    return false;
                }
            }
            if **gr == Ty::Unknown || **wr == Ty::Unknown {
                return true;
            }
            return self.ok_types(gr, wr);
        }
        // Empty (void) is never assignable to a value type; but a `return;` in
        // a void fn and statement-position block tails are handled elsewhere.
        false
    }

    fn check_assignable(&mut self, want: &Ty, got: &Ty, span: Span, what: &str) {
        if !self.ok_types(want, got) {
            self.err_note(
                span,
                format!("type mismatch in {what}: expected `{want}`, found `{got}`"),
                format!(
                    "`{}` does not satisfy `{}`; use `as` for an explicit conversion",
                    got, want
                ),
            );
        }
    }

    /// `check_assignable`, except a `byte` target also accepts an `int` literal
    /// in 0..=255 or an ASCII `char` literal (mirrors the byte-domain rule
    /// match patterns already apply). This is what makes string/`List<byte>`
    /// buffer building readable (`s[i] = 'a'`, `buf[i] = 32`).
    fn check_assign_rhs(&mut self, want: &Ty, got: &Ty, value: &Expr, span: Span, what: &str) {
        if *want == Ty::Byte {
            if let ExprKind::Lit(lit) = &value.kind {
                let fits_byte = match lit {
                    Lit::Int { value } => *value >= 0 && *value <= 255,
                    Lit::Char(c) => (*c as u32) <= 0x7F,
                    _ => false,
                };
                if fits_byte {
                    return;
                }
            }
        }
        self.check_assignable(want, got, span, what);
    }

    /// A `&T` value may be *stored or returned* only when the source already
    /// IS the reference (`&T`), or when it is a managed referent sharing its
    /// identity (no borrow; the GC keeps it alive). Storing/returning a bare
    /// scalar referent is rejected up front: the borrow would dangle as soon
    /// as the borrowing function's frame dies. (Call arguments keep the
    /// implicit borrow: `f(x)` for `f(p: &T)`.)
    fn guard_stored_ref(&mut self, want: &Ty, got: &Ty, span: Span, what: &str) {
        let Ty::Ref(inner) = want else { return };
        if got == want {
            return;
        }
        let referent_is_managed = !matches!(
            inner.as_ref(),
            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Byte
        );
        if got == inner.as_ref() && referent_is_managed {
            return;
        }
        if got == inner.as_ref() {
            self.err_note(
                span,
                format!(
                    "cannot initialize {what} by borrowing a bare `{got}` value into a `{want}` reference; `&T` references may only be formed from an existing `&T` value or a managed referent"
                ),
                "pass the existing `&T` value, or borrow at a call argument instead",
            );
        }
    }

    // ---- type table lookups ------------------------------------------------

    fn class_table(&self, name: &str) -> Option<ClassTable> {
        match self.resolved.types.get(name) {
            Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => Some(t.clone()),
            _ => None,
        }
    }

    /// Substitute concrete args for a type's generic `Var`s.
    fn subst(&self, ty: &Ty, args: &HashMap<String, Ty>) -> Ty {
        ty.subst(args)
    }

    /// The `(name, generic-args)` pair that backs this type for member lookup.
    fn type_key(&self, ty: &Ty) -> Option<(String, HashMap<String, Ty>)> {
        match ty {
            Ty::Class(n, a) | Ty::Struct(n, a) | Ty::Enum(n, a) => {
                let params = self
                    .resolved
                    .types
                    .get(n)
                    .map(|e| e.generics().to_vec())
                    .unwrap_or_default();
                let mut map = HashMap::new();
                for (i, p) in params.iter().enumerate() {
                    map.insert(p.clone(), a.get(i).cloned().unwrap_or(Ty::Unknown));
                }
                Some((n.clone(), map))
            }
            Ty::Interface(n, _) => Some((n.clone(), HashMap::new())),
            _ => None,
        }
    }

    fn implements_interface(&self, class: &str, iface: &str) -> bool {
        let mut pending: Vec<String> = vec![class.to_string()];
        let mut seen: Vec<String> = Vec::new();
        while let Some(name) = pending.pop() {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let table = match self.resolved.types.get(&name) {
                Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => t.clone(),
                _ => continue,
            };
            if table.implements.iter().any(|t| t.named() == Some(iface)) {
                return true;
            }
            if let Some(p) = table
                .extends
                .as_ref()
                .and_then(|t| t.named().map(str::to_string))
            {
                pending.push(p);
            }
        }
        false
    }

    /// The element type `T` of an `Iterable<T>`: either an interface type
    /// `Iterable<int>` directly, or a class/struct that transitively declares
    /// `implements Iterable<...>` (the class's own generic args substitute
    /// through the implements declaration, so `MyList<string>` yields
    /// `string`). `none` when the value is not iterable via the protocol.
    fn iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        if let Ty::Interface(n, args) = ty {
            return (n == "Iterable").then(|| args.first().cloned().unwrap_or(Ty::Unknown));
        }
        let (name, args_map) = self.type_key(ty)?;
        let mut chain: Vec<String> = vec![name];
        let mut seen: Vec<String> = Vec::new();
        while let Some(cname) = chain.pop() {
            if seen.contains(&cname) {
                continue;
            }
            seen.push(cname.clone());
            let Some(table) = self.class_table(&cname) else {
                continue;
            };
            for i in &table.implements {
                if let Ty::Interface(iname, iargs) = i {
                    if iname == "Iterable" {
                        let t = iargs.first().cloned().unwrap_or(Ty::Unknown);
                        return Some(self.subst(&t, &args_map));
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

    /// True if `desc` extends/structurally inherits `anc` through the class
    /// chain (used for upcast/downcast checks in casts).
    fn is_ancestor(&self, anc: &str, desc: &str) -> bool {
        let mut pending: Vec<String> = vec![desc.to_string()];
        let mut seen: Vec<String> = Vec::new();
        while let Some(name) = pending.pop() {
            if name == anc {
                return true;
            }
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let table = match self.resolved.types.get(&name) {
                Some(TypeTableEntry::Class(t)) | Some(TypeTableEntry::Struct(t)) => t.clone(),
                _ => continue,
            };
            if let Some(p) = table
                .extends
                .as_ref()
                .and_then(|t| t.named().map(str::to_string))
            {
                pending.push(p);
            }
        }
        false
    }

    /// True if `got`'s declared interfaces include `want` (or an ancestor).
    fn conforms_to(&self, got: &Ty, want: &Ty) -> bool {
        let (Some(gotn), Some(wantn)) = (got.named(), want.named()) else {
            return false;
        };
        if wantn == gotn {
            return true;
        }
        if !matches!(want, Ty::Interface(..)) {
            return false;
        }
        self.implements_interface(gotn, wantn)
    }

    // ---- member lookup -----------------------------------------------------

    fn find_field(
        &self,
        class: &str,
        name: &str,
        args: &HashMap<String, Ty>,
    ) -> Option<(crate::resolve::FieldInfo, bool)> {
        let mut chain: Vec<String> = vec![class.to_string()];
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
            if let Some(f) = table.fields.iter().find(|f| f.name == name) {
                return Some((f.clone(), true));
            }
            if let Some(f) = table.consts.iter().find(|f| f.name == name) {
                return Some((f.clone(), true));
            }
            if let Some(p) = table
                .extends
                .as_ref()
                .and_then(|t| t.named().map(str::to_string))
            {
                chain.push(p);
            }
        }
        let _ = args;
        None
    }

    fn find_method(&self, class: &str, name: &str) -> Option<CallableInfo> {
        let mut chain: Vec<String> = vec![class.to_string()];
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
            if let Some(m) = table.methods.iter().find(|m| m.name == name) {
                return Some(m.clone());
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

    fn find_property(&self, class: &str, name: &str) -> Option<crate::resolve::PropertyInfo> {
        let table = self.class_table(class)?;
        table.properties.iter().find(|p| p.name == name).cloned()
    }

    fn find_named_ctor(&self, class: &str, name: &str) -> Option<CtorInfo> {
        let table = self.class_table(class)?;
        table
            .named_ctors
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.clone())
    }

    /// The instance field names that carry an initializer in the class/struct
    /// declaration, used to derive the synthesized constructor's parameters.
    fn initialized_fields(&self, name: &str) -> Vec<String> {
        let members = self.prog.items.iter().find_map(|item| match &item.kind {
            ItemKind::Class(c) if c.name == name => Some(&c.members[..]),
            ItemKind::Struct(c) if c.name == name => Some(&c.members[..]),
            _ => None,
        });
        match members {
            Some(members) => members
                .iter()
                .filter_map(|m| match m {
                    ClassMember::Field {
                        name,
                        init: Some(_),
                        is_static: false,
                        ..
                    } => Some(name.clone()),
                    _ => None,
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Parameter types of the constructor a named constructor delegates to:
    /// the explicit primary constructor, or the synthesized one over the
    /// chain (see `synthesized_ctor_param_tys`).
    fn primary_ctor_param_tys(&self, table: &ClassTable) -> Vec<Ty> {
        if let Some(c) = &table.ctor {
            c.params.iter().map(|p| p.ty.clone()).collect()
        } else {
            self.synthesized_ctor_param_tys(&table.name)
        }
    }

    /// The nearest explicit-primary-constructor ancestor of `name` (the
    /// deepest class in the chain that declares a primary `constructor`),
    /// together with the number of instance-field slots belonging to classes
    /// up to and including that ancestor. Classes with only *named*
    /// constructors are still synthesized, so they never count as explicit.
    fn nearest_explicit_primary_field_count(&self, name: &str) -> Option<(String, usize)> {
        // Ancestry, rootmost first, so the recorded slot is the absolute end
        // slot of the ancestor's own instance fields (the same layout the
        // emitter computes in `nearest_explicit_primary_ancestor`).
        let mut chain = vec![name.to_string()];
        let mut cur = name.to_string();
        while let Some(p) = self.class_table(&cur).and_then(|t| {
            t.extends
                .as_ref()
                .and_then(|e| e.named().map(str::to_string))
        }) {
            chain.push(p.clone());
            cur = p;
            if chain.len() > 64 {
                break;
            }
        }
        chain.reverse();
        let mut found = None;
        let mut count = 0usize;
        for cname in chain {
            let t = self.class_table(&cname);
            let own = t
                .as_ref()
                .map(|t| t.fields.iter().filter(|f| !f.is_static).count())
                .unwrap_or(0);
            if t.as_ref().map(|t| t.ctor.is_some()).unwrap_or(false) {
                found = Some((cname.clone(), count + own));
            }
            count += own;
        }
        found
    }

    /// Synthesized-constructor parameter types for `name`: the nearest
    /// explicit-primary ancestor's constructor parameters (forwarded through
    /// the synthesized `super(...)`) followed by every non-static field
    /// strictly below that ancestor without a declared initializer. With no
    /// such ancestor, the whole chain's uninitialized fields become the
    /// parameters (superclass fields first, matching the emitter's
    /// instance-slot order).
    fn synthesized_ctor_param_tys(&self, name: &str) -> Vec<Ty> {
        let mut out = Vec::new();
        let cut = match self.nearest_explicit_primary_field_count(name) {
            Some((a, cut)) => {
                if let Some(t) = self.class_table(&a) {
                    if let Some(c) = &t.ctor {
                        out.extend(c.params.iter().map(|p| p.ty.clone()));
                    }
                }
                cut
            }
            None => 0usize,
        };
        let mut chain = vec![name.to_string()];
        let mut cur = name.to_string();
        while let Some(p) = self.class_table(&cur).and_then(|t| {
            t.extends
                .as_ref()
                .and_then(|e| e.named().map(str::to_string))
        }) {
            chain.push(p.clone());
            cur = p;
            if chain.len() > 64 {
                break;
            }
        }
        chain.reverse();
        let mut count = 0usize;
        for cname in chain {
            if let Some(t) = self.class_table(&cname) {
                let own = t.fields.iter().filter(|f| !f.is_static).count();
                if count >= cut {
                    let initialized = self.initialized_fields(&cname);
                    for f in t
                        .fields
                        .iter()
                        .filter(|f| !f.is_static && !initialized.contains(&f.name))
                    {
                        out.push(f.ty.clone());
                    }
                }
                count += own;
            }
        }
        out
    }

    /// Check a named constructor: the body must be exactly one `this(...)`
    /// delegation to the primary constructor, whose arguments are checked
    /// against the primary constructor's parameters.
    fn check_named_ctor_body(&mut self, cd: &ConstructorDecl, table: &ClassTable) {
        let Some(call) = named_ctor_delegation(&cd.body) else {
            self.err(
                cd.span,
                "named constructor body must be a single `this(...)` delegation to the primary constructor",
            );
            return;
        };
        let params = self.primary_ctor_param_tys(table);
        self.named_ctor_params = Some(params);
        self.named_ctor_delegated = false;
        let _ = self.check_expr(call);
        self.named_ctor_params = None;
        self.named_ctor_delegated = false;
    }

    // ---- bodies ------------------------------------------------------------

    fn check_fn_signature_bodies(&mut self, f: &FnDecl, attrs: &[Attribute]) {
        self.push_scope();
        self.fn_generics = f.generics.iter().map(|g| g.name.clone()).collect();
        self.ret_ty = self
            .resolved
            .fns
            .get(&f.name)
            .and_then(|fns| fns.first())
            .map(|c| c.ret.clone())
            .unwrap_or(Ty::Empty);
        self.ret_manual = attrs.iter().any(|a| a.name == "manualAlloc");
        if self.ret_manual && !matches!(self.ret_ty, Ty::Class(..) | Ty::Struct(..)) {
            self.err_note(
                f.span,
                "`#[manualAlloc]` on a function requires a class or struct return type",
                "the caller receives ownership of the returned instance",
            );
        }
        for p in &f.params {
            let manual = self.check_param_attributes(p);
            let ty = self.resolve_param_ty(p);
            self.declare(&p.name, ty.clone(), true);
            if manual {
                if !matches!(ty, Ty::Class(..) | Ty::Struct(..)) {
                    self.err_note(
                        p.span,
                        format!(
                            "`#[manualAlloc]` parameter `{}` must be a class or struct type",
                            p.name
                        ),
                        "ownership applies to class/struct instances",
                    );
                }
                self.mark_manual(&p.name);
            }
            if let Some(d) = &p.default {
                let dt = self.check_expr(d);
                self.check_assignable(&self.resolve_param_ty(p), &dt, p.span, "default argument");
            }
        }
        if let Some(body) = &f.body {
            self.check_fn_body(body);
        }
        self.ret_ty = Ty::Empty;
        self.ret_manual = false;
        self.fn_generics = Vec::new();
        self.pop_scope();
    }

    /// Validate `#[...]` field attributes and report whether the field is
    /// `#[manualAlloc]` (owned, freed recursively with its object).
    fn check_field_attributes(
        &mut self,
        attrs: &[Attribute],
        ty: Option<&Ty>,
        is_static: bool,
        is_const: bool,
        span: Span,
    ) -> bool {
        let mut manual = false;
        for a in attrs {
            match a.name.as_str() {
                "manualAlloc" => {
                    if !a.args.is_empty() {
                        self.err(a.span, "`#[manualAlloc]` takes no arguments");
                    }
                    if manual {
                        self.err(a.span, "duplicate `#[manualAlloc]` attribute");
                    }
                    manual = true;
                }
                other => self.err(a.span, format!("unknown attribute `#[{other}]`")),
            }
        }
        if manual {
            if is_static {
                self.err(span, "`#[manualAlloc]` is not supported on static fields");
            }
            if is_const {
                self.err(span, "`#[manualAlloc]` is not supported on constants");
            }
            if let Some(t) = ty {
                if !matches!(t, Ty::Class(..) | Ty::Struct(..)) {
                    self.err_note(
                        span,
                        format!(
                            "`#[manualAlloc]` field must be a class or struct type, found `{t}`"
                        ),
                        "ownership applies to class/struct instances",
                    );
                }
            }
        }
        manual
    }

    /// Treat `value` as an owned position: move a manual binding, accept a fresh
    /// allocation, or reject anything else. `what` names the destination.
    fn consume_into_owned(&mut self, value: &Expr, span: Span, what: &str) {
        if let Some(src) = self.manual_move_source(value) {
            self.mark_moved(&src);
        } else if self.is_fresh_allocation(value) {
            self.consume_owned(value);
        } else {
            self.err_note(
                span,
                format!("{what} takes ownership of its value"),
                "assign a `#[manualAlloc]` binding or a fresh allocation",
            );
        }
    }

    /// Validate `#[...]` attributes on a parameter and report whether it is
    /// `#[manualAlloc]` (the function takes ownership of the argument).
    fn check_param_attributes(&mut self, p: &Param) -> bool {
        let mut manual = false;
        for a in &p.attrs {
            match a.name.as_str() {
                "manualAlloc" => {
                    if !a.args.is_empty() {
                        self.err(a.span, "`#[manualAlloc]` takes no arguments");
                    }
                    if manual {
                        self.err(a.span, "duplicate `#[manualAlloc]` attribute");
                    }
                    manual = true;
                }
                other => self.err(a.span, format!("unknown attribute `#[{other}]`")),
            }
        }
        manual
    }

    fn resolve_param_ty(&self, p: &Param) -> Ty {
        p.ty.as_ref()
            .map(|t| {
                let mut generics = self.fn_generics.clone();
                if let Some(gt) = self.self_ty.as_ref().and_then(|t| self.type_key(t)) {
                    generics.extend(gt.1.keys().cloned());
                }
                self.resolved_fn_ty(t, &generics)
            })
            .unwrap_or(Ty::Unknown)
    }

    fn resolved_fn_ty(&self, te: &TypeExpr, generics: &[String]) -> Ty {
        // Resolve against the final module tables, so user-defined types and
        // this module's generics are visible.
        let mut g = generics.to_vec();
        if let Some(gt) = self.self_ty.as_ref().and_then(|t| self.type_key(t)) {
            for k in gt.1.keys() {
                if !g.contains(k) {
                    g.push(k.clone());
                }
            }
        }
        self.resolved
            .resolve_ty(te, self.instantiated_generics(&g), self.diags)
    }

    /// Generics with `self_args` substituted for the current class, so method
    /// param types that reference class generics resolve to the instantiated
    /// type (or stay `Var` where possible).
    fn instantiated_generics<'b>(&self, generics: &'b [String]) -> &'b [String] {
        let _ = self.self_args.is_empty();
        generics
    }

    fn check_fn_body(&mut self, body: &FnBody) {
        match body {
            FnBody::Block(b) => {
                let expr_ty = self.check_block(b);
                self.check_return_expr(&expr_ty, b.span);
                if let Some(e) = &b.expr {
                    self.check_owned_tail(e);
                }
            }
            FnBody::Expr(e) => {
                let t = self.check_expr(e);
                self.check_return_expr(&t, e.span);
                self.check_owned_tail(e);
            }
        }
    }

    /// Handle a block/expression tail value as an ownership position: either it
    /// is the owned return of a `#[manualAlloc]` function, or a leaked result.
    fn check_owned_tail(&mut self, e: &Expr) {
        if self.ret_manual {
            self.consume_owned(e);
        } else if self.pending_owned_calls.remove(&e.span) {
            self.err_note(
                e.span,
                "result of a `#[manualAlloc]` function is owned by the caller",
                "bind it with `#[manualAlloc] let`, pass it to an owned parameter, or return it",
            );
        }
    }

    fn check_return_expr(&mut self, got: &Ty, span: Span) {
        if *got == Ty::Empty {
            // Tail expr was a statement block; nothing to check for void fns.
        }
        if self.ret_ty != Ty::Empty && *got != Ty::Empty {
            let want = self.ret_ty.clone();
            self.check_assignable(&want, got, span, "return value");
            self.guard_stored_ref(&want, got, span, "a return value");
        }
    }

    fn check_block(&mut self, b: &Block) -> Ty {
        self.push_scope();
        let mut last = Ty::Empty;
        for s in &b.stmts {
            last = self.check_stmt(s);
        }
        if let Some(e) = &b.expr {
            last = self.check_expr(e);
        }
        self.pop_scope();
        last
    }

    /// Check one statement, then report any `#[manualAlloc]`-returning call
    /// whose result was not consumed into an owned position within it.
    fn check_stmt(&mut self, s: &Stmt) -> Ty {
        let before = self.pending_owned_calls.clone();
        let t = self.check_stmt_inner(s);
        let leaked: Vec<Span> = self
            .pending_owned_calls
            .iter()
            .filter(|sp| !before.contains(sp))
            .copied()
            .collect();
        for sp in leaked {
            self.err_note(
                sp,
                "result of a `#[manualAlloc]` function is owned by the caller",
                "bind it with `#[manualAlloc] let`, pass it to an owned parameter, or return it",
            );
            self.pending_owned_calls.remove(&sp);
        }
        t
    }

    fn check_stmt_inner(&mut self, s: &Stmt) -> Ty {
        match s {
            Stmt::Let {
                pattern,
                ty,
                init,
                mutable,
                attrs,
                span,
            } => {
                let manual = self.check_let_attributes(attrs);
                let annotated = ty
                    .as_ref()
                    .map(|t| self.resolved_fn_ty(t, &self.fn_generics));
                let inferred = init.as_ref().map(|e| self.check_expr(e));
                // A `&T` binding may not borrow a bare scalar into storage; the
                // declared (pattern first, then statement) reference type
                // decides what `got` must be.
                let pat_want = match pattern {
                    Pattern::Binding { ty: Some(ann), .. } => {
                        Some(self.resolved_fn_ty(ann, &self.fn_generics))
                    }
                    _ => None,
                };
                if let (Some(want), Some(got)) = (pat_want.or_else(|| annotated.clone()), &inferred)
                {
                    self.guard_stored_ref(&want, got, *span, "a `let` binding");
                }
                let final_ty = match (&annotated, &inferred) {
                    (Some(a), Some(i)) => {
                        self.check_assignable(a, i, *span, "initializer");
                        a.clone()
                    }
                    (Some(a), None) => a.clone(),
                    (None, Some(i)) => i.clone(),
                    (None, None) => Ty::Unknown,
                };
                let moved_from = init.as_ref().and_then(|e| self.manual_move_source(e));
                if manual {
                    if let Some(src) = &moved_from {
                        // Ownership transfer: `#[manualAlloc] let y = x`
                        // moves the allocation out of `x`.
                        self.mark_moved(src);
                    } else if init.is_none() {
                        self.err(*span, "`#[manualAlloc]` requires an allocation initializer");
                    } else if !matches!(final_ty, Ty::Class(..) | Ty::Struct(..)) {
                        self.err_note(
                            *span,
                            format!("`#[manualAlloc]` requires a class or struct type, found `{final_ty}`"),
                            "manual allocation applies to class/struct instances",
                        );
                    } else if let Some(init) = init {
                        // A `#[manualAlloc]` function result is consumed here.
                        self.consume_owned(init);
                    }
                } else if let Some(src) = &moved_from {
                    self.err_note(
                        *span,
                        format!("cannot bind `#[manualAlloc]` value `{src}` to a managed binding"),
                        format!("take ownership with `#[manualAlloc] let ... = {src}`, or `{src}.free()`"),
                    );
                }
                self.bind_pattern(pattern, &final_ty, *mutable);
                if manual {
                    if let Pattern::Binding { name, .. } = pattern {
                        self.mark_manual(name);
                    }
                }
                final_ty
            }
            Stmt::Const {
                name,
                ty,
                value,
                span,
            } => {
                let vt = self.check_expr(value);
                if let Some(t) = ty {
                    let tt = self.resolved_fn_ty(t, &self.fn_generics);
                    self.check_assignable(&tt, &vt, *span, "const initializer");
                    self.guard_stored_ref(&tt, &vt, *span, "a `const` binding");
                    self.declare(name, tt, false);
                } else {
                    self.declare(name, vt.clone(), false);
                }
                vt
            }
            Stmt::Return { value, span } => {
                let vt = match value {
                    Some(e) => self.check_expr(e),
                    None => Ty::Empty,
                };
                if let Some(src) = value.as_ref().and_then(|e| self.manual_move_source(e)) {
                    if self.ret_manual {
                        // Ownership passes to the caller.
                        self.mark_moved(&src);
                    } else {
                        self.err_note(
                            *span,
                            format!(
                                "cannot return `#[manualAlloc]` value `{src}` from a function that does not own its result"
                            ),
                            format!(
                                "mark the function `#[manualAlloc]` to transfer `{src}` to the caller"
                            ),
                        );
                    }
                } else if self.ret_manual {
                    // A `#[manualAlloc]` function may return a fresh allocation
                    // or another owned call directly.
                    if let Some(e) = value {
                        self.consume_owned(e);
                    }
                }
                if self.ret_ty == Ty::Empty {
                    if value.is_some() {
                        self.err_note(
                            *span,
                            "cannot return a value from a function with no return type",
                            "declare a return type with `-> T`, or remove the value",
                        );
                    }
                } else {
                    self.check_assignable(&self.ret_ty.clone(), &vt, *span, "return value");
                    self.guard_stored_ref(&self.ret_ty.clone(), &vt, *span, "a return value");
                }
                Ty::Empty
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    self.err(*span, "`break`/`continue` used outside of a loop");
                }
                Ty::Empty
            }
            Stmt::While { cond, body, .. } => {
                self.check_bool_cond(cond);
                self.loop_depth += 1;
                self.check_block(body);
                self.loop_depth -= 1;
                Ty::Empty
            }
            Stmt::For { header, body, .. } => {
                self.check_for_header(header);
                self.loop_depth += 1;
                self.check_block(body);
                self.loop_depth -= 1;
                Ty::Empty
            }
            Stmt::Try {
                body,
                binder,
                handler,
                finally,
                span,
            } => {
                self.check_block(body);
                if let (Some(name), Some(handler)) = (binder, handler) {
                    // The payload is a `string`, so the handler always knows
                    // what it holds. Declared before the block so the handler's
                    // own scope nests inside it.
                    self.push_scope();
                    self.declare(name, Ty::String, false);
                    self.check_block(handler);
                    self.pop_scope();
                } else if let Some(handler) = handler {
                    self.check_block(handler);
                }
                if let Some(finally) = finally {
                    self.check_block(finally);
                }
                let _ = span;
                Ty::Empty
            }
            Stmt::Throw { value, span } => {
                let t = self.check_expr(value);
                if t != Ty::Unknown && t != Ty::String {
                    self.err_note(
                        *span,
                        "a `throw` payload must be a `string`",
                        format!("found `{t}`"),
                    );
                }
                self.guard_stored_ref(&Ty::String, &t, *span, "a `throw` payload");
                Ty::Empty
            }
            Stmt::Expr(e) => {
                let t = self.check_expr(e);
                let _ = t;
                Ty::Empty
            }
            Stmt::Empty(_) => Ty::Empty,
        }
    }

    fn check_bool_cond(&mut self, e: &Expr) {
        let t = self.check_expr(e);
        if t != Ty::Unknown && t != Ty::Bool {
            self.err_note(e.span, "condition must be a `bool`", format!("found `{t}`"));
        }
    }

    fn check_for_header(&mut self, header: &ForHeader) {
        match header {
            ForHeader::In { pattern, sequence } => {
                // Iterating an immutable `&T` borrow iterates its referent.
                let st = self.check_expr(sequence);
                let st = match st {
                    Ty::Ref(inner) => *inner,
                    other => other,
                };
                let elem = match &st {
                    Ty::List(_) | Ty::Range(_) => st.elem().unwrap_or(Ty::Unknown),
                    Ty::Map(k, v) => {
                        // `for ((k, v) in m)` binds the key and the value; any
                        // other pattern binds the value as before.
                        if let Pattern::Tuple(parts) = pattern {
                            if parts.len() != 2 {
                                self.err(
                                    pattern.span(),
                                    "map entries pattern must bind exactly two names `(k, v)`",
                                );
                                Ty::Unknown
                            } else {
                                Ty::Tuple(vec![k.as_ref().clone(), v.as_ref().clone()])
                            }
                        } else {
                            st.elem().unwrap_or(Ty::Unknown)
                        }
                    }
                    Ty::String => Ty::Byte,
                    Ty::Unknown => Ty::Unknown,
                    other => match self.iterable_elem(&st) {
                        Some(e) => e,
                        None => {
                            self.err_note(
                                sequence.span,
                                format!(
                                    "`for (x in ...)` requires a sequence (List, string, Map, range, or an `Iterable<T>`), found `{other}`"
                                ),
                                format!("found `{other}`"),
                            );
                            Ty::Unknown
                        }
                    },
                };
                self.bind_pattern(pattern, &elem, true);
            }
            ForHeader::Range { init, cond, step } => {
                let _ = self.check_stmt(init);
                self.check_bool_cond(cond);
                let _ = self.check_expr(step);
            }
        }
    }

    fn bind_pattern(&mut self, p: &Pattern, ty: &Ty, mutable: bool) {
        match p {
            Pattern::Wildcard => {}
            Pattern::Binding { name, ty: ann, .. } => {
                // A pattern annotation like `let m: int?` is parsed into the
                // pattern; it wins over the inferred value type and is checked
                // against it.
                let final_ty = match ann {
                    Some(te) => {
                        let at = self.resolved_fn_ty(te, &self.fn_generics);
                        if *ty != Ty::Unknown {
                            self.check_assignable(&at, ty, p.span(), "binding");
                        }
                        at
                    }
                    None => ty.clone(),
                };
                self.declare(name, final_ty, mutable);
            }
            Pattern::Literal(_) => {}
            Pattern::Tuple(parts) => {
                if let Ty::Tuple(tys) = ty {
                    for (i, part) in parts.iter().enumerate() {
                        let t = tys.get(i).cloned().unwrap_or(Ty::Unknown);
                        self.bind_pattern(part, &t, mutable);
                    }
                } else if *ty != Ty::Unknown {
                    self.err(p.span(), "tuple pattern does not match a tuple value");
                }
            }
            Pattern::Variant { path, payloads } => {
                // `some(v)` binds the payload against an option scrutinee.
                if path.len() == 1 && path[0] == "some" && ty.is_option() {
                    let inner = ty.inner_option().unwrap_or(Ty::Unknown);
                    if payloads.len() != 1 {
                        self.err(p.span(), "`some` pattern takes exactly one payload binding");
                    } else {
                        self.bind_pattern(&payloads[0], &inner, mutable);
                    }
                    return;
                }
                // `Enum.Variant(p1, p2)` binding. If only the variant name is
                // given, the enum type is inferred from the value.
                let (enum_name, variant_name) = match path.len() {
                    2 => (path[0].clone(), path[1].clone()),
                    1 => match ty {
                        Ty::Enum(en, _) => (en.clone(), path[0].clone()),
                        _ => {
                            if *ty != Ty::Unknown {
                                self.err(
                                    p.span(),
                                    "enum variant pattern requires a value of enum type",
                                );
                            }
                            return;
                        }
                    },
                    _ => return,
                };
                if *ty == Ty::Unknown {
                    return;
                }
                if let Some(TypeTableEntry::Enum(t)) = self.resolved.types.get(&enum_name) {
                    if let Some((_, ftypes, _)) =
                        t.variants.iter().find(|(n, ..)| n == &variant_name)
                    {
                        for (i, part) in payloads.iter().enumerate() {
                            let ft = ftypes.get(i).cloned().unwrap_or(Ty::Unknown);
                            self.bind_pattern(part, &ft, mutable);
                        }
                    } else {
                        self.err(
                            p.span(),
                            format!("enum `{enum_name}` has no variant `{variant_name}`"),
                        );
                    }
                }
            }
            Pattern::Or(parts) => {
                for part in parts {
                    self.bind_pattern(part, ty, mutable);
                }
            }
        }
    }

    fn check_class_bodies(&mut self, c: &ClassDecl) {
        let table = self.class_table(&c.name).unwrap_or_else(|| ClassTable {
            name: c.name.clone(),
            span: c.span,
            visibility: c.visibility,
            generics: c.generics.iter().map(|g| g.name.clone()).collect(),
            extends: None,
            implements: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            properties: Vec::new(),
            ctor: None,
            named_ctors: Vec::new(),
            consts: Vec::new(),
        });
        self.self_ty = Some(Ty::Class(
            c.name.clone(),
            table.generics.iter().cloned().map(Ty::Var).collect(),
        ));
        self.self_args = table
            .generics
            .iter()
            .map(|g| (g.clone(), Ty::Var(g.clone())))
            .collect();

        for m in &c.members {
            match m {
                ClassMember::Field {
                    attrs,
                    init: Some(e),
                    ty,
                    span,
                    is_static,
                    const_,
                    ..
                } => {
                    let ft = self.check_expr(e);
                    let w = ty.as_ref().map(|t| self.resolved_fn_ty(t, &table.generics));
                    let w = w.or_else(|| {
                        self.resolved.types.get(&c.name).and_then(|e| match e {
                            TypeTableEntry::Class(t) | TypeTableEntry::Struct(t) => t
                                .fields
                                .iter()
                                .find(|f| f.name == m.name())
                                .map(|f| self.subst(&f.ty, &self.self_args)),
                            _ => None,
                        })
                    });
                    let w = match w {
                        Some(t) if !matches!(t, Ty::Unknown) => Some(t),
                        _ => Some(ft.clone()),
                    };
                    let manual =
                        self.check_field_attributes(attrs, w.as_ref(), *is_static, *const_, *span);
                    if manual && ty.is_none() {
                        self.err(
                            *span,
                            format!(
                                "owned field `{}` needs an explicit type annotation",
                                m.name()
                            ),
                        );
                    }
                    if let Some(w) = w {
                        self.check_assignable(&w, &ft, *span, "field initializer");
                        self.guard_stored_ref(&w, &ft, *span, "a field");
                    }
                    if manual {
                        self.consume_into_owned(e, *span, "owned field");
                    }
                }
                ClassMember::Field {
                    attrs,
                    init: None,
                    ty,
                    span,
                    is_static,
                    const_,
                    ..
                } => {
                    let w = ty.as_ref().map(|t| self.resolved_fn_ty(t, &table.generics));
                    let manual =
                        self.check_field_attributes(attrs, w.as_ref(), *is_static, *const_, *span);
                    if manual {
                        self.err_note(
                            *span,
                            format!(
                                "owned field `{}` must be initialized where it is declared",
                                m.name()
                            ),
                            "give it an initializer such as `= Node(...)`",
                        );
                    }
                }
                ClassMember::Const {
                    value, ty, span, ..
                } => {
                    let vt = self.check_expr(value);
                    if let Some(t) = ty {
                        let tt = self.resolved_fn_ty(t, &table.generics);
                        self.check_assignable(&tt, &vt, *span, "const initializer");
                    }
                }
                ClassMember::Method(md) => self.check_method_body(md, &table),
                ClassMember::Constructor(cd) => {
                    self.push_scope();
                    self.ret_ty = Ty::Empty;
                    self.fn_generics = Vec::new();
                    for p in &cd.params {
                        let pt = self.resolve_param_ty(p);
                        self.declare(&p.name, pt, true);
                    }
                    if cd.name.is_some() {
                        self.check_named_ctor_body(cd, &table);
                    } else {
                        // A `super(...)` delegation is only valid inside a
                        // primary constructor body of a class with a parent.
                        self.super_ctor_params = table.extends.as_ref().map(|p| {
                            let parent = self
                                .class_table(&p.named().map(str::to_string).unwrap_or_default());
                            parent
                                .map(|pt| self.primary_ctor_param_tys(&pt))
                                .unwrap_or_default()
                        });
                        self.check_block(&cd.body);
                        self.super_ctor_params = None;
                    }
                    self.pop_scope();
                }
                ClassMember::Init(b) => {
                    // `init` runs after the fields and constructor body, with
                    // `this` live.
                    self.push_scope();
                    self.ret_ty = Ty::Empty;
                    self.fn_generics = Vec::new();
                    self.check_block(b);
                    self.pop_scope();
                }
                ClassMember::Deinit(b) => {
                    // `deinit` is the GC-sweep finalizer: `this` is live and it
                    // returns unit (a bare `return` is allowed).
                    self.push_scope();
                    self.ret_ty = Ty::Empty;
                    self.fn_generics = Vec::new();
                    self.check_block(b);
                    self.pop_scope();
                }
                ClassMember::Property(p) => self.check_property_bodies(p, &table.generics),
            }
        }
        self.self_ty = None;
        self.self_args = HashMap::new();
    }

    fn check_struct_bodies(&mut self, s: &StructDecl) {
        let table = self.class_table(&s.name).unwrap_or_else(|| ClassTable {
            name: s.name.clone(),
            span: s.span,
            visibility: s.visibility,
            generics: s.generics.iter().map(|g| g.name.clone()).collect(),
            extends: None,
            implements: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            properties: Vec::new(),
            ctor: None,
            named_ctors: Vec::new(),
            consts: Vec::new(),
        });
        self.self_ty = Some(Ty::Struct(
            s.name.clone(),
            table.generics.iter().cloned().map(Ty::Var).collect(),
        ));
        self.self_args = table
            .generics
            .iter()
            .map(|g| (g.clone(), Ty::Var(g.clone())))
            .collect();
        for m in &s.members {
            match m {
                ClassMember::Field {
                    attrs,
                    init: Some(e),
                    ty,
                    span,
                    is_static,
                    const_,
                    ..
                } => {
                    let ft = self.check_expr(e);
                    let w = ty
                        .as_ref()
                        .map(|t| self.resolved_fn_ty(t, &table.generics))
                        .or_else(|| {
                            self.resolved.types.get(&s.name).and_then(|e| match e {
                                TypeTableEntry::Class(t) | TypeTableEntry::Struct(t) => t
                                    .fields
                                    .iter()
                                    .find(|f| f.name == m.name())
                                    .map(|f| self.subst(&f.ty, &self.self_args)),
                                _ => None,
                            })
                        });
                    let w = match w {
                        Some(t) if !matches!(t, Ty::Unknown) => Some(t),
                        _ => Some(ft.clone()),
                    };
                    let manual =
                        self.check_field_attributes(attrs, w.as_ref(), *is_static, *const_, *span);
                    if manual && ty.is_none() {
                        self.err(
                            *span,
                            format!(
                                "owned field `{}` needs an explicit type annotation",
                                m.name()
                            ),
                        );
                    }
                    if let Some(w) = w {
                        self.check_assignable(&w, &ft, *span, "field initializer");
                        self.guard_stored_ref(&w, &ft, *span, "a field");
                    }
                    if manual {
                        self.consume_into_owned(e, *span, "owned field");
                    }
                }
                ClassMember::Field {
                    attrs,
                    init: None,
                    ty,
                    span,
                    is_static,
                    const_,
                    ..
                } => {
                    let w = ty.as_ref().map(|t| self.resolved_fn_ty(t, &table.generics));
                    let manual =
                        self.check_field_attributes(attrs, w.as_ref(), *is_static, *const_, *span);
                    if manual {
                        self.err_note(
                            *span,
                            format!(
                                "owned field `{}` must be initialized where it is declared",
                                m.name()
                            ),
                            "give it an initializer such as `= Node(...)`",
                        );
                    }
                }
                ClassMember::Method(md) => self.check_method_body(md, &table),
                ClassMember::Property(p) => self.check_property_bodies(p, &table.generics),
                _ => {}
            }
        }
        self.self_ty = None;
        self.self_args = HashMap::new();
    }

    fn check_method_body(&mut self, md: &MethodDecl, table: &ClassTable) {
        self.push_scope();
        self.fn_generics = md.generics.iter().map(|g| g.name.clone()).collect();
        self.ret_ty = self
            .resolved
            .types
            .get(&table.name)
            .and_then(|e| match e {
                TypeTableEntry::Class(t) | TypeTableEntry::Struct(t) => t
                    .methods
                    .iter()
                    .find(|m| m.name == md.name)
                    .map(|m| m.ret.clone()),
                _ => None,
            })
            .unwrap_or(Ty::Empty);
        // Declare `this` in scope.
        if let Some(st) = &self.self_ty {
            self.declare("this", st.clone(), false);
        }
        for p in &md.params {
            let pt = self.resolve_param_ty(p);
            self.declare(&p.name, pt.clone(), true);
            if let Some(d) = &p.default {
                let dt = self.check_expr(d);
                self.check_assignable(&pt, &dt, p.span, "default argument");
            }
        }
        if let Some(body) = &md.body {
            self.check_fn_body(body);
        }
        self.ret_ty = Ty::Empty;
        self.fn_generics = Vec::new();
        self.pop_scope();
    }

    /// Type-check a property's accessor bodies. The getter is a value of the
    /// property type and runs with `this` live; the setter receives the new
    /// value bound to `value` and reads `this`.
    fn check_property_bodies(&mut self, p: &PropertyDecl, generics: &[String]) {
        let pty =
            p.ty.as_ref()
                .map(|t| self.resolved_fn_ty(t, generics))
                .unwrap_or(Ty::Unknown);
        if let Some(get) = &p.get {
            self.push_scope();
            self.fn_generics = Vec::new();
            self.ret_ty = pty.clone();
            if let Some(st) = &self.self_ty {
                if !p.is_static {
                    self.declare("this", st.clone(), false);
                }
            }
            let t = match get {
                PropertyAccessor::Expr(e) => self.check_expr(e),
                PropertyAccessor::Block(b) => self.check_block(b),
            };
            // Like `fn ... -> T`: a block ending in `return` has already
            // checked its value, so an `Empty` tail is not gated again.
            if t != Ty::Empty {
                self.check_assignable(&pty, &t, p.span, "property getter");
            }
            self.pop_scope();
        }
        if let Some(set) = &p.set {
            self.push_scope();
            self.fn_generics = Vec::new();
            self.ret_ty = Ty::Empty;
            if let Some(st) = &self.self_ty {
                if !p.is_static {
                    self.declare("this", st.clone(), false);
                }
            }
            self.declare("value", pty.clone(), true);
            let _ = match set {
                PropertyAccessor::Expr(e) => self.check_expr(e),
                PropertyAccessor::Block(b) => self.check_block(b),
            };
            self.pop_scope();
        }
    }

    fn check_interface_conformance(&mut self) {
        for entry in self.resolved.types.values() {
            let (name, implements) = match entry {
                TypeTableEntry::Class(t) | TypeTableEntry::Struct(t) => {
                    (t.name.clone(), t.implements.clone())
                }
                TypeTableEntry::Enum(t) => (t.name.clone(), t.implements.clone()),
                TypeTableEntry::Interface(_) => continue,
            };
            for i in implements {
                let Some(iface_name) = i.named().map(str::to_string) else {
                    continue;
                };
                let Some(TypeTableEntry::Interface(iface)) = self.resolved.types.get(&iface_name)
                else {
                    continue;
                };
                for member in &iface.members {
                    if member.is_property {
                        let has = self.find_property(&name, &member.name).or_else(|| {
                            self.find_field(&name, &member.name, &HashMap::new())
                                .map(|(f, _)| crate::resolve::PropertyInfo {
                                    name: f.name,
                                    visibility: f.visibility,
                                    is_static: f.is_static,
                                    ty: f.ty,
                                    has_get: true,
                                    has_set: f.mutable,
                                    span: f.span,
                                })
                        });
                        if has.is_none() {
                            self.err(
                                member.span,
                                format!(
                                    "`{name}` declares `implements {iface_name}` but has no member `{}`",
                                    member.name
                                ),
                            );
                        }
                    } else if self.find_method(&name, &member.name).is_none() {
                        self.err(
                            member.span,
                            format!(
                                "`{name}` declares `implements {iface_name}` but has no method `{}`",
                                member.name
                            ),
                        );
                    }
                }
            }
        }
    }

    // ---- expressions ------------------------------------------------------

    fn check_expr(&mut self, e: &Expr) -> Ty {
        let t = self.check_expr_impl(e);
        self.types.insert(e.span, t.clone());
        t
    }

    fn check_expr_impl(&mut self, e: &Expr) -> Ty {
        match &e.kind {
            ExprKind::Lit(Lit::String(parts)) => self.string_ty(parts),
            ExprKind::Lit(l) => self.lit_ty(l),
            ExprKind::Ident(name) => self.ident_ty(e, name),
            ExprKind::This => self.self_ty.clone().unwrap_or_else(|| {
                self.err(e.span, "`this` used outside of a class body");
                Ty::Unknown
            }),
            ExprKind::Super => self.super_ty(e),
            ExprKind::Call { callee, args } => self.check_call(e, callee, args),
            ExprKind::Member { object, name } => self.check_member(e, object, name),
            ExprKind::OptAccess { object, name } => self.check_opt_access(e, object, name),
            ExprKind::OptUnwrap(inner) => self.check_opt_unwrap(e, inner),
            ExprKind::Index { object, index } => self.check_index(e, object, index),
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(e, *op, lhs, rhs),
            ExprKind::Unary { op, operand } => self.check_unary(e, *op, operand),
            ExprKind::Assign { target, op, value } => self.check_assign(e, target, *op, value),
            ExprKind::Lambda {
                params,
                return_ty,
                body,
                ..
            } => self.check_lambda(e, params, return_ty.as_ref(), body),
            ExprKind::If {
                cond,
                then,
                else_else,
            } => self.check_if(e, cond, then, else_else.as_deref()),
            ExprKind::Match { scrutinee, arms } => self.check_match(e, scrutinee, arms),
            ExprKind::Await(inner) => {
                // `await <future>` drives a zero-argument function (a future
                // returned by an `async fn`) to completion and yields its value.
                let t = self.check_expr(inner);
                match t {
                    Ty::Fn(ps, r) if ps.is_empty() => *r,
                    Ty::Unknown => Ty::Unknown,
                    other => {
                        self.err(
                            e.span,
                            format!(
                                "cannot `await` a value of type `{other}`; a future is a zero-argument function (`() -> T`) returned by an `async fn`"
                            ),
                        );
                        Ty::Unknown
                    }
                }
            }
            ExprKind::GenericCall { name, type_args } => self.generic_fn_ty(e, name, type_args),
            ExprKind::Cast { expr, ty, kind } => self.check_cast(e, expr, ty, *kind),
            ExprKind::Unsafe(b) => {
                self.unsafe_depth += 1;
                let t = self.check_block(b);
                self.unsafe_depth -= 1;
                t
            }
            ExprKind::Block(b) => self.check_block(b),
            ExprKind::Tuple(items) => Ty::Tuple(items.iter().map(|i| self.check_expr(i)).collect()),
            ExprKind::Array(items) => {
                if items.is_empty() {
                    Ty::List(Box::new(Ty::Unknown))
                } else {
                    let elem = self.check_expr(&items[0]);
                    for i in items.iter().skip(1) {
                        let t = self.check_expr(i);
                        self.check_assignable(&elem, &t, i.span, "array element");
                    }
                    Ty::List(Box::new(elem))
                }
            }
            ExprKind::Map(pairs) => {
                if pairs.is_empty() {
                    Ty::Map(Box::new(Ty::Unknown), Box::new(Ty::Unknown))
                } else {
                    let k = self.check_expr(&pairs[0].0);
                    if k != Ty::Unknown
                        && !matches!(
                            k,
                            Ty::String
                                | Ty::Int
                                | Ty::Float
                                | Ty::Bool
                                | Ty::Char
                                | Ty::Byte
                                | Ty::List(..)
                                | Ty::Map(..)
                                | Ty::Class(..)
                                | Ty::Struct(..)
                                | Ty::Enum(..)
                                | Ty::Interface(..)
                                | Ty::Range(..)
                                | Ty::Option(..)
                                | Ty::Tuple(..)
                        )
                    {
                        self.err(
                            pairs[0].0.span,
                            format!(
                                "map keys must be `string`, `int`, `float`, `bool`, `char`, `byte`, a list, a map, a class/struct, an enum, an option, or a tuple, found `{k}`"
                            ),
                        );
                    }
                    let vt = self.check_expr(&pairs[0].1);
                    for (kk, vv) in pairs.iter().skip(1) {
                        let tk = self.check_expr(kk);
                        let tv = self.check_expr(vv);
                        self.check_assignable(&k, &tk, kk.span, "map key");
                        self.check_assignable(&vt, &tv, vv.span, "map value");
                    }
                    Ty::Map(Box::new(k), Box::new(vt))
                }
            }
            ExprKind::Range { start, end, incl } => {
                let st = self.check_expr(start);
                let _ = self.check_expr(end);
                let _ = incl;
                Ty::Range(Box::new(st))
            }
        }
    }

    fn lit_ty(&self, lit: &Lit) -> Ty {
        match lit {
            Lit::Int { .. } => Ty::Int,
            Lit::Float { .. } => Ty::Float,
            Lit::String(_) => Ty::String,
            Lit::Char(_) => Ty::Char,
            Lit::Bool(_) => Ty::Bool,
            Lit::None => Ty::None,
        }
    }

    /// Type an interpolated string: check every embedded expression (so the
    /// codegen side table has its type and unresolved names are reported), and
    /// the literal itself is a `string`.
    fn string_ty(&mut self, parts: &[StrPart]) -> Ty {
        for part in parts {
            if let StrPart::Expr(e) = part {
                let _ = self.check_expr(e);
            }
        }
        Ty::String
    }

    fn ident_ty(&mut self, e: &Expr, name: &str) -> Ty {
        if let Some(l) = self.lookup(name) {
            if l.freed {
                self.err(e.span, format!("use of `{name}` after `free()`"));
            } else if l.moved {
                self.err_note(
                    e.span,
                    format!("use of `{name}` after it was moved"),
                    "a `#[manualAlloc]` value has a single owner; bind the new name and use that",
                );
            }
            return l.ty.clone();
        }
        if let Some(fns) = self.resolved.fns.get(name) {
            let t = self.fn_sig(fns);
            let _ = e;
            return t;
        }
if let Some(c) = self.resolved.consts.iter().find(|c| c.name == name) {
            return c.ty.clone();
        }
        if let Some(g) = self.resolved.globals.iter().find(|g| g.name == name) {
            return g.ty.clone();
        }
        // Implicit receiver: a bare field/property/method name inside a
        // class or struct body refers to `this.name`.
        if let Some(st) = &self.self_ty {
            if let Some((cn, args_map)) = self.type_key(st) {
                if let Some(f) = self.find_field(&cn, name, &args_map) {
                    return self.subst(&f.0.ty, &args_map);
                }
                if let Some(p) = self.find_property(&cn, name) {
                    return p.ty.clone();
                }
                if let Some(m) = self.find_method(&cn, name) {
                    return Ty::Fn(
                        m.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(self.method_ret_ty(&m, &HashMap::new())),
                    );
                }
            }
        }
        // Type name used as a constructor/static receiver.
        if let Some(entry) = self.resolved.types.get(name) {
            return match entry {
                TypeTableEntry::Class(t) => Ty::Class(
                    t.name.clone(),
                    t.generics.iter().cloned().map(Ty::Var).collect(),
                ),
                TypeTableEntry::Struct(t) => Ty::Struct(
                    t.name.clone(),
                    t.generics.iter().cloned().map(Ty::Var).collect(),
                ),
                TypeTableEntry::Enum(t) => Ty::Enum(
                    t.name.clone(),
                    t.generics.iter().cloned().map(Ty::Var).collect(),
                ),
                TypeTableEntry::Interface(t) => Ty::Interface(
                    t.name.clone(),
                    t.generics.iter().cloned().map(Ty::Var).collect(),
                ),
            };
        }
        self.err_note(
            e.span,
            format!("use of undeclared name `{name}`"),
            "names must be declared before use",
        );
        Ty::Unknown
    }

    fn fn_sig(&self, fns: &[CallableInfo]) -> Ty {
        let Some(f) = fns.first() else {
            return Ty::Unknown;
        };
        // An `async fn` used as a value is a function returning a future
        // (`() -> R`), not the result `R` itself.
        let ret = if f.is_async && f.generics.is_empty() {
            Ty::Fn(Vec::new(), Box::new(f.ret.clone()))
        } else {
            f.ret.clone()
        };
        Ty::Fn(
            f.params.iter().map(|p| p.ty.clone()).collect(),
            Box::new(ret),
        )
    }

    /// Type of a method call. For an `async` method this is the future
    /// (`() -> R`), which `await` then drives.
    fn method_ret_ty(&self, m: &CallableInfo, map: &HashMap<String, Ty>) -> Ty {
        let ret = self.subst(&m.ret, map);
        if m.is_async {
            Ty::Fn(Vec::new(), Box::new(ret))
        } else {
            ret
        }
    }

    fn super_ty(&mut self, e: &Expr) -> Ty {
        let Some(st) = &self.self_ty else {
            self.err(e.span, "`super` used outside of a class body");
            return Ty::Unknown;
        };
        let Some((name, _)) = self.type_key(st) else {
            return Ty::Unknown;
        };
        let Some(table) = self.class_table(&name) else {
            return Ty::Unknown;
        };
        table.extends.unwrap_or_else(|| {
            self.err(e.span, "this class has no parent to call `super` on");
            Ty::Unknown
        })
    }

    fn check_call(&mut self, e: &Expr, callee: &Expr, args: &[CallArg]) -> Ty {
        // `super(...)` inside a primary constructor body delegates to the
        // parent constructor; its arguments are checked against the parent's
        // constructor parameters.
        if matches!(&callee.kind, ExprKind::Super) {
            let Some(params) = self.super_ctor_params.clone() else {
                self.err(
                    e.span,
                    "`super(...)` can only be used as a constructor delegation",
                );
                return Ty::Unknown;
            };
            self.check_args(e, &params, args);
            return Ty::Empty;
        }

        // `this(...)` inside a named constructor delegates to the primary
        // constructor; its arguments are checked against that signature.
        if matches!(&callee.kind, ExprKind::This) {
            let Some(params) = self.named_ctor_params.clone() else {
                self.err(
                    e.span,
                    "`this(...)` can only be used as a named constructor's delegation",
                );
                return Ty::Unknown;
            };
            if self.named_ctor_delegated {
                self.err(
                    e.span,
                    "a named constructor may only delegate to `this(...)` once",
                );
            }
            self.named_ctor_delegated = true;
            self.check_args(e, &params, args);
            return Ty::Empty;
        }

        // Raw-buffer builtins `alloc(T, count)` and `free(p)`. Both are
        // `unsafe`-only; the emitter lowers them to `pickle_raw_alloc` /
        // `pickle_raw_free` over manual heap memory the GC never traces.
        if let ExprKind::Ident(bname) = &callee.kind {
            if bname == "alloc" || bname == "free" {
                return self.check_raw_builtin(e, bname, args);
            }
            if bname == "assert" {
                return self.check_assert(e, args);
            }
            // `abs(x)`: numeric unary, result has the argument's type.
            if bname == "abs" {
                return self.check_abs(e, args);
            }
            // `min(a, b)` / `max(a, b)`: numeric binary, both same type.
            if bname == "min" || bname == "max" {
                return self.check_minmax(e, bname, args);
            }
            // `clamp(x, lo, hi)`: numeric ternary, all same type.
            if bname == "clamp" {
                return self.check_clamp(e, args);
            }
            // `str(x)`: numeric/bool/char value rendered as `string`.
            if bname == "str" {
                return self.check_str(e, args);
            }
            // `range(...)`: always a `List<int>`.
            if bname == "range" {
                return self.check_range(e, args);
            }
            // `read_file(path)`: whole-file bytes as `string`, or `none`.
            if bname == "read_file" {
                return self.check_read_file(e, args);
            }
            // `write_file(path, text)`: overwrite, reporting a `bool`.
            if bname == "write_file" {
                return self.check_write_file(e, args);
            }
            // `file_exists(path)`: `bool`.
            if bname == "file_exists" {
                return self.check_file_exists(e, args);
            }
            // `delete(path)`: removes a file; `bool` success.
            if bname == "delete" {
                return self.check_delete(e, args);
            }
            // `mkdir(path)`: creates a directory; `bool` success.
            if bname == "mkdir" {
                return self.check_mkdir(e, args);
            }
            // `list_dir(path)`: `List<string>?` of child paths.
            if bname == "list_dir" {
                return self.check_list_dir(e, args);
            }
            // `bytes(s)`: a string's raw bytes as a `List<byte>`.
            if bname == "bytes" {
                return self.check_bytes(e, args);
            }
            // `stream_open_read/write/append(path)`: buffered file streams
            // as `Stream?`.
            if bname == "stream_open_read"
                || bname == "stream_open_write"
                || bname == "stream_open_append"
            {
                return self.check_stream_open(e, bname, args);
            }
            // `stdout_stream()` / `stderr_stream()`: write-only console streams.
            if bname == "stdout_stream" || bname == "stderr_stream" {
                return self.check_stream_singleton(e, bname, args);
            }
            // `args()`: the program's command-line arguments.
            if bname == "args" {
                return self.check_args_builtin(e, args);
            }
            // `run_cmd(...)`: subprocess capture as `(int?, string, string, bool)?`.
            if bname == "run_cmd" {
                return self.check_run_cmd(e, args);
            }
            // `run_spawn(...)`: a `Stream?` handle to a running child.
            if bname == "run_spawn" {
                return self.check_run_spawn(e, args);
            }
            // `proc_*`: queries on a process-handle `Stream`.
            if bname == "proc_wait"
                || bname == "proc_running"
                || bname == "proc_code"
                || bname == "proc_kill"
                || bname == "proc_in_close"
            {
                return self.check_proc_handle(e, bname, args);
            }
            if bname == "proc_out_read" || bname == "proc_err_read" {
                return self.check_proc_read(e, bname, args);
            }
            if bname == "proc_in_write" {
                return self.check_proc_write(e, args);
            }
            // `current_dir()`: the working directory as `string?`.
            if bname == "current_dir" {
                return self.check_current_dir(e, args);
            }
            // `thread_spawn(body)` / `once_run(o, body)`: the two builtins that
            // are handed a closure object to run on another thread. Their bodies
            // have to be real zero-argument functions — the runtime reads slot
            // 0 of the object as a code address, so anything else would be a
            // wild call.
            if bname == "thread_spawn" {
                return self.check_task_body(e, bname, args, 0);
            }
            if bname == "once_run" {
                return self.check_task_body(e, bname, args, 1);
            }
            // `exit(code)`: terminate the process with a numeric code.
            if bname == "exit" {
                return self.check_exit(e, args);
            }
            // `captureBegin()`: enter the test-harness capture mode.
            if bname == "captureBegin" {
                return self.check_capture_begin(e, args);
            }
            // `captureTake()`: drain the captured failure message as
            // `string?`, or `none`.
            if bname == "captureTake" {
                return self.check_capture_take(e, args);
            }
            // Testing-framework `expect(value)`.
            if bname == "expect" && !self.resolved.fns.contains_key("expect") {
                return self.check_expect(e, args);
            }
        }
        // `expect(v).toEqual(w)` / `.not().toBe(w)` / ... chains.
        if let ExprKind::Member { object, name } = &callee.kind {
            if name != "not" && is_expect_method(name) && self.expect_chain_depth(object) > 0 {
                if let Some(src) = self.expect_source(object) {
                    return self.check_expect_method(e, src, name, args);
                }
            }
        }

        // `.free()` on a `#[manualAlloc]` binding.
        if let ExprKind::Member { object, name } = &callee.kind {
            if name == "free" {
                if let ExprKind::Ident(id) = &object.kind {
                    if self.is_manual(id) {
                        let _ = self.check_expr(object);
                        return self.check_free(e, id, args);
                    }
                }
                let rt = self.check_expr(object);
                let has_user_free = self
                    .type_key(&rt)
                    .is_some_and(|(c, _)| self.find_method(&c, "free").is_some());
                if !has_user_free {
                    self.err_note(
                        e.span,
                        "`free` is only available on a `#[manualAlloc]` binding",
                        "declare the binding with `#[manualAlloc] let x = T()`",
                    );
                    return Ty::Unknown;
                }
            }
        }

        // `Enum.Variant(...)` constructor call.
        if let ExprKind::Member { object, name } = &callee.kind {
            if let ExprKind::Ident(enum_name) = &object.kind {
                if let Some(TypeTableEntry::Enum(t)) = self.resolved.types.get(enum_name) {
                    let ftypes = t
                        .variants
                        .iter()
                        .find(|(n, ..)| n == name)
                        .map(|(_, ft, _)| ft.clone());
                    let ftypes = match ftypes {
                        Some(ft) => ft,
                        None => {
                            self.err(
                                callee.span,
                                format!("enum `{enum_name}` has no variant `{name}`"),
                            );
                            return Ty::Unknown;
                        }
                    };
                    self.check_args(e, &ftypes, args);
                    return Ty::Enum(t.name.clone(), Vec::new());
                }
            }
        }

        // A generic class/struct constructor call `Box<int>(args)`: check the
        // arguments against the explicit or synthesized constructor parameters
        // substituted under the type arguments, and type the call as the
        // concrete instantiation. Inside a generic function or class body the
        // type arguments may resolve to `Var`s (`Box<T>`, `Box<Pair<A, B>>`),
        // which stay parametric in the recorded type and materialize when the
        // enclosing body is instantiated.
        if let ExprKind::GenericCall { name, type_args } = &callee.kind {
            if let Some(entry) = self.resolved.types.get(name) {
                match entry {
                    TypeTableEntry::Class(_) | TypeTableEntry::Struct(_) => {}
                    TypeTableEntry::Enum(_) | TypeTableEntry::Interface(_) => {
                        self.err(e.span, format!("`{name}` cannot be constructed directly"));
                        return Ty::Unknown;
                    }
                }
                let arg_tys: Vec<Ty> = type_args
                    .iter()
                    .map(|te| self.resolved_fn_ty(te, &self.fn_generics))
                    .collect();
                let table = self.class_table(name).unwrap();
                if arg_tys.len() != table.generics.len() {
                    self.err(
                        e.span,
                        format!(
                            "`{name}` takes {} type argument(s), found {}",
                            table.generics.len(),
                            arg_tys.len()
                        ),
                    );
                    return Ty::Unknown;
                }
                let map: HashMap<String, Ty> = table
                    .generics
                    .iter()
                    .cloned()
                    .zip(arg_tys.iter().cloned())
                    .collect();
                let params: Vec<Ty> = if let Some(ctor) = &table.ctor {
                    ctor.params
                        .iter()
                        .map(|p| self.subst(&p.ty, &map))
                        .collect()
                } else {
                    self.synthesized_ctor_param_tys(name)
                        .iter()
                        .map(|t| self.subst(t, &map))
                        .collect()
                };
                self.check_args(e, &params, args);
                return match entry {
                    TypeTableEntry::Class(c) => Ty::Class(c.name.clone(), arg_tys),
                    TypeTableEntry::Struct(s) => Ty::Struct(s.name.clone(), arg_tys),
                    _ => Ty::Unknown,
                };
            }
            // Not a type name: falls through to the generic-function check
            // below.
        }

        // Constructor call: `TypeName(args)` where callee is a type name.
        if let ExprKind::Ident(cname) = &callee.kind {
            if let Some(entry) = self.resolved.types.get(cname) {
                let ct = match entry {
                    TypeTableEntry::Class(c) => Ty::Class(
                        c.name.clone(),
                        c.generics.iter().cloned().map(Ty::Var).collect(),
                    ),
                    TypeTableEntry::Struct(s) => Ty::Struct(
                        s.name.clone(),
                        s.generics.iter().cloned().map(Ty::Var).collect(),
                    ),
                    TypeTableEntry::Interface(_) | TypeTableEntry::Enum(_) => {
                        self.err(e.span, format!("`{cname}` cannot be constructed directly"));
                        return Ty::Unknown;
                    }
                };
                if let Some((cn, _)) = self.type_key(&ct) {
                    if let Some(m) = self.find_method(&cn, cname) {
                        let params: Vec<Ty> = m.params.iter().map(|p| p.ty.clone()).collect();
                        self.check_args(e, &params, args);
                        return self.method_ret_ty(&m, &HashMap::new());
                    }
                }
                if let Some(c) = self.class_table(cname) {
                    if let Some(ctor) = &c.ctor {
                        // Explicit constructor: check against its parameters.
                        let params: Vec<Ty> = ctor.params.iter().map(|p| p.ty.clone()).collect();
                        self.check_args(e, &params, args);
                    } else {
                        // Synthesized constructor: parameters are the fields
                        // without initializers (those run during construction),
                        // superclass fields first.
                        let params: Vec<Ty> = self.synthesized_ctor_param_tys(cname);
                        self.check_args(e, &params, args);
                    }
                }
                return ct;
            }
        }

        // Regular function call by name (user or builtin).
        if let ExprKind::Ident(cname) = &callee.kind {
            if let Some(fns) = self.resolved.fns.get(cname) {
                let first = fns
                    .iter()
                    // Prefer the signature whose fixed-param count best fits
                    // the positional arguments; variadic rest stays loose.
                    .min_by_key(|f| {
                        let fixed = f.params.iter().filter(|p| !p.rest).count();
                        fixed.abs_diff(args.iter().filter(|a| !a.spread).count())
                    })
                    .cloned()
                    .unwrap_or_else(|| fns[0].clone());
                if !first.generics.is_empty() {
                    // A generic function called without explicit type
                    // arguments: infer them from the arguments, then check
                    // against the substituted signature.
                    let Some(t) = self.infer_generic_call(e, &first, args) else {
                        return Ty::Unknown;
                    };
                    if let Some(flags) = self.manual_param_fns.get(cname).cloned() {
                        self.check_manual_args(&flags, args);
                    }
                    if self.manual_ret_fns.contains(cname) {
                        self.pending_owned_calls.insert(e.span);
                    }
                    return t;
                }
                self.check_args_info(e, &first.params, args);
                if let Some(flags) = self.manual_param_fns.get(cname).cloned() {
                    self.check_manual_args(&flags, args);
                }
                if self.manual_ret_fns.contains(cname) {
                    self.pending_owned_calls.insert(e.span);
                }
                if first.is_async && first.generics.is_empty() && !matches!(first.ret, Ty::Empty) {
                    // Calling an `async fn` does not run the body: it returns a
                    // captured 0-arg function (the future) typed `() -> R`,
                    // which `await` drives.
                    return Ty::Fn(Vec::new(), Box::new(first.ret.clone()));
                }
                return first.ret.clone();
            }
        }

        let ct = self.check_expr(callee);
        match &ct {
            Ty::Fn(params, ret) => {
                self.check_args(e, params, args);
                *ret.clone()
            }
            Ty::Unknown => Ty::Unknown,
            _ => {
                self.err_note(
                    e.span,
                    format!("attempt to call a non-function value of type `{ct}`"),
                    "only functions and type constructors can be called",
                );
                Ty::Unknown
            }
        }
    }

    /// Validate the raw-buffer builtins `alloc(T, count)` and `free(p)`.
    ///
    /// `alloc(int, 8)` allocates a raw buffer and is typed `*int`;
    /// `free(p)` releases it and is typed unit. Both are `unsafe`-only, and
    /// the element type is currently restricted to scalars — buffers holding
    /// managed values are not traced by the GC, so a raw address stored under
    /// a managed pointee would be collected as a PickleObject.
    fn check_raw_builtin(&mut self, e: &Expr, name: &str, args: &[CallArg]) -> Ty {
        if self.unsafe_depth == 0 {
            self.err(
                e.span,
                format!("`{name}` may only be used inside an `unsafe` block"),
            );
        }
        match name {
            "alloc" => {
                if args.len() != 2 {
                    self.err(e.span, "`alloc(T, count)` takes two arguments");
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Unknown;
                }
                let ty = &args[0];
                let count = &args[1];
                let ExprKind::Ident(ty_name) = &ty.value.kind else {
                    self.err(ty.value.span, "`alloc` element type must be a type name");
                    let _ = self.check_expr(&count.value);
                    return Ty::Unknown;
                };
                let wrapped = TypeExpr {
                    span: ty.value.span,
                    kind: TypeExprKind::Path(vec![ty_name.clone()]),
                };
                let inner = self.resolved_fn_ty(&wrapped, &[]);
                self.types.insert(ty.value.span, inner.clone());
                match &inner {
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Char => {}
                    other => {
                        self.err(
                            ty.value.span,
                            format!(
                                "`alloc` currently only supports scalar element types (int, float, bool, char), found `{other}`"
                            ),
                        );
                        let _ = self.check_expr(&count.value);
                        return Ty::Unknown;
                    }
                }
                let ct = self.check_expr(&count.value);
                if ct != Ty::Unknown && ct != Ty::Int {
                    self.err(count.value.span, "`alloc` count must be an `int`");
                }
                Ty::Ptr(Box::new(inner))
            }
            "free" => {
                if args.len() != 1 {
                    self.err(e.span, "`free(p)` takes one argument");
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Empty;
                }
                let p = &args[0].value;
                let at = self.check_expr(p);
                match &at {
                    Ty::Ptr(_) => {}
                    other => self.err(
                        p.span,
                        format!("`free` expects a pointer argument, found `{other}`"),
                    ),
                }
                Ty::Empty
            }
            _ => unreachable!(),
        }
    }

    /// Validate `assert(cond, msg?)`: the condition must be a `bool`, an
    /// optional second argument a `string` (only used when it fails).
    fn check_assert(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.is_empty() || args.len() > 2 {
            self.err(
                e.span,
                "`assert(cond, msg?)` takes a condition and an optional message",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Empty;
        }
        let cond = self.check_expr(&args[0].value);
        self.check_assignable(&Ty::Bool, &cond, args[0].value.span, "assert condition");
        if let Some(arg) = args.get(1) {
            let mt = self.check_expr(&arg.value);
            self.check_assignable(&Ty::String, &mt, arg.value.span, "assert message");
        }
        Ty::Empty
    }

    /// `expect(value)` typed as the value's type.
    fn check_expect(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`expect(value)` takes exactly one value");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        self.check_expr(&args[0].value)
    }

    /// Statement count of an `expect(...)` / `expect(...).not()` chain.
    fn expect_chain_depth(&self, e: &Expr) -> usize {
        match &e.kind {
            ExprKind::Call { callee, args }
                if args.len() == 1 && !args[0].spread && args[0].name.is_none() =>
            {
                if let ExprKind::Ident(n) = &callee.kind {
                    if n == "expect" {
                        return 1;
                    }
                }
                0
            }
            ExprKind::Call { callee, args } if args.is_empty() => {
                if let ExprKind::Member { object, name } = &callee.kind {
                    if name == "not" {
                        let d = self.expect_chain_depth(object);
                        if d > 0 {
                            return d + 1;
                        }
                    }
                }
                0
            }
            _ => 0,
        }
    }

    /// The value argument of the outermost `expect(...)` in an assertion chain.
    fn expect_source<'x>(&self, e: &'x Expr) -> Option<&'x Expr> {
        match &e.kind {
            ExprKind::Call { callee, args }
                if args.len() == 1 && !args[0].spread && args[0].name.is_none() =>
            {
                if let ExprKind::Ident(n) = &callee.kind {
                    if n == "expect" {
                        return Some(&args[0].value);
                    }
                }
                None
            }
            ExprKind::Call { callee, args } if args.is_empty() => {
                if let ExprKind::Member { object, name } = &callee.kind {
                    if name == "not" {
                        return self.expect_source(object);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Validate an `expect(v).<method>(...)` assertion.
    fn check_expect_method(&mut self, e: &Expr, src: &Expr, method: &str, args: &[CallArg]) -> Ty {
        for a in args {
            if a.spread {
                self.err(
                    a.span,
                    "spread arguments are not supported in an expectation",
                );
                return Ty::Empty;
            }
            if a.name.is_some() {
                self.err(
                    a.span,
                    "named arguments are not supported in an expectation",
                );
                return Ty::Empty;
            }
        }
        let vt = self.check_expr(src);
        let is_numeric = |t: &Ty| matches!(t, Ty::Int | Ty::Float);
        match method {
            "toBe" | "toEqual" => {
                if args.len() != 1 {
                    self.err(e.span, format!("`.{method}(expected)` takes one argument"));
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Empty;
                }
                let at = self.check_expr(&args[0].value);
                // Cross-numeric (`int` vs `float`) compares are allowed.
                if !(is_numeric(&vt) && is_numeric(&at)) {
                    self.check_assignable(&vt, &at, args[0].value.span, "expectation argument");
                }
            }
            "toBeTruthy" | "toBeFalsy" | "toBeNull" | "toExist" => {
                if !args.is_empty() {
                    self.err(e.span, format!("`.{method}()` takes no arguments"));
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                }
            }
            "toHaveLength" => {
                if args.len() != 1 {
                    self.err(e.span, "`.toHaveLength(n)` takes one argument");
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Empty;
                }
                match &vt {
                    Ty::String | Ty::List(_) | Ty::Map(_, _) => {}
                    Ty::Unknown => {}
                    other => self.err(
                        src.span,
                        format!(
                            "`.toHaveLength()` needs a string, list, or map value, found `{other}`"
                        ),
                    ),
                }
                let lt = self.check_expr(&args[0].value);
                self.check_assignable(
                    &Ty::Int,
                    &lt,
                    args[0].value.span,
                    "`.toHaveLength()` argument",
                );
            }
            "toContain" => {
                if args.len() != 1 {
                    self.err(e.span, "`.toContain(value)` takes one argument");
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Empty;
                }
                let at = self.check_expr(&args[0].value);
                match &vt {
                    Ty::String => self.check_assignable(
                        &Ty::String,
                        &at,
                        args[0].value.span,
                        "`.toContain()` argument",
                    ),
                    Ty::List(inner) => self.check_assignable(
                        inner,
                        &at,
                        args[0].value.span,
                        "`.toContain()` argument",
                    ),
                    Ty::Map(k, _) => {
                        self.check_assignable(k, &at, args[0].value.span, "`.toContain()` argument")
                    }
                    Ty::Unknown => {}
                    other => self.err(
                        src.span,
                        format!(
                            "`.toContain()` needs a string, list, or map value, found `{other}`"
                        ),
                    ),
                }
            }
            "toBeGreaterThan" | "toBeLessThan" => {
                if args.len() != 1 {
                    self.err(e.span, format!("`.{method}(n)` takes one argument"));
                    for a in args {
                        let _ = self.check_expr(&a.value);
                    }
                    return Ty::Empty;
                }
                match &vt {
                    Ty::Int | Ty::Float | Ty::Unknown => {}
                    other => self.err(
                        src.span,
                        format!("`.{method}()` needs a numeric value, found `{other}`"),
                    ),
                }
                let at = self.check_expr(&args[0].value);
                if !is_numeric(&at) && !matches!(at, Ty::Unknown) {
                    self.err(
                        args[0].value.span,
                        format!("`.{method}()` argument must be numeric, found `{at}`"),
                    );
                }
            }
            other => {
                self.err(e.span, format!("unknown expectation method `.{other}()`"));
            }
        }
        Ty::Empty
    }

    /// Validate `x.free()` on a `#[manualAlloc]` binding and mark it released.
    fn check_free(&mut self, e: &Expr, name: &str, args: &[CallArg]) -> Ty {
        for a in args {
            self.err(a.span, "`free()` takes no arguments");
            let _ = self.check_expr(&a.value);
        }
        if self.is_freed(name) {
            self.err(e.span, format!("`{name}` was already freed"));
            return Ty::Empty;
        }
        self.mark_freed(name);
        Ty::Empty
    }

    fn generic_fn_ty(&mut self, e: &Expr, name: &str, type_args: &[TypeExpr]) -> Ty {
        let Some(fns) = self.resolved.fns.get(name) else {
            self.err(e.span, format!("unknown generic function `{name}`"));
            return Ty::Unknown;
        };
        let Some(f) = fns
            .iter()
            .find(|c| !(c.span.file.0 == 0 && c.span.end == 0))
            .cloned()
        else {
            return self.fn_sig(fns);
        };
        // Instantiate the callee signature with the explicit type arguments,
        // resolved in the caller's namespace. This threads the callee's `Var`s
        // through the caller's own generic parameters, so the emitter can fully
        // substitute them at a concrete call site. When the arity mismatches,
        // the generic signature is returned so the surrounding expression keeps
        // a loose type (the emit-time arity diagnostic still fires).
        let args: Vec<Ty> = type_args
            .iter()
            .map(|te| self.resolved_fn_ty(te, &self.fn_generics))
            .collect();
        if f.generics.len() != args.len() {
            self.err(
                e.span,
                format!(
                    "`{name}` takes {} type argument(s), found {}",
                    f.generics.len(),
                    args.len()
                ),
            );
            return Ty::Fn(
                f.params.iter().map(|p| p.ty.clone()).collect(),
                Box::new(f.ret.clone()),
            );
        }
        let map: HashMap<String, Ty> = f
            .generics
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let params: Vec<Ty> = f.params.iter().map(|p| self.subst(&p.ty, &map)).collect();
        let ret = self.subst(&f.ret, &map);
        // An `async fn` called with explicit type arguments returns a future
        // (`() -> R`), exactly like the inferred-generic path.
        let ret = if f.is_async {
            Ty::Fn(Vec::new(), Box::new(ret))
        } else {
            ret
        };
        Ty::Fn(params, Box::new(ret))
    }

    fn check_args(&mut self, e: &Expr, params: &[Ty], args: &[CallArg]) {
        // Smallest overload wins is not implemented; we check against the
        // declared signature literally (positional, then named/rest). A
        // spread (`...list`) must be the single, final argument; its elements
        // are checked against every parameter position it fills.
        let mut position = 0usize;
        let mut spread_elem: Option<(Span, Ty)> = None;
        for (idx, a) in args.iter().enumerate() {
            if a.spread {
                if idx != args.len() - 1 {
                    self.err(
                        a.span,
                        "the spread argument (`...list`) must be the last argument in the call",
                    );
                }
                if spread_elem.is_some() {
                    self.err(
                        a.span,
                        "only one spread argument (`...list`) is allowed per call",
                    );
                }
                let at = self.check_expr(&a.value);
                match at {
                    Ty::List(inner) => {
                        if spread_elem.is_none() {
                            spread_elem = Some((a.span, (*inner).clone()));
                        }
                    }
                    Ty::Unknown => {}
                    _ => {
                        self.err(
                            a.span,
                            format!("a spread argument must be a `List`, found `{at}`"),
                        );
                    }
                }
                continue;
            }
            if let Some(name) = &a.name {
                self.err_note(
                    a.span,
                    format!("named argument `{name}` is not supported for this call"),
                    "positional arguments are expected here",
                );
                let _ = self.check_expr(&a.value);
                position += 1;
                continue;
            }
            let want = match params.get(position) {
                Some(p) => p.clone(),
                None => {
                    let _ = self.check_expr(&a.value);
                    self.err(e.span, "too many arguments in call");
                    position += 1;
                    continue;
                }
            };
            let got = self.check_expr(&a.value);
            self.check_assignable(&want, &got, a.span, "argument");
            position += 1;
        }
        if let Some((sspan, elem)) = spread_elem {
            // The spread fills every parameter from `position` onward.
            if position < params.len() {
                for p in params.iter().skip(position) {
                    self.check_assignable(p, &elem, sspan, "argument");
                }
            } else if position > params.len() {
                self.err(e.span, "too many arguments in call");
            }
        } else if position < params.len() {
            self.err(
                e.span,
                format!("expected {} argument(s), found {}", params.len(), position),
            );
        }
    }

    /// Check a generic function call `name(args)` by **inferring** its type
    /// arguments from the (already-checked) arguments. Every generic parameter
    /// must be pinned by an argument's type; an unpinned one reports an error
    /// telling the caller to write the type arguments explicitly. The
    /// substituted signature is checked exactly once (arguments are not
    /// re-checked), and the substituted return type is returned.
    fn infer_generic_call(&mut self, e: &Expr, f: &CallableInfo, args: &[CallArg]) -> Option<Ty> {
        let mut map: HashMap<String, Ty> = HashMap::new();
        let mut got: HashMap<Span, Ty> = HashMap::new();
        for (position, a) in args.iter().enumerate() {
            let t = self.check_expr(&a.value);
            got.insert(a.value.span, t.clone());
            if a.name.is_none() && !a.spread {
                if let Some(want) = f.params.get(position) {
                    crate::ty::infer_from(&want.ty, &t, &mut map);
                }
            }
        }
        let missing: Vec<&String> = f
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
            self.err(
                e.span,
                format!(
                    "cannot infer the type argument{plural} {names} for `{}`; \
                     specify them explicitly like `{}<...>(...)`",
                    f.name, f.name
                ),
            );
            return None;
        }
        let params: Vec<Ty> = f.params.iter().map(|p| self.subst(&p.ty, &map)).collect();
        self.check_args_typed(e, &params, args, &got);
        let ret = self.subst(&f.ret, &map);
        // Calling an inferred `async fn` returns the future, not the body's
        // result: the future is a captured zero-argument function `() -> R`.
        Some(if f.is_async {
            Ty::Fn(Vec::new(), Box::new(ret))
        } else {
            ret
        })
    }

    /// Argument checking against a signature whose expressions have already
    /// been checked: reads the argument types back from `got` instead of
    /// re-checking them (so inference's single pass stays single).
    fn check_args_typed(
        &mut self,
        e: &Expr,
        params: &[Ty],
        args: &[CallArg],
        got: &HashMap<Span, Ty>,
    ) {
        let mut position = 0usize;
        let mut spread_elem: Option<(Span, Ty)> = None;
        for (idx, a) in args.iter().enumerate() {
            if a.spread {
                if idx != args.len() - 1 {
                    self.err(
                        a.span,
                        "the spread argument (`...list`) must be the last argument in the call",
                    );
                }
                if spread_elem.is_some() {
                    self.err(
                        a.span,
                        "only one spread argument (`...list`) is allowed per call",
                    );
                }
                let gt = got.get(&a.value.span).cloned().unwrap_or(Ty::Unknown);
                match gt {
                    Ty::List(inner) => {
                        if spread_elem.is_none() {
                            spread_elem = Some((a.span, (*inner).clone()));
                        }
                    }
                    Ty::Unknown => {}
                    other => {
                        self.err(
                            a.span,
                            format!("a spread argument must be a `List`, found `{other}`"),
                        );
                    }
                }
                continue;
            }
            if let Some(name) = &a.name {
                self.err_note(
                    a.span,
                    format!("named argument `{name}` is not supported for this call"),
                    "positional arguments are expected here",
                );
                position += 1;
                continue;
            }
            let want = match params.get(position) {
                Some(p) => p.clone(),
                None => {
                    self.err(e.span, "too many arguments in call");
                    position += 1;
                    continue;
                }
            };
            let got_ty = got.get(&a.value.span).cloned().unwrap_or(Ty::Unknown);
            self.check_assignable(&want, &got_ty, a.span, "argument");
            position += 1;
        }
        if let Some((sspan, elem)) = spread_elem {
            if position < params.len() {
                for p in params.iter().skip(position) {
                    self.check_assignable(p, &elem, sspan, "argument");
                }
            } else if position > params.len() {
                self.err(e.span, "too many arguments in call");
            }
        } else if position < params.len() {
            self.err(
                e.span,
                format!("expected {} argument(s), found {}", params.len(), position),
            );
        }
    }

    /// Variadic-aware argument checking against `ParamInfo`s. Params flagged
    /// `rest` absorb any remaining positional arguments without arity errors.
    fn check_args_info(
        &mut self,
        e: &Expr,
        params: &[crate::resolve::ParamInfo],
        args: &[CallArg],
    ) {
        let rest_ty = params.iter().rev().find(|p| p.rest).map(|p| p.ty.clone());
        let fixed = params.iter().filter(|p| !p.rest).count();
        let mut position = 0usize;
        let mut spread_elem: Option<(Span, Ty)> = None;
        for (idx, a) in args.iter().enumerate() {
            if a.spread {
                if idx != args.len() - 1 {
                    self.err(
                        a.span,
                        "the spread argument (`...list`) must be the last argument in the call",
                    );
                }
                if spread_elem.is_some() {
                    self.err(
                        a.span,
                        "only one spread argument (`...list`) is allowed per call",
                    );
                }
                let at = self.check_expr(&a.value);
                match at {
                    Ty::List(inner) => {
                        if spread_elem.is_none() {
                            spread_elem = Some((a.span, (*inner).clone()));
                        }
                    }
                    Ty::Unknown => {}
                    _ => {
                        self.err(
                            a.span,
                            format!("a spread argument must be a `List`, found `{at}`"),
                        );
                    }
                }
                continue;
            }
            if let Some(name) = &a.name {
                self.err_note(
                    a.span,
                    format!("named argument `{name}` is not supported for this call"),
                    "positional arguments are expected here",
                );
                let _ = self.check_expr(&a.value);
                position += 1;
                continue;
            }
            let want = if position < fixed {
                params[position].ty.clone()
            } else {
                match &rest_ty {
                    Some(rt) => rt.clone(),
                    None => {
                        let _ = self.check_expr(&a.value);
                        self.err(e.span, "too many arguments in call");
                        position += 1;
                        continue;
                    }
                }
            };
            let got = self.check_expr(&a.value);
            self.check_assignable(&want, &got, a.span, "argument");
            position += 1;
        }
        if let Some((sspan, elem)) = spread_elem {
            match &rest_ty {
                // A rest callee (println, print, ...) absorbs every element
                // into its variadic tail, wherever the spread lands.
                Some(rt) => {
                    if position < fixed {
                        let mut missing = String::new();
                        for p in params.iter().take(fixed).skip(position) {
                            if !missing.is_empty() {
                                missing.push_str(", ");
                            }
                            missing.push_str(&p.name);
                        }
                        self.err_note(
                            e.span,
                            format!(
                                "missing argument{} for parameter{} `{}`",
                                if missing.contains(',') { "s" } else { "" },
                                if missing.contains(',') { "s" } else { "" },
                                missing
                            ),
                            "all required parameters must be supplied",
                        );
                    } else if !matches!(rt, Ty::Unknown) {
                        self.check_assignable(rt, &elem, sspan, "argument");
                    }
                }
                // A fixed-arity function: the spread fills the remaining
                // parameter positions, exactly like `check_args`.
                None => {
                    if position < params.len() {
                        for p in params.iter().skip(position) {
                            self.check_assignable(&p.ty, &elem, sspan, "argument");
                        }
                    } else if position > params.len() {
                        self.err(sspan, "too many arguments in call");
                    }
                }
            }
        } else if position < fixed && args.iter().all(|a| a.name.is_none()) {
            // Missing required arguments.
            let missing = &params[position..fixed];
            if !missing.is_empty() {
                self.err_note(
                    e.span,
                    format!(
                        "missing argument{} for parameter{} `{}`",
                        if missing.len() == 1 { "" } else { "s" },
                        if missing.len() == 1 { "" } else { "s" },
                        missing
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "all required parameters must be supplied",
                );
            }
        }
    }

    fn check_member(&mut self, e: &Expr, object: &Expr, name: &str) -> Ty {
        // Type-qualified static access: `Foo.bar`.
        if let ExprKind::Ident(tname) = &object.kind {
            if let Some(entry) = self.resolved.types.get(tname) {
                let gname = entry.name().to_string();
                if let Some(f) = self.find_field(&gname, name, &HashMap::new()) {
                    if !f.0.is_static {
                        self.err(
                            e.span,
                            format!(
                                "instance field `{name}` must be accessed on an instance of `{gname}`"
                            ),
                        );
                        return Ty::Unknown;
                    }
                    return self.subst(&f.0.ty, &HashMap::new());
                }
                if let Some(p) = self.find_property(&gname, name) {
                    if !p.is_static {
                        self.err(
                            e.span,
                            format!(
                                "instance property `{name}` must be accessed on an instance of `{gname}`"
                            ),
                        );
                        return Ty::Unknown;
                    }
                    return p.ty.clone();
                }
                if let Some(m) = self.find_method(&gname, name) {
                    return Ty::Fn(
                        m.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(self.method_ret_ty(&m, &HashMap::new())),
                    );
                }
                if let Some(nc) = self.find_named_ctor(&gname, name) {
                    let ret = match entry {
                        TypeTableEntry::Class(c) => Ty::Class(
                            c.name.clone(),
                            c.generics.iter().cloned().map(Ty::Var).collect(),
                        ),
                        TypeTableEntry::Struct(s) => Ty::Struct(
                            s.name.clone(),
                            s.generics.iter().cloned().map(Ty::Var).collect(),
                        ),
                        _ => Ty::Unknown,
                    };
                    return Ty::Fn(
                        nc.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(ret),
                    );
                }
                // Bare enum variant access: `Color.Red` when the variant has
                // no payload fields.
                if let TypeTableEntry::Enum(t) = entry {
                    if let Some((_, fields, _)) = t.variants.iter().find(|(n, _, _)| n == name) {
                        if fields.is_empty() {
                            return Ty::Enum(t.name.clone(), Vec::new());
                        }
                        self.err(
                            e.span,
                            format!(
                                "variant `{name}` carries {} payload field(s); use `{name}(...)`",
                                fields.len()
                            ),
                        );
                        return Ty::Unknown;
                    }
                }
                self.err(e.span, format!("no member `{name}` on type `{tname}`"));
                return Ty::Unknown;
            }
        }

        let ot = self.check_expr(object);
        // Member access through a raw pointer (`p.field`) auto-dereferences
        // and needs `unsafe`; through an immutable `&T` borrow it is a safe
        // read.
        let ot = match ot {
            Ty::Ptr(inner) => {
                if self.unsafe_depth == 0 {
                    self.err(
                        e.span,
                        "pointer access may only be used inside an `unsafe` block",
                    );
                }
                *inner
            }
            Ty::Ref(inner) => *inner,
            other => other,
        };
        if let Ty::List(inner) = &ot {
            // Builtin list methods (slice subset).
            return match name {
                "push" => Ty::Fn(vec![inner.as_ref().clone()], Box::new(Ty::Empty)),
                "pop" => Ty::Fn(vec![], Box::new(inner.as_ref().clone())),
                "remove" => Ty::Fn(vec![Ty::Int], Box::new(inner.as_ref().clone())),
                "insert" => Ty::Fn(vec![Ty::Int, inner.as_ref().clone()], Box::new(Ty::Empty)),
                "sort" if is_list_sortable(inner) => Ty::Fn(vec![], Box::new(Ty::Empty)),
                "sort" => {
                    self.err(
                        e.span,
                        format!(
                            "`sort` needs a `List` of int, float, byte, char, bool, or string (got `List<{}>`)",
                            inner.bare_name()
                        ),
                    );
                    Ty::Unknown
                }
                other => {
                    self.err(
                        e.span,
                        format!("no member `{other}` on `List<{}>`", inner.bare_name()),
                    );
                    Ty::Unknown
                }
            };
        }
        if let Ty::Map(k, v) = &ot {
            // Builtin map methods (slice subset).
            return match name {
                "has" => Ty::Fn(vec![k.as_ref().clone()], Box::new(Ty::Bool)),
                "get" => Ty::Fn(vec![k.as_ref().clone()], Box::new(v.clone().opt_of())),
                "remove" => Ty::Fn(vec![k.as_ref().clone()], Box::new(v.clone().opt_of())),
                "keys" => Ty::Fn(vec![], Box::new(Ty::List(k.clone()))),
                "values" => Ty::Fn(vec![], Box::new(Ty::List(v.clone()))),
                other => {
                    self.err(
                        e.span,
                        format!(
                            "no member `{other}` on `Map<{}, {}>`",
                            k.bare_name(),
                            v.bare_name()
                        ),
                    );
                    Ty::Unknown
                }
            };
        }
        if let Ty::Stream = ot {
            // Builtin stream methods (buffered I/O).
            return match name {
                "read" => Ty::Fn(
                    vec![Ty::Int],
                    Box::new(Ty::Option(Box::new(Ty::List(Box::new(Ty::Byte))))),
                ),
                "write" => Ty::Fn(vec![Ty::List(Box::new(Ty::Byte))], Box::new(Ty::Int)),
                "flush" => Ty::Fn(vec![], Box::new(Ty::Bool)),
                "close" => Ty::Fn(vec![], Box::new(Ty::Bool)),
                other => {
                    self.err(
                        e.span,
                        format!("no member `{other}` on `Stream` (members are `read`, `write`, `flush`, `close`)"),
                    );
                    Ty::Unknown
                }
            };
        }
        if let Ty::Interface(iface, iargs) = &ot {
            // Interface members type calls on interface-typed receivers. The
            // declared signatures may use the interface's generic parameters,
            // which the instantiation args (`Container<int>`) substitute.
            let mut imap = HashMap::new();
            if let Some(TypeTableEntry::Interface(it)) = self.resolved.types.get(iface) {
                for (i, g) in it.generics.iter().enumerate() {
                    imap.insert(g.clone(), iargs.get(i).cloned().unwrap_or(Ty::Unknown));
                }
            }
            let mut chain: Vec<String> = vec![iface.to_string()];
            let mut seen: Vec<String> = Vec::new();
            while let Some(iname) = chain.pop() {
                if seen.contains(&iname) {
                    continue;
                }
                seen.push(iname.clone());
                let Some(TypeTableEntry::Interface(ent)) = self.resolved.types.get(&iname) else {
                    continue;
                };
                if let Some(m) = ent.members.iter().find(|m| m.name == name) {
                    if m.is_property {
                        self.err_note(
                            e.span,
                            format!("property `{name}` on interface `{iface}`"),
                            "interface property access is not lowered yet (interface methods are); use the interface's methods",
                        );
                        return Ty::Unknown;
                    }
                    return Ty::Fn(
                        m.params.iter().map(|p| self.subst(&p.ty, &imap)).collect(),
                        Box::new(self.subst(&m.ty, &imap)),
                    );
                }
                for ext in &ent.extends {
                    if let Some(n) = ext.named() {
                        chain.push(n.to_string());
                    }
                }
            }
            self.err_note(
                e.span,
                format!("no member `{name}` on interface `{iface}`"),
                "an interface declares method signatures; check the spelling",
            );
            return Ty::Unknown;
        }
        let Some((class, args_map)) = self.type_key(&ot) else {
            self.err_note(
                e.span,
                format!("cannot access member `{name}` on value of type `{ot}`"),
                "member access requires a class, struct, enum, or interface value",
            );
            return Ty::Unknown;
        };
        if let Some(f) = self.find_field(&class, name, &args_map) {
            if f.0.is_static {
                self.err(
                    e.span,
                    format!(
                        "static field `{name}` must be accessed on the type `{class}`, not on an instance"
                    ),
                );
                return Ty::Unknown;
            }
            return self.subst(&f.0.ty, &args_map);
        }
        if let Some(p) = self.find_property(&class, name) {
            if p.is_static {
                self.err(
                    e.span,
                    format!(
                        "static property `{name}` must be accessed on the type `{class}`, not on an instance"
                    ),
                );
                return Ty::Unknown;
            }
            return self.subst(&p.ty, &args_map);
        }
        if let Some(m) = self.find_method(&class, name) {
            return Ty::Fn(
                m.params
                    .iter()
                    .map(|p| self.subst(&p.ty, &args_map))
                    .collect(),
                Box::new(self.method_ret_ty(&m, &args_map)),
            );
        }
        self.err_note(
            e.span,
            format!("no member `{name}` on `{class}`"),
            format!("`{class}` defines fields, properties, and methods; check the spelling"),
        );
        Ty::Unknown
    }

    fn check_opt_access(&mut self, e: &Expr, object: &Expr, name: &str) -> Ty {
        let ot = self.check_expr(object);
        let Some(inner) = ot.inner_option() else {
            self.err_note(
                e.span,
                format!("`?.` on a non-option value of type `{ot}`"),
                "optional chaining requires an `Option` receiver",
            );
            return Ty::Unknown;
        };
        let Some((class, args_map)) = self.type_key(&inner) else {
            return Ty::Unknown;
        };
        let member_ty = if let Some(f) = self.find_field(&class, name, &args_map) {
            self.subst(&f.0.ty, &args_map)
        } else if let Some(p) = self.find_property(&class, name) {
            self.subst(&p.ty, &args_map)
        } else if let Some(m) = self.find_method(&class, name) {
            Ty::Fn(
                m.params
                    .iter()
                    .map(|p| self.subst(&p.ty, &args_map))
                    .collect(),
                Box::new(self.method_ret_ty(&m, &args_map)),
            )
        } else {
            self.err(e.span, format!("no member `{name}` on `{class}`"));
            Ty::Unknown
        };
        member_ty.opt_of()
    }

    fn check_opt_unwrap(&mut self, e: &Expr, inner: &Expr) -> Ty {
        let t = self.check_expr(inner);
        match t.inner_option() {
            Some(inner_t) => inner_t,
            None => {
                self.err_note(
                    e.span,
                    format!("`?` cannot unwrap non-option value of type `{t}`"),
                    "only `Option` values may be unwrapped",
                );
                t
            }
        }
    }

    /// A map index must be assignable to the map's key type. Byte-keyed maps
    /// additionally accept in-range `int` literals (via `check_assign_rhs`).
    fn check_map_index(&mut self, key_ty: &Ty, index_ty: &Ty, index: &Expr, span: Span) {
        if *index_ty == Ty::Unknown {
            return;
        }
        if *key_ty == Ty::Byte {
            self.check_assign_rhs(key_ty, index_ty, index, span, "map index");
            return;
        }
        if !self.ok_types(key_ty, index_ty) {
            self.err(span, format!("map index must be a `{key_ty}`"));
        }
    }

    fn check_index(&mut self, e: &Expr, object: &Expr, index: &Expr) -> Ty {
        let ot0 = self.check_expr(object);
        let it = self.check_expr(index);
        // Indexing through an immutable `&T` borrow reads through the
        // referent: a collection/string referent is indexed exactly like the
        // value itself, a scalar referent is addressed like a pointer element.
        let ot = match &ot0 {
            Ty::Ref(inner)
                if !matches!(inner.as_ref(), Ty::Int | Ty::Float | Ty::Bool | Ty::Char) =>
            {
                (**inner).clone()
            }
            other => other.clone(),
        };
        match &ot {
            Ty::List(_) | Ty::Range(_) => {
                if it != Ty::Unknown && it != Ty::Int {
                    self.err(e.span, "index must be an `int`");
                }
                ot.elem().unwrap_or(Ty::Unknown)
            }
            Ty::Map(key_ty, val_ty) => {
                self.check_map_index(key_ty, &it, index, e.span);
                val_ty.as_ref().clone()
            }
            Ty::String => {
                if it != Ty::Unknown && it != Ty::Int {
                    self.err(e.span, "string index must be an `int`");
                }
                Ty::Byte
            }
            Ty::Tuple(items) => Ty::Tuple(items.clone()),
            Ty::Ptr(inner) => {
                if it != Ty::Unknown && it != Ty::Int {
                    self.err(e.span, "pointer index must be an `int`");
                }
                if self.unsafe_depth == 0 {
                    self.err(
                        e.span,
                        "pointer indexing may only be used inside an `unsafe` block",
                    );
                }
                match inner.as_ref() {
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Byte => (**inner).clone(),
                    other => {
                        self.err(
                            e.span,
                            format!("indexing a pointer to a `{other}` value is not supported yet"),
                        );
                        Ty::Unknown
                    }
                }
            }
            // Indexing through an immutable `&T` borrow reads the referent
            // (safe; no `unsafe` needed). Non-scalar referents were
            // dereferenced above, so only scalar referents reach here.
            Ty::Ref(inner) => {
                if it != Ty::Unknown && it != Ty::Int {
                    self.err(e.span, "pointer index must be an `int`");
                }
                (**inner).clone()
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.err(e.span, format!("cannot index a value of type `{other}`"));
                Ty::Unknown
            }
        }
    }

    fn check_binary(&mut self, e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) -> Ty {
        let lt = self.check_expr(lhs);
        let rt = self.check_expr(rhs);
        use BinOp::*;
        match op {
            Add | Sub | Mul | Div | Mod | Pow => {
                if lt != Ty::Unknown && rt != Ty::Unknown {
                    let numeric_ok = lt.is_numeric() && rt.is_numeric();
                    let string_ok = matches!(op, Add) && lt == Ty::String && rt == Ty::String;
                    if !numeric_ok && !string_ok {
                        self.err_note(
                            e.span,
                            format!("operator `{op:?}` requires numeric or string operands, found `{lt}` and `{rt}`"),
                            "`+` concatenates strings; arithmetic operators need numbers",
                        );
                    }
                }
                if lt == Ty::Float || rt == Ty::Float {
                    Ty::Float
                } else if matches!(op, Add) && (lt == Ty::String || rt == Ty::String) {
                    Ty::String
                } else {
                    Ty::Int
                }
            }
            Shl | Shr | BitAnd | BitOr | BitXor => {
                if lt != Ty::Unknown && lt != Ty::Int {
                    self.err(e.span, "bitwise operators require `int` operands");
                }
                let _ = rt;
                Ty::Int
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                self.check_comparison(e, op, &lt, &rt);
                Ty::Bool
            }
            And | Or => {
                // `lt`/`rt` are already `check_expr(lhs)`/`check_expr(rhs)`.
                // Calling `check_bool_cond` here would re-walk both operands,
                // doubling the work at every level of a left-leaning
                // `a || b || c` chain (T(n) = 2*T(n-1), i.e. exponential) and
                // re-emitting any diagnostic the operands already produced.
                if lt != Ty::Unknown && lt != Ty::Bool {
                    self.err_note(
                        lhs.span,
                        "condition must be a `bool`",
                        format!("found `{lt}`"),
                    );
                }
                if rt != Ty::Unknown && rt != Ty::Bool {
                    self.err_note(
                        rhs.span,
                        "condition must be a `bool`",
                        format!("found `{rt}`"),
                    );
                }
                Ty::Bool
            }
            Range | RangeIncl => Ty::Range(Box::new(lt)),
            NullCoalesce => {
                let inner = lt.inner_option();
                match inner {
                    Some(inner_t) => {
                        if rt != Ty::Unknown {
                            self.check_assignable(&inner_t, &rt, rhs.span, "`??` default");
                        }
                        inner_t
                    }
                    None => {
                        self.err_note(
                            e.span,
                            "left side of `??` is not an `Option`",
                            "`a ?? b` yields `a` when present, else `b`",
                        );
                        lt
                    }
                }
            }
            Send => Ty::Unknown,
            Is => {
                let _ = lt;
                let _ = rt;
                Ty::Bool
            }
            In => {
                let ot = self.check_expr(lhs);
                let _ = self.check_expr(rhs);
                if !matches!(ot, Ty::List(_) | Ty::Map(_, _) | Ty::String) {
                    let _ = ot;
                }
                Ty::Bool
            }
        }
    }

    /// Comparison operands must actually be comparable. Concrete rules:
    /// numbers compare with every operator (mixed `int`/`float` included);
    /// `char` pairs compare with every operator; `==`/`!=` may additionally
    /// pair identical types (string, bool, enum, class, struct, option, list,
    /// fn, ...). Everything else — ordering strings or bools, a `bool`
    /// compared with an `int`, a `char` with an `int`, mismatched shapes —
    /// is an error rather than silently lowering an ill-typed `icmp`/`fcmp`
    /// (a `bool > int` used to reach the JIT verifier and crash). Unresolved
    /// types (a generic `T`) skip the gate; the emitter sees them only under
    /// a concrete substitution.
    fn check_comparison(&mut self, e: &Expr, op: BinOp, lt: &Ty, rt: &Ty) {
        if *lt == Ty::Unknown || *rt == Ty::Unknown || lt.has_var() || rt.has_var() {
            return;
        }
        let eq_only = matches!(op, BinOp::Eq | BinOp::Ne);
        let scalar = lt.is_numeric() && rt.is_numeric() || matches!((lt, rt), (Ty::Char, Ty::Char));
        if scalar {
            return;
        }
        // A `byte` (from string indexing/iteration) compares with an ASCII
        // `char` literal: the literal slots into the 0..=255 byte domain. Any
        // other `byte`/`char` mix would compare a raw byte to a decoded scalar,
        // so it is rejected; a non-ASCII literal has no byte value at all.
        if matches!((lt, rt), (Ty::Byte, Ty::Char) | (Ty::Char, Ty::Byte)) {
            let char_part = match &e.kind {
                ExprKind::Binary { lhs, rhs, .. } if *lt == Ty::Byte => rhs.as_ref(),
                ExprKind::Binary { lhs, rhs, .. } if *rt == Ty::Byte => lhs.as_ref(),
                _ => {
                    self.err(
                        e.span,
                        "`byte` values only compare with `int`s or an ASCII `char` literal",
                    );
                    return;
                }
            };
            if let ExprKind::Lit(Lit::Char(c)) = &char_part.kind {
                if *c as u32 <= 0x7F {
                    return;
                }
                self.err(
                    e.span,
                    format!(
                        "a non-ASCII char literal has no `byte` value: `{c}` (its bytes are multi-byte UTF-8)"
                    ),
                );
            } else {
                self.err(
                    e.span,
                    "`byte` values only compare with `int`s or an ASCII `char` literal",
                );
            }
            return;
        }
        if eq_only && lt == rt {
            return; // identity/equality on identical types (strings, enums, ...)
        }
        if lt == rt {
            self.err(
                e.span,
                format!("operator `{op:?}` is not supported for `{lt}` operands"),
            );
        } else {
            self.err(
                e.span,
                format!("operator `{op:?}` requires comparable operands, found `{lt}` and `{rt}`"),
            );
        }
    }

    fn check_unary(&mut self, e: &Expr, op: UnOp, operand: &Expr) -> Ty {
        let t = self.check_expr(operand);
        match op {
            UnOp::Neg => {
                if t != Ty::Unknown && !t.is_numeric() {
                    self.err(
                        e.span,
                        format!("unary `-` requires a numeric operand, found `{t}`"),
                    );
                }
                t
            }
            UnOp::Not => {
                if t != Ty::Unknown && t != Ty::Bool {
                    self.err(
                        e.span,
                        format!("unary `!` requires a `bool` operand, found `{t}`"),
                    );
                }
                Ty::Bool
            }
            UnOp::BitNot => {
                if t != Ty::Unknown && t != Ty::Int {
                    self.err(
                        e.span,
                        format!("unary `~` requires an `int` operand, found `{t}`"),
                    );
                }
                Ty::Int
            }
            UnOp::AddrOf => {
                if self.unsafe_depth == 0 {
                    self.err(e.span, "`&` may only be used inside an `unsafe` block");
                }
                match &t {
                    Ty::Class(..) | Ty::Struct(..) | Ty::Ptr(..) | Ty::String | Ty::Unknown => {
                        Ty::Ptr(Box::new(t))
                    }
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Byte => {
                        if !matches!(&operand.kind, ExprKind::Ident(_)) {
                            self.err(e.span, "`&` of a scalar requires a local variable");
                            return Ty::Unknown;
                        }
                        Ty::Ptr(Box::new(t))
                    }
                    other => {
                        self.err(
                            e.span,
                            format!(
                                "`&` currently only supports class, struct, pointer, and scalar values, found `{other}`"
                            ),
                        );
                        Ty::Unknown
                    }
                }
            }
            UnOp::Deref => {
                // Raw pointers (`*T`) need `unsafe`; dereferencing a safe
                // `&T` borrow just reads through it.
                let borrow = matches!(t, Ty::Ref(_));
                if self.unsafe_depth == 0 && !borrow {
                    self.err(e.span, "`*` may only be used inside an `unsafe` block");
                }
                match t {
                    Ty::Ptr(inner) | Ty::Ref(inner) => *inner,
                    Ty::Unknown => Ty::Unknown,
                    other => {
                        self.err_note(
                            e.span,
                            format!("cannot dereference a value of type `{other}`"),
                            "only pointers (`*T`) and references (`&T`) can be dereferenced",
                        );
                        Ty::Unknown
                    }
                }
            }
        }
    }

    fn check_assign(&mut self, e: &Expr, target: &Expr, op: AssignOp, value: &Expr) -> Ty {
        let _ = op;
        let vt = self.check_expr(value);
        let moved_from = self.manual_move_source(value);
        match &target.kind {
            ExprKind::Ident(name) => {
// A top-level `var` is assignable by name from anywhere in the
                // module; there is no shadow-frame entry to consult.
                if let Some(g) = self.resolved.globals.iter().find(|g| g.name == *name) {
                    // `vt` is the type of `value` computed once above; checking
                    // the expression again here would double-emit diagnostics.
                    self.check_assignable(
                        &g.ty,
                        &vt,
                        target.span,
                        "assignment to a top-level `var`",
                    );
                    self.guard_stored_ref(
                        &g.ty,
                        &vt,
                        target.span,
                        "an assignment to a top-level `var`",
                    );
                    return Ty::Empty;
                }
                match self.lookup(name).cloned() {
                    Some(l) => {
                        if !l.mutable {
                            self.err(
                                target.span,
                                format!("cannot assign to immutable binding `{name}`"),
                            );
                        }
                        if l.manual {
                            self.err_note(
                                target.span,
                                format!("cannot overwrite `#[manualAlloc]` binding `{name}`"),
                                format!(
                                    "`free()` `{name}` first, then bind a new value with `let`"
                                ),
                            );
                        } else if let Some(src) = &moved_from {
                            self.err_note(
                                target.span,
                                format!(
                                    "cannot assign `#[manualAlloc]` value `{src}` to managed binding `{name}`"
                                ),
                                format!("take ownership with `#[manualAlloc] let ... = {src}`, or `{src}.free()`"),
                            );
                        }
                        self.check_assignable(&l.ty, &vt, target.span, "assignment");
                        self.guard_stored_ref(&l.ty, &vt, target.span, "an assignment");
                    }
                    None => {
                        // Implicit receiver field assignment: `count = x`
                        // inside a class body writes `this.count`.
                        if let Some(st) = &self.self_ty {
                            if let Some((cn, args_map)) = self.type_key(st) {
                                if let Some(f) = self.find_field(&cn, name, &args_map) {
                                    if !f.0.mutable {
                                        self.err(
                                            target.span,
                                            format!("cannot assign to immutable field `{name}`"),
                                        );
                                    }
                                    if f.0.manual {
                                        self.consume_into_owned(
                                            value,
                                            target.span,
                                            &format!("owned field `{name}`"),
                                        );
                                    } else if let Some(src) = &moved_from {
                                        self.err_note(
                                            target.span,
                                            format!(
                                                "cannot store `#[manualAlloc]` value `{src}` in managed field `{name}`"
                                            ),
                                            "an owned value must stay in an owned position",
                                        );
                                    }
                                    self.check_assignable(&f.0.ty, &vt, target.span, "assignment");
                                    self.guard_stored_ref(
                                        &f.0.ty,
                                        &vt,
                                        target.span,
                                        "an assignment",
                                    );
                                    return Ty::Empty;
                                }
                            }
                        }
                        self.err(
                            target.span,
                            format!("cannot assign to undeclared name `{name}`"),
                        );
                    }
                }
            }
            ExprKind::Unary {
                op: UnOp::Deref,
                operand,
            } => {
                // Dereferencing a raw pointer needs `unsafe`; writing
                // through an immutable `&T` borrow is rejected outright.
                let pt = self.check_expr(operand);
                let is_borrow = matches!(pt, Ty::Ref(_));
                if self.unsafe_depth == 0 && !is_borrow {
                    self.err(target.span, "`*` may only be used inside an `unsafe` block");
                }
                match pt {
                    Ty::Ptr(inner) => {
                        self.check_assignable(&inner, &vt, target.span, "assignment");
                    }
                    Ty::Ref(_) => {
                        self.err(
                            target.span,
                            "cannot write through an immutable reference (`&T`)",
                        );
                    }
                    Ty::Unknown => {}
                    other => {
                        self.err_note(
                            target.span,
                            format!("cannot dereference a value of type `{other}`"),
                            "only pointers (`*T`) and references (`&T`) can be dereferenced",
                        );
                    }
                }
            }
            ExprKind::Member { object, name } => {
                // Type-qualified static property assignment: `Type.prop = v`.
                if let ExprKind::Ident(tname) = &object.kind {
                    if let Some(entry) = self.resolved.types.get(tname) {
                        let gname = entry.name().to_string();
                        if let Some(f) = self.find_field(&gname, name, &HashMap::new()) {
                            if !f.0.is_static {
                                self.err(
                                    target.span,
                                    format!(
                            "instance field `{name}` must be assigned on an instance of `{gname}`"
                        ),
                                );
                            } else if !f.0.mutable {
                                self.err(
                                    target.span,
                                    format!("cannot assign to immutable static field `{name}`"),
                                );
                            }
                            self.check_assignable(&f.0.ty, &vt, target.span, "assignment");
                            self.guard_stored_ref(&f.0.ty, &vt, target.span, "an assignment");
                            return Ty::Empty;
                        }
                        if let Some(p) = self.find_property(&gname, name) {
                            if !p.is_static {
                                self.err(
                        target.span,
                        format!(
                            "instance property `{name}` must be assigned on an instance of `{gname}`"
                        ),
                    );
                            } else if !p.has_set {
                                self.err(
                                    target.span,
                                    format!("static property `{name}` has no setter"),
                                );
                            }
                            self.check_assignable(&p.ty, &vt, target.span, "assignment");
                            self.guard_stored_ref(&p.ty, &vt, target.span, "an assignment");
                            return Ty::Empty;
                        }
                        self.err(
                            target.span,
                            format!("no assignable member `{name}` on `{tname}`"),
                        );
                        return Ty::Empty;
                    }
                }
                let ot = self.check_expr(object);
                let ot = match ot {
                    Ty::Ptr(inner) => {
                        if self.unsafe_depth == 0 {
                            self.err(
                                target.span,
                                "pointer access may only be used inside an `unsafe` block",
                            );
                        }
                        *inner
                    }
                    Ty::Ref(_) => {
                        self.err(
                            target.span,
                            format!(
                                "cannot write to a field `{name}` through an immutable reference (`&T`)"
                            ),
                        );
                        Ty::Unknown
                    }
                    other => other,
                };
                if let Some((class, args_map)) = self.type_key(&ot) {
                    if let Some(f) = self.find_field(&class, name, &args_map) {
                        if f.0.is_static {
                            self.err(
                                target.span,
                                format!(
                                    "static field `{name}` must be assigned on the type `{class}`, not on an instance"
                                ),
                            );
                        } else if !f.0.mutable {
                            self.err(
                                target.span,
                                format!("cannot assign to immutable field `{name}`"),
                            );
                        } else if f.0.manual {
                            self.consume_into_owned(
                                value,
                                target.span,
                                &format!("owned field `{name}`"),
                            );
                        } else if let Some(src) = &moved_from {
                            self.err_note(
                                target.span,
                                format!(
                                    "cannot store `#[manualAlloc]` value `{src}` in managed field `{name}`"
                                ),
                                "an owned value must stay in an owned position",
                            );
                        }
                        self.check_assignable(&f.0.ty, &vt, target.span, "assignment");
                        self.guard_stored_ref(&f.0.ty, &vt, target.span, "an assignment");
                    } else if let Some(p) = self.find_property(&class, name) {
                        if !p.has_set {
                            self.err(target.span, format!("property `{name}` has no setter"));
                        }
                        self.check_assignable(
                            &self.subst(&p.ty, &args_map),
                            &vt,
                            target.span,
                            "assignment",
                        );
                        self.guard_stored_ref(
                            &self.subst(&p.ty, &args_map),
                            &vt,
                            target.span,
                            "an assignment",
                        );
                    } else {
                        self.err(
                            target.span,
                            format!("no assignable member `{name}` on `{class}`"),
                        );
                    }
                } else {
                    self.err(
                        target.span,
                        "cannot assign to a member of a non-class value",
                    );
                }
            }
            ExprKind::Index { object, index } => {
                if let Some(src) = &moved_from {
                    self.err_note(
                        target.span,
                        format!(
                            "cannot store `#[manualAlloc]` value `{src}` in a managed collection"
                        ),
                        "an owned value must stay in an owned position",
                    );
                }
                let ot = self.check_expr(object);
                let it = self.check_expr(index);
                match &ot {
                    Ty::List(_) | Ty::Range(_) if it != Ty::Unknown && it != Ty::Int => {
                        self.err(target.span, "index must be an `int`");
                    }
                    Ty::Map(k, _) if it != Ty::Unknown => {
                        self.check_map_index(k, &it, index, target.span);
                    }
                    _ => {}
                }
                let elem_ty = match &ot {
                    Ty::List(inner) => inner.as_ref().clone(),
                    Ty::Map(_, v) => v.as_ref().clone(),
                    Ty::String => Ty::Byte,
                    Ty::Ptr(inner) => {
                        if it != Ty::Unknown && it != Ty::Int {
                            self.err(target.span, "pointer index must be an `int`");
                        }
                        if self.unsafe_depth == 0 {
                            self.err(
                                object.span,
                                "pointer stores may only be used inside an `unsafe` block",
                            );
                        }
                        match inner.as_ref() {
                            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Byte => {
                                (**inner).clone()
                            }
                            other => {
                                self.err(
                                    target.span,
                                    format!(
                                        "storing through a pointer to a `{other}` value is not supported yet"
                                    ),
                                );
                                Ty::Unknown
                            }
                        }
                    }
                    Ty::Ref(_) => {
                        self.err(
                            target.span,
                            "cannot write through an immutable reference (`&T`)",
                        );
                        Ty::Unknown
                    }
                    _ => {
                        self.err(
                            target.span,
                            "index assignment target must be a List, Map, string, or pointer",
                        );
                        Ty::Unknown
                    }
                };
                self.types.insert(target.span, elem_ty.clone());
                self.check_assign_rhs(&elem_ty, &vt, value, target.span, "assignment");
                self.guard_stored_ref(&elem_ty, &vt, target.span, "an assignment");
            }
            _ => {
                self.err(
                    target.span,
                    "assignment target must be a variable, member, or index",
                );
            }
        }
        let _ = e.span;
        vt
    }

    fn check_lambda(
        &mut self,
        e: &Expr,
        params: &[Param],
        return_ty: Option<&TypeExpr>,
        body: &FnBody,
    ) -> Ty {
        self.push_scope();
        let mut param_tys = Vec::new();
        for p in params {
            let pt = self.resolve_param_ty(p);
            param_tys.push(pt.clone());
            self.declare(&p.name, pt, true);
        }
        let saved_ret = self.ret_ty.clone();
        if let Some(ty) = return_ty {
            self.ret_ty = self.resolved_fn_ty(ty, &self.fn_generics);
        } else {
            self.ret_ty = Ty::Unknown;
        }
        self.check_fn_body(body);
        let ret = if let Some(ty) = return_ty {
            self.resolved_fn_ty(ty, &self.fn_generics)
        } else {
            // No annotation: infer the return type from the body tail, exactly
            // like the emitter's pre-pass will for the hoisted function.
            match body {
                FnBody::Expr(be) => self.types.get(&be.span).cloned().unwrap_or(Ty::Empty),
                FnBody::Block(bb) => {
                    if let Some(be) = &bb.expr {
                        self.types.get(&be.span).cloned().unwrap_or(Ty::Empty)
                    } else {
                        Ty::Empty
                    }
                }
            }
        };
        self.ret_ty = saved_ret;
        self.pop_scope();
        let _ = e.span;
        Ty::Fn(param_tys, Box::new(ret))
    }

    /// `abs(x)` over an `int` or `float` argument; the result has the
    /// argument's type.
    fn check_abs(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`abs(x)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        match &at {
            Ty::Int | Ty::Float | Ty::Unknown => at,
            _ => {
                self.err(
                    args[0].value.span,
                    "`abs` requires an `int` or `float` argument",
                );
                Ty::Unknown
            }
        }
    }

    /// `min(a, b)` / `max(a, b)`: two arguments of the same numeric type;
    /// the result has that type.
    fn check_minmax(&mut self, e: &Expr, bname: &str, args: &[CallArg]) -> Ty {
        if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                format!("`{bname}(a, b)` takes exactly two arguments"),
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let lt = self.check_expr(&args[0].value);
        let rt = self.check_expr(&args[1].value);
        let good = |t: &Ty| matches!(t, Ty::Int | Ty::Float | Ty::Unknown);
        if !good(&lt) || !good(&rt) {
            self.err(
                e.span,
                format!("`{bname}` requires two `int` or `float` arguments"),
            );
            return Ty::Unknown;
        }
        match (&lt, &rt) {
            (Ty::Float, _) | (_, Ty::Float) => Ty::Float,
            (Ty::Int, _) | (_, Ty::Int) => Ty::Int,
            _ => Ty::Unknown,
        }
    }

    /// `clamp(x, lo, hi)`: three arguments of the same numeric type;
    /// the result has that type.
    fn check_clamp(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 3 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(e.span, "`clamp(x, lo, hi)` takes exactly three arguments");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let mut t = Ty::Unknown;
        for a in args {
            let at = self.check_expr(&a.value);
            if !matches!(at, Ty::Int | Ty::Float | Ty::Unknown) {
                self.err(a.value.span, "`clamp` arguments must be `int` or `float`");
                return Ty::Unknown;
            }
            if matches!(at, Ty::Float) {
                t = Ty::Float;
            } else if matches!(at, Ty::Int) && !matches!(t, Ty::Float) {
                t = Ty::Int;
            }
        }
        t
    }

    /// `str(x)`: an `int`, `float`, `bool`, or `char` value rendered as
    /// `string` (matching the runtime's `pickle_str_from_*` family).
    fn check_str(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`str(x)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        match &at {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Unknown => Ty::String,
            Ty::List(inner) if matches!(&**inner, Ty::Byte) => Ty::String,
            _ => {
                self.err(
                    args[0].value.span,
                    "`str` requires an `int`, `float`, `bool`, `char`, or `List<byte>` argument",
                );
                Ty::Unknown
            }
        }
    }
    fn check_bytes(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`bytes(s)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`bytes` requires a `string` argument");
        }
        Ty::List(Box::new(Ty::Byte))
    }
    fn check_range(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.is_empty() || args.len() > 3 {
            self.err(
                e.span,
                "`range(end)`, `range(start, end)`, or `range(start, end, step)` expected",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        for a in args {
            if a.name.is_some() || a.spread {
                self.err(a.span, "`range` takes only plain positional arguments");
            }
            let at = self.check_expr(&a.value);
            if !matches!(at, Ty::Int | Ty::Unknown) {
                self.err(a.value.span, "`range` bounds must be integers");
            }
        }
        Ty::List(Box::new(Ty::Int))
    }

    /// `read_file(path)`: reads a whole file as `string`, or `none` when the
    /// file cannot be read. Errors are values (no exceptions).
    fn check_read_file(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`read_file(path)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`read_file` requires a `string` path");
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::String))
    }

    /// `write_file(path, text)`: overwrites `path` with `text`'s UTF-8 bytes
    /// and returns `true` on success, `false` on failure.
    fn check_write_file(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                "`write_file(path, text)` takes exactly two arguments",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        for (i, a) in args.iter().enumerate() {
            let at = self.check_expr(&a.value);
            if !matches!(at, Ty::String | Ty::Unknown) {
                self.err(
                    a.value.span,
                    format!(
                        "`write_file` argument {} must be a `string`",
                        if i == 0 { "path" } else { "text" }
                    ),
                );
                return Ty::Unknown;
            }
        }
        Ty::Bool
    }

    /// `file_exists(path)`: whether the path exists on the filesystem.
    fn check_file_exists(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`file_exists(path)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`file_exists` requires a `string` path");
            return Ty::Unknown;
        }
        Ty::Bool
    }

    /// `delete(path)`: removes a file at `path`; `false` when it is missing or
    /// cannot be removed.
    fn check_delete(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`delete(path)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`delete` requires a `string` path");
            return Ty::Unknown;
        }
        Ty::Bool
    }

    /// `mkdir(path)`: creates a directory at `path`; `false` when it already
    /// exists or cannot be created.
    fn check_mkdir(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`mkdir(path)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`mkdir` requires a `string` path");
            return Ty::Unknown;
        }
        Ty::Bool
    }

    /// `list_dir(path)`: lists a directory as `List<string>?` of child paths
    /// (full paths), or `none` when the path is not a readable directory.
    fn check_list_dir(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`list_dir(path)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(args[0].value.span, "`list_dir` requires a `string` path");
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::List(Box::new(Ty::String))))
    }

    /// `stream_open_read/write/append(path)`: opens a buffered file stream,
    /// returning `Stream?` (`none` when the path cannot be opened).
    fn check_stream_open(&mut self, e: &Expr, bname: &str, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(
                e.span,
                format!("`{bname}(path)` takes exactly one argument"),
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::String | Ty::Unknown) {
            self.err(
                args[0].value.span,
                format!("`{bname}` requires a `string` path"),
            );
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::Stream))
    }

    /// `stdout_stream()` / `stderr_stream()`: a write-only console stream.
    fn check_stream_singleton(&mut self, e: &Expr, bname: &str, args: &[CallArg]) -> Ty {
        if !args.is_empty() {
            self.err(e.span, format!("`{bname}()` takes no arguments"));
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        Ty::Stream
    }

    /// `args()`: the program's command-line arguments (excluding the program
    /// name) as a `List<string>`.
    fn check_args_builtin(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if !args.is_empty() {
            self.err(e.span, "`args()` takes no arguments");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        Ty::List(Box::new(Ty::String))
    }

    /// A process-handle operand: `h` must be a `Stream` (or untyped).
    fn check_handle(&mut self, a: &CallArg) -> bool {
        let at = self.check_expr(&a.value);
        if matches!(at, Ty::Stream | Ty::Unknown) {
            return true;
        }
        self.err(a.value.span, "the process handle must be a `Stream`");
        false
    }

    /// `run_cmd(argv, cwd, envs, env_removes, stdin_bytes, max_out,
    /// timeout_ms, inherit_io)`: runs a subprocess to completion and returns
    /// `(int?, string, string, bool)?` — exit code, stdout, stderr, and a
    /// timeout flag — or `none` when the program cannot be spawned.
    fn check_run_cmd(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 8 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                "`run_cmd(argv, cwd, envs, env_removes, stdin_bytes, max_out, timeout_ms, inherit_io)` takes exactly eight argument(s)",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        // argv: List<string>
        let av = self.check_expr(&args[0].value);
        if !matches!(av, Ty::List(ref i) if matches!(**i, Ty::String) | matches!(**i, Ty::Unknown))
            && !matches!(av, Ty::Unknown)
        {
            self.err(
                args[0].value.span,
                "`run_cmd`'s `argv` must be a `List<string>`",
            );
            return Ty::Unknown;
        }
        // cwd?: string | none
        let cwd = self.check_expr(&args[1].value);
        if !matches!(cwd, Ty::String | Ty::Unknown | Ty::None)
            && !matches!(cwd, Ty::Option(ref i) if matches!(**i, Ty::String | Ty::Unknown))
        {
            self.err(
                args[1].value.span,
                "`run_cmd`'s `cwd` must be a `string` or `none`",
            );
            return Ty::Unknown;
        }
        // envs?: Map<string, string> | none
        let envs = self.check_expr(&args[2].value);
        if !matches!(
            envs,
            Ty::Map(ref k, ref v)
                if (matches!(**k, Ty::String) || matches!(**k, Ty::Unknown))
                    && (matches!(**v, Ty::String) || matches!(**v, Ty::Unknown))
        ) && !matches!(envs, Ty::Unknown | Ty::None)
            && !matches!(envs, Ty::Option(ref i) if matches!(**i, Ty::Map(..)))
        {
            self.err(
                args[2].value.span,
                "`run_cmd`'s `envs` must be a `Map<string, string>` or `none`",
            );
            return Ty::Unknown;
        }
        // env_removes?: List<string> | none
        let rm = self.check_expr(&args[3].value);
        if !matches!(
            rm,
            Ty::List(ref i)
                if matches!(**i, Ty::String) | matches!(**i, Ty::Unknown)
        ) && !matches!(rm, Ty::Unknown | Ty::None)
            && !matches!(rm, Ty::Option(ref i) if matches!(**i, Ty::List(ref j) if matches!(**j, Ty::String | Ty::Unknown)))
        {
            self.err(
                args[3].value.span,
                "`run_cmd`'s `env_removes` must be a `List<string>` or `none`",
            );
            return Ty::Unknown;
        }
        // stdin_bytes?: List<byte> | none
        let si = self.check_expr(&args[4].value);
        if !matches!(
            si,
            Ty::List(ref i) if matches!(**i, Ty::Byte) | matches!(**i, Ty::Unknown)
        ) && !matches!(si, Ty::Unknown | Ty::None)
            && !matches!(si, Ty::Option(ref i) if matches!(**i, Ty::List(ref j) if matches!(**j, Ty::Byte | Ty::Unknown)))
        {
            self.err(
                args[4].value.span,
                "`run_cmd`'s `stdin_bytes` must be a `List<byte>` or `none`",
            );
            return Ty::Unknown;
        }
        // max_out?: int | none
        let mx = self.check_expr(&args[5].value);
        if !matches!(mx, Ty::Int | Ty::Unknown | Ty::None)
            && !matches!(mx, Ty::Option(ref i) if matches!(**i, Ty::Int))
        {
            self.err(
                args[5].value.span,
                "`run_cmd`'s `max_out` must be an `int` or `none`",
            );
            return Ty::Unknown;
        }
        // timeout_ms?: int | none
        let tm = self.check_expr(&args[6].value);
        if !matches!(tm, Ty::Int | Ty::Unknown | Ty::None)
            && !matches!(tm, Ty::Option(ref i) if matches!(**i, Ty::Int))
        {
            self.err(
                args[6].value.span,
                "`run_cmd`'s `timeout_ms` must be an `int` or `none`",
            );
            return Ty::Unknown;
        }
        // inherit_io: bool
        let inh = self.check_expr(&args[7].value);
        if !matches!(inh, Ty::Bool | Ty::Unknown) {
            self.err(
                args[7].value.span,
                "`run_cmd`'s `inherit_io` must be a `bool`",
            );
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::Tuple(vec![
            Ty::Option(Box::new(Ty::Int)),
            Ty::String,
            Ty::String,
            Ty::Bool,
        ])))
    }

    /// `run_spawn(argv, cwd, envs, env_removes, max_out)`: start a child and
    /// return a `Stream?` handle for asynchronous `proc_*` access.
    fn check_run_spawn(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 5 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                "`run_spawn(argv, cwd, envs, env_removes, max_out)` takes exactly five arguments",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let av = self.check_expr(&args[0].value);
        if !matches!(av, Ty::List(ref i) if matches!(**i, Ty::String) | matches!(**i, Ty::Unknown))
            && !matches!(av, Ty::Unknown)
        {
            self.err(
                args[0].value.span,
                "`run_spawn`'s `argv` must be a `List<string>`",
            );
            return Ty::Unknown;
        }
        let cwd = self.check_expr(&args[1].value);
        if !matches!(cwd, Ty::String | Ty::Unknown | Ty::None)
            && !matches!(cwd, Ty::Option(ref i) if matches!(**i, Ty::String | Ty::Unknown))
        {
            self.err(
                args[1].value.span,
                "`run_spawn`'s `cwd` must be a `string` or `none`",
            );
            return Ty::Unknown;
        }
        let envs = self.check_expr(&args[2].value);
        if !matches!(
            envs,
            Ty::Map(ref k, ref v)
                if (matches!(**k, Ty::String) || matches!(**k, Ty::Unknown))
                    && (matches!(**v, Ty::String) || matches!(**v, Ty::Unknown))
        ) && !matches!(envs, Ty::Unknown | Ty::None)
            && !matches!(envs, Ty::Option(ref i) if matches!(**i, Ty::Map(..)))
        {
            self.err(
                args[2].value.span,
                "`run_spawn`'s `envs` must be a `Map<string, string>` or `none`",
            );
            return Ty::Unknown;
        }
        let rm = self.check_expr(&args[3].value);
        if !matches!(
            rm,
            Ty::List(ref i)
                if matches!(**i, Ty::String) | matches!(**i, Ty::Unknown)
        ) && !matches!(rm, Ty::Unknown | Ty::None)
            && !matches!(rm, Ty::Option(ref i) if matches!(**i, Ty::List(ref j) if matches!(**j, Ty::String | Ty::Unknown)))
        {
            self.err(
                args[3].value.span,
                "`run_spawn`'s `env_removes` must be a `List<string>` or `none`",
            );
            return Ty::Unknown;
        }
        let mx = self.check_expr(&args[4].value);
        if !matches!(mx, Ty::Int | Ty::Unknown | Ty::None)
            && !matches!(mx, Ty::Option(ref i) if matches!(**i, Ty::Int))
        {
            self.err(
                args[4].value.span,
                "`run_spawn`'s `max_out` must be an `int` or `none`",
            );
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::Stream))
    }

    /// `proc_wait(h)` / `proc_running(h)` / `proc_code(h)` / `proc_kill(h)` /
    /// `proc_in_close(h)`: single-handle process queries.
    fn check_proc_handle(&mut self, e: &Expr, bname: &str, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(
                e.span,
                format!("`{bname}(handle)` takes exactly one argument"),
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        if !self.check_handle(&args[0]) {
            return Ty::Unknown;
        }
        match bname {
            "proc_code" => Ty::Option(Box::new(Ty::Int)),
            _ => Ty::Bool,
        }
    }

    /// `thread_spawn(body)` and `once_run(o, body)`. `body_index` is the
    /// argument that has to be a zero-argument function: a lambda, or a name
    /// bound to one. The other arguments are handles (`int`).
    fn check_task_body(
        &mut self,
        e: &Expr,
        bname: &str,
        args: &[CallArg],
        body_index: usize,
    ) -> Ty {
        let label = if body_index == 0 {
            "`thread_spawn(body)`".to_string()
        } else {
            "`once_run(once, body)`".to_string()
        };
        if args.len() != body_index + 1 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(e.span, format!("{label} takes exactly one body argument"));
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        for (i, a) in args.iter().enumerate() {
            if i == body_index {
                continue;
            }
            let t = self.check_expr(&a.value);
            if !matches!(t, Ty::Int | Ty::Unknown) {
                self.err(a.value.span, "a synchronisation handle is an `int`");
                return Ty::Unknown;
            }
        }
        let body = self.check_expr(&args[body_index].value);
        match &body {
            Ty::Fn(pty, prt) => {
                if !pty.is_empty() {
                    self.err(
                        args[body_index].value.span,
                        format!("{label}'s body must take no parameters"),
                    );
                } else if !matches!(prt.as_ref(), Ty::Empty | Ty::Unknown) {
                    self.err(
                        args[body_index].value.span,
                        format!("{label}'s body must return nothing"),
                    );
                }
            }
            Ty::Unknown => {}
            other => {
                self.err(
                    args[body_index].value.span,
                    format!("{label}'s body must be a function, found `{}`", other),
                );
            }
        }
        if bname == "once_run" {
            Ty::Bool
        } else {
            Ty::Int
        }
    }

    /// `proc_out_read(h, n)` / `proc_err_read(h, n)`: drained bytes as
    /// `List<byte>?`.
    fn check_proc_read(&mut self, e: &Expr, bname: &str, args: &[CallArg]) -> Ty {
        if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                format!("`{bname}(handle, n)` takes exactly two arguments"),
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        if !self.check_handle(&args[0]) {
            return Ty::Unknown;
        }
        let n = self.check_expr(&args[1].value);
        if !matches!(n, Ty::Int | Ty::Unknown) {
            self.err(
                args[1].value.span,
                format!("`{bname}`'s `n` must be an `int`"),
            );
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::List(Box::new(Ty::Byte))))
    }

    /// `proc_in_write(h, b)`: write `b` (a `List<byte>`) to the child's stdin,
    /// returning the number of bytes written.
    fn check_proc_write(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 2 || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(
                e.span,
                "`proc_in_write(handle, b)` takes exactly two arguments",
            );
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        if !self.check_handle(&args[0]) {
            return Ty::Unknown;
        }
        let b = self.check_expr(&args[1].value);
        if !matches!(
            b,
            Ty::List(ref i) if matches!(**i, Ty::Byte) | matches!(**i, Ty::Unknown)
        ) && !matches!(b, Ty::Unknown)
        {
            self.err(
                args[1].value.span,
                "`proc_in_write`'s `b` must be a `List<byte>`",
            );
            return Ty::Unknown;
        }
        Ty::Int
    }

    /// `current_dir()`: the workspace directory as `string?`.
    fn check_current_dir(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if !args.is_empty() {
            self.err(e.span, "`current_dir()` takes no arguments");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::String))
    }

    /// `exit(code)`: terminate the process with a numeric exit code.
    fn check_exit(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if args.len() != 1 || args[0].name.is_some() || args[0].spread {
            self.err(e.span, "`exit(code)` takes exactly one argument");
            for a in args {
                let _ = self.check_expr(&a.value);
            }
            return Ty::Unknown;
        }
        let at = self.check_expr(&args[0].value);
        if !matches!(at, Ty::Int | Ty::Unknown) {
            self.err(args[0].value.span, "`exit` requires an `int` exit code");
            return Ty::Unknown;
        }
        Ty::Empty
    }

    /// `captureBegin()`: enter the test-harness capture mode (turns failing
    /// asserts into recorded failures) without clearing any pending message.
    fn check_capture_begin(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if !args.is_empty() || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(e.span, "`captureBegin()` takes no arguments");
            return Ty::Unknown;
        }
        Ty::Empty
    }

    /// `captureTake()`: drain the captured failure message as `string?`, or
    /// `none` when nothing was recorded.
    fn check_capture_take(&mut self, e: &Expr, args: &[CallArg]) -> Ty {
        if !args.is_empty() || args.iter().any(|a| a.name.is_some() || a.spread) {
            self.err(e.span, "`captureTake()` takes no arguments");
            return Ty::Unknown;
        }
        Ty::Option(Box::new(Ty::String))
    }

    fn check_if(&mut self, e: &Expr, cond: &IfCond, then: &Block, else_else: Option<&Expr>) -> Ty {
        // For `if (let pattern = value)` the binding stays in scope for the
        // then-block only: a failing pattern skips the binding entirely, so
        // the else branch must not see it.
        let bind_scope = match cond {
            IfCond::Cond(c) => {
                self.check_bool_cond(c);
                false
            }
            IfCond::Binding { pattern, value } => {
                let vt = self.check_expr(value);
                self.check_match_pattern(pattern, &vt, value.span);
                self.push_scope();
                self.bind_pattern(pattern, &vt, false);
                true
            }
        };
        // Analyze each branch from the same entry state and merge the moved
        // bits, so a value moved on one path is treated as moved afterwards
        // without falsely rejecting a move that happens on both paths.
        let before = self.capture_moved();
        let tt = self.check_block(then);
        let mut after = self.capture_moved();
        if bind_scope {
            self.pop_scope();
        }
        let _ = e.span;
        if let Some(else_e) = else_else {
            self.set_moved(&before);
            let et = self.check_expr(else_e);
            let after_else = self.capture_moved();
            Self::union_moved(&mut after, &after_else);
            if tt != Ty::Empty && et != Ty::Empty {
                match Self::merge_flow_types(&tt, &et) {
                    Some(merged) => {
                        self.set_moved(&after);
                        return merged;
                    }
                    None => self.err_note(
                        else_e.span,
                        format!("`if` branches have mismatched types: `{tt}` and `{et}`"),
                        "both branches of an if-expression must produce the same type",
                    ),
                }
            }
            self.set_moved(&after);
            tt
        } else {
            self.set_moved(&after);
            Ty::Empty
        }
    }

    /// Merge the types produced by two flow branches. A bare value promotes to
    /// an option when exactly one side is `none` (or when one side is the
    /// option of the other); `Unknown` absorbs everything. Returns `None` when
    /// the types are genuinely incompatible.
    fn merge_flow_types(a: &Ty, b: &Ty) -> Option<Ty> {
        if a == b {
            return Some(a.clone());
        }
        match (a, b) {
            (Ty::None, Ty::None) => Some(Ty::None),
            (Ty::None, b) => Some(b.opt_of()),
            (a, Ty::None) => Some(a.opt_of()),
            (Ty::Option(ia), _) if ia.as_ref() == b => Some(a.clone()),
            (_, Ty::Option(ib)) if ib.as_ref() == a => Some(b.clone()),
            (Ty::Unknown, _) | (_, Ty::Unknown) => Some(Ty::Unknown),
            _ => None,
        }
    }

    fn check_match(&mut self, e: &Expr, scrutinee: &Expr, arms: &[MatchArm]) -> Ty {
        let st = self.check_expr(scrutinee);
        let mut result = Ty::Empty;
        let before = self.capture_moved();
        let mut merged: Option<Vec<HashMap<String, (bool, bool)>>> = None;
        for arm in arms {
            self.set_moved(&before);
            self.push_scope();
            self.check_match_pattern(&arm.pattern, &st, arm.span);
            self.bind_pattern(&arm.pattern, &st, false);
            if let Some(g) = &arm.guard {
                self.check_bool_cond(g);
            }
            let bt = self.check_expr(&arm.body);
            self.pop_scope();
            let after_arm = self.capture_moved();
            match &mut merged {
                Some(acc) => Self::union_moved(acc, &after_arm),
                None => merged = Some(after_arm),
            }
            if result == Ty::Empty {
                result = bt;
            } else if bt != Ty::Empty {
                match Self::merge_flow_types(&result, &bt) {
                    Some(merged) => result = merged,
                    None => self.err_note(
                        arm.span,
                        format!("match arms produce inconsistent types: `{result}` and `{bt}`"),
                        "all arms of a match expression must produce the same type",
                    ),
                }
            }
        }
        if let Some(acc) = merged {
            self.set_moved(&acc);
        }
        let _ = e.span;
        result
    }

    /// Validate that a match arm's pattern can actually match a scrutinee of
    /// type `st`. Binding/wildcard/tuple patterns are structurally fine;
    /// literals must share the scrutinee's domain, and string patterns may
    /// not interpolate.
    fn check_match_pattern(&mut self, p: &Pattern, st: &Ty, span: Span) {
        if matches!(st, Ty::Unknown | Ty::Var(_)) {
            return;
        }
        match p {
            Pattern::Wildcard | Pattern::Binding { .. } => {}
            Pattern::Tuple(parts) => match st {
                Ty::Tuple(tys) => {
                    if tys.len() != parts.len() {
                        self.err_note(
                            span,
                            format!(
                                "a {}-element tuple pattern cannot match a {}-tuple",
                                parts.len(),
                                tys.len()
                            ),
                            "tuple patterns must line up with the scrutinee's arity",
                        );
                    } else {
                        for (part, t) in parts.iter().zip(tys.iter()) {
                            self.check_match_pattern(part, t, span);
                        }
                    }
                }
                other => {
                    self.err_note(
                        span,
                        format!("a tuple pattern cannot match a value of type `{other}`"),
                        "tuple patterns require a tuple scrutinee",
                    );
                }
            },
            Pattern::Or(alts) => {
                for a in alts {
                    self.check_match_pattern(a, st, span);
                }
                if alts.iter().any(Self::pattern_introduces_binding) {
                    self.err_note(
                        span,
                        "an or-pattern alternative may not bind a name",
                        "`a | b` joins patterns that only test values (literals, `_`, `some(_)`, `Tag(_)`)",
                    );
                }
            }
            Pattern::Literal(l) => self.check_match_literal(l, st, span),
            Pattern::Variant { path, payloads } => {
                if path.len() == 1 && path[0] == "some" {
                    if !st.is_option() {
                        self.err_note(
                            span,
                            format!("`some` patterns require an option value, not `{st}`"),
                            "match `none`/`some(v)` against an option scrutinee",
                        );
                    } else {
                        let inner = st.inner_option().unwrap_or(Ty::Unknown);
                        for pl in payloads {
                            self.check_match_pattern(pl, &inner, span);
                        }
                    }
                    return;
                }
                // A variant path on a non-enum, non-option scrutinee is a
                // mismatch; enum variants are validated while binding.
                if !st.is_option() && !matches!(st, Ty::Enum(..)) && path.len() >= 2 {
                    self.err(
                        span,
                        format!(
                            "variant pattern `{}` cannot match a value of type `{st}`",
                            path.join(".")
                        ),
                    );
                }
                if st.is_option() && !(path.len() == 1 && path[0] == "some") {
                    self.err(
                        span,
                        "patterns on an option value must be `some(v)`, `none`, or `_`",
                    );
                }
            }
        }
    }

    /// True when a pattern names a binding at any depth (so it cannot appear
    /// as an or-pattern alternative, which would otherwise need identical
    /// bindings in every arm).
    fn pattern_introduces_binding(p: &Pattern) -> bool {
        match p {
            Pattern::Binding { .. } => true,
            Pattern::Wildcard | Pattern::Literal(_) => false,
            Pattern::Variant { payloads, .. } => {
                payloads.iter().any(Self::pattern_introduces_binding)
            }
            Pattern::Tuple(parts) => parts.iter().any(Self::pattern_introduces_binding),
            Pattern::Or(alts) => alts.iter().any(Self::pattern_introduces_binding),
        }
    }

    fn check_match_literal(&mut self, l: &Lit, st: &Ty, span: Span) {
        let ok = match (l, st) {
            (Lit::Int { .. } | Lit::Float { .. }, Ty::Int | Ty::Float | Ty::Byte) => true,
            (Lit::Bool(_), Ty::Bool) => true,
            (Lit::Char(_), Ty::Char) => true,
            (Lit::Char(c), Ty::Byte) => *c as u32 <= 0x7F,
            (Lit::String(parts), Ty::String) => parts.iter().all(|p| matches!(p, StrPart::Text(_))),
            (Lit::None, _) => st.is_option(),
            _ => false,
        };
        if ok {
            return;
        }
        if let Lit::String(parts) = l {
            if parts.iter().any(|p| matches!(p, StrPart::Expr(_))) {
                self.err_note(
                    span,
                    "string match patterns may not interpolate",
                    "compare a literal inside the arm body instead",
                );
                return;
            }
        }
        let what = match l {
            Lit::Int { .. } => "an integer",
            Lit::Float { .. } => "a float",
            Lit::Bool(_) => "`true`/`false`",
            Lit::Char(_) => "a character literal",
            Lit::String(_) => "a string literal",
            Lit::None => "`none`",
        };
        self.err_note(
            span,
            format!("{what} pattern cannot match a value of type `{st}`"),
            "match patterns must share the scrutinee's type",
        );
    }

    fn check_cast(&mut self, e: &Expr, expr: &Expr, ty: &TypeExpr, kind: CastKind) -> Ty {
        let src = self.check_expr(expr);
        let target = self.resolved_fn_ty(ty, self.instantiated_generics(&self.fn_generics));
        match kind {
            CastKind::Is => {
                // `is` must check against a type the source can actually be.
                // Unknown types are not yet resolved; allow them through.
                if src != Ty::Unknown && target != Ty::Unknown && !self.cast_related(&src, &target)
                {
                    self.err_note(
                        e.span,
                        format!("`{src} is {target}` can never succeed"),
                        "cast target must be assignable from, or an ancestor/descendant of, the source",
                    );
                }
                Ty::Bool
            }
            CastKind::As | CastKind::TryAs => {
                if src != Ty::Unknown
                    && target != Ty::Unknown
                    && !self.cast_related(&src, &target)
                    // Casts between the char scalar and an integer scalar
                    // reinterpret the code point value (`char as int`,
                    // `int as char`, including `byte`). `is` does not use
                    // this relation: a char has no sub/supertype.
                    && !matches!(
                        (&src, &target),
                        (Ty::Char, Ty::Int)
                            | (Ty::Char, Ty::Byte)
                            | (Ty::Int, Ty::Char)
                            | (Ty::Byte, Ty::Char)
                    )
                {
                    self.err_note(
                        e.span,
                        format!("cannot cast `{src}` to `{target}`"),
                        "cast target must be assignable from, or an ancestor/descendant of, the source",
                    );
                }
                if kind == CastKind::TryAs {
                    target.opt_of()
                } else {
                    target
                }
            }
        }
    }

    /// Whether `from` can be safely cast to `to`: numeric conversions,
    /// option<->inner, class inheritance (either direction), and
    /// conforming interfaces.
    fn cast_related(&self, from: &Ty, to: &Ty) -> bool {
        if from == to {
            return true;
        }
        if from.is_numeric() && to.is_numeric() {
            return true;
        }
        // Unwrap option layers on either side.
        if let Some(inner) = from.inner_option() {
            if self.cast_related(&inner, to) {
                return true;
            }
        }
        if let Some(inner) = to.inner_option() {
            if self.cast_related(from, &inner) {
                return true;
            }
        }
        // Same-kind structural/type relations.
        match (from, to) {
            (Ty::Class(a, _), Ty::Class(b, _)) | (Ty::Struct(a, _), Ty::Struct(b, _)) => {
                a == b
                    || self.is_ancestor(a, b)
                    || self.is_ancestor(b, a)
                    || self.conforms_to(from, to)
                    || self.conforms_to(to, from)
            }
            (Ty::Interface(a, _), Ty::Interface(b, _)) => {
                a == b || self.conforms_to(from, to) || self.conforms_to(to, from)
            }
            (Ty::Class(a, _) | Ty::Struct(a, _), Ty::Interface(b, _)) => {
                self.implements_interface(a, b)
            }
            (Ty::Interface(a, _), Ty::Class(b, _) | Ty::Struct(b, _)) => {
                self.implements_interface(b, a)
            }
            (Ty::Enum(a, _), Ty::Enum(b, _)) => a == b,
            (Ty::List(a), Ty::List(b)) | (Ty::Range(a), Ty::Range(b)) => self.cast_related(a, b),
            (Ty::Map(ka, va), Ty::Map(kb, vb)) => {
                self.cast_related(ka, kb) && self.cast_related(va, vb)
            }
            (Ty::Fn(pa, ra), Ty::Fn(pb, rb)) => {
                pa.len() == pb.len() && {
                    let mut ok = true;
                    for (x, y) in pa.iter().zip(pb.iter()) {
                        if !self.cast_related(x, y) {
                            ok = false;
                        }
                    }
                    ok && self.cast_related(ra, rb)
                }
            }
            _ => false,
        }
    }
}

impl ClassMember {
    fn name(&self) -> String {
        match self {
            ClassMember::Field { name, .. }
            | ClassMember::Const { name, .. }
            | ClassMember::Property(PropertyDecl { name, .. })
            | ClassMember::Method(MethodDecl { name, .. }) => name.clone(),
            ClassMember::Constructor(c) => format!("constructor {}", c.span),
            ClassMember::Init(_) => "init".into(),
            ClassMember::Deinit(_) => "deinit".into(),
        }
    }
}

impl Pattern {
    /// Patterns do not carry their own span in the AST yet; error reporting
    /// for pattern mismatches uses the enclosing span passed by the caller.
    fn span(&self) -> Span {
        Span::new(crate::diag::FileId(0), 0, 0)
    }
}
