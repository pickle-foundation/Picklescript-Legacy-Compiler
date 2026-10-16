# tools/selfhost_gate.ps1 - battery gate: the pickle toolchain self-hosts.
#
# Proves the two properties that make Rust a seed-only dependency:
#   1. FIXED POINT  the pickle seed (a `build` driver on the pickle runtime)
#                   rebuilds pickle\cli\build.pkl byte-identically to itself.
#   2. RT ARCHIVE   the seed-built rtlib driver regenerates tools\diff\rtlib.ar
#                   byte-identically, so the runtime archive is itself pickled.
#
# When no seed is present the toolchain is still on the Rust provider; the gate
# reports that and passes (nothing to prove yet) rather than failing. Enable the
# pickle provider by generating the seed once: `tools\selfhost.ps1 -SeedOnly`.
#
# Both checks are stamp-gated on (seed + archive + driver tree), so a repeat run
# with unchanged inputs is instant.

$ErrorActionPreference = "Continue"
# UNPINNED: the port's shadow frames make the automatic collector safe here.
Remove-Item Env:PKL_NO_AUTO_GC -ErrorAction SilentlyContinue
. (Join-Path $PSScriptRoot "battery_common.ps1")

function Get-Sha([string]$path) {
    if (Test-Path -LiteralPath $path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash } else { "" }
}

if (-not (Test-SelfHostSeed)) {
    Write-Output "RUN selfhost: no pickle seed (Rust provider active; run tools\selfhost.ps1 -SeedOnly to enable)"
    Write-Output "PASS selfhost: not enabled (Rust provider)"
    exit 0
}

$port = "tools\diff\port"
$work = "tools\diff\selfhost"
$buildDrv = Join-Path $port "pklc_build.exe"
$rtlDrv   = Join-Path $port "pklc_rtlib.exe"

if (-not (Invoke-BuildDriver "pickle\cli\build.pkl" $buildDrv)) { Write-Output "FAIL selfhost: pklc_build driver build failed"; exit 1 }
if (-not (Invoke-BuildDriver "pickle\cli\rtlib.pkl" $rtlDrv))   { Write-Output "FAIL selfhost: pklc_rtlib driver build failed"; exit 1 }

$H = (Get-Sha $selfhostSeed) + "|" + (Get-Sha $selfhostArchive) + "|" + (Get-DriverHash)
$stamp = Join-Path $work "gate.hash"
if ((Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $H)) {
    Write-Output "PASS selfhost: fixed-point + rt-archive (cached)"
    exit 0
}

New-Item -ItemType Directory -Path $work -Force | Out-Null
$ok = $true

# 1) Fixed point: the seed compiles the build driver and gets itself back.
$fp = Join-Path $work "gate_build.exe"
$null = Invoke-NativeQuiet $selfhostSeed @("pickle\cli\build.pkl", $selfhostArchive, $fp)
if (ByteEq $fp $selfhostSeed) {
    Write-Output "PASS selfhost fixed-point: seed rebuilds pickle\cli\build.pkl byte-identically"
} else {
    $ok = $false
    Write-Output "FAIL selfhost fixed-point: seed != rebuild of pickle\cli\build.pkl"
}

# 2) Runtime archive: the seed-built rtlib driver reproduces rtlib.ar.
$ar = Join-Path $work "gate_rtlib.ar"
$null = Invoke-NativeQuiet $rtlDrv @("pickle\runtime", $ar)
if (ByteEq $ar $selfhostArchive) {
    Write-Output "PASS selfhost rt-archive: pickled rtlib driver reproduces $selfhostArchive"
} else {
    $ok = $false
    Write-Output "FAIL selfhost rt-archive: $selfhostArchive != regenerated archive"
}

if ($ok) { Set-Content -LiteralPath $stamp -Value $H -NoNewline; exit 0 }
exit 1
