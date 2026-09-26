# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/)
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Compound assignment `op=` (`+=`, `-=`, `*=`, `/=`) in all 7 assignment target shapes (locals, fields, statics, map entries, string index, `&T` params, raw pointers) + null-safe string concat.
- Refutable-ish `let`/`var` tuple destructuring (`let (a, b) = t`), `if (let <literal> = v)` / `if (let (a, b) = t)`, and `for ((a, b) in xs)` tuple patterns (irrefutable forms; refutable `let some(v)` still bails E0900).
- `Option`/`Tuple` map keys (`Map<int?, V>` / `Map<(A,B), V>`) — `none` sentinel, deep-hash/eq arms, `packMapKey` boxing, with the port emitter/checker mirrored.
- Spread list arguments (`f(1, ...xs)`), `Range`/`RangeIncl` binary lowering, and stored-range `for (x in r)` iteration — with the port emitter's `Ty.Range` for-in arm matching the Rust lowering byte-for-byte.
- Fixture `tests/pickle/spread_main.pkl` exercising spread-call arity and stored-range loops.
- `tools/battery_full.ps1`: 11-gate port-parity battery with `PKL_NO_AUTO_GC=1`-baked oracle artifacts (GATE11 = stdlib byte-parity across recursive `stdlib/**/*_test.pkl`, 14 files) and `history_test` joining the compiler-selfhost unit suite (GATE7 15/15).
- Byte-exact slicing fix for the ported frontend (`subStr`/`stripModuleLine`): per-byte `"{b as char}"` re-encoded every ≥0x80 byte as 2-byte UTF-8; fixed with `str([b as byte])` raw-byte appends so `countChars("a¢€😀")` counts 4 scalars, not 10.

### Added
- Workspace skeleton: `compiler`, `runtime`, `cli` crates.
- Front end: lexer, parser, AST, resolver, type checker.
- `pickle check` and `pickle ast` commands.
- Runtime object model (managed object headers, class descriptor tables).
- Project restructured for open-source release under Apache-2.0.