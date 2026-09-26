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

### `compiler-selfhost-x64-seedparams-rcx-clobber.patch`

Fixes corrupted arguments at **param slot 0** of selfhost-compiled functions
(e.g. a `string` param printing as `List(len=...)` / `Map(len=...)` / `true`).

Cause: `X64Emitter.prologue()` built the shadow-frame pointer with
`lea rcx, [rbp-N]`, but `rcx` is also `intArgReg(0)` — the first incoming
argument. `emitFunc()` ran `prologue()` and *then* `seedParams()`, so
`seedParams` spilled the already-clobbered `rcx` (a frame pointer) into slot 0.
Slot 1 (`rdx`) and later were unaffected, which is what made the bug look like
a string/arg-marshalling or cell-numbering problem.

Fix: move the `lea rcx` + `pickle_shadow_push` out of `prologue()` into a new
`pushShadow()` and call it after `seedParams()`, so incoming args are spilled
into their cells before `rcx` is reused. `popShadow()` also uses `leaRbp(1)` but
runs at function exit once params are dead, so it is safe as-is.

Regression covered by `compiler\tests\fixtures\modules\main.pkl` (passes under
both `tools\selfhost_probe.ps1` default and `-SelfhostLink`).
