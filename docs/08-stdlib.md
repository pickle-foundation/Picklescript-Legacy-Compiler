# PickleScript — Standard Library Plan

Namespace: everything lives under modules, mirroring the module system.

## Layout

```
import * from std.io          print, println, readLine, stderr
import * from std.fs          File, Dir, Path, readFile, writeFile, exists
import * from std.net         Tcp, Udp, Tls, Host, resolve
import * from std.http        Client, Request, Response, Server, get, post
import * from std.json        Json, parse, serialize, stringify
import * from std.collections List, Map, Set, Deque, Vec, Stack, Queue, OrderedMap, PriorityQueue
import * from std.math        sin, cos, sqrt, abs, min, max, pow, floor, ceil, random, PI, E, TAU
import * from std.crypto      sha256, md5, aes, hmac, randBytes, base64, hex
import * from std.process     run, spawn, capture, exit, env
import * from std.thread      Task, Channel, Mutex, CondVar, Semaphore, AtomicInt, Once, spawn, taskCount, currentId, sleep, yieldNow
import * from std.time        now, Instant, Duration, sleep, Timer, DateTime
import * from std.text        String buffer, Unicode helpers, scan/format, regex
import * from std.gc          collect, stats, Weak, Arena
import * from std.os          args, exit, tempDir, homeDir, cwd, platform
import * from std.test        assert, assertEqual, assertThrows, bench
```

## Design principles

1. **The stdlib is written in PickleScript** on top of a thin runtime
   layer (the `pickle_*` runtime ABI). Compiler intrinsics are only where
   the language cannot express something (allocation, atomic ops).
2. Consistent naming: verbs do (`readFile`), nouns are (`File`, `Dir`);
   no `getX()`/`setX()` — properties instead.
3. Errors are values, not exceptions: functions return `T?` or
   `Result<T, Error>`; postfix `?` unwraps an option or panics with
   context. A `try`/`catch`-with-patterns form is reserved for cross-module
   failures and marked experimental.
4. Async versions are `await`-able on the same name as sync (`http.get`
   and `await http.get` share the implementation; extra `Async` suffixes
   are banned style).
5. Collections: uniform interpolation, slicing, and iteration protocol
   (interface `Iterable<T>` -> `Iterator<T>` with `next()`).

## What ships in `std` (self-hosting-driven split)

A lot of the primitive surface already lives below `std` in the runtime
`pickle_*` ABI + compiler intrinsics (`print`/`println`/`flush`, `str()`
conversions, `min`/`max`/`clamp`/`abs`/`range`, box/unbox for `T?`, list
push/pop, map has/keys/values, string cmp/len, `pickle_runtime_reset`).
`std` itself is the **PickleScript-written layer on top of that ABI** — the
modules the self-hosted compiler will actually import.

**The bytes↔string bridge is an intrinsic now:** `bytes(s: string) -> List<byte>`
returns a byte-exact copy of the string's UTF-8 bytes (with NUL bytes
preserved), and `str(xs: List<byte>) -> string` is the exact inverse — the
builtin `str()`, already used for scalar conversion, additionally accepts a
`List<byte>` and reassembles a string from those bytes. `s[i]` and
`for (b in s)` already read the same bytes as `bytes(s)[i]` (both typed
`byte` per the model in `04-types.md`). These compose the `std.text` bridge:
```
let bs = bytes("café")   // List<byte>: c a f é(2 bytes)
let back = str(bs)       // "café"
```

**The `Iterable`/`Iterator` protocol surface is language-level** (landed
2026-09-21): every module sees prelude interfaces
`Iterable<T> { fn iterator() -> Iterator<T> }` and
`Iterator<T> { fn next() -> T? }`. Any class that declares
`implements Iterable<T>` is iterable by `for (x in it)`, which lowers to
`it.iterator()` then `next()`-until-`none` (see `03-classes.md` and
`06-compiler.md`). Builtin sequence types (`List`, `Map`, `Range`, `string`)
keep their direct lowering and do not implement the interfaces in this slice;
`std.collections` will provide concrete collection types on top of the
programmable protocol. A user declaration of `Iterable`/`Iterator` shadows the
prelude.

**Must-have (blocks rewriting the compiler in Pickle):**
- `std.text` — mutable string/byte builder, UTF-8 helpers, `format`, and a
  bytes↔string bridge. A compiler reads source text and emits source text.
- `std.collections` — `List`/`Map` (largely present) plus `remove`/`insert`,
  a hash `Set`, and string interning for symbol tables.
- `std.fs` — read/write/exists plus directory listing. The whole-file
  intrinsics (`read_file`/`write_file`/`file_exists`/`delete`/`mkdir`/
  `list_dir`) and the buffered `Stream` layer ship as flat intrinsics;
- `std.os` — `args`, `exit`, env for the `pickle` driver.
- `std.math` — `pow`/`floor`/`ceil`/`sin`/`cos`/`sqrt`, `PI`/`E`/`TAU`.
- `std.test` — the `expect`/hooks/`bench` layer already steer the `.pkl`
  suites; `std.test` gave them a home.
- `std.time` — `now`/`Instant`/`Duration` for `analyzing N files`-style
  diagnostics.

**Coverage (the real-workload clause of 01-philosophy; often 1.x):**
- `std.json` (parse/serialize — first-class for tooling / a language server).
- `std.net` + `std.http` — typed client/server, streaming bodies.
- `std.process` / `std.thread` — run/spawn/capture, channels, mutexes.
- `std.crypto` — pure-Rust, no OpenSSL (SHA/HMAC/AES/ChaCha20 + base64/hex).
- `std.gc` — stats/collect; arenas are the optional self-host memory answer.

**Deliberately not in `std`:** reflexive sugar owned by the intrinsic/runtime
layer (`print` family), and compiler-internal machinery that belongs in the
compiler itself rather than the standard library.

## Collections quick view

- `List<T>` — resizeable array, indexing, `add/remove/insert`, slicing.
- `Map<K, V>` — hash map, insertion ordered, `[]` access, `has`,
  `keys()`, enumerate `for ((k, v) in map)`.
- `Set<T>` — hash set.
- `Deque<T>` — double-ended queue.
- `Vec<T>` — SIMD-friendly fixed-capacity buffer (math-heavy code).

Literals:
```
let xs = [1, 2, 3]
let m  = { "a": 1, "b": 2 }
let s  = {1, 2, 3}       // Set when element type is monomorphic
let bs = [byte] { 1, 2 }
```

Compiled list operations today: array literals `[a, b, c]`, `xs[i]` reads
and assignments (including `+=`-style compounds), `len(xs)`, `xs.push(v)` /
`xs.pop()`, `xs.remove(i)` (remove by index, returns the element),
`xs.insert(i, v)`, and `xs.sort()` (in-place ascending for
`List<int/float/byte/char/bool/string>` — element comparison happens on the
boxed values), `for (x in xs)` iteration, and `print`/`println` on a list
(prints `List(len=N)`). Scalar elements are boxed at the runtime boundary;
strings/lists and other managed values pass through as pointers. Lists of
`char` and string byte-indexing/iteration are lowered: `List<char>` goes
through the scalar-list path (see the `char` bullet in `06-compiler.md`), and
`s[i]` / `for (c in s)` are byte-addressed reads typed `byte` per the locked
string model (`04-types.md`). `print(c)` on a `char` writes the scalar as
UTF-8 and `print(b)` on a `byte` writes the raw byte, so non-ASCII text
round-trips through the copy loop `for (c in s) { out += "{c}" }`.

**String buffer building is compiled.** `s[i] = v` writes a byte of a string
(copy-on-write: the value is replaced, other aliases keep the old bytes) where
`v` is a `byte`-typed value, an `int` literal in `0..=255`, or an ASCII `char`
literal — plus `+=`/`-=`-style compounds over the current byte (`s[i] += 1`
lower-cases/shifts). Targets are variables and field slots: locals,
`this.x`, bare instance fields (`text[i] = ...` inside a method), bare static
fields (`sbuf[i] = ...`), and `obj.field[i] = ...`. Together with `s[i]`
reads, a compiler can build a string byte-by-byte without a `List<byte>`
staging buffer. Not lowered yet: writes through `&T` parameters (still
rejected), and nested targets like `m[k][j] = ...` (strings are
copy-on-write); `Type.staticField[i] = ...` stores through the type name are
also out.

Compiled map operations today: `{ "a": 1 }` literals, `m[k]` reads and
assignments (including `+=`-style compounds), `m.has(k)`, `m.get(k) -> V?`
and `m.remove(k) -> V?` (both return the value as an option — `none` when the
key is absent — so `?` / `?.` / `??` / `if (let some(v) = ...)` compose
directly), and `m.keys()` / `m.values()` (fresh `List`s). Keys are lowerable
as `string`, a boxed scalar (`int`/`float`/`bool`/`char`/`byte`), or a
composite object (list, map, enum, class/struct instance); the runtime hashes
and compares them structurally with a cycle guard (see `06-compiler.md`).
**Entries iteration is compiled**:
`for ((k, v) in m)` binds the key and the value per
trip (lowered as lockstep key/value snapshots — both walk the same entry
array), and `for (v in m)` still iterates just the values.

## I/O model

A `Stream` is an open, buffered I/O handle backed by a `BufReader`/`BufWriter`
around a file (or the process's stdout/stderr). Constructors are flat builtins:

- `stream_open_read(path) -> Stream?` — opens `path` for reading; `none` when
  it cannot be opened.
- `stream_open_write(path) -> Stream?` — opens `path` for writing, creating
  and truncating it; `none` when it cannot be opened.
- `stream_open_append(path) -> Stream?` — opens `path` for appending, creating
  it if absent; `none` when it cannot be opened.
- `stdout_stream() -> Stream` / `stderr_stream() -> Stream` — write-only
  bindings to the console. `stdout_stream().write(...)` routes through the same
  console bridge as `print`, so test captures see it.

A stream is used through its methods (dispatched like the `List`/`Map`
builtin methods):

- `s.read(n: int) -> List<byte>?` — reads up to `n` bytes: a short, non-empty
  list at a file's tail; `none` at end of stream, on a closed/non-readable
  stream, or when the underlying read fails. Errors are values.
- `s.write(bytes: List<byte>) -> int` — writes the byte list, returning how
  many bytes were written (`0` on a closed/non-writable stream, less than the
  list length if the write fails partway).
- `s.flush() -> bool` — pushes buffered output to the host (`true` for a
  reader, which buffers nothing; `false` once closed).
- `s.close() -> bool` — flushes and releases the handle. Idempotent: the first
  close reports `true`, a second one `false`, and every later `read`/`write`/
  `flush` is a null/no-op.

Bytes round-trip through the `bytes(...)`/`str(...)` bridges, exactly like the
whole-file layer: `s.write(bytes(read_file(p)))` and
`str(s.read(n) ?? bytes(""))`.

The whole-file layer of `std.fs` ships as intrinsics on the string
(opaque-byte) ABI, the first runtime piece the self-hosted compiler needs to
load source:

- `read_file(path) -> string?` — reads a whole file byte-exact as `string`,
  or `none` when the file cannot be read.
- `write_file(path, text) -> bool` — overwrites `path` with `text`'s bytes.
- `file_exists(path) -> bool` — metadata probe (works for directories too).
- `delete(path) -> bool` — removes a file or empty directory; `false` when it
  is missing or cannot be removed.
- `mkdir(path) -> bool` — creates a directory (parents are not implied);
  `false` when it already exists or cannot be created.
- `list_dir(path) -> List<string>?` — child paths (files and subdirectories,
  full paths joined with the host separator) in host order, or `none` when
  the path is not a readable directory.

Round-trips are byte-exact (no UTF-8 re-validation). `List<byte>` reads
compose from the `bytes(...)` bridge, so no dedicated byte-list I/O is needed:
`bytes(read_file(p))` materializes a file's bytes for in-place processing and
`write_file(p, str(xs))` stores them back.

## Process

The `std.os` process surface ships as intrinsics:

- `args() -> List<string>` — the program's command-line arguments as a fresh
  `List<string>` on every call, **excluding the program name** (`argv[1..]`).
  Under `pickle run`/`pickle irx-run`, trailing `--` arguments are forwarded
  verbatim (`pickle run prog.pkl -- a b`); an AOT-built executable sees the
  real process `argv` (a single leading `--` separator is stripped, matching
  the `run`/`irx-run` convention). `pickle test` runs start fresh, so `args()`
  is empty in the test runner. Every call materializes a new list — mutating one
  snapshot never affects another.
- `exit(code: int) -> void` — flushes buffered output and terminates the
  process with `code` (clamped to the host's exit-code range; guaranteed not
  to return).

`args()`/`exit()` in a Pickle source are what let the self-hosted compiler's
driver accept a target-path argument: `pickle run pickle/cli/main.pkl --
target.pkl` forwards the target as `args()[0]` (port gap 3, resolved and wired
2026-09-24). The ported drivers prefer `args()` targets and fall back to their
previous file-derived target lists when no trailing `--` args are given.

## Networking & HTTP

`std.net` wraps OS sockets with TCP/UDP + TLS (rustls via runtime crate, or
no external C libs). `std.http` is a typed client/server on top: requests,
responses, streaming bodies, URL params, multipart.

## Crypto & math

Pure-Rust implementations, no OpenSSL dependency: SHA-2/3, HMAC, AES-128/256
(modes GCM/CTR/CBC), ChaCha20-Poly1305, curve arithmetic for signatures
later. Math: transcendental + vector/repr helpers.

## Testing

`std.test` (landed in `stdlib/test.pkl`, workspace crate `std`) is the
group-aware runner behind `describe`/hooks, now written in PickleScript and
byte-identical to the original Rust runner (`runtime/src/test.rs`). It exports
`TestCase` (a test `name` with a `body: fn () -> string?` that returns the
failure detail or none), `Hook` (`group`, `kind`, `body: fn ()`), the hook
constants `HOOK_BEFORE_ALL`/`BEFORE_EACH`/`AFTER_EACH`/`AFTER_ALL`, the name
helpers `splitTestName`, `groupPrefixes`, `lastSeg`, and
`runTests(tests, hooks) -> int` (0 = all pass, 1 = any failed). `group > desc`
test names become `describe` headers; a `bool`-returning body is not a thing —
return the failure `string?` or none.

In a module:

```
test fn name() { ... }
```
or `test("desc", ...)`/`it` suites; `describe` groups nest, and
`beforeAll`/`beforeEach`/`afterEach`/`afterAll` hooks run per group (each
group may declare its own; beforeAll root-first, afterAll leaf-first). Runs
print a `describe <name>` header per opened group and `test <desc> ... ok|FAILED`
lines per test, then a `test result: N passed; M failed` summary; test names are
the desugared `group > group > desc` path so `--filter` matches describe and
test names alike. `expect(value).matcher(...)` gives readable failure output; the
matchers are `.toBe` (deep/structural equality on managed values), `.toEqual`
(same as `toBe`), `.toBeTruthy`, `.toBeFalsy`, `.toBeNull`, `.toExist`,
`.toHaveLength(n)` (string, list, or map), `.toContain(x)` (string substring
or list element), `.toBeGreaterThan(x)`, `.toBeLessThan(x)`, and any of them
can be negated with a leading `.not()`. Assertions:
`assert(cond, msg?)`. `pickle test` discovers `*_test.pkl`/`*.test.pkl` files
and runs them, reporting per-module, per-group results. `#[tag("name")]` on a
test item attaches tags (repeatable) and `pickle test --tag name` runs only the
tests carrying one.

## Release plan

`std.io`, `std.collections`, `std.text`, `std.math`, `std.time`, `std.json`
ship with 1.0; networking, crypto, HTTP, processes, threads in the 1.x line
as they stabilize. Each module is exercised by the compiler's own test suite
before it is declared stable.