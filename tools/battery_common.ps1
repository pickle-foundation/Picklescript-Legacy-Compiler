# Shared helpers for tools/battery_full.ps1 and tools/battery_prep.ps1.
#
# Fast byte-equality via SHA256, content-hash stamps for incremental driver
# rebuilds and oracle regeneration, and the reseeding logic both scripts use.

$batteryCommonCli = (Resolve-Path "target\debug\pickle-cli.exe").Path

# Run a native command with its output discarded, leaving the caller's
# $ErrorActionPreference untouched. Under "Stop" a native stderr line forwarded via
# 2>&1 would otherwise become a terminating error record before the exit code is seen.
function Invoke-NativeQuiet([string]$exe, [string[]]$argv) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $exe @argv 2>&1 | Out-Null
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    $code
}

# Byte equality without the old per-byte PowerShell loop: cheap length check
# first, then a SHA256 over each file's bytes.
function ByteEq([string]$a, [string]$b) {
    if (-not (Test-Path -LiteralPath $a)) { return $false }
    if (-not (Test-Path -LiteralPath $b)) { return $false }
    $fa = Get-Item -LiteralPath $a
    $fb = Get-Item -LiteralPath $b
    if ($fa.Length -ne $fb.Length) { return $false }
    (Get-FileHash -LiteralPath $a -Algorithm SHA256).Hash -ieq (Get-FileHash -LiteralPath $b -Algorithm SHA256).Hash
}

# Incremental SHA256 over a set of files, sorted by relative path so the result
# is stable across runs on the same tree content.
function Get-TreeHash([string[]]$dirs) {
    $files = @()
    foreach ($d in $dirs) {
        if (Test-Path -LiteralPath $d) {
            $files += Get-ChildItem -LiteralPath $d -Recurse -Filter *.pkl -File
        }
    }
    $hasher = [System.Security.Cryptography.SHA256]::Create()
    foreach ($f in ($files | Sort-Object FullName)) {
        $bytes = [System.IO.File]::ReadAllBytes($f.FullName)
        $null = $hasher.TransformBlock($bytes, 0, $bytes.Length, $null, 0)
    }
    if ((Test-Path -LiteralPath $batteryCommonCli)) {
        $cliBytes = [System.IO.File]::ReadAllBytes($batteryCommonCli)
        $null = $hasher.TransformBlock($cliBytes, 0, $cliBytes.Length, $null, 0)
    }
    $null = $hasher.TransformFinalBlock([byte[]]@(), 0, 0)
    [BitConverter]::ToString($hasher.Hash).Replace("-", "").ToLowerInvariant()
}

# Content hash that gates oracle regeneration in tools/battery_prep.ps1: every
# input the oracles are derived from (the self trees, the battery, the stdlib the
# ported drivers import, and the Rust compiler binary that produces the oracles).
$batteryPrepDirs = @("compiler-selfhost", "cli-selfhost", "runtime-selfhost", "stdlib", "tests\pickle")

# Source trees embedded in the AOT drivers (compiler + cli + stdlib imports).
$batteryDriverDirs = @("compiler-selfhost", "cli-selfhost", "stdlib")

function Get-PrepHash { Get-TreeHash $batteryPrepDirs }

function Get-DriverHash { Get-TreeHash $batteryDriverDirs }

# Build an AOT driver unless a stamp next to it proves the embedded sources are
# unchanged since it was last built. Returns $true when the exe exists afterwards.
function Invoke-BuildDriver([string]$src, [string]$out) {
    $stamp = "$out.hash"
    $want = Get-DriverHash
    if ((Test-Path -LiteralPath $out) -and (Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $want)) {
        return $true
    }
    $null = Invoke-NativeQuiet $batteryCommonCli @("build", $src, "-o", $out)
    if (Test-Path -LiteralPath $out) {
        Set-Content -LiteralPath $stamp -Value $want -NoNewline
        return $true
    }
    return $false
}

# Stamp for a ported-build artifact (an exe produced by pklc_build / pklc_run):
# the ported compiler tree hash plus hashes of the belt-and-suspenders inputs the
# artifact is derived from (the .pkl source and the runtime link archive). When the
# stamp matches, the exe on disk is byte-stable from last run, so the expensive
# compile+link can be skipped and only the run-vs-oracle comparison repeated.
function Get-ArtifactStamp([string]$source, [string]$lib) {
    $h = @()
    $h += Get-DriverHash
    if (Test-Path -LiteralPath $source) { $h += (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash }
    if (Test-Path -LiteralPath $lib) { $h += (Get-FileHash -LiteralPath $lib -Algorithm SHA256).Hash }
    ($h -join "|")
}

# Builds $out from $source against $lib via $builder ($pklc_build or $pklc_run)
# unless a stamp shows the inputs unchanged. Returns $true when $out exists.
function Invoke-Artifact([string]$builder, [string]$source, [string]$lib, [string]$out) {
    $stamp = "$out.hash"
    $want = Get-ArtifactStamp $source $lib
    if ((Test-Path -LiteralPath $out) -and (Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $want)) {
        return $true
    }
    $rc = Invoke-NativeQuiet $builder @($source, $lib, $out)
    if ($rc -eq 0 -and (Test-Path -LiteralPath $out)) {
        Set-Content -LiteralPath $stamp -Value $want -NoNewline
        return $true
    }
    return $false
}