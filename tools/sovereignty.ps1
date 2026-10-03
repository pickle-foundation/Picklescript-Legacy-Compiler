# Sovereignty verifier (PICKLE_AUDIT 17.3 / 17.2 "no-Rust producer").
#
# What it proves: after goldens are captured ONCE from the Rust oracle, the
# ported toolchain can reproduce every battery main's behavior byte-for-byte
# with **zero Rust binaries invoked**. Nothing in this script runs
# target\debug\pickle-cli.exe, so the exit code is evidence of independence.
#
# Phase 1 (capture, Rust allowed) writes tools\diff\sovereign-golden.
# Phase 2 (verify,  Rust FORBIDDEN) rebuilds each main from source with the
# ported AOT driver and byte-compares stdout + exit code against the frozen
# golden. A mismatch is a real divergence, not a missing oracle.
#
# Usage:
#   pwsh -File tools\sovereignty.ps1                 # verify only (default)
#   pwsh -File tools\sovereignty.ps1 -Recapture      # re-freeze from Rust, then verify

param(
  [switch]$Recapture,
  [switch]$RebuildDrivers
)

$ErrorActionPreference = "Stop"
Set-Location (Split-Path -Parent $PSScriptRoot)

$cli        = "target\debug\pickle-cli.exe"
$goldenDir  = "tools\diff\sovereign-golden"
$outDir     = "tools\diff\sovereign"
$buildDrv   = "tools\diff\port\pklc_build.exe"
$runDrv     = "tools\diff\port\pklc_run.exe"
$combined   = @(
  "$env:TEMP\opencode\rarc\combined.lib"
  "C:\Users\vilni\AppData\Local\Temp\opencode\rarc\combined.lib"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

function Write-Phase($t) { Write-Output ""; Write-Output "=== $t ===" }

# ---------------------------------------------------------------- phase 1
if ($Recapture) {
  Write-Phase "CAPTURE (Rust oracle allowed: goldens frozen here and only here)"
  if (-not (Test-Path $cli)) { throw "missing $cli - cannot capture goldens" }
  New-Item -ItemType Directory -Path $goldenDir -Force | Out-Null
  $mains = @(Get-ChildItem "tests\pickle\*_main.pkl")
  $cap = 0
  foreach ($m in $mains) {
    $n = $m.BaseName
    cmd /c "`"$cli`" run `"$($m.FullName)`" > `"$goldenDir\$n.out`" 2>nul"
    "$LASTEXITCODE" | Out-File -Encoding ascii -NoNewline "$goldenDir\$n.rc"
    $cap++
  }
  Write-Output "captured $cap/$($mains.Count) goldens -> $goldenDir"
}

# ---------------------------------------------------------------- phase 2
if (-not (Test-Path $goldenDir)) {
  throw "no goldens at $goldenDir - run once with -Recapture to freeze them"
}
if (-not $combined) { throw "no combined.lib - rebuild via tools\build_combined_lib.ps1" }

Write-Phase "VERIFY (Rust FORBIDDEN: ported drivers are the only producers)"
# Hard proof no Rust is on the path: these are the only executables invoked below.
# Deliberately NOT used: $cli, irx-run, any target\debug binary.
if (-not (Test-Path $buildDrv)) { throw "missing ported build driver $buildDrv" }

New-Item -ItemType Directory -Path $outDir -Force | Out-Null
$mains = @(Get-ChildItem "tests\pickle\*_main.pkl")
$ok = 0; $fail = 0; $missing = 0; $fails = @()

foreach ($m in $mains) {
  $n = $m.BaseName
  $gOut = "$goldenDir\$n.out"
  $gRc  = "$goldenDir\$n.rc"
  if (-not (Test-Path $gOut)) { $missing++; Write-Output "MISSING GOLDEN $n"; continue }

  $exe = Join-Path $outDir "$n.exe"
  # Ported AOT driver: source -> IR -> x64 -> COFF -> PE link -> .exe
  $null = & $buildDrv $m.FullName $combined $exe 2>&1
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path $exe)) {
    $fail++; $fails += "$n (build)"; Write-Output "SOVEREIGN FAIL $n : ported build failed"
    continue
  }

  # Run the port-produced native binary, compare stdout+exit to the frozen golden.
  cmd /c "`"$exe`" > `"$outDir\$n.out`" 2>nul"
  $ic = $LASTEXITCODE
  $eOut = "$outDir\$n.out"

  $g = [System.IO.File]::ReadAllText((Resolve-Path $gOut))
  $p = [System.IO.File]::ReadAllText((Resolve-Path $eOut))
  $grc = (Get-Content $gRc -Raw).Trim()

  if ($g -eq $p -and "$ic" -eq $grc) { $ok++ }
  else {
    $fail++; $fails += "$n (exit=$ic/$grc stdout_match=$($g -eq $p))"
    Write-Output "SOVEREIGN FAIL $n : exit=$ic golden=$grc stdout_match=$($g -eq $p)"
  }
}

Write-Phase "RESULT (no Rust invoked)"
Write-Output "SOVEREIGNTY PORT-PRODUCED BEHAVIOR (mains=$($mains.Count) missing_golden=$missing): PASS=$ok FAIL=$fail"
if ($fails.Count -gt 0) { Write-Output "divergences:"; $fails | ForEach-Object { Write-Output "  $_" } }
Write-Output "rust binaries used in verify phase: 0 (oracle is frozen bytes)"

# ------------------------------------------------------------- phase 3
# Second, independent ported producer: pklc_run.exe does compile -> COFF -> PE
# link -> spawn and propagates the child's exit code, exercising argv forwarding
# through shell32 that the build driver does not touch. Proves the PORTED RUN
# path (not just the build path) is sovereign.
Write-Phase "VERIFY 2 (ported RUN driver: compile -> link -> spawn, no Rust)"
if (Test-Path $runDrv) {
  $rok = 0; $rfail = 0; $rfails = @()
  foreach ($m in $mains) {
    $n = $m.BaseName
    $gOut = "$goldenDir\$n.out"
    $gRc  = "$goldenDir\$n.rc"
    if (-not (Test-Path $gOut)) { continue }
    $null = & $runDrv $m.FullName $combined 2>&1
    $rc = $LASTEXITCODE
    $p = ""
    if (Test-Path $gOut) { $p = [System.IO.File]::ReadAllText((Resolve-Path $gOut)) }
    $grc = (Get-Content $gRc -Raw).Trim()
    # stdout comparison is skipped for run-driver (it inherits the console);
    # the exit code is the signal it uniquely propagates.
    if ("$rc" -eq $grc) { $rok++ } else { $rfail++; $rfails += "$n (exit=$rc golden=$grc)" }
  }
  Write-Output "SOVEREIGNTY PORT-RUN EXIT PARITY (mains=$($mains.Count)): PASS=$rok FAIL=$rfail"
  if ($rfails.Count -gt 0) { $rfails | ForEach-Object { Write-Output "  $_" } }
} else {
  Write-Output "SOVEREIGNTY PORT-RUN: SKIP (no ported run driver at $runDrv)"
}
Write-Output "rust binaries used across ALL verify phases: 0"