# Shared helpers for tools/battery_full.ps1 and tools/battery_prep.ps1.
#
# Fast byte-equality via SHA256, content-hash stamps for incremental driver
# rebuilds and oracle regeneration, and the reseeding logic both scripts use.

# The Rust cli is the bootstrap/genesis tool and the behaviour oracle. It is not
# required once a pickle seed exists and the oracles are frozen, so a missing cli
# is not an error here; callers that truly need it check Test-Path themselves.
$batteryCommonCli = "target\debug\pickle-cli.exe"
if (Test-Path -LiteralPath $batteryCommonCli) { $batteryCommonCli = (Resolve-Path $batteryCommonCli).Path }

# Pickle self-host seed: a pickled `build` driver (pickle\cli\build.pkl compiled by
# the pickle toolchain, linked against the pickle runtime archive, so it runs on the
# PICKLE runtime). When the seed and the archive it links against both exist, every
# AOT driver below is built by the seed instead of the Rust cli, so Rust leaves the
# driver loop. Create the pair once with `tools\selfhost.ps1` (genesis); delete the
# seed to fall back to the Rust cli. Absence of the seed keeps the old behaviour.
$selfhostSeed = "tools\diff\selfhost\seed.exe"
$selfhostArchive = "tools\diff\rtlib.ar"

function Test-SelfHostSeed {
    (Test-Path -LiteralPath $selfhostSeed) -and (Test-Path -LiteralPath $selfhostArchive)
}

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

# Run a native exe to completion or until `timeoutSec` elapses, writing its stdout
# VERBATIM (raw bytes, no re-encoding) to `outPath` and discarding stderr. On timeout
# the WHOLE process tree is killed (the AOT run/test drivers spawn a linked child that
# would otherwise be orphaned and keep the pipe open) and the sentinel 124 is returned.
#
# This exists because a mis-compiled or contract-divergent program can hang: e.g. the
# ported runtime implements the nostd single-thread contract, so a program that blocks
# on a channel from an inline `thread_spawn` body never wakes. Without a bound, that
# stalls the entire battery.
$script:NativeTimeoutExit = 124
function Invoke-NativeTimeout([string]$exe, [string[]]$argv, [string]$outPath, [int]$timeoutSec) {
    if (Test-Path -LiteralPath $outPath) { Remove-Item -LiteralPath $outPath -Force }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = (Resolve-Path -LiteralPath $exe).Path
    $psi.Arguments = (($argv | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.WorkingDirectory = (Get-Location).Path
    $proc = [System.Diagnostics.Process]::Start($psi)
    $outFs = [System.IO.File]::Create($outPath)
    $errFs = [System.IO.Stream]::Null
    $copyOut = $proc.StandardOutput.BaseStream.CopyToAsync($outFs)
    $copyErr = $proc.StandardError.BaseStream.CopyToAsync($errFs)
    $timedOut = -not $proc.WaitForExit($timeoutSec * 1000)
    if ($timedOut) {
        & taskkill /PID $proc.Id /T /F 2>&1 | Out-Null
        $proc.WaitForExit(5000) | Out-Null
    }
    try { $copyOut.Wait(5000) | Out-Null } catch { }
    try { $copyErr.Wait(5000) | Out-Null } catch { }
    $code = if ($timedOut) { $script:NativeTimeoutExit } else { $proc.ExitCode }
    $outFs.Dispose()
    $proc.Dispose()
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
# is stable across runs on the same tree content. The Rust cli is deliberately NOT
# folded in: these hashes gate driver/artifact rebuilds and must be identical whether
# or not a Rust toolchain is present (a Rust-free checkout with a pickle seed reuses
# the same stamps). Provider identity is folded in explicitly where it matters.
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
    $null = $hasher.TransformFinalBlock([byte[]]@(), 0, 0)
    [BitConverter]::ToString($hasher.Hash).Replace("-", "").ToLowerInvariant()
}

# Content hash that gates oracle regeneration in tools/battery_prep.ps1: every
# input the oracles are derived from (the self trees, the battery, the stdlib the
# ported drivers import, and the Rust compiler binary that produces the oracles).
$batteryPrepDirs = @("pickle\compiler", "pickle\cli", "pickle\runtime", "stdlib", "tests\pickle")

# Source trees embedded in the AOT drivers (compiler + cli + stdlib imports).
$batteryDriverDirs = @("pickle\compiler", "pickle\cli", "stdlib")

function Get-PrepHash { Get-TreeHash $batteryPrepDirs }

function Get-DriverHash { Get-TreeHash $batteryDriverDirs }

# Inputs the frozen IR/IRX/lex oracles are derived from: the 13 self targets
# (pickle\compiler + pickle\cli) and the battery sources the lex oracles cover.
# pickle\runtime is included: cli/main (a self target) imports runtime.* sources,
# so those modules are compiled into its IR - a runtime edit shifts its fn indices
# and must regenerate the oracle. stdlib stays excluded: no oracle depends on it.
$batteryOracleDirs = @("pickle\compiler", "pickle\cli", "pickle\runtime", "tests\pickle")

function Get-OracleHash { Get-TreeHash $batteryOracleDirs }

# Frozen BEHAVIOUR goldens (run/test/irx/cli) committed as tools\diff\golden.zip and
# captured by tools\capture_goldens.ps1. They are keyed by the sources they are taken
# from (the battery mains and the stdlib test suite), NOT by the Rust cli: once frozen
# they let the battery compare port behaviour without a Rust toolchain.
$batteryGoldenDirs = @("tests\pickle", "stdlib", "tests\uncaught")
$goldenDir = "tools\diff\golden"
$goldenZip = "tools\diff\golden.zip"

function Get-GoldenHash { Get-TreeHash $batteryGoldenDirs }

# Stable filesystem key for a golden's target path: `tests\pickle\foo_test.pkl`
# becomes `tests_pickle_foo_test`. The `test` goldens embed the very path string
# they were keyed on, so callers must pass the same spelling used at capture.
function Get-GoldenKey([string]$path) {
    $p = $path -replace '\\', '/'
    if ($p.EndsWith(".pkl")) { $p = $p.Substring(0, $p.Length - 4) }
    if ($p.StartsWith("./")) { $p = $p.Substring(2) }
    ($p -replace '[/:]', '_')
}

# Build an AOT driver unless a stamp next to it proves the embedded sources are
# unchanged since it was last built. Returns $true when the exe exists afterwards.
# The stamp folds in the build provider (self-host seed vs Rust cli), so switching
# between them invalidates stale drivers instead of reusing the wrong runtime.
function Invoke-BuildDriver([string]$src, [string]$out) {
    $stamp = "$out.hash"
    if (Test-SelfHostSeed) {
        $want = (Get-DriverHash) + "|selfhost:" + (Get-FileHash -LiteralPath $selfhostSeed -Algorithm SHA256).Hash
    } else {
        $provider = ""
        if (Test-Path -LiteralPath $batteryCommonCli) { $provider = "|rust:" + (Get-FileHash -LiteralPath $batteryCommonCli -Algorithm SHA256).Hash }
        $want = (Get-DriverHash) + $provider
    }
    if ((Test-Path -LiteralPath $out) -and (Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $want)) {
        return $true
    }
    # Remove any previous artefact first: a build tool can exit 0 having written
    # nothing (mis-typed target, unresolved import), and stamping over a stale exe
    # would then silently pin the WRONG driver for every later gate. With the exe
    # gone a failed build returns $false and is retried instead of masked.
    if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Force }
    if (Test-SelfHostSeed) {
        $null = Invoke-NativeQuiet $selfhostSeed @($src, $selfhostArchive, $out)
    } else {
        $null = Invoke-NativeQuiet $batteryCommonCli @("build", $src, "-o", $out)
    }
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

# Pickle-built runtime archive in place of the Rust combined.lib (which needs
# cargo tools\build_combined_lib.ps1). pickle\cli/rtlib.pkl is AOT-built to a
# driver, then lowers every pickle\runtime member to COFF and packs them into one
# GNU .ar (tools\diff\rtlib.ar) — the exact archive build.pkl/run.pkl link against.
# It is stamp-gated on the driver tree (compiler+cli+stdlib) plus the runtime
# member sources, so it only rebuilds when any input changed. Returns the archive
# path, or $null when the driver build or the archive write fails.
$rtlibArchive = "tools\diff\rtlib.ar"
$rtlibDriver = "tools\diff\port\pklc_rtlib.exe"
$rtlibRuntimeDir = "pickle\runtime"

function Get-RtlibArchive() {
    if (-not (Invoke-BuildDriver "pickle\cli\rtlib.pkl" $rtlibDriver)) {
        Write-Host "RTLIB-ARCHIVE: driver build failed"
        return $null
    }
    $stamp = "$rtlibArchive.hash"
    $want = (Get-DriverHash) + "|" + (Get-TreeHash @("$rtlibRuntimeDir"))
    if ((Test-Path -LiteralPath $rtlibArchive) -and (Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $want)) {
        return $rtlibArchive
    }
    Write-Host "RTLIB-ARCHIVE: rebuilding $rtlibArchive ($($rtlibDriver) $rtlibRuntimeDir)"
    $null = Invoke-NativeQuiet $rtlibDriver @($rtlibRuntimeDir, $rtlibArchive)
    if (Test-Path -LiteralPath $rtlibArchive) {
        Set-Content -LiteralPath $stamp -Value $want -NoNewline
        return $rtlibArchive
    }
    Write-Host "RTLIB-ARCHIVE: write failed"
    return $null
}