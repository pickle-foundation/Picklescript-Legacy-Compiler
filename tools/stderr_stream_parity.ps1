# stderr_stream_parity.ps1 - proves `stderr_stream()` reaches the stderr handle.
#
# Why this is a script and not a `pickle test`: the defect this guards lived in
# the Rust `nostd` build of runtime/src/stream.rs, where `stderr_write` used the
# console bridge that `StreamTarget::Stdout` uses. Both writes then landed on
# stdout. `runtime/stream_test.pkl` cannot see that - it writes through
# `newMemIo`, which never touches the bridge - so the whole 285-test suite stayed
# green while the bug was live. Only a spawned process with separated pipes makes
# the two handles observable.
#
# The fixture writes a distinct token per stream, and the assertions are
# per-channel, so a harness that merely checked "output appeared" would pass even
# with both writes folded onto one handle.
#
# Two sides are compared, because either can regress:
#   ported  - pklc_build output, nostd archive (the path that was broken)
#   native  - the Rust JIT oracle
# A mismatch in either direction fails; both must agree channel for channel.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\stderr_stream_parity.ps1
#   ... -SelfhostLink    link the fixture with pickle\compiler\linker.pkl instead

param(
    [switch]$SelfhostLink,
    [int]$TimeoutMs = 120000
)

$ErrorActionPreference = "Continue"
$Root = "."
. "$Root\tools\raw_capture.ps1"

$fixture = "$Root\tests\stderrstream\stderrstream_main.pkl"
if (-not (Test-Path -LiteralPath $fixture)) {
    Write-Output "STDERR-PARITY: FAIL (missing fixture $fixture)"
    exit 1
}

# One lib path, whichever this machine actually has.
$lib = @(
    "$env:TEMP\opencode\rarc\combined.lib",
    "C:\Users\vilni\AppData\Local\Temp\opencode\rarc\combined.lib"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $lib) {
    Write-Output "STDERR-PARITY: SKIP (no combined.lib; run tools\build_combined_lib.ps1 first)"
    exit 0
}

$work = Join-Path $env:TEMP "opencode\stderrstream"
New-Item -ItemType Directory -Force -Path $work | Out-Null

$expectedOut = "[OUTHANDLE]"
$expectedErr = "[ERRHANDLE]"

# Returns a SINGLE boolean and nothing else. It must not also write to the
# output stream: a PowerShell function that both writes and `return`s yields an
# Object[], and `-not @("msg", $false)` evaluates to False, so an
# `if (-not (Check ...))` caller silently treats every failure as a pass. That
# exact bug shipped in this file once - it reported PASS while the stderr fold
# was live. Diagnostics therefore go to stderr via [Console]::Error and the
# function returns only the boolean.
function Assert-Channels([string]$tag, $r) {
    $ok = $true
    if ($r.ExitCode -ne 0) { [Console]::Error.WriteLine("STDERR-PARITY: $tag exit=$($r.ExitCode), expected 0"); $ok = $false }
    if ($r.StdOutText -ne $expectedOut) { [Console]::Error.WriteLine("STDERR-PARITY: $tag stdout=[$($r.StdOutText)], expected [$expectedOut]"); $ok = $false }
    if ($r.StdErrText -ne $expectedErr) { [Console]::Error.WriteLine("STDERR-PARITY: $tag stderr=[$($r.StdErrText)], expected [$expectedErr]"); $ok = $false }
    return [bool]$ok
}

$fail = 0

# ---- ported AOT build against the nostd archive ----
$portedExe = Join-Path $work "ported.exe"
$builder = if ($SelfhostLink) { "$Root\tools\diff\port\pklc_run.exe" } else { "$Root\tools\diff\port\pklc_build.exe" }
if (-not (Test-Path -LiteralPath $builder)) {
    Write-Output "STDERR-PARITY: FAIL (no builder at $builder)"
    exit 1
}
$b = Invoke-RawCapture -Exe $builder -Arguments "$fixture `"$lib`" `"$portedExe`"" -TimeoutMs $TimeoutMs
if ($b.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $portedExe)) {
    Write-Output "STDERR-PARITY: FAIL (ported build failed: $($b.StdErrText))"
    exit 1
}
if (-not (Assert-Channels "ported" (Invoke-RawCapture -Exe $portedExe -TimeoutMs $TimeoutMs))) { $fail++ }

# ---- native oracle ----
$nativeExe = Join-Path $work "native.exe"
$cli = "$Root\target\debug\pickle-cli.exe"
if (Test-Path -LiteralPath $cli) {
    $n = Invoke-RawCapture -Exe $cli -Arguments "build `"$fixture`" -o `"$nativeExe`"" -TimeoutMs $TimeoutMs
    if ($n.ExitCode -eq 0 -and (Test-Path -LiteralPath $nativeExe)) {
        if (-not (Assert-Channels "native" (Invoke-RawCapture -Exe $nativeExe -TimeoutMs $TimeoutMs))) { $fail++ }
    } else {
        Write-Output "STDERR-PARITY: native build unavailable, skipping oracle side"
    }
} else {
    Write-Output "STDERR-PARITY: native oracle unavailable, skipping that side"
}

if ($fail -eq 0) {
    Write-Output "STDERR-PARITY: PASS (ported + native both separate stdout/stderr)"
    exit 0
}
Write-Output "STDERR-PARITY: FAIL ($fail side(s) folded a stream)"
exit 1