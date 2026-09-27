# Regenerate every artifact that tools/battery_full.ps1 compares against.
#
# tools/diff/ is gitignored, so a clean checkout has none of it. The battery compares
# port output against pre-existing Rust oracles; with no oracles on disk GATE1/2/3/4/5
# have nothing to compare and score 0 without saying why. This script is the missing
# half of the battery: it rebuilds those inputs from the Rust compiler on every run, so
# the gate is reproducible from a clean tree and no oracle can silently go stale.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\battery_prep.ps1

$ErrorActionPreference = "Stop"
$cli = (Resolve-Path "target\debug\pickle-cli.exe").Path
$repoAbs = (Resolve-Path ".").Path

foreach ($d in @("tools\diff\oracle", "tools\diff\port", "tools\diff\gen2")) {
    New-Item -ItemType Directory -Path $d -Force | Out-Null
}

# Native stdout must be captured with cmd redirection: PowerShell's `>` and Out-File
# re-encode native output, and the battery byte-compares these files.
function Invoke-Raw([string]$outPath, [string]$exe, [string[]]$argv) {
    $parts = @("`"$exe`"") + ($argv | ForEach-Object { "`"$_`"" })
    cmd /c (($parts -join " ") + " > `"$outPath`" 2>nul")
    if ($LASTEXITCODE -ne 0) { throw "PREP FAILED: $exe $($argv -join ' ') (exit $LASTEXITCODE)" }
}

# 13 self targets: 12 compiler-selfhost modules + cli-selfhost/main.
# Every source is passed in its absolute spelling. The JIT refuses to recompile a target
# whose path string is exactly the relative import path of an already-loaded module
# (compiler.resolve, compiler.front, cli.main) - it crashes or returns an error module.
# The emitted bytes are identical either way, so one absolute spelling keeps this simple.
$Targets = @("ast", "check", "diag", "emit", "error", "front", "ir", "lexer", "parser", "resolve", "token", "ty", "main")

function TargetPath([string]$t) {
    if ($t -eq "main") { return (Join-Path $repoAbs "cli-selfhost\main.pkl") }
    return (Join-Path $repoAbs "compiler-selfhost\$t.pkl")
}

# Rust oracles for the 13 self targets (GATE1/2/5) and the gen2 driver (GATE3).
foreach ($t in $Targets) {
    $src = TargetPath $t
    Invoke-Raw "tools\diff\oracle\$t.pkl_gen.ir"  $cli @("ir", $src)
    Invoke-Raw "tools\diff\oracle\$t.pkl_gen.irx" $cli @("irx", $src)
}
$gen2 = Join-Path $repoAbs "cli-selfhost\gen2_run_one.pkl"
Invoke-Raw "tools\diff\oracle\gen2_run_one.pkl.ir"  $cli @("ir", $gen2)
Invoke-Raw "tools\diff\oracle\gen2_run_one.pkl.irx" $cli @("irx", $gen2)

# Port side (GATE1/3): the port compiling every self source, including itself.
# run_one_self.pkl derives <base> by stripping the .pkl suffix, so an absolute path
# still lands on port/<t>_self.ir.
foreach ($t in $Targets) {
    $src = TargetPath $t
    & $cli run "cli-selfhost\run_one_self.pkl" -- $src 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "PREP FAILED: run_one_self.pkl on $t (exit $LASTEXITCODE)" }
}
& $cli run "cli-selfhost\run_one_self.pkl" -- $gen2 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "PREP FAILED: run_one_self.pkl on gen2_run_one.pkl (exit $LASTEXITCODE)" }

# GATE4 lex oracles for the runtime-independent battery. os_args_* is skipped: its
# assertions depend on argv, so it has no meaningful standalone oracle and GATE4
# treats a missing oracle as a skip.
$battery = @(Get-ChildItem "tests\pickle\*_main.pkl") + @(Get-ChildItem "tests\pickle\*_test.pkl")
$lexCount = 0
foreach ($f in $battery) {
    if ($f.BaseName.StartsWith("os_args")) { continue }
    Invoke-Raw "tools\diff\oracle\$($f.BaseName).pkl.lex" $cli @("lex", (Join-Path $repoAbs "tests\pickle\$($f.Name)"))
    $lexCount++
}

Write-Output "PREP COMPLETE: 14 IR/IRX oracle pairs, 14 port self artifacts, $lexCount lex oracles"
