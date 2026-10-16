# SOVEREIGN SELFLINK GATE
#
# Proves a zero-Rust-object executable — the literal empty-archive link:
# links pickle\cli\main.pkl via pickle\cli\selflink.pkl against an EMPTY
# runtime directory, so the exe is built from exactly ONE object (the program
# itself) with ZERO members and ZERO unresolved symbols. All runtime `#[symbol]`
# bodies the build image needs arrive through cli/main.pkl's own `runtime.*`
# imports (import-activation coverage); no Rust archive and no residual member
# is required. Then proves the exe is FUNCTIONAL and byte-equivalent to the FROZEN
# Rust goldens (tools\diff\golden\cli\V.out and the abi_box_main lex oracle):
#   1. The empty-dir selflink resolves with objects=1 and zero unresolved.
#   2. The exe runs `-V` -> "pickle 0.1.0", exit 0.
#   3. The exe runs one compiler pipeline (lex on a copy of the abi_box_main
#      fixture) and its artifact is byte-identical to the frozen `pickle-cli lex`
#      oracle (tools\diff\oracle\abi_box_main.pkl.lex).
#
# The whole gate runs with NO Rust cli present: the selfhost linker is AOT-built by
# the pickle seed and every comparison is against a frozen golden.
#
# Exit code is 0 only when every step passes.

$ErrorActionPreference = "Stop"
# UNPINNED: the port's shadow frames root reachable managed homes, so the
# automatic collector is safe under heavy compile workloads. (PKL_NO_AUTO_GC
# remains available for debugging.)
Remove-Item Env:PKL_NO_AUTO_GC -ErrorAction SilentlyContinue
$Root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $Root
. (Join-Path $PSScriptRoot "battery_common.ps1")
$slprog = "pickle\cli\selflink.pkl"
$target = "pickle\cli\main.pkl"
$work = "tests\tmp\sovereign_selflink"
New-Item -ItemType Directory -Path $work -Force | Out-Null

# Port selfhost linker (AOT-built by the pickle seed): compiles main.pkl and links it
# against the PICKLE runtime directory. No Rust cli or cargo anywhere in this gate.
$slDriver = "tools\diff\port\pklc_selflink.exe"
if (-not (Invoke-BuildDriver $slprog $slDriver)) {
    Write-Output "FAIL selflink driver build : $slDriver"
    exit 1
}

$pass = 0
$fail = 0

function Cert([string]$label, [bool]$ok, [string]$detail) {
    if ($ok) {
        Write-Output "PASS $label"
        $script:pass++
    } else {
        Write-Output "FAIL $label : $detail"
        $script:fail++
    }
}

# ---- 1. pickle-only link. The runtime dir is EMPTY: the program object must
#       link with zero members (objects=1) and zero unresolved symbols - the
#       strongest form of the empty-archive/zero-Rust-object proof. Every
#       `#[symbol]` body the build image needs arrives via cli/main.pkl's own
#       `runtime.*` imports (import-activation); nothing is left for members. ----
$exe = Join-Path $work "sl_main_empty.exe"
$blank = Join-Path $work "blankrt"
New-Item -ItemType Directory -Path $blank -Force | Out-Null
Remove-Item -LiteralPath $exe -ErrorAction SilentlyContinue
$linkOut = & $slDriver $target $exe $blank 2>&1
$unresolved = @($linkOut | Select-String -Pattern "SELFLINK UNRESOLVED").Count
$okLine = @($linkOut | Select-String -Pattern "^SELFLINK OK:.* objects=1").Count
$hasFail = @($linkOut | Select-String -Pattern "SELFLINK FAIL").Count
Cert "selflink-resolved (objects=1, unresolved=0)" ($unresolved -eq 0 -and $okLine -eq 1 -and $hasFail -eq 0) (($linkOut | Select-String -Pattern "SELFLINK" | ForEach-Object { $_.Line }) -join "; ")
if (-not (Test-Path -LiteralPath $exe)) { Write-Output "FAIL selflink exe missing"; exit 1 }

# ---- 2. fast path: -V (vs frozen golden cli/V.out) ----
$portV = Join-Path $work "portV.out"
$vrc = Invoke-NativeTimeout $exe @("-V") $portV 60
Cert "selflink-exe -V" ($vrc -eq 0 -and (ByteEq $portV "$goldenDir\cli\V.out")) "exit=$vrc"

# ---- 3. compiler pipeline parity: lex a copy, byte-compare vs frozen lex oracle ----
$fixture = Join-Path $work "fixture.pkl"
Copy-Item -LiteralPath "tests\pickle\abi_box_main.pkl" -Destination $fixture -Force
$runOut = Join-Path $work "port_run.txt"
$rc = Invoke-NativeTimeout $exe @($fixture) $runOut 180
$portLex = "tools\diff\port\fixture.pkl.lex"
$oracle = "tools\diff\oracle\abi_box_main.pkl.lex"
$byteEq = ($rc -eq 0 -and (Test-Path -LiteralPath $portLex) -and (ByteEq $portLex $oracle))
Cert "selflink-exe lex-parity" ($rc -eq 0 -and $byteEq) "exit=$rc byteEq=$byteEq"
Remove-Item -Force "tools\diff\port\fixture.pkl.lex" -ErrorAction SilentlyContinue

# ---- pass/fail summary ----
Write-Output "SOVEREIGN SELFLINK (steps=$($pass+$fail)): PASS=$pass FAIL=$fail"
if ($fail -ne 0) { exit 1 }
exit 0