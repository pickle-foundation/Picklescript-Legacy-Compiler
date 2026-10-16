# Freeze the Rust compiler's BEHAVIOUR outputs so tools/battery_full.ps1 can
# compare the port against them without a Rust toolchain present.
#
# tools/battery_prep.ps1 already freezes the Rust IR/IRX/lex ORACLES (compile-time
# artifacts). This script freezes the rest of the reference behaviour:
#
#   * run/<main>.{out,exit}   - `pickle run tests\pickle/<main>.pkl` stdout+exit
#   * irx/<main>.irx          - `pickle irx tests\pickle/<main>.pkl` (IRX text)
#   * test/<key>.{out,exit}   - `pickle test <path>` stdout+exit for every
#                               tests\pickle and stdlib _test.pkl
#   * cli/<cmd>.{out,exit}    - fixed-output CLI commands (-V/--version/jar)
#   * uncaught/<stem>.{out,err,exit} - `pickle run tests\uncaught/<stem>.pkl`
#                               stdout+stderr+exit, for the crash-stanza gate
#
# Goldens are keyed by content, not path, so a byte-stable source produces the
# same golden regardless of where the battery invokes it from. The `test`
# goldens DO embed the target path (the `running N test(s) from <path>` banner),
# so the caller must pass the same spelling the battery will - this script uses
# the battery's spellings (repo-relative with backslashes).
#
# The result ships as one compressed bundle (tools\diff\golden.zip); the
# extracted dir stays gitignored scratch. Re-run only when the reference
# behaviour itself changes (i.e. the sources the goldens are taken from).
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\capture_goldens.ps1

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "battery_common.ps1")
$repoAbs = (Resolve-Path ".").Path
$goldenDir = "tools\diff\golden"
$goldenZip = "tools\diff\golden.zip"

if (-not (Test-Path -LiteralPath $batteryCommonCli)) {
    throw "CAPTURE FAILED: no Rust cli at $batteryCommonCli to capture goldens from"
}
$cli = $batteryCommonCli

Remove-Item -LiteralPath $goldenDir -Recurse -Force -ErrorAction SilentlyContinue
foreach ($d in @("run", "irx", "test", "cli", "uncaught")) {
    New-Item -ItemType Directory -Path "$goldenDir\$d" -Force | Out-Null
}

# Native stdout must be captured with cmd redirection: PowerShell's `>` re-encodes
# native output, and the battery byte-compares these files.
function Invoke-Cap([string]$outPath, [string]$exe, [string[]]$argv) {
    $parts = @("`"$exe`"") + ($argv | ForEach-Object { "`"$_`"" })
    cmd /c (($parts -join " ") + " > `"$outPath`" 2>nul")
    $LASTEXITCODE
}

# As Invoke-Cap, but keeps stderr too: the uncaught report is written to stderr,
# so the crash-stanza gate compares it byte for byte.
function Invoke-CapErr([string]$outPath, [string]$errPath, [string]$exe, [string[]]$argv) {
    $parts = @("`"$exe`"") + ($argv | ForEach-Object { "`"$_`"" })
    cmd /c (($parts -join " ") + " > `"$outPath`" 2> `"$errPath`"")
    $LASTEXITCODE
}

function Save-Exit([string]$base, [int]$code) {
    Set-Content -LiteralPath "$base.exit" -Value $code -NoNewline
}

$sw = [System.Diagnostics.Stopwatch]::StartNew()

# ---- run + irx goldens for every battery main ---------------------------------
$mains = @(Get-ChildItem "tests\pickle\*_main.pkl")
foreach ($m in $mains) {
    $base = $m.BaseName
    $target = "tests\pickle\$($m.Name)"
    if ($base -ne "os_args_main") {
        Save-Exit "$goldenDir\run\$base" (Invoke-Cap "$goldenDir\run\$base.out" $cli @("run", $target))
    }
    Save-Exit "$goldenDir\irx\$base" (Invoke-Cap "$goldenDir\irx\$base.irx" $cli @("irx", $target))
}
# os_args_main with real argv: the ported run driver exercises this lane too.
Save-Exit "$goldenDir\run\os_args_main.args" (Invoke-Cap "$goldenDir\run\os_args_main.args.out" $cli @("run", "tests\pickle\os_args_main.pkl", "--", "exit", "exit"))
Write-Output "CAPTURE run+irx ($($mains.Count) mains): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

# sync_gc_live_main is PORT-DERIVED (a deliberate divergence, like main's IR oracle).
# It probes the reclamation-gate registry around `abiGcCollect(true)`. The port now
# roots reachable managed homes with shadow frames, so the reachable list+boxes
# SURVIVE the collect (count 3). The Rust reference had no stack rooting and freed
# them (count 0) - the exact unsafety the port's shadow frames fix. The port's own
# frozen transcript therefore stands in as this seam's reference.
$syncBase = "$goldenDir\run\sync_gc_live_main"
$syncTxt = "made: count 3 bytes 112`nlive after collect: count 3 bytes 112`nlive after second: count 3 bytes 112`nregistry seam ok`n"
[System.IO.File]::WriteAllText((Join-Path $repoAbs "$syncBase.out"), $syncTxt)
Set-Content -LiteralPath "$syncBase.exit" -Value 0 -NoNewline

# ---- test goldens: tests\pickle and the stdlib suite --------------------------
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$testFiles = @(Get-ChildItem "tests\pickle\*_test.pkl")
foreach ($f in $testFiles) {
    $target = "tests\pickle\$($f.Name)"
    $key = Get-GoldenKey $target
    Save-Exit "$goldenDir\test\$key" (Invoke-Cap "$goldenDir\test\$key.out" $cli @("test", $target))
}
$stdlibTests = @(Get-ChildItem "stdlib" -Recurse -Filter *_test.pkl)
foreach ($f in $stdlibTests) {
    $target = $f.FullName.Substring($repoAbs.Length + 1)
    $key = Get-GoldenKey $target
    Save-Exit "$goldenDir\test\$key" (Invoke-Cap "$goldenDir\test\$key.out" $cli @("test", $target))
}
Write-Output "CAPTURE test ($($testFiles.Count) battery + $($stdlibTests.Count) stdlib): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

# ---- fixed-output CLI commands ------------------------------------------------
# `history`/`builds` are deliberately NOT captured: their output depends on the
# live git log, so there is no stable golden for them.
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$cliCases = @(
    @("-V", "V"),
    @("--version", "version"),
    @("jar", "jar")
)
foreach ($c in $cliCases) {
    $name = $c[1]
    Save-Exit "$goldenDir\cli\$name" (Invoke-Cap "$goldenDir\cli\$name.out" $cli @($c[0]))
}
Write-Output "CAPTURE cli ($($cliCases.Count) commands): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

# ---- uncaught crash-stanza goldens: exit + stdout + stderr --------------------
# `pickle run` on a fixture with an uncaught throw writes the report to stderr and
# exits 1; uncaught_none_clean exits 0 with empty stderr. All three channels are
# frozen so the selfhost-link gate can prove parity without a Rust oracle.
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$uncaught = @(Get-ChildItem "tests\uncaught\*.pkl")
foreach ($f in $uncaught) {
    $target = "tests\uncaught\$($f.Name)"
    $base = $f.BaseName
    Save-Exit "$goldenDir\uncaught\$base" (Invoke-CapErr "$goldenDir\uncaught\$base.out" "$goldenDir\uncaught\$base.err" $cli @("run", $target))
}
Write-Output "CAPTURE uncaught ($($uncaught.Count) fixtures): $($sw.Elapsed.TotalSeconds.ToString('F1'))s"

# ---- content stamp + bundle ---------------------------------------------------
$hash = Get-GoldenHash
Set-Content -LiteralPath "$goldenDir\.golden_hash" -Value $hash -NoNewline
Compress-Archive -Path $goldenDir -DestinationPath $goldenZip -Force
$zipLen = (Get-Item -LiteralPath $goldenZip).Length
Write-Output "CAPTURE COMPLETE: $goldenZip ($zipLen bytes), hash=$hash"
