# PickleScript — Compiler Architecture

Implemented in Rust, organized as a workspace.

## Pipeline

```
Source files
   |
   v
Lexer -------------------- tokens ----------> Parser
   |                                             |
   |                                             v
   |                                           AST (untyped)
   |                                             |
   |                                             v
   |                                    Resolver (names, scopes,
   |                                     modules, visibility)
   |                                             |
   |                                             v
   |                                    Type Checker (bidirectional
   |                                     inference, interfaces,
   |                                     nullability)
   |                                             |
   |                                             v
   |                                    Semantic IR (AstLowered:
   |                                     typed, monomorphic-ready,
   |                                     inlined-able)
   |                                             |
   |                                             v
   |                                   Mono (generics -> concrete
   |                                    instantiations)
   |                                             |
   |                                             v
   |                                    PickleIR (SSA-ish, explicit
   |                                     GC safepoints, shadow-stack
   |                                     slots, call lowering)
   |                                             |
   |                                             v
   |                                    Cranelift backend
   |                                      (module, functions, data)
   |                                             |
   |                                             v
   |                                    cranelift-object -> COFF/ELF
   |                                             |
   |                                             v
   |                                    rustc (native system linker)
   |                                             |
   |                                             v
   |                                    Native executable  (+ lib runtime)
   v
```

## Crates

| Crate              | Role                                                               |
|--------------------|--------------------------------------------------------------------|
| `pickle-compiler`  | lexer, parser, AST, resolver, type checker, PickleIR, Cranelift backend, formatter |
| `pickle-runtime`   | GC, object model, builtins, stdio, entry point, exported C ABI; linked as an rlib by `pickle build` |
| `pickle-cli`       | the `pickle` tool: project loading, subcommands, build orchestration, REPL driver |

`pickle-cli` depends on `pickle-compiler`; the object file produced by the
compiler is linked against `pickle-runtime`.

## Front end

- **Diag**: source files, byte spans, severities; errors annotate code with
  a caret and a hint. Diagnostics stream to a colored reporter (TTY) or
  plain text (CI).
- **Lexer**: maximal-munch tokenizer over `Option<char>` peeked through a
  byte-indexed source (no `\0` end sentinel; a literal NUL is reported as an
  error and lexing continues). Raw `r"..."`, triple-quoted `"""..."""`, and
  interpolated strings share one escape handler (`\n \t \r \\ \" \{ \} \0`,
  `\u{...}`); interpolation re-enters the full scanner with brace-depth
  tracking so nested literals/operators lex correctly. Doc comments are
  trivia attached to the following token. Bad characters are skipped with a
  diagnostic instead of aborting the stream. Newlines are significant tokens
  (statement boundaries) plus a separate "continues" rule computed in the
  parser, so multi-line expressions still work.
- **Parser**: recursive descent, precedence climbing for binary operators.
  Produces an untyped AST with spans. Fails fast with recovery at the next
  declaration boundary so one session reports many errors.
- **Loader** (front): discovers an optional workspace `picklescript.toml`
  (`[workspace] members = [{ name, path }, ...]`, `[package]`, `[paths]`,
  `[registry]`, lockfile) when a file is loaded, then resolves import module
  paths. Paths search the importing directory up to `<cwd>/src/`; a leading
  segment matching a workspace member is authoritative and resolves inside
  that crate (root -> `lib.pkl`/`main.pkl`, deeper segments -> module files or
  package dirs). TOML is hand-parsed (a tight subset); no dependency added.
- **Resolver**: module graph → import graph; scope trees; binding of every
  identifier; visibility checks (private/protected); duplicate detection;
  method override validation; generic parameter scopes. Name collection is
  **two-pass**: every declared name (and an identity/generics-only shell for
  every class, struct, enum, and interface) is registered before any body is
  resolved, so a body may reference a type declared earlier *or later* in the
  file — type and payload references may be forward, self-referential,
  mutually recursive, or cyclic.
- **Type checker**: bottom-up inference with expected-type propagation,
  signature unification across calls, interface conformance checks, option
  flattening, `is`/`as` validation, operator method lookup, monomorphization
  requests.

## Middle end

- **AstLowered**: typed AST with resolved symbols, implicit conversions
  materialized, desugaring of `??`, `?.`, ranges, lambdas -> closures,
  `+` on strings -> concat calls, pattern matching -> branch lowering.
- **Mono**: template instantiations for generic classes/functions and
  interfaces-as-generic-bounds; dedupes instantiations; sets concrete
  layouts.
- **PickleIR**: high-level typed IR with:
  - structural terms (blocks, calls, loads/stores, allocs with class ids)
  - SSA values
  - explicit GC safepoints and shadow-stack slot descriptors
  - invariants: every managed value either lives in a shadow slot or is
    spilled before any call/alloc.

## Back end

Cranelift (streaming, fast, register-allocated, no C dependency) > native
machine code. `cranelift-module` + `cranelift-object` emit a single
relocatable COFF/ELF object for a whole module. A future `llvm` feature
switch will target LLVM for profile-guided and aggressive optimization,
sharing PickleIR.

`pickle build` emits the object, compiles `pickle-runtime` as an rlib with
`cargo`, and links them with `rustc` acting as the linker driver (so the
install's native system linker handles layout and CRT bootstrap). The object
exports `pickle_main` and imports the runtime's `pickle_*` helpers; a small
generated shim provides the process `main`, boots the runtime
(`pickle_runtime_init`), calls `pickle_main`, flushes output, and shuts the
collector down.

Two details keep the AOT path self-contained:

- float `%` lowers to a `pickle_fmod` defined *inside* the generated object
  as `a - floor(a / b) * b` CLIF, so the binary has no dependency on a
  `libm` `fmod` symbol;
- symbol references are carried between lowering phases as Cranelift
  test-case names, which the object backend rejects; emission remaps them to
  user names (namespace 0 = functions, 1 = data) before writing the object.
  Symbols the object *defines itself* (like the `pickle_fmod` binding above)
  must be registered in the same symbol maps, or their references keep a
  test-case name and emission panics.

Every other runtime helper is a plain imported `pickle_*` symbol resolved at
link time against the runtime rlib (the JIT resolves the same names through
its symbol table). That includes the list slice ABI added later: `pickle_box_*` /
`pickle_unbox_*` for scalar list elements and `pickle_list_new` / `len` /
`get` / `set` / `push` / `pop` / `remove` / `insert` / `sort` for `List<T>`,
which flow through this shared extern path with no extra object glue. The map
slice ABI follows the same
path: `pickle_map_new` / `set` / `get_boxed` / `has` / `len` / `keys` /
`values`.

One design note on `Map<K, V>`: to keep object reads cheap and GC-safe, an
index read passes an explicit *default* value down to `pickle_map_get_boxed`
(the value type's zero — `0`/`0.0`/`false`, an interned empty string, or
null). The runtime returns it unchanged when the key is absent, so the
emitter never unboxes a null pointer. Keys are managed objects: a `string`
object, a boxed scalar (`int`/`float`/`bool`/`char`/`byte`), or a composite
object (list, map, enum, class/struct instance). Scalar keys are boxed at the
boundary (`pickle_box_*`); composite keys pass through as their object
pointer. The runtime hashes and compares keys by class id plus payload:
scalars as `(class_id, payload)`, strings over their bytes, and composites
structurally (deep) with a cycle guard that falls back to root-identity
semantics for cyclic structures (see `runtime/src/map.rs`). The type checker
enforces key types at literal, index, and method sites; optional, tuple,
reference, function, and pointer keys are not lowered.

Enums follow the same extern path. An enum value is a managed object
(`PEnum`) whose class id is `PICKLE_CLASS_ENUM` (7). Streams are managed
objects too (class id `PICKLE_CLASS_STREAM`, 9) whose slot 0 is a *raw*
`Box` pointer to the open handle; user classes start at
`PICKLE_CLASS_USER_BASE`, 10.
Its payload slot 0 holds the variant tag as a *raw* `i64` (unmanaged, skipped
by the tracer); payload slots 1.. hold the variant's fields, boxed with the
same element rules as `List<T>` elements. Field count is derived at runtime
from the object size (`(size - header)/8 - 1`), so all variants of an enum
get an object sized for their widest variant. The ABI is
`pickle_enum_new(tag, count)` / `pickle_enum_set_field(obj, i, value)` /
`pickle_enum_tag(obj)` / `pickle_enum_field(obj, i)`.

Lowering rules:

- `Enum.Variant(args...)` compiles to `pickle_enum_new` plus one
  `pickle_enum_set_field` per argument, boxing scalar payloads.
- A bare zero-field variant (`Color.Red`) compiles to
  `pickle_enum_new(tag, 0)`.
- `match (s) { case V(p..) -> body ... }` keeps the scrutinee in a managed
  shadow slot (so the GC retains it across arm allocations), reads the tag
  once, and lowers to a `BranchIf` chain comparing `tag == variant_index`.
  Payload bindings unbox each field into fresh slots; `case _` / `case name`
  catch-all arms are a plain branch.
- A chain that reaches its end without a catch-all arm calls
  `pickle_panic_no_match`, which faults with "pickle: match is not
  exhaustive" — the checker does not require exhaustiveness, so misses fail
  loudly at runtime instead of reading garbage.
- Patterns beyond `Variant`/`Wildcard`/`Binding` payload names bail with
  "not lowered yet". A guarded arm lowers into a `guard_in` block between the
  tag-check target and the body: payload bindings (or the binding-name for a
  guarded catch-all) are live there, the guard evaluates to a bool, and a
  `BranchIf` on the guard sends the value to the body or falls through to the
  next arm chain. Guard-false is not a match failure: the search continues (a
  guarded catch-all keeps the chain live, and a chain that exhausts every arm
  falls into the `pickle_panic_no_match` trap). Guards may use `&&`/`||`,
  which lower through the ordinary short-circuit `logic()` path.
- Non-enum scrutinees share the same per-arm chain shape via
  `lower_generic_match`: the value is stored in a shadow slot of its scalar IR
  domain, and `pattern_test_cond` builds the branch condition per pattern —
  string literals via `pickle_str_cmp == 0`, numeric/bool/char literals via a
  const `==` (integer literals are `itof`-widened against a float scrutinee),
  `none` as `!opt_is_present`, `some(v)` as `opt_is_present`. `bind_match_value`
  then binds the scrutinee (unboxed through `opt_resolve` for `some(v)`) into
  fresh slots. `if (let pattern = value)` is `if_let_expr`: the value is
  evaluated once, `pattern_test_cond` gates the then-block (a bare binding/wild
  card pattern is a plain branch), and bindings are scoped to the then-block
  only, since a failed `some(v)` test must not leak a stale binding to `else`.
- Numeric promotion at mixed float/int operands (`r > 0`, `f * i`) inserts an
  `itof` (i64 -> f64) conversion instruction on the int side before the binop,
  so the JIT and AOT always see same-width operands.
- Comparison operators are the only binops whose result is `bool` but whose
  operand types vary (`int`/`float`/`char`/`bool` args). The checker gates
  them with `check_comparison` (numbers, chars, and `==`/`!=` on identical
  types only), and the emitter has a belt-and-suspenders `cmp_types_compatible`
  guard in `binary()` that bails "comparison ... with incompatible operand
  types is not lowered yet" rather than emit a mismatched-width `icmp` (which
  the Cranelift verifier would reject: `i8` bool vs `i64` int), e.g. the bug
  `x < 3 > x / 2` parsed as `(x < 3) > (x / 2)`. `char` is an `I32`-width
  operand for ordering ops.

Classes use the same object-model extern path. A class instance is a plain
managed object (`PickleObject`) whose runtime `ClassDescriptor` carries the
name (for printing) and a field-validity mask. Descriptors are allocated *at
runtime* rather than baked into the generated binary: `main`'s entry block
opens with one `pickle_class_register(name_ptr, name_len, slot_count, mask,
finalizer, parent)` void call per class, in registration order, so descriptor id
== call site id. Call sites inside constructors hardcode that id (`8 + emitted
index`), so no id is threaded through the lowering; `pickle_class_register`
returns the id but emitters discard it. `StrAddr` lowerings pull the name bytes
from the data section (`pkl_strdata_N`) — AOT marks those externs as data, JIT
lowers them to symbol addresses; the pointer is never treated as a GC object.
The `finalizer` argument is the address of `pkl_<TypeName>_deinit`, or `0`; the
`parent` argument is the superclass id, or `0` for a root class.

Lowering rules:

- A `deinit { ... }` lowers to `pkl_<TypeName>_deinit(this) -> unit`: slot 0 is
  `this`, the body runs like a constructor body (unit result, `this` live), and
  the function's address is passed to `pickle_class_register` through a
  `FuncAddr` const (`addrof fn#N`, typed `int` so the raw code pointer never
  enters the GC trace frame). A class without `deinit` passes `0`.
- Single inheritance (`class Child extends Parent`) is lowered for a subset:
  state, methods, `override fn`, `super.m()`, hierarchy casts, and constructor
  chaining. Classes must
  be registered **ancestor-first** (the emitter pulls superclasses in before
  subclasses), so a superclass always has a lower id. Instance fields are laid
  out **parent-first**: a subclass's object slots begin with the superclass's
  instance fields, then its own; the absolute slot is `parent_total + own_index`,
  and the registration `slot_count` (and the constructor's `pickle_class_new`
  field count) is the whole ancestry's total. A subclass inherits its parent's
  method and property dispatch entries (its own declarations win) and derives
  its synthesized constructor parameters from all instance fields without
  initializers, superclass first. `super.m(...)` lowers to a direct call of the
  superclass method on the same receiver.
- Constructors in a hierarchy use **`super(...)` chaining**. A subclass
  constructor that opens with `super(args)` has the chain inlined into its own
  `pkl_<Name>_new`: after `pickle_class_new` allocates the whole hierarchy and
  every field initializer (all ancestry, parent-first) runs, the evaluated
  `super` arguments bind the parent constructor's parameters — or, for a
  synthesized parent, are boxed into its uninitialized field slots — and the
  parent constructor body runs, recursing up the chain. Parameter binding for
  pointer-typed parameters option-wraps the argument the same way a direct
  constructor call does. The rest of the child's body then runs, followed by
  the hierarchy's `init` blocks (still run once, in the leaf constructor).
  Named constructors lowdown unchanged: they delegate via `this(...)` to the
  class's primary constructor, which absorbs the chain. Validation: an explicit
  constructor whose parent declares an explicit constructor must start with
  `super(...)` (else E0900). A class with no explicit primary constructor may
  still sit below an ancestor that declares one: its **synthesized constructor
  forwards** the nearest explicit ancestor's constructor parameters `super(...)`
  from the top (leading slots), and then takes only the uninitialized instance
  fields strictly below that ancestor (initialized ones are filled by their
  field initializer first). The example chain in `ctor_test.pkl` covers a
  synthesized leaf one level below an explicit-primary base, a sibling that
  mixes initialized/uninitialized fields, and a synthesized leaf several levels
  below through synthesized intermediates.
- A method redefined lower down with `override fn` is called through a
  **virtual dispatch cascade**. `build_virtual_dispatch` runs once after all
  classes register and, for every static receiver class, records the
  descendants that *own-define* a different implementation of each visible
  (own or inherited) instance method, deepest-derived first. A call whose
  method has a cascade entry (`virtual_method_call`) tests the receiver's
  runtime class id against each branch with `pickle_class_is` and calls the
  matching implementation, falling back to the statically-resolved fid — which
  is always the nearest owning ancestor's body, so a plain `WorkingDog()`
  instance inherits `Dog`'s implementation even though the dispatch table has
  an entry for `WorkingDog`. Keying by the *static receiver class* (not only
  the defining class) is what makes a mid-hierarchy receiver that merely
  inherits the method still reach a deeper override at runtime. `super.m(...)`
  bypasses the cascade (it always calls this class's implementation) and a
  method with no cascade entry is an ordinary direct call.
- Not-yet-lowered inheritance edges bail loudly instead of miscompiling:
  a subclass whose superclass is itself unbounded. `implements`/interfaces
  **lower** (see the "Interface dispatch" note below). Generic classes target
  these same edges when instantiated: an instantiation whose plan hits
  `extends`/`override`/named constructor/`deinit`/property/static-field/const
  bails with an E0900 (`implements` on a generic class now works — its
  binding is substituted per instantiation).
- **Interface dispatch** (lowered 2026-09-21): a receiver typed as an
  interface lowers a method call into a `pickle_class_is(receiver, cid)`
  cascade over every registered class that transitively `implements` that
  interface (deepest-derived-first, so a subclass override wins), each arm
  calling the class's resolved method function; receivers that match no
  static branch fall back to `pickle_iface_method(receiver, iface_id,
  method_index)` — a per-(class, interface) runtime method table lookup whose
  indirect call is zero-guarded to a loud panic. Interface ids are
  compiler-baked ints in a separate space (one per (name, type-argument)
  instantiation); `is`/`as`/`as?` against interfaces lower to
  `pickle_class_implements` / `pickle_iface_cast` / `pickle_class_is`. The
  indirect call's signature is derived from source types (`this: Ptr` +
  declared params, return = the call site's type) rather than from the
  target function's IR params, which are not built yet for deferred generic
  instantiations.
- **`Iterable`/`Iterator` protocol `for in`** (lowered 2026-09-21): prelude
  interfaces `Iterable<T> { fn iterator() -> Iterator<T> }` and
  `Iterator<T> { fn next() -> T? }` are seeded into every module's type table
  by the resolver (`Ty::Var("T")` members, so generic substitution just works;
  a user declaration overwrites them). `for (x in seq)` where `seq` is an
  `Iterable<elem>` value — an interface-typed receiver or a class/struct that
  transitively declares `implements Iterable<...>` (its own generic args
  substitute through the declaration) — lowers to `it = seq.iterator()` then a
  loop calling `it.next()`, storing the `T?` result, branching on
  `opt_is_present`, and binding `x` via `opt_resolve` (scalars unboxed).
  Both protocol calls reuse the interface dispatch core (via a zero-arg entry
  that marshals just the `this` receiver); the `Iterable<elem>` and
  `Iterator<elem>` views share the same iface-id space as user `implements`
  declarations. Builtin `List`/`Map`/`Range`/`string` sequences and
  `for ((k, v) in m)` keep their direct lowering and don't implement the
  interfaces in this slice.
- A generic *function* call `name<T,...>(args)` monomorphizes at the call
  site: the callee is instantiated over the resolved type arguments
  (substituted under the enclosing instantiation, so `id<U>` inside an
  in-progress `outer<U>` yields the caller's `U`), the instantiation is
  shared across identical type-argument lists, and the call lowers to a
  static call of `pkl_<name>_<suffix>`. When a generic function is called
  *without* `<...>` (`id(3)`, `first([1, 2, 3], 0)`), its type arguments are
  **inferred from the argument types** instead — the same unification in the
  checker and the emitter (`Ty::infer_from`): a `T` parameter pairs with the
  argument's concrete type, nested types (`List<T>`, `T?`, `(T) -> T`,
  `&T`, `Map<K, V>`) unify structurally, and one-pass argument checking
  prevents double diagnostics. Every generic parameter must be pinned by at
  least one argument; otherwise the call errors with "cannot infer the type
  argument `…` for `…`; specify them explicitly" and, in the emitter, bails.
The inferred instantiation is exactly the explicit one (`pkl_id_fn__int` is
   shared), so inference and hand-written type arguments never diverge.
- A generic function used **as a value** lowers by *pinning its type
   arguments* and reusing the normal fn-value machinery. With explicit
   arguments (`let f = id<int>`; `apply(id<int>, 3)`) the checked `GenericCall`
   resolves its type args (substituted under the enclosing instantiation) and
   emits a zero-capture closure whose slot 0 holds a generated **forwarder
   trampoline** — a `fn.value` class function `pkl_tramp_{target id}` built from
   `FnSource::Trampoline`, which forwards `(env, args...)` to `pkl_id__int` and
   is registered/`push_class_func`'d inline, consumed by the `fid_list` build
   loop. Passing a *bare* generic fn name (`apply(id, 3)`) infers its type
   arguments from the expected `fn`-typed parameter (`Ty::infer_from` on the
   signature), and materializes the same shared instantiation at the argument
   site (`borrow_arg`), so value forms and direct calls share one
   `pkl_id__int`. `Ty::infer_from` only pins a variable from *concrete*
   evidence (a type argument or hint that itself contains variables is
   skipped), which is what lets `apply_twice(id, 3)` infer `id`'s `T` without
   corrupting `apply_twice`'s own `T`. A value with no way to pin its type
   arguments (`let f = id`) keeps a half-open `fn (T) -> T` signature that the
   checker flags at the first use, and the emitter bails if one ever slips
   through; instantiations with unresolved `Var` type arguments bail cleanly
   with a "cannot instantiate …with unresolved type arguments" diagnostic.
- Lambdas **declared inside a generic function body** (`fn make_identity<T>()
   -> fn (T) -> T { (x: T) => x }`) are skipped at the pre-registration walk
   (`register_lambdas` runs under a `generic_fn_ctx` flag): their signatures
   carry `Var`s no module-scope hoist could type. Each instantiation that
   lowers the lambda registers its own hoisted body on first contact — in
   `lambda_value`, keyed `(enclosing instantiation id, span)` in `lambda_fids`
   — via `register_lambda_at`, which substitutes that instantiation into the
   parameter types, captures, and return type, and seeds `instanton_subst` for
   the copy's own build so nested lambdas inside it resolve the same
   substitution. Distinct instantiations therefore get distinct, correctly
   typed closure bodies (`make_identity<int>()` and `make_identity<float>()`
   emit `pkl_closure_*` bodies with `int` vs `float` parameters). Lambdas
   inside generic *class/struct* members still bail cleanly ("a lambda inside
   the generic class/struct … is not lowered yet").
- Generic classes and structs (`class Box<T>` / `struct Pair<A, B>`) lower by
  **lazy per-use-site materialization**. The first reference with a concrete
  type-argument list (`Box<int>(…)`, a `b.value`/`b.read()` access on a
  `Box<int>`, a generic fn call building `Pair<int, string>`) registers an
  *instantiation plan* at codegen
  time: a substituted `ClassTable` under the mangled display name (`Box<int>`),
  a fresh stable class id (`PICKLE_CLASS_USER_BASE + classes.len()`, allocated
  in deterministic plan order; the `pickle_class_register` preamble is injected
  *after* the
  build loop via `inject_class_registrations`), and mangled symbols for the
  constructor and methods — `pkl_Box_new__int`, `pkl_Box_read_int`,
  statics `pkl_<T>_sm_<m>_<suffix>` — deduplicated with a `_v{n}` counter on
  symbol collisions. Ctor/method/field layouts resolve through the
  substituted table (`table_of` falls back to plan tables), so concrete field
  types, slot counts, and scalar boxing rules all follow the type arguments.
  The same `class_id_of(ty, span)` resolution routes member reads, member
  assignments, optional access, and method dispatch for generic receivers.
  Returns `Ok(None)` (defer to the enclosing build) when the receiver's type
  still carries an unsubstituted `Var`, which keeps `Box<T>` inside an
  in-progress generic function working.
- `TypeName(args...)` compiles to `pkl_<TypeName>_new(args...)`: slot 0 of the
  object is `pickle_class_new(id, field_count)`, then each field value is boxed
  per the scalar list rules and stored with `pickle_obj_slot_set`. Only
  non-static fields occupy slots; a class with 64+ fields empties the mask.
  A generic instantiation's constructor is `pkl_<TypeName>_new__<suffix>` with
  the substituted field/parameter types; spread arguments in an instantiation
  call bail.
- Constructor parameters are the explicit `constructor(...)`'s parameters when
  one is declared; otherwise they are the instance fields *without*
  initializers (declaration order, superclass fields first). Fields with
  initializers (`var x: int = 0`) and `init { ... }` blocks run during
  construction: field initializers across the ancestry in root-first order, then
  the explicit constructor body (an implicit one has none — unless it opens with
  `super(...)`, which inlines the parent chain first), then any `init`
  blocks (also root-first). `this` is live throughout, so initializers can read
  earlier fields and the body can assign any member. An initialized field
  therefore needs no constructor argument, but still occupies a slot.
- A named constructor (`constructor.name(params) { this(...) }`) lowers to a
  receiver-less factory `pkl_<TypeName>_nc_<name>(params) -> Ptr`. Its body must
  be exactly one `this(args)` delegation to the primary constructor; the factory
  evaluates the delegation arguments (option-wrapped like any call), calls
  `pkl_<TypeName>_new`, and returns its pointer. `TypeName.name(args)` dispatches
  to that factory; a named constructor creates no cell and no per-class state.
- Instance methods compile to `pkl_<TypeName>_<m>`, with the receiver passed
  first as a managed pointer (IR param slot 0, declared as `this`); static
  methods compile to `pkl_<TypeName>_sm_<m>` with no receiver. Async/body-less
  methods and methods with default/rest params are skipped like their
  top-level counterparts; `override fn` methods lower with the same shape (and
  drive the dispatch cascade described above). Methods of a generic class
  instantiation follow the same shapes under the mangled symbols above
  (`pkl_Box_read_int`, `pkl_<T>_sm_<m>_<suffix>`).
- `this` loads the receiver slot. An undefined identifier inside an instance
  method falls back to `this.<name>`; `obj.field` read/write and compound
  assigns (`+=`) lower through `pickle_obj_slot_get`/`pickle_obj_slot_set`
  with scalar unbox/box on the boundary, and a member chain like
  `p.home.x` reads the nested object first via the normal member path.
- `Type.staticMethod(...)` and `instance.method(...)` dispatch through
  `method_ids` (parameterized by class id and method name); `this` is the
  first call argument for instance methods.
- Instance properties compile to `pkl_<TypeName>_<p>_get` and
  `pkl_<TypeName>_<p>_set` functions. A getter takes `this` in slot 0 and
  returns the property type; a setter takes `this` and a `value` parameter and
  returns unit. `obj.prop` reads dispatch the getter and `obj.prop = v` writes
  dispatch the setter; a bare property name inside an instance method falls
  back to `this.<prop>` through the getter. Getters/setters use `=> expr` or
  `{ ... }` bodies and may reference fields and methods through `this`.
  Compound assignment to a property bails with "compound assignment to a
  property is not lowered yet". Static properties compile to receiver-less
  `pkl_<TypeName>_sm_<p>_get`/`_set` functions dispatched through a separate
  map keyed by class id and property name (so instance dispatch can never read
  a static accessor), and the checker requires them to be reached through the
  type name: `Type.prop` reads call the getter, `Type.prop = v` writes call the
  setter, a static getter body may reference other statics via `Type.other`,
  and instance receivers cannot reach static properties (or vice versa).
  Statics with a getter but no setter are read-only.
- Static fields (`static var`) compile to runtime cells keyed by
  `(class id, static slot)`: `pickle_static_get`/`pickle_static_set` hold one
  managed pointer per field, and each cell is registered as a GC root so its
  pointee survives collection. Every static field is initialized once at
  program start by the synthesized `pkl_static_init` function — the declared
  value, or the field type's default zero/null — which `main`'s entry block
  calls right after class registration. `Type.field` reads/writes and compound
  `op=` go through the cell (scalars box/unbox on the boundary), and a bare
  static-field name inside the class's own static method resolves to the same
  cell. The checker enforces direction: a static field is reached through the
  type name, an instance field only through an instance. A subclass reference
  resolves the ancestor that declares the field: `Child.field` reads/writes the
  declaring class's cell (own declarations win), and the `static_field` helper
  walks the ancestry (own class first, then up) returning the declaring class's
  id, slot, and field info so reads/writes target the right cells.
- `const` members are compile-time values: the emitter inlines their
  initializer at every read (`Type.NAME`, or the bare name inside the class
  body), evaluating it in the declaring class's scope so one constant may
  reference another. No cell or static-init entry is created; the emitted code
  is the same as writing the literal at the use site. Assigning a constant and
  reaching one through an instance are rejected by the checker, and a cyclic
  constant is a codegen error ("cyclic `const` initialization"). Constants are
  inherited like static fields: reads walk the ancestry (own declarations win)
  and evaluate the initializer in the declaring class's scope.
- Bound instance-method values (`let f = c.m`) build a one-capture closure
  (`FnSource::MethodTrampoline`): slot 0 is a forwarder `pkl_mtramp_*`, slot 1
  the packed receiver. When the method is overridden somewhere, the forwarder
  dispatches on the receiver's runtime class (`pickle_class_is` cascade,
  exactly like `virtual_method_call`) before falling back to the statically
  resolved implementation; otherwise it calls the target method directly. The
  forwarder is cached per (receiver class, method name, fid) so virtual and
  non-virtual sites share one function each.
- `char` values are boxed/unboxed exactly like the other scalars: the runtime
  provides a dedicated `char` box class (id 6), so `char` fields, `List<char>`
  elements, and `char` map *values* store `pickle_box_char`/`pickle_unbox_char`
  pointers and load them back through the scalar list path. Under the locked
  string/char model (`04-types.md`) a `char` is a Unicode scalar; at the IR
  boundary it is an i32/UCS-4 scalar (0..=0x10FFFF), never a surrogate half.
  `char` → `string` lowers to `pickle_str_from_char`, which encodes the scalar
  as UTF-8. An absent map key of type `char` reads back the zero value `'\0'`.
- `s[i]` on a `string` lowers to `pickle_str_get`: a bounds-checked **byte**
  read at the **`int` (`i64`) ABI** (out-of-range panics). `for (c in s)`
  iterates a string through the same path (`pickle_str_len` bound +
  `pickle_str_get` per trip). Whether indexed or looped, the element's static
  type is `byte` and non-ASCII text yields the raw UTF-8 byte, never a decoded
  scalar — strings are byte-addressed by design, matching Rust
  `s.as_bytes()[i]`. A scalar-exact view is the reserved future `s.chars()`;
  nothing decodes UTF-8 implicitly today. `pickle_str_get` returns `i64`, so a
  byte plugs straight into `int` arithmetic and comparisons. A byte that needs
  to be text again lowers to `pickle_str_from_byte`, which writes that single
  byte, so `"{b}"` round-trips non-ASCII text; `print` of a `byte` is
  `pickle_print_byte` (raw byte, reads back both bytes of `"é"` correctly).
- `char` stays a Unicode scalar at the IR (`i32`) and prints/interpolates as
  UTF-8, matching byte-level round-trips: `print(c)` lowers to
  `pickle_print_char`. The checker rejects too-few-argument constructor
  calls, so a miscounted synthesized-ctor call reports "expected N
  argument(s), found M" instead of reaching a codegen crash.
- `println`/`print` of a class/struct value lowers to `pickle_print_obj`,
  which prints the class name plus `Class` fields when available.

Options lower as a managed pointer with no dedicated allocation:

- `Option<T>` (`T?`) occupies a `ptr` slot/return; `none` is a null constant.
  `some(v)` is `v` itself when `v` is already managed (class/struct/enum/
  string/`List`/`Map`/nested option) and a `pickle_box_<scalar>` pointer when
  `v` is a primitive. A boxed value is a class, so it is never boxed twice.
- The checker does not rewrite the lifted expression's type, so the emitter
  lifts by *value* type: a value whose natural type is a scalar flowing into a
  `ptr` boundary — annotated/typed `let`/`const`, `return`, a function tail
  expression, a call argument at a `T?` parameter, a ctor/field store, or a
  property setter — is boxed once; managed values pass through. `none`/managed
  values are never boxed.
- `a ?? b` evaluates `a`, null-tests it, and yields `a`'s unboxed value when
  present or the already-evaluated `b` otherwise. Its type is the inner type,
  so it assigns to `T`, not `T?`.
- Postfix `?` unwraps: it null-tests and calls `pickle_panic_none_unwrap`
  ("pickle: unwrapped none value") on `none`, otherwise unboxes/returns the
  value. (`!` is logical not.)
- `obj?.member` evaluates the receiver once, null-tests it, reads the field or
  dispatches the property getter on the present branch (lifting a scalar
  member back into an option), and yields `none` on the absent branch. The
  member's declared type — a field slot's type or the property table's `ty` —
  drives the re-lift.

Casts lower for the statically-known subset:

- `x as float` / `x as int` emit `itof` / `ftoi` (the latter saturating, so
  NaN/out-of-range clamps instead of trapping).
- Scalar reinterpret casts between the integer families and `char`: `int`/`byte`
  share one machine domain, so `int as byte` / `byte as int` fold to an identity
  move; `char as int` / `char as byte` zero-extend the i32 code point to a word
  (`chartoi`), and `int as char` / `byte as char` narrow a word down to the i32
  code point (`itochar`). `is` does not use that relation — a char value is
  neither an int nor a byte — so `x is int` on a char still rejects.
- Casting to an option — `x as? T`, or `as T?` — always yields `T?` and never
  panics: a present value is converted (numeric) and boxed, `none` stays
  `none`. The checker types `as?` as `T?`.
- `x as T` where `x: T?` asserts presence: it unwraps (panicking on `none`
  through `pickle_panic_none_unwrap`) then converts.
- `x is T` is a presence test when `x: T?` and `T` is `x`'s inner type;
  otherwise it folds statically (`T is T` -> true, `int is float` -> false).
- Class `is`/`as` across a lowered inheritance edge compile to runtime calls.
  An upcast (the target is an ancestor of the static type) folds to the value
  itself — `x is Ancestor` is `true`, `x as Ancestor` is the identity. A
  downcast emits `pickle_class_is(obj, id) -> bool` for `is` and
  `pickle_class_cast(obj, id) -> Ptr` for `as`, which walks the registered
  parent chain and panics on a mismatch. Interface edges still bail ("not
  lowered yet"), as do casts to unbounded classes; the checker accepts the
  related type pairs and codegen decides.

No semicolons, no headers, no Makefiles — `pickle build <file>` does all of
the above.

## Compiler phases as commands

`pickle` subcommands map to pipeline stages so failing stages are debuggable:

- `pickle build`   — full pipeline to executable.
- `pickle run`     — build (or JIT in dev) then execute.
- `pickle fmt`     — parse -> print canonical source (formatter).
- `pickle check`   — parse + resolve + typecheck only (fast CI gate).
- `pickle ast`     — dump AST.
- `pickle lex`     — dump the canonical token stream, one token per line
  (`Kind start:end` plus kind-specific payload fields: `text=`, `suf=`,
  `isf=`, `int=`, `ch=`, `T("...")`/`E[tok; ...]` string segments, and a
  trailing `doc=` for doc-comment trivia). Newline and Eof tokens are
  included. The format is the serialized contract the self-hosted lexer
  (`compiler-selfhost/lexer.pkl`) reproduces byte-for-byte, so `pickle lex`
  output on the Rust and self-hosted compilers diff cleanly.
- `pickle ir`      — dump typed IR.
- `pickle test`  — discover test modules under `tests/` (or a given path) and JIT-compile, run, report. Directory targets only pick up `*_test.pkl` / `*.test.pkl` files. Supports `test fn name()` and `test("desc", ...)`/`it` suites with `describe` groups and `beforeAll`/`beforeEach`/`afterEach`/`afterAll` hooks; `--filter` narrows by test-name substring (matches the desugared path, so `describe`/`test` names both filter). `#[tag("name")]` on a test item attaches test tags (repeatable); `--tag name` runs only tests carrying it (repeatable, any-tag match, AND-combined with `--filter`).
- `pickle repl`    — incremental front end + JIT.

## Error philosophy

Errors are: precise (span-accurate), kinded (syntax/type/name/…), and
actionable (hint). The compiler never emits a wall of cascading errors for
one root cause; after an error in a declaration it resynchronizes at the
next declaration.