# Regenerate every artifact that tools/battery_full.ps1 compares against.
#
# tools/diff/ is gitignored for scratch, but the Rust ORACLES (IR/IRX/lex) live under
# tools/diff/oracle and are FROZEN and committed: a clean checkout has them, so the
# battery has something to compare against and the Rust cli is not needed to prepare.
#
#   * Oracle phase  - the 14 IR/IRX pairs and the lex oracles come from the Rust
#     compiler. They are regenerated only when the sources they are derived from
#     change (a content hash over pickle\compiler + pickle\cli + tests\pickle, stored
#     in tools\diff\oracle\.oracle_hash). When that hash matches, the committed oracles
#     are used verbatim and the Rust cli is never invoked - so prepping works with no
#     Rust toolchain. A source change with no Rust cli present is a hard error.
#   * Port phase    - the port compiling every self source comes from the pickle
#     toolchain (the AOT run_one_self driver), never from Rust; rebuilt when its
#     inputs change.
#
# The whole prep is gated on a content hash of every input; an unchanged tree returns
# immediately.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\battery_prep.ps1

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "battery_common.ps1")
$repoAbs = (Resolve-Path ".").Path
$stampFile = "tools\diff\port\.prep_hash"
$oracleStamp = "tools\diff\oracle\.oracle_hash"

foreach ($d in @("tools\diff\oracle", "tools\diff\port", "tools\diff\gen2")) {
    New-Item -ItemType Directory -Path $d -Force | Out-Null
}

# The frozen Rust oracles ship as one compressed bundle (tools\diff\oracle.zip); the
# extracted dir stays gitignored scratch. On a clean checkout extract it so the oracle
# phase below can use them without the Rust cli.
$oracleZip = "tools\diff\oracle.zip"
if ((Test-Path -LiteralPath $oracleZip) -and -not (Test-Path -LiteralPath "tools\diff\oracle\.oracle_hash")) {
    Write-Output "PREP extracting frozen oracles from $oracleZip"
    Expand-Archive -LiteralPath $oracleZip -DestinationPath "tools\diff" -Force
}

$stamp = Get-PrepHash
if ((Test-Path -LiteralPath $stampFile) -and ((Get-Content -LiteralPath $stampFile -Raw).Trim() -eq $stamp)) {
    Write-Output "PREP SKIPPED: inputs unchanged since last regen"
    exit 0
}

# Native stdout must be captured with cmd redirection: PowerShell's `>` and Out-File
# re-encode native output, and the battery byte-compares these files.
function Invoke-Raw([string]$outPath, [string]$exe, [string[]]$argv) {
    $parts = @("`"$exe`"") + ($argv | ForEach-Object { "`"$_`"" })
    cmd /c (($parts -join " ") + " > `"$outPath`" 2>nul")
    if ($LASTEXITCODE -ne 0) { throw "PREP FAILED: $exe $($argv -join ' ') (exit $LASTEXITCODE)" }
}

# 13 self targets: 12 pickle\compiler modules + pickle\cli/main.
# Every source is passed in its absolute spelling. The JIT refuses to recompile a target
# whose path string is exactly the relative import path of an already-loaded module
# (compiler.resolve, compiler.front, cli.main) - it crashes or returns an error module.
# The emitted bytes are identical either way, so one absolute spelling keeps this simple.
$Targets = @("ast", "check", "diag", "emit", "error", "front", "ir", "lexer", "parser", "resolve", "token", "ty", "main")
$gen2 = Join-Path $repoAbs "pickle\cli\gen2_run_one.pkl"

function TargetPath([string]$t) {
    if ($t -eq "main") { return (Join-Path $repoAbs "pickle\cli\main.pkl") }
    return (Join-Path $repoAbs "pickle\compiler\$t.pkl")
}

# ---- Oracle phase: frozen Rust IR/IRX + lex oracles ---------------------------
$oracleSrcHash = Get-OracleHash
$oraclesFrozen = (Test-Path -LiteralPath $oracleStamp) -and ((Get-Content -LiteralPath $oracleStamp -Raw).Trim() -eq $oracleSrcHash)
if ($oraclesFrozen) {
    Write-Output "PREP oracles FROZEN (sources unchanged; Rust cli not used)"
} else {
    if (-not (Test-Path -LiteralPath $batteryCommonCli)) {
        throw "PREP FAILED: oracle sources changed but no Rust cli at $batteryCommonCli to regenerate them. Restore the committed oracles (tools\diff\oracle) or rebuild the Rust cli."
    }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    foreach ($t in $Targets) {
        $src = TargetPath $t
        Invoke-Raw "tools\diff\oracle\$t.pkl_gen.ir"  $batteryCommonCli @("ir", $src)
        Invoke-Raw "tools\diff\oracle\$t.pkl_gen.irx" $batteryCommonCli @("irx", $src)
    }
    Invoke-Raw "tools\diff\oracle\gen2_run_one.pkl.ir"  $batteryCommonCli @("ir", $gen2)
    Invoke-Raw "tools\diff\oracle\gen2_run_one.pkl.irx" $batteryCommonCli @("irx", $gen2)
    Write-Output "PREP oracles (14 IR/IRX pairs, REGENERATED from Rust): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

    # GATE4 lex oracles. os_args_* is skipped: its assertions depend on argv, so it has
    # no meaningful standalone oracle and GATE4 treats a missing oracle as a skip.
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $battery = @(Get-ChildItem "tests\pickle\*_main.pkl") + @(Get-ChildItem "tests\pickle\*_test.pkl")
    $lexCount = 0
    foreach ($f in $battery) {
        if ($f.BaseName.StartsWith("os_args")) { continue }
        Invoke-Raw "tools\diff\oracle\$($f.BaseName).pkl.lex" $batteryCommonCli @("lex", (Join-Path $repoAbs "tests\pickle\$($f.Name)"))
        $lexCount++
    }
    Write-Output "PREP lex oracles ($lexCount, REGENERATED from Rust): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"
    Set-Content -LiteralPath $oracleStamp -Value $oracleSrcHash -NoNewline
}

# ---- Port phase (GATE1/3): the port compiling every self source, including itself.
# run_one_self.pkl derives <base> by stripping the .pkl suffix, so an absolute path
# still lands on port/<t>_self.ir. The driver is AOT-built once and reused across all
# targets: a JIT `pickle run` per target pays ~5-30s of interpreter startup each time,
# the compiled driver finishes each target in well under a second (and GATE3 proves the
# compiled driver emits byte-identical artifacts to the JIT).
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$pklcRunone = "tools\diff\port\pklc_runone.exe"
if (-not (Invoke-BuildDriver "pickle\cli\run_one_self.pkl" $pklcRunone)) {
    throw "PREP FAILED: could not AOT-build run_one_self.pkl"
}
foreach ($t in $Targets) {
    $code = Invoke-NativeQuiet $pklcRunone @((TargetPath $t))
    if ($code -ne 0) { throw "PREP FAILED: run_one_self on $t (exit $code)" }
}
$code = Invoke-NativeQuiet $pklcRunone @($gen2)
if ($code -ne 0) { throw "PREP FAILED: run_one_self on gen2_run_one.pkl (exit $code)" }
Write-Output "PREP port-side self (14 targets via AOT driver): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

Set-Content -LiteralPath $stampFile -Value $stamp -NoNewline
$mode = "regenerated"
if ($oraclesFrozen) { $mode = "frozen" }
Write-Output "PREP COMPLETE: oracles=$mode, 14 port self artifacts"
