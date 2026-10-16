<#
.SYNOPSIS
Auto-collection trigger parity: byte-compares the automatic GC trigger between
the JIT oracle and a rust-free selflinked executable, on probes that allocate
far past a pinned threshold WITHOUT an explicit `gcCollect`.

.DESCRIPTION
The port's automatic trigger lives inside `abiEngineRegister` (the register
moment adopted by every maker through `abi_raw`), reproducing native
`Gc::alloc -> maybe_collect`: once bytes-since-GC crosses the threshold the
crossing allocation collects with the fresh object pinned across its own cycle,
and the one-time `PKL_NO_AUTO_GC` / `PKL_GC_THRESHOLD` environment read runs
allocation-free ahead of that first crossing.

Each target's selflinked exe and the `pickle-cli run` oracle are run in the
SAME environment (no `PKL_NO_AUTO_GC` pin - that is the point) and their
stdout bytes AND exit codes must agree. Every default probe prints the
collection count, so this harness also fails if the port loses the trigger
(ported cycles 0 vs oracle cycles >0).

The `-NoAutoGcCheck` cert additionally pins `PKL_NO_AUTO_GC=1` for one probe
and requires BOTH arms to report cycles 0 - proving the env-parsing pin still
suppresses the ported trigger exactly like native.

.PARAMETER Target
Only run the probe whose relative .pkl path matches this substring.
#>
param(
    [string]$Target = ""
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $Root
. (Join-Path $PSScriptRoot "battery_common.ps1")
$cli = (Resolve-Path "target\debug\pickle-cli.exe").Path
$sl = "pickle\cli\selflink.pkl"
$rt = (Resolve-Path "pickle\runtime").Path
$slDriver = Join-Path $PSScriptRoot "diff\port\pklc_selflink.exe"
if (-not (Invoke-BuildDriver $sl "tools\diff\port\pklc_selflink.exe")) {
    Write-Output "AUTO-COLLECT PARITY: selflink driver build failed"
    exit 1
}
$work = "tests\tmp\auto_collect_parity"
New-Item -ItemType Directory -Force -Path $work | Out-Null

$targets = @(
    "tests\tmp\repro_min.pkl",
    "tests\tmp\repro_list.pkl",
    "tests\tmp\auto_collect_probe.pkl",
    "tests\tmp\repro_n4.pkl",
    "tests\tmp\repro_n8.pkl",
    "tests\tmp\repro_n16.pkl",
    "tests\tmp\repro_n32.pkl",
    "tests\tmp\repro_n64.pkl",
    "tests\tmp\stress\gc_auto_trigger.pkl"
)
if ($Target) {
    $targets = @($targets | Where-Object { $_ -like "*$Target*" })
    if (-not $targets) { Write-Output "AUTO-COLLECT PARITY: no target matches '$Target'"; exit 1 }
}

$pass = 0
$fail = 0
function Cert([string]$label, [bool]$ok, [string]$detail) {
    if ($ok) { Write-Output "PASS $label"; $script:pass++ }
    else { Write-Output "FAIL $label : $detail"; $script:fail++ }
}

foreach ($target in $targets) {
    $stem = [System.IO.Path]::GetFileNameWithoutExtension($target)
    $exe = Join-Path $work "$stem.exe"
    Remove-Item $exe -ErrorAction SilentlyContinue
    $linkOut = & $slDriver $target $exe $rt 2>&1
    $linkTxt = ($linkOut | Out-String)
    $linked = (@($linkOut | Select-String "SELFLINK UNRESOLVED").Count -eq 0 -and
               @($linkOut | Select-String "^SELFLINK OK:").Count -eq 1 -and
               $linkTxt -notmatch "SELFLINK FAIL")
    if (-not $linked) {
        $un = ($linkOut | Select-String "SELFLINK UNRESOLVED" | ForEach-Object { $_.Line.Trim() }) -join "; "
        Cert "$stem rust-free-link" $false "unresolved=$un"
        continue
    }
    $prt = Join-Path $work "$stem.port.txt"; $ort = Join-Path $work "$stem.orc.txt"
    Remove-Item $prt, $ort -ErrorAction SilentlyContinue
    cmd /c "`"$exe`" > `"$prt`" 2>nul"
    $prc = $LASTEXITCODE
    cmd /c "`"$cli`" run `"$target`" > `"$ort`" 2>nul"
    $orc = $LASTEXITCODE
    $p = [System.IO.File]::ReadAllText($prt)
    $o = [System.IO.File]::ReadAllText($ort)
    $eq = ($prc -eq $orc -and $p -eq $o)
    Cert "$stem auto-collect parity" $eq "rc=$prc/$orc out=$($p.Trim())"
}

# Pinned-off cert: with PKL_NO_AUTO_GC=1 both arms must report cycles 0.
$probe = "tests\tmp\repro_n64.pkl"
$exe = Join-Path $work "n64-pinned.exe"
Remove-Item $exe -ErrorAction SilentlyContinue
$linkOut = & $slDriver $probe $exe $rt 2>&1
$linked = (@($linkOut | Select-String "^SELFLINK OK:").Count -eq 1 -and (($linkOut | Out-String) -notmatch "SELFLINK FAIL"))
if ($linked) {
    $prt = Join-Path $work "n64-pinned.port.txt"; $ort = Join-Path $work "n64-pinned.orc.txt"
    Remove-Item $prt, $ort -ErrorAction SilentlyContinue
    $Old = [Environment]::GetEnvironmentVariable("PKL_NO_AUTO_GC")
    [Environment]::SetEnvironmentVariable("PKL_NO_AUTO_GC", "1")
    try {
        cmd /c "`"$exe`" > `"$prt`" 2>nul"
        $prc = $LASTEXITCODE
        cmd /c "`"$cli`" run `"$probe`" > `"$ort`" 2>nul"
        $orc = $LASTEXITCODE
    } finally {
        [Environment]::SetEnvironmentVariable("PKL_NO_AUTO_GC", $Old)
    }
    $p = [System.IO.File]::ReadAllText($prt)
    $o = [System.IO.File]::ReadAllText($ort)
    $zeroCycles = (($p -match "cycles 0") -and ($o -match "cycles 0"))
    $eq = ($prc -eq $orc -and $p -eq $o -and $zeroCycles)
    Cert "PKL_NO_AUTO_GC=1 pin parity" $eq "rc=$prc/$orc out=$($p.Trim())"
} else {
    Cert "PKL_NO_AUTO_GC=1 pin parity" $false "selflink failed"
}

Write-Output "AUTO-COLLECT PARITY ($($targets.Count + 1) certs): PASS=$pass FAIL=$fail"
Remove-Item (Join-Path $work "*.exe"), (Join-Path $work "*.txt") -ErrorAction SilentlyContinue
if ($fail -ne 0) { exit 1 }
exit 0