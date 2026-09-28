# PickleScript — Memory Model

## Default: garbage collected

Normal PickleScript code never mentions memory.

```
class Account {
    name: string
    transactions: List<int>
}

fn main() {
    let a = Account("Ada")
    let b = a                // shared reference
    let copy = Object.clone(a) // only if you ask
}
```

- Assignments copy *values* (structs, primitives) and share *references*
  (classes, strings, collections).
- Objects are allocated on a GC heap, moved in/out of functions by
  reference. No ownership, no lifetimes, no let/const/borrow ceremony in
  safe code.
- Locals, fields, collection elements are all managed uniformly.
- Strings are immutable and UTF-8; substring/slice operations copy.

## Deterministic memory: `#[manualAlloc]`

Safe PickleScript is garbage collected. When you want to control exactly when
an object dies — a pool, a mesh, a buffer, a node in a hot loop — annotate the
binding with `#[manualAlloc]`:

```
#[manualAlloc]
let mesh = Mesh("terrain")
// ... use mesh ...
mesh.free()                       // runs deinit(), returns the memory now
```

Rules:

- The initializer must construct a `class` or `struct` object.
- A `#[manualAlloc]` binding is the object's **single owner**. While it is
  live the collector never reclaims the object and never runs its `deinit`;
  the object counts as a GC root, so anything it references stays alive too.
- `free()` runs `deinit` (if any) exactly once, then releases the memory back
  to the heap for reuse. Calling `free()` a second time, or reading the
  binding after `free()`, is a compile error.
- `free()` is only valid on a manual binding. Annotating a binding whose type
  is not a class/struct, passing arguments to the attribute, or using an
  unknown attribute is a compile error.

A manual object can be **moved** to another owner. Assigning it to a new
`#[manualAlloc]` binding, passing it to a `#[manualAlloc]` parameter, or
returning it from a `#[manualAlloc]` function transfers ownership; the old
binding can no longer be used:

```
#[manualAlloc]
let mesh = Mesh("terrain")
#[manualAlloc]
let other = mesh              // moves out of `mesh`
// mesh.load()                // error: use after move
other.free()
```

A function can own what it receives and what it returns:

```
fn consume(#[manualAlloc] w: Widget) {
    w.free()                  // callee owns `w`
}

#[manualAlloc]
fn make() -> Widget {
    return Widget(1)          // caller now owns the result
}
```

A `#[manualAlloc]` function result must be consumed into an owned position — a
`#[manualAlloc] let`, an owned parameter, or an owned return — otherwise it is
reported as a leak. You can call an owned parameter with a fresh allocation
directly (`consume(Widget(1))`); the callee adopts it at entry.

Owned **fields** make an object responsible for a manual child. Annotate the
field and give it an explicit type and an initializer:

```
class Node {
    value: int
}

class Tree {
    #[manualAlloc] root: Node = Node(0)
}

#[manualAlloc]
let tree = Tree()
tree.free()                   // frees `tree`, then `root`, recursively
```

- An owned field must be a `class`/`struct` type, must be an instance field
  (not `static`/`const`), and must be assigned an explicit type plus an
  initializer. Like any other field it can be reassigned later.
- Assigning to an owned field takes ownership: the value must be a fresh
  allocation or a moved `#[manualAlloc]` binding. The value the field held is
  released first, so reassignment never leaks.
- Freeing the holder — explicitly with `free()`, or implicitly when a
  GC-managed holder is swept — releases its owned fields recursively. Owned
  fields follow their holder everywhere, including through inheritance.
- Cycles between owned fields are rejected at run time rather than looping:
  each block is released at most once.

Storing a manual value into an ordinary managed field is still a compile
error: an owned value must stay in an owned position. Raw pointers live in
`unsafe`; there are no lifetimes and no borrow checker — the model is "one
owner, free it once".

## Collector

The v1 collector is a **non-moving, stop-the-world, mark-and-sweep GC**
with the following properties:

- Threads report live references through a **shadow stack** managed by the
  compiler: the function prologue allocates a fixed-size slot frame,
  registers it with the runtime's thread-local GC data, and codegen spills
  every live managed pointer into its slot around each GC safe point
  (allocation, function call, channel send, await).
- Allocation is a bump/small-region allocation over a free-list triaged by
  size class.
- Sweeping returns dead objects to the free list; consecutive free blocks
  coalesce.
- Weak references exist (`weak T`) but do not keep objects alive.
- Finalizers (`deinit`) run during sweep, off the allocation path, at most
  once per dead object, in unspecified order; they must not allocate. They are
  best-effort: an object that is never reclaimed (e.g. live until exit) is
  never finalized.
- A fresh allocation is pinned across the collection its own allocation may
  trigger, so an object under construction is never swept (or finalized)
  before its constructor can store it.
- The GC is **incremental-izable** later; v1 reserves a stop-the-world
  pause for simplicity and predictability.

### Diagnostics env vars

All are debug gates in the runtime (each documented in the source at its
gate); they are off by default.

- `PKL_NO_AUTO_GC=1` — disable the collector's automatic collection so the
  allocation path stops running `gcCollect`. Used to isolate GC-triggered
  bugs from allocator ones.
- `PKL_GC_TRACE=1` — verbose sweep / new-object tracing (the allocator prints
  `NEW`/`SWEEP` lines) inside the heap. Uses Win32 `GetEnvironmentVariable`.
- `PKL_PANIC_BT=1` — print a Rust `Backtrace` from the panic hook before
  exiting, so an abort inside the runtime names its own call chain.
- `PKL_ICED_DEBUG=1` / `PKL_ICED_SYMS=1` — CLI debugging of the JIT symbol
  table (iced codegen/dump hooks).

The bulk of the exploratory gates (`PKL_PIN_MAPS`, `PKL_ALL_KLED`,
`PKL_CLS_DUMP`, `PKL_SWEPT`, `PKL_LIVE_EDGES`, `PKL_CENSUS`) were removed once
their instrumentation resolved: they were chase-the-corruption probes for a
**latent use-after-free** in the selfhost build. The finding and the
v1 hardening shipped in their place are:

- The collector now **harden-copies** `deep_hash`'s user-class walk: a key
  whose block header reports `FREE_SENTINEL` or an implausible size is hashed
  by identity instead of walking raw memory (which previously dereferenced a
  misaligned `0x3d`). The stale key then makes lookups *miss* rather than
  crash the map table.
- The JIT spills a `LoadSlot` into the shadow cell before a slot is
  overwritten by a later `StoreSlot`, so a loaded managed value is never
  left living only in a register across an allocating call.

Both are root-cause fixes for the class: an object freed at a collection
while still referenced only through an untraced raw value table (the
compiler's IR constants/regs buffers), exactly the profile of the armed
forced-collect repro. The natural (auto-GC) path never reproduces it and the
armed variant is no longer part of the build.

## Safe vs unsafe

```
class Account {
    var branch: string
}

#[manualAlloc]
let account = Account("Ada")           // safe: deterministic owner

unsafe {
    let ptr: *Account = &account       // raw address of a managed object
    ptr.branch = "NYC"                 // `.` auto-dereferences a raw pointer
    println((*ptr).branch)             // explicit dereference
}
account.free()
```

`unsafe { }` is a scope: inside it the raw-pointer operators below are
allowed, and the block's value is its tail expression (like any block).

Implemented today:

- `*T` raw pointer types (`let ptr: *Account = ...`).
- `&expr` address-of, `*p` dereference.
- Field access through a pointer: `p.field` auto-dereferences (there is no
  separate `->` operator; `(*p).field` is always available).
- Assigning through a pointer to a class, struct, or pointer value:
  `*p = v` rebinds the pointer, and `p.field = v` writes the field.
- Scalar raw pointers: `*int`, `*float`, `*bool`, `*char` hold the address
  of a local, and `*p` reads / `(*p) = v` writes through them. A scalar
  pointer is carried as an unmanaged address (never GC-tracked). Address-of
  a scalar requires a local variable, so `&n` is legal but `&(1 + 2)` is not.
- Raw buffers: `alloc(T, count)` / `free(p)` over an unmanaged block and
  `p[i]` element reads/writes with stride (see "Raw buffers" below).

Line-leading `*`: the parser reads a line that starts with `*` as a
multiplication continuation, so a store through a scalar pointer on its own
line uses the parenthesized form `(*p) = v` (or `;` to end the previous
statement).

### Raw buffers (`alloc` / `free` / indexing)

```
unsafe {
    var buf: *int = alloc(int, 8)    // 8 ints, 64 raw bytes
    buf[0] = 10
    buf[1] = 20
    buf[2] = buf[0] + buf[1]
    println(buf[2])                  // element read
    free(buf)
}
```

- `alloc(T, count)` returns `*T` over a non-GC block; `free(p)` releases it.
  Both are `unsafe`-only. Element type `T` must be a scalar (`int`, `float`,
  `bool`, `char`); a buffer holding managed values is not lowered, because
  the GC never traces raw memory and would collect a stored object as a
  PickleObject.
- `p[i]` reads and `p[i] = v` writes the `i`-th element, scaled by the
  element stride: 8 bytes for `int`/`float`, 4 for `char`, 1 for `bool`.
  Compound forms (`p[i] += v`) load-modify-store.
- Blocks are uninitialized: initialize an element before reading it. Reads
  of never-written elements are whatever the allocator left in memory.
- `free(p)` is checked: freeing a pointer that was not allocated (double
  free, foreign pointer) reports `fatal: pickle: raw free of a pointer that
  was not allocated (double free?)` through the runtime panic path and exits.
- The checker does not track a raw buffer past `free`; the runtime registry
  catches double frees, but a buffer used after being freed is unchecked
  (like C).

Rules enforced by the type checker:

- `&` and `*` (and pointer field access) are only legal inside `unsafe`.
- `&` accepts class, struct, pointer, and scalar values; a scalar operand
  must be a local binding. `*p` requires `p` to be a raw pointer.
- Pointer values are non-owning: they neither keep an object alive nor free
  it. A `#[manualAlloc]` owner still performs the deterministic free.

### Managed-pointee buffers and arenas — plan

Raw buffers (`alloc`) are deliberately scalar-only today; the GC never
traces raw memory, so a `*Class` buffer would both let the collector reap
objects that are still "in" the buffer and overwrite object bytes blindly.
The compiler (lexer output, parser node pools, codegen scratch) wants two
things that are currently missing. This is the `[R]` design; neither is
lowered yet.

**Managed-pointee buffers (`alloc(T, n)` with managed `T`).** Drop the
scalar restriction by giving the raw block a *traced backing* rather than an
untracked one:

1. **Runtime primitive** `pickle_buffer_alloc(class_id, n)` allocates a
   GC-managed slab object whose slots are marked as traced fields (like a
   `PList` with a fixed capacity and element kind = the pointee class). The
   slab is an ordinary non-moving GC object, so an **interior pointer**
   (`buf` = `slab.data`) stays stable across collections — the collector
   never moves, and mark/sweep visit the slab's slots, keeping every stored
   object alive. Scalar and managed slabs are the same `pickle_raw_alloc`
   ABI entry points; only the traced flag differs.
2. **Emitter**: `alloc(Class, n)` lowers to the traced-slab allocator and an
   interior-pointer cast to `*Class`; `p[i]` uses the class's `field_size`
   stride and its field mask (slot = object ref, markable); `free(p)` unroots
   the slab (forgets it, like `#[manualAlloc]` free) instead of returning
   memory to the OS.
3. **Bounds safety**: element stores into a traced slab are still raw
   `unsafe`; bounds are the developer's contract, matching raw buffers today.
   The free-list coalescing path must refuse a block whose interior pointers
   are live — satisfied automatically because the slab is one live GC object,
   not an unmanaged extent.

**Arenas (`Arena<T>`).** Compose the traced slab into a bump region:

1. `arena Arena<T> {}` in the stdlib uses the *proposed* declaration form
   shown under "Custom allocators and arenas" below — note that no `arena`
   lexer keyword or grammar exists today, so the form is documented only,
   not yet parseable;
2. an arena is a GC-managed slab chain: `arena.alloc(v)` bump-writes `v` at
   the next slot (traced, so pointees stay alive) and grows by chaining a new
   slab; `arena.free()` drops the whole chain at once;
3. an arena provides LIFO/region lifetime as an optimization (single
   drop vs N frees, contiguous slots for the lexer's token pool); it is NOT
   required for self-hosting — `List<T>` already gives the compiler
   amortized growth, so arenas land with the stdlib `Iterable`/`Iterator`
   work, not before it.

**Sequencing for the compiler**: the self-hosted codegen benefits from
managed-pointee buffers only if the frontend wants contiguous pools; the
current plan is to develop the compiler on `List<T>` + whole-file I/O +
`#[manualAlloc]` exclusively and treat managed buffers (and arenas) as a
post-self-hosting optimization, keeping the intrinsic surface minimal until
a measured need exists.

### Immutable references (`&T` parameters)

An `&T` parameter is a read-only lens on its argument: the caller borrows the
value automatically and there is no `&` at the call site. It is not a
Rust-style borrow — there are no lifetimes, no borrow checker, and a borrow
never blocks reads, calls, or re-borrows. The only rule is that a `&T` can be
read through and never written through.

```
fn receipt(items: &List<int>, total: &int) -> int {
    var sum = total[0]          // `&int` reads the caller's local in place
    for (v in items) {          // iteration dereferences the list referent
        sum = sum + v
    }
    return sum
}

fn main() {
    var p = 1000
    var price = [3, 5]
    println(receipt(price, p))  // implicit borrow, no `&`
}
```

When `T` is smaller than a word you don't need to understand the mechanics: a
`&int` is carried as the address of the argument's storage, so `*p` and
`p[0]` alias its memory between the call's start and end. Because the borrow
cannot write, a call cannot observe its own side effect through the alias.

Semantics and rules:

- `&T` may be stored and returned like any other type once a reference *value*
  exists: `let b: &int = a` (a second `&T`), `let c: &Point = obj` (a managed
  referent shares its identity, GC keeps it alive), fields/consts of `&T`, and
  `-> &T` returns forwarding an existing `&T`. The one thing that is still
  rejected is *forming* a reference to a bare scalar in a stored or returned
  position — `let x: &int = 5`, `return 5`, `field: &int = 5`, `x = 5` all
  error with "`&T` references may only be formed from an existing `&T` value
  or a managed referent" (the borrow would dangle once the frame dies).
  Callers still pass the bare value for a `&T` parameter; the compiler borrows
  implicitly.
- Reading is fully supported through a `&T`: `*p` and `p[0]` for scalars,
  `p.field`, `p.method(...)`, `p[i]`, and `for (v in p)` — each dereferences
  the referent. Class/struct referents pass by identity; `&int`-style referents
  pass by address of a local.
- Writing is always rejected, inside or outside `unsafe`: `(*p) = v` and
  `p[i] = v` ("cannot write through an immutable reference (`&T`)"), and
  `p.field = v` ("cannot write to a field ... through an immutable reference").
- Borrowing a `#[manualAlloc]` value does not move or free it — the borrowed
  node can be read through several `&T` parameters and the owner still frees
  it exactly once (`node.free()`).
- `List<&T>` is now supported: a list of references carries reference *values*
  (addresses for scalar referents, identities for managed referents), so
  forwarding a `&T` through a list keeps working while the referent lives.
  `List<&T?>` (an option of a reference) is still rejected ("lists of `&T`
  references are not supported yet").

The syntax is reserved for these references only; the pointer operators above
are unchanged.

## Custom allocators and arenas

For engine-style code:

```
arena Arena<SceneData> {           // region allocation
    ...
}

let scratch = Arena()
let item = scratch.alloc(Vec<Collider>())
// freed as a whole when scratch goes out of scope
```

Design: `Arena<T>` is a class in the stdlib; custom *global* allocators are
registered through the runtime (`SetGlobalAllocator`) and participate in GC
as non-moving spaces in v1. Manual heap (`malloc`/`free`) is
`unsafe`-gated. (Current status: arena *syntax* is proposed as a future
stdlib keyword — no `arena` token/grammar exists in the compiler today, so
the declaration form above is documented but not yet parseable; the lowering
design — a bump region over a traced slab chain — is described in
"Managed-pointee buffers and arenas — plan" above. Arenas are a
post-self-hosting stdlib feature, not a compiler-core dependency.)

## Pointers and references — precise rules

| Syntax          | Meaning                                  |
|-----------------|------------------------------------------|
| `&expr`         | address of a class/struct/pointer value, or a scalar local |
| `*T`            | raw pointer type (unsafe only)           |
| `&T`            | immutable-reference parameter type (implicit borrow) |
| `*ptr`          | dereference (read a scalar pointee)      |
| `(*ptr) = v`    | store through a scalar/pointer pointee   |
| `ptr.field`     | field through a raw pointer (auto-deref) |
| `p[n]`          | index through pointer: element read/write with stride (unsafe) |
| `alloc(T, n)`   | raw buffer of `n` scalar `T`s (unsafe)    |
| `free(p)`       | release a raw buffer (unsafe, checked)    |

`*T` is a non-owning pointer. There is no manual free in safe code, and the
`#[manualAlloc]` owner is still what frees the object.

## GC and the C side

The runtime exports a C ABI (`extern "C"`) surface:

- `pickle_gc_alloc`, `pickle_gc_alloc_string`
- `pickle_gc_register_thread` / `pickle_gc_unregister_thread`
- `pickle_gc_collect(force: bool)`
- `pickle_allocator_set` (custom allocator)

The compiler generates code against this ABI; a future FFI layer reuses the
same surface.

## Closures

A closure value is a managed object of a synthetic per-capture-count class
(`__closure_N`, one class for every supported number of captures). Its slot 0
holds the hoisted body's code address; slots 1..N hold the captured values,
boxed like list elements and marked as traced fields. Because the captures
live in a traced object, a closure keeps them alive as long as the closure
itself is reachable, and a closure returned from a function (or stored in
another object) behaves like any other value. See `docs/02-syntax.md` for the
source-level rules and `examples/closures`.

## Object layout

```
struct PickleObjectHeader {
    next:      *mut PickleObject,  // GC free-list/live-list link
    class_id:  u32,
    flags:     u32,                // mark, etc.
    size:      u32,                // total object bytes
    _pad:      u32,
}   // 24 bytes

struct PString  { header, len: usize, bytes: [u8] }
struct PList    { header, len, cap, data: *mut void }  // element-kind tag in class table
struct PMap     { header, entries: *mut Entry, len, cap }
struct PStream  { header, inner: *mut StreamInner }    // raw Box: never traced
```

Class descriptors are emitted by the compiler into the generated object
image; each carries `field_count`, a managed-field bitmask, and a name, so
the collector marks exactly the pointer-bearing fields and no others. `stream`
is the sole builtin with a *zero* managed-field mask plus a finalizer: its slot 0
holds a raw `Box` to a `BufReader`/`BufWriter` that the collector must not
follow, so `StreamInner` is heap-managed and released by `stream_finalizer`
(descriptor flag `PICKLE_CLASS_FLAG_FINALIZER`) when a stream that was never
`close()`d is swept.

## Safety guarantees summarized

- Safe code: no use-after-free, no double-free, no null deref (options
  checked statically), no buffer overrun in bounds-checked indexing.
- Unsafe code: the developer is responsible for the same invariants the GC
  would have preserved; mismatches are caught best-effort in debug builds
  (`-D` enables poison + bounds checks everywhere).
- All heap allocation paths are GC safe points by construction.