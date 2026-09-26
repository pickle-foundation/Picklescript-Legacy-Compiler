# Local-only patch overrides

`/compiler-selfhost/`, `/cli-selfhost/` and `/runtime-selfhost/` are gitignored
(see `.gitignore`, "self-hosting port sources (local-only until the switchover;
never pushed)"). Fixes to those sources therefore never show up in `git status`
and are lost if the directory is regenerated.

This directory preserves such fixes as unified diffs against their repo-relative
paths, so they can be reapplied after a regen.

## Applying

From the repo root:

```powershell
git apply tools\patches\<patch>.patch
```

Verify with `--check` first, or dry-run without touching the tree:

```powershell
git apply --check tools\patches\<patch>.patch
```

## Patches

### `compiler-selfhost-x64-shadow-frame-overlap.patch`

Two bugs where the shadow frame overlaps other frame data in
`compiler-selfhost\x64.pkl`. Both corrupt values only for *some* functions, which
is why the failures looked unrelated.

**1. `rcx` clobbered before param spill.** Corrupted arguments at **param slot 0**
(e.g. a `string` param printing as `List(len=...)` / `Map(len=...)` / `true`).

`X64Emitter.prologue()` built the shadow-frame pointer with `lea rcx, [rbp-N]`,
but `rcx` is also `intArgReg(0)` — the first incoming argument. `emitFunc()` ran
`prologue()` and *then* `seedParams()`, so `seedParams` spilled the already-clobbered
`rcx` (a frame pointer) into slot 0. Slot 1 (`rdx`) and later were unaffected,
which is what made the bug look like a string/arg-marshalling or cell-numbering
problem.

Fix: move the `lea rcx` + `pickle_shadow_push` out of `prologue()` into a new
`pushShadow()` and call it after `seedParams()`, so incoming args are spilled
into their cells before `rcx` is reused. `popShadow()` also uses `leaRbp(1)` but
runs at function exit once params are dead, so it is safe as-is.

**2. Scalar region started on the frame's `prev` field.** `analyze()` allocated
the unmanaged slot/temp region with `var used = shadowBytes`, so the first scalar
could land exactly on K == `shadowBytes` — which is the shadow frame's `prev`
pointer. `pickle_shadow_push` then overwrote it, corrupting a *scalar* parameter.

Because the region advances with `alignK(used, tyWidth(t))` and
`tyWidth(Int) == 64`, this triggers exactly when `shadowBytes % 64 == 0`
(equivalently `ncells ≡ 6 (mod 8)`), so it appeared in a scattered subset of
functions. A function with a `string` param had `shadowBytes == 32` and escaped;
`fn band(n: int) -> string` had 6 cells (`shadowBytes == 64`) and its `n` was
destroyed, so every guard case fell through to the last one. Params that were
seeded but never read also masked it, which is why several string-argument
probes still passed.

Fix: start the scalar region strictly past the frame, `var used = shadowBytes + CELL_BYTES`,
so scalars are always below the frame base.

Regression covered by `compiler\tests\fixtures\modules\main.pkl` (passes under both
`tools\selfhost_probe.ps1` default and `-SelfhostLink`) and by the `tests\pickle`
corpus (`implements_main.pkl`, `iterable_main.pkl`, `match_non_enum_main.pkl`).
