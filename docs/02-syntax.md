# PickleScript — Syntax Specification

Version 1. Files use the `.pkl` extension.

## Lexical conventions

- Identifiers: `[A-Za-z_][A-Za-z0-9_]*`. Type names are conventionally
  `UpperCamelCase`; the compiler warns on style violations but doesn't
  require them.
- Keywords are lowercase. A small set is *reserved* (cannot appear as an
  identifier); a larger set is *contextual* — reserved only at its syntax
  position and an ordinary identifier everywhere else (see
  [Keyword model](#keyword-model)).
- Comments: `// line`, `/// doc line`, `/* block */`, `/** doc block */`.
  Doc comments attach to the following declaration.
- Numeric literals:
  - `42`, `1_000_000`, `0xFF`, `0b1011`, `0o17`
  - `0.5`, `3.14e2`, suffix types: `7u8`, `-2i32`, `2.5f32` (default `int`,
    `float`)
- String literals:
  - `"hello"` — interpolated; `{expr}` embeds `expr.toString()`.
    `"Total: {total}"`. Escapes: `\n \t \r \\ \" \{ \} \0 \u{1F600}`.
  - `r"raw text {not inserted}"` — no interpolation, no escapes.
  - `"""multi\nline\nstring"""` — multiline string, interpolation allowed;
    interiors may span physical lines and end only at the next `"""`.
    A literal `{` is written `\{` (as in an interpolated string).
- Character literal: `'a'` — a Unicode scalar, type `char`. Strings are
  UTF-8 bytes, not `char` arrays (see the locked string/char model in
  `04-types.md`). In a `byte` comparison, an ASCII `char` literal (like
  `s[i] == 'a'`) is accepted and lowered as its byte value; a non-ASCII
  literal such as `'é'` has no byte value and is a compile error there.

## Statements

No semicolons. A statement ends at a newline or a closing brace, unless the
line is *visibly incomplete* (ends with an operator, a `,`, or an open
delimiter `( [ {`, or continues with `.` / `?` / `->`), in which case the
expression continues onto the next line.

```
let x = f(          // incomplete: open paren
    1, 2,
)
let y = 1 +
        2           // incomplete: trailing operator
let z = 3           // complete
```

### Declarations

```
let name = expr            // immutable binding
let name: Type = expr      // annotated
var count = 0              // mutable
const MAX = 100            // compile-time constant
const NAME: string = "pkl"
```

`let` bindings may be pattern based: `let (a, b) = pair`.

### Control flow

```
if (cond) { } else if (cond) { } else { }

if (let user = findById(7)) { }   // conditional binding (none => else)

while (cond) { }

for (let i = 0; i < n; i++) { }

for (item in items) { }           // iteration over any Iterable

for (i in 0..10) { }              // half-open range
for (i in 0..=10) { }             // inclusive range

break
continue
return [expr] | return          // bare return == return none, if option
```

> **Implemented subset (current compiler).** `if (let pattern = value)` lower as a
> conditional binding: `some(v)`/`none` against an option value gate the
> then-block, and the binding is visible only inside it. A plain `let x = value`
> binding is unconditional. `match` lowers over enums (variant patterns) **and**
> non-enums: literal patterns against `int`/`float`/`byte`/`char`/`bool`/`string`
> scrutinees (string patterns must be non-interpolated), plus `some(v)`/`none`
> against options; arms may carry `if` guards that fall through on `false`. A
> catch-all is written `case _ -> body`, `else -> body`, or the historical
> bare `case -> body`; a non-exhaustive chain faults with "pickle: match is
> not exhaustive" at runtime. `match` also lowers `a | b` Or patterns
> (literals, `Dir.North` enum-variant cases, `some(v)`), tuple patterns
> (`case (a, b)`), nested payloads (`some((a, b))`), and composite
> (list/map/class/struct/enum/option/tuple) scrutinees. What still bails
> "not lowered yet": `let (a, b) = ...` / `if (let (a, b) = ...)` (and
> `if (let <literal> = ...)`) top-level destructuring, empty tuple literals,
> and nested enum-payload bindings (`case Tag(some(x))`).

### Expression statements

```
account.deposit(20)
print("done")
```

### Declarations & expressions together

```
// Single-expression bodies keep small functions compact.
fn double(x: int) -> int => x * 2
```

## Functions

```
fn greet(name: string, punctuation: string = ".") -> string {
    return "Hello, {name}{punctuation}"
}

fn add(a: int, b: int) -> int { a + b }    // tail expression = value

fn describe() -> (string, int) {           // tuple return
    return ("pickle", 7)
}

fn join(...items: string) -> string { }    // rest parameter

fn apply(fn f: (int) -> int, n: int) -> int { f(n) }
```

- Calls: positional and named arguments:
  `greet("Sam")`, `greet(name: "Sam")`, `moveTo(x: 10, y: 20)`.
- Overloads resolved by argument types (arity + types). Default arguments
  fill the tail.
- Lambda (expression body): `(x) => x * 2`, `(x, y) => x + y`.
- Lambda (block body, explicit return type): `(x: int) -> int { x * x }`.
- Lambda parameters can be annotated individually: `(x: int) => x`. Type
  annotations are required when the checker has no other context to infer
  from; an untyped `(x) => x` with no expected type is an error.
- Closures capture their free variables by value. A capturing lambda is
  lowered to a hoisted body of the form `fn (env: ptr, args...)` plus an
  object holding the capture; a module-level function used as a value
  (`let by = add`) gets a generated forwarder so the same calling convention
  applies in both cases. See `examples/closures`.
- A generic function used as a value needs its type arguments pinned. Give
  them explicitly (`let idI = id<int>`) or pass the bare name to a parameter
  whose expected type is `fn` and pins them (`apply(id, 4)` when
  `apply(f: fn (int) -> int, ...)`); the concrete instantiation is shared with
  direct calls. A bare `let idU = id` with nothing to pin `T` is an error —
  the generic signature leaks an unresolved type variable into any use.
- Calling through `fn`-typed values (lambda variables, function parameters,
  call results) is a dynamic call; it works under both `pickle run` (JIT) and
  `pickle build` (AOT).

## Operators

Binary operators, loosest to tightest:

| Precedence | Operators                    |
|-----------|------------------------------|
| low       | `=` `+=` `-=` `*=` `/=` `%=` `<<=` `>>=` `&=` `|=` `^=` |
|           | `..` `..=` `<-`             |
|           | `||` `??`                   |
|           | `&&`                        |
|           | `|`                         |
|           | `^`                         |
|           | `&`                         |
|           | `==` `!=` `<` `<=` `>` `>=` `is` `in` |
|           | `<<` `>>`                   |
|           | `+` `-`                     |
|           | `*` `/` `%`                 |
|           | `**`                        |
| high      | `!` `-` (unary) `~`         |
|           | `.` `?.` `?` `(` `[`        |

- `==` is structural for structs and enums, identity for classes unless
  overridden (`operator == (other) -> bool`).
- Comparison operators (`==` `!=` `<` `<=` `>` `>=`) are checked against the
  operand types: numbers (mixed `int`/`float`), `byte` (treated as an `int`
  at the ABI), and `char` pairs support every
  operator; `==`/`!=` additionally accept identical types (string, bool, enum,
  struct, class, option, list, fn). `byte` compares with `int`/`float` and
  with ASCII `char` literals (never with a non-ASCII literal or a `char`
  variable). Ordering strings/bools or comparing
  mismatched types (e.g. `bool > int` from a chained `x < 3 > x / 2`) is a
  checker error.
- `is` type test; `in` membership / map key test.
- Options: `x?` force-unwraps a `T?` and panics on `none`; `x?.name` is safe
  navigation (short-circuits to `none`); `a ?? b` is `a` if present, else `b`.
  The unary bits: `!` not, `-` negate, `~` bitwise not.
- `&expr` address-of (inside `unsafe`), `*T` raw pointer / `&T` immutable
  reference param types, `p.field` field access through a pointer
  (auto-dereferences; there is no `->` operator), `<-` channel send:
  `ch <- value`.
- There is no `::`, `?:`, or `:=` operator, and no word forms `and`/`or` —
  write `&&` and `||`.

## Modules

A file may declare its module identity:

```
module app.util              // path. Identifies the file.
```

Modules are imported positionally (the path must match the `module` declared by
the target file):

```
import text                            // qualified: text.render(...)
import text as t                       // aliased: t.render(...)
import render from text                // unqualified: render(...)
import render from text as r           // unqualified under a new name: r(...)
import { render, helper as shout } from text   // brace items
import * from text                     // unqualified: everything public
public import text                     // re-export: consumers get text.*
```

`import <path>` binds a **module name** (its last segment unless aliased) and
lets you reference exports qualified (`text.render`, `t.render`), classes
(`text.Person(...)`), enums (`text.Color.Red`), and their variants. The forms
with `from` bring the named **symbols** (`import render from text`, brace
lists, `import * from text`) into the file's bare namespace; `... as r`
rebinds one under a new local name. `public import` re-exports (namespace or
flattened items).

Import paths resolve relative to the **importing file's directory**, then each
ancestor up to and including the process working directory, then `<cwd>/src/`;
`a.b` maps to `a/b.pkl` first, else the **package** directory `a/b/` (a
package = the union of its member `.pkl` files). A workspace `picklescript.toml`
`[workspace] members` entry makes a leading crate name authoritative: the crate
root maps to `lib.pkl`/`main.pkl` and deeper segments to module files or package
dirs inside the crate's own directory. A `module` declaration that differs from
the path used to load the file is an error (E0208). Declaring `module` without
importing is fine: the file is an ordinary single-source module.

All reachable modules are flattened into one program before type checking, so
within a loaded program unique names are visible across module boundaries.

### Colliding exports

When two loaded modules export the same top-level **function, test, or const**,
each reference in a file that imports both must say which one is meant:

- qualified reference: `math.ops.helper()` — unambiguous when both module names
  are in scope (`import math.ops; import text.format`) or via a module alias;
- alias: `import helper from math.ops as mathHelper` — binds the bare name to
  that one provider;
- otherwise a bare `helper()` is rejected:

```
error[E0209] ambiguous imported name `helper`
  = note: `helper` is provided by module `math.ops` and module `text.format`
  = note: import helper from `math.ops` as helperA / import helper from
          `text.format` as helperB
```

Same-named **types** (classes, structs, enums, interfaces) across modules remain
a hard duplicate-declaration error (E0201): a type's name must be unique in the
merged program. Module path segments are identifiers, so reserved words such as
`case` cannot appear in a path (`import case from text` is a syntax error).

## Keyword model

Two categories. Both are lowercase; neither contains `spawn` (a spelling
reserved for future channels, not a token today).

**Reserved keywords** — bind everywhere and may never be used as identifiers:
`module import from fn let var const class struct enum interface if else while
for in break continue return match case true false none is as this super`. Also
`await` and `unsafe` stay reserved at expression positions (`await x`,
`unsafe { }`), so they cannot name an identifier there.

**Contextual keywords** — bind only at their grammar position and are ordinary
identifiers everywhere else:

```
get set init deinit
constructor operator property
static override public private protected
extends implements
async task channel
```

`from` is reserved exclusively for the import grammar (`import x from mod`).
`use` is **no longer a keyword**: the old `use a.b.item` import syntax was
replaced by `import item from a.b`, and `use` now lexes and binds as an
ordinary identifier.

Examples of valid uses as identifiers:

```
let use = 1                     // local named "use"
let task = 2                    // local named "task"
class User { init: int }        // field named "init"
fn channel(v: int) -> int => v  // method named "channel"
```

The same words still bind at their declaration slots: `get`/`set` inside a
`property`, `init { }` / `deinit { }` bodies, an `operator` member, a
`constructor`, an access modifier, `class C extends B`, `class C implements I`,
and `async task = ...`.
`await x` lives at the prefix-operator position, reserved there.

`task { ... }` is sugar for `thread_spawn(fn() { ... })`: the block becomes a
zero-argument closure that runs on a new OS thread, and the checker enforces
that the closure body returns nothing (an int-producing body must end with a
voiding form such as `let _ = expr`). `thread_join(task)` blocks until the
task finishes; `channel_recv` on a channel shared into a task is the typical
blocking rendezvous. A bare `task` not followed by `{` is still an ordinary
identifier.

Field names in a class member slot accept the contextual set except
`constructor`, `operator`, `property`, `static`, `override`, `public`,
`private`, `protected` — those words bind at the member slot (a field can
still be named `get`, `set`, `init`, ...).

### Match catch-alls

A fall-through match arm may be written three ways, meaning the same thing:

```
match (kind) {
    case Kind.Savings -> print("savings")
    case _ -> print("checking")      // wildcard pattern
}
// or
    else -> print("checking")        // worded catch-all
// or (historical)
    case -> print("checking")        // bare-arrow catch-all
```

## Example walking

```
module app

import text

class Account {

    name: string
    balance: int = 0
    private pin: int = 8962

    constructor(name: string) {
        this.name = name
    }

    fn deposit(amount: int) {
        balance += amount
    }

    fn enough(amount: int) -> bool => balance >= amount

    property label: string {
        get => "{name}: {balance}"
    }
}

enum Kind { Checking Savings }

fn main() {
    let a = Account("Ada")
    a.deposit(20)
    print("Balance: {a.balance}")
    var kind = Kind.Checking
    match (kind) {
        case Kind.Savings -> print("savings")
        case -> print("checking")
    }
}
```