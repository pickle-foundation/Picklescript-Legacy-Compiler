# PickleScript

**Familiar syntax. Garbage collected by default. Compiled ahead of time.**

PickleScript is a standalone, native-compiled, general-purpose programming
language. It targets the same problems people currently reach for C#, Go, or
Java with — backend services, desktop apps, CLI tools, and libraries — and it
keeps a path into systems programming when you need it.

## Quick tour

```
module hello

fn main() {
    let recipient = "world"
    print("Hello, {recipient}!")
}
```

## Build from source

```
cargo build --release
```

This produces `pickle` (the CLI) and `pickle_runtime` (the runtime static
library used to link executables).

## Use

```
pickle new myapp
cd myapp
pickle run
pickle build           # -> native executable
pickle test
pickle fmt
pickle repl
```

The `build` and `run` subcommands work today on a single source file; `test`
uses a whole directory:

```
pickle run  examples/hello-world/src/main.pkl    # JIT-compile and run
pickle build examples/hello-world/src/main.pkl    # -> main.exe next to the source
pickle test tests/ # or `src/`                            # run every `*_test.pkl`/`*.test.pkl`
```

`pickle build` lowers the module to machine code (Cranelift), emits a
relocatable COFF/ELF object, links it against the runtime, and writes a
native executable with no manual linking steps. Use `-o <path>` to choose
the output.

## Project layout

```
project/
    pickle.toml        # project manifest
    src/
        main.pkl       # entry point (fn main)
        util.pkl       # helper module
```

`pickle build` discovers `src/*.pkl`, resolves `module`/`import` edges,
compiles them to machine code, links against the runtime, and produces an
executable. No headers. No makefiles. No CMake.

## Language

- C-family syntax, braces, parentheses around conditions, no semicolons.
- `let` / `var` / `const` bindings.
- Classes, structs, enums, interfaces, generics, single inheritance.
- Garbage collected by default; `unsafe` gives pointers and custom
  allocators.
- Pattern matching, optionals (`T?`, `none`), string interpolation,
  closures, `async`/`await`, tasks, channels.
- Integrated toolchain: build / run / fmt / test / package / doc / repl.

## Design & docs

See `docs/`:

1. [Philosophy](docs/01-philosophy.md)
2. [Syntax](docs/02-syntax.md)
3. [Class system](docs/03-classes.md)
4. [Type system](docs/04-types.md)
5. [Memory model](docs/05-memory.md)
6. [Compiler architecture](docs/06-compiler.md)
7. [Runtime architecture](docs/07-runtime.md)
8. [Standard library plan](docs/08-stdlib.md)
9. [Advanced error reporting](docs/11-advanced-error-reporting.md)
10. [Compiler history](docs/12-compiler-history.md)
11. [Error codes](docs/error-codes.md)

## Repository layout

```
compiler/          # pickle-compiler: lexer, parser, type checker, IR, codegen, formatter
runtime/           # pickle-runtime: GC, object model, builtins, entry point
cli/               # pickle-cli: the `pickle` tool
stdlib/            # std.* source libraries (PickleScript)
docs/              # language & compiler design
rfcs/              # language design proposals
examples/          # example projects
tests/             # cross-crate test suites
benchmarks/        # compiler/runtime benchmarks
tools/             # build & release tooling
scripts/           # CI / dev scripts
```

This project is a single Rust workspace. `pickle build` runs the whole chain
from `src/*.pkl` to a native executable: it discovers files, resolves
`module`/`import` edges, compiles them to machine code, links the runtime,
and produces a binary — no headers, no makefiles, no semicolons.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).