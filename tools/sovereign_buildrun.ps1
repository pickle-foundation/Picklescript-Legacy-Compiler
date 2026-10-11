# SOVEREIGN BUILDRUN GATE — the product build path without Rust.
#
# GATE12/12b build + run every battery main against the PICKLE runtime archive
# (tools\diff\rtlib.ar, built by pickle\cli/rtlib.pkl), so the battery needs no
# cargo and no combined.lib. This gate goes still deeper: each main is lowered and
# linked by the pickle selfhost linker (pickle\cli\selflink.pkl) directly against
# the PICKLE runtime DIRECTORY (pickle\runtime) — members are themselves pickle
# `#[symbol]` bodies, and no archive step happens at all. The producted exe is then
# run and its stdout+exit byte-compared to the Rust JIT
# oracle (`pickle-cli run`), exactly like GATE12.
#
# Default runs a representative subset covering every runtime member category
# (box/print, strings, maps+alloc, threads/sync, fs/io, os/process, plus the
# globals/GC-churn mains that exercise the abi_gc sweep path and the map
# tombstone-churn regression). Pass -All
# to run every tests\pickle\*_main.pkl (the full 55-main Rust-free audit; ~23
# min).
#
# Exit code 0 only when every target links Rust-free and matches the oracle.

param(
    [string[]]$Targets = @(
        "abi_box_main", "abi_string_main", "map_stress_main",
        "sync_threads_main", "fs_main", "current_dir_main",
        "globals_ptr_main", "globals_string_main", "map_field_stress_main",
        "map_tombstone_churn_main"
    ),
    [switch]$All
)
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $Root
. (Join-Path $PSScriptRoot "battery_common.ps1")
$cli = (Resolve-Path "target\debug\pickle-cli.exe").Path
$sl = "pickle\cli\selflink.pkl"
$rt = (Resolve-Path "pickle\runtime").Path

# The selfhost linker is itself AOT-built and stamp-gated on the port source
# trees (pickle_common's Get-DriverHash), so the build+run path never shells
# through `pickle-cli run` (the Rust JIT) — the Rust binary is used only to
# bootstrap this one driver and to freeze the oracle below.
$slDriver = Join-Path $PSScriptRoot "diff\port\pklc_selflink.exe"
if (-not (Invoke-BuildDriver $sl "tools\diff\port\pklc_selflink.exe")) {
    Write-Output "SOVEREIGN BUILDRUN: selflink driver build failed"
    exit 1
}
$work = "tests\tmp\sovereign_buildrun"
New-Item -ItemType Directory -Path $work -Force | Out-Null

if ($All) {
    $Targets = @(Get-ChildItem "tests\pickle\*_main.pkl" | ForEach-Object { $_.BaseName })
}

$pass = 0
$fail = 0
function Cert([string]$label, [bool]$ok, [string]$detail) {
    if ($ok) { Write-Output "PASS $label"; $script:pass++ }
    else { Write-Output "FAIL $label : $detail"; $script:fail++ }
}

foreach ($name in $Targets) {
    $target = "tests\pickle\$name.pkl"
    $exe = Join-Path $work "$name.exe"
    Remove-Item $exe -ErrorAction SilentlyContinue
    $linkOut = & $slDriver $target $exe $rt 2>&1
    $linkTxt = ($linkOut | Out-String)
    $unres = @($linkOut | Select-String "SELFLINK UNRESOLVED").Count
    $okLine = @($linkOut | Select-String "^SELFLINK OK:").Count
    $linked = ($unres -eq 0 -and $okLine -eq 1 -and $linkTxt -notmatch "SELFLINK FAIL")
    if (-not $linked) {
        $un = ($linkOut | Select-String "SELFLINK UNRESOLVED" | ForEach-Object { $_.Line.Trim() }) -join "; "
        Cert "$name rust-free-link" $false "unresolved=$unres ok=$okLine $un"
        continue
    }
    # run ported exe vs oracle, battery-style (stdout to file, capture exit)
    $prt = Join-Path $work "p.txt"; $ort = Join-Path $work "o.txt"
    Remove-Item $prt, $ort -ErrorAction SilentlyContinue
    cmd /c "`"$exe`" > `"$prt`" 2>nul"
    $prc = $LASTEXITCODE
    cmd /c "`"$cli`" run `"$target`" > `"$ort`" 2>nul"
    $orc = $LASTEXITCODE
    $p = [System.IO.File]::ReadAllText($prt)
    $o = [System.IO.File]::ReadAllText($ort)
    $eq = ($prc -eq $orc -and $p -eq $o)
    Cert "$name build+run parity" $eq "rc=$prc/$orc len=$($p.Length)/$($o.Length)"
}

Write-Output "SOVEREIGN BUILDRUN (rust-free build+run, $($Targets.Count) mains): PASS=$pass FAIL=$fail"
Remove-Item (Join-Path $work "*.exe"), (Join-Path $work "*.txt") -ErrorAction SilentlyContinue
if ($fail -ne 0) { exit 1 }
exit 0