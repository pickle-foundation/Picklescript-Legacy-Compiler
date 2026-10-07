# Self-rebuild drift test (PICKLE_AUDIT.md 17.8).
#
# "Repeated self-compile cycles under the ported toolchain produce byte-identical
# artifacts (no divergence over evolution)." This measures that claim directly:
# it runs the ported AOT self-compile driver over every self target several times
# in a row and requires every cycle to be byte-identical to cycle 1, and to the
# Rust oracle that GATE1 already pins the port to.
#
# Why cycles can differ at all if nothing changed: the driver holds no cross-run
# state, but the compiler is not obviously deterministic from the outside - symbol
# table ordering, hash-map iteration, cached resolution state and any counter that
# leaks into an emitted name would show up here as a one-bit drift that GATE1
# (single run vs oracle) cannot see. GATE1 proves the port agrees with Rust once.
# This proves it agrees with itself every time.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\selfhost_drift.ps1
#        powershell ... -File tools\selfhost_drift.ps1 -Cycles 5
#
# Exit code 0 when every cycle is byte-identical, 1 otherwise.

param([int]$Cycles = 3)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "battery_common.ps1")
$repoAbs = (Resolve-Path ".").Path
$portDir = "tools\diff\port"
$driftDir = "tools\diff\drift"

# Same 13 self targets as tools/battery_prep.ps1: the 12 pickle\compiler modules plus
# pickle\cli/main. These are the sources whose self-compilation PICKLE_AUDIT.md 17.2
# and 17.4 rest on - if they drift, every later gate is measuring a moving target.
$Targets = @("ast", "check", "diag", "emit", "error", "front", "ir", "lexer", "parser", "resolve", "token", "ty", "main")

function TargetPath([string]$t) {
    if ($t -eq "main") { return (Join-Path $repoAbs "pickle\cli\main.pkl") }
    return (Join-Path $repoAbs "pickle\compiler\$t.pkl")
}

# run_one_self.pkl derives <base> from the target path by stripping ".pkl" and writes
# tools/diff/port/<base>_self.{ir,irx}, so a base maps 1:1 onto both the port artifact
# and the oracle battery_prep.ps1 wrote as tools/diff/oracle/<base>.pkl_gen.{ir,irx}.
function ArtifactPath([string]$base, [string]$ext) { Join-Path $portDir "$base`_self.$ext" }
function OraclePath([string]$base, [string]$ext) { Join-Path "tools\diff\oracle" "$base.pkl_gen.$ext" }

if ($Cycles -lt 2) { Write-Output "DRIFT ABORT: -Cycles must be >= 2 to have something to compare"; exit 1 }

New-Item -ItemType Directory -Path $driftDir -Force | Out-Null

$driver = Join-Path $portDir "pklc_runone.exe"
Write-Output "DRIFT driver: building pklc_runone.exe from pickle\cli\run_one_self.pkl"
if (-not (Invoke-BuildDriver "pickle\cli\run_one_self.pkl" $driver)) {
    Write-Output "DRIFT FAILED: could not AOT-build pickle\cli\run_one_self.pkl"
    exit 1
}

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$cycleDirs = @()
for ($c = 1; $c -le $Cycles; $c++) {
    $cd = Join-Path $driftDir "c$c"
    if (Test-Path -LiteralPath $cd) { Remove-Item -LiteralPath $cd -Recurse -Force }
    New-Item -ItemType Directory -Path $cd -Force | Out-Null

    foreach ($t in $Targets) {
        $code = Invoke-NativeQuiet $driver @((TargetPath $t))
        if ($code -ne 0) {
            Write-Output "DRIFT FAILED: run_one_self on $t in cycle $c (exit $code)"
            exit 1
        }
        foreach ($ext in @("ir", "irx")) {
            $src = ArtifactPath $t $ext
            if (-not (Test-Path -LiteralPath $src)) {
                Write-Output "DRIFT FAILED: cycle $c produced no $src"
                exit 1
            }
            Copy-Item -LiteralPath $src -Destination (Join-Path $cd "$t.$ext") -Force
        }
    }
    $cycleDirs += $cd
    Write-Output ("DRIFT cycle {0}/{1}: {2} artifacts snapshotted ({3:F1}s)" -f $c, $Cycles, ($Targets.Count * 2), $sw.Elapsed.TotalSeconds)
}

# Cycle-to-cycle stability. c1 is the reference; every later cycle must match it byte
# for byte, per target per format.
$drift = 0
$cmp = 0
for ($c = 2; $c -le $Cycles; $c++) {
    foreach ($t in $Targets) {
        foreach ($ext in @("ir", "irx")) {
            $cmp++
            $ref = Join-Path $cycleDirs[0] "$t.$ext"
            $now = Join-Path $cycleDirs[($c - 1)] "$t.$ext"
            if (-not (ByteEq $ref $now)) {
                $drift++
                Write-Output "DRIFT MISMATCH: $t.$ext differs between cycle 1 and cycle $c"
            }
        }
    }
}

# Port-vs-Rust on every cycle, not just the one battery_prep happened to leave behind.
# This is the "no divergence from the reference" half of the same claim.
$oracleCmp = 0
$oracleMiss = 0
foreach ($t in $Targets) {
    foreach ($ext in @("ir", "irx")) {
        $oracle = OraclePath $t $ext
        if (-not (Test-Path -LiteralPath $oracle)) { $oracleMiss++; continue }
        $oracleCmp++
        foreach ($cd in $cycleDirs) {
            if (-not (ByteEq $oracle (Join-Path $cd "$t.$ext"))) {
                $drift++
                Write-Output "DRIFT MISMATCH: $t.$ext in $cd differs from the Rust oracle"
            }
        }
    }
}

$total = ($Targets.Count * 2)
Write-Output ""
Write-Output "DRIFT targets: $($Targets.Count) x 2 formats = $total artifacts per cycle"
Write-Output "DRIFT cycles:  $Cycles (total $($Targets.Count * 2 * $Cycles) artifact comparisons)"
Write-Output "DRIFT cycle-to-cycle: $cmp compared, $drift mismatched"
Write-Output "DRIFT vs Rust oracle: $oracleCmp oracle files across $Cycles cycles ($oracleMiss not on disk, skipped)"

if ($drift -ne 0) {
    Write-Output "DRIFT FAILED: $drift byte mismatch(es) - the self-compile is not a fixed point"
    exit 1
}
if ($oracleCmp -eq 0) {
    Write-Output "DRIFT INCONCLUSIVE: no oracles on disk - run tools\battery_prep.ps1 first"
    exit 1
}
Write-Output "DRIFT PASS: $Cycles consecutive self-compile cycles are byte-identical and match the Rust oracle ($($sw.Elapsed.TotalSeconds.ToString('F1'))s)"
exit 0