# Enumerate the runtime extern surface: exactly which pickle_* symbols emitted code
# calls, and which of them still have no ported implementation.
#
# PICKLE_AUDIT.md 17.1 records the trap this script exists to close. An empty link
# archive makes the linker report *one program's* unresolved names, and that list was
# read as the work order: "11 symbols". It was 11 for a test fixture, 13 for a closure
# example, 17 for raw pointers. The count is workload-dependent, so a plan built from
# one probe is a guess wearing a number.
#
# The surface is not a function of any single program - it is the union over every
# program shape the toolchain must support. This script takes that union properly:
# it compiles every shape in the repo to IRX and reads the extern table directly out
# of the artifact, so nothing depends on link-order or on which program happened to be
# probed. IRX stores extern names hex-encoded ("irx externs N" then "extern <i> <hex>"),
# which is why grepping the .ir/.irx for "pickle_" finds nothing.
#
# Then it crosses-references the ported runtime: a symbol counts as ported when some
# pickle/runtime/*.pkl declares it with #[symbol("...")], which is the same convention
# the string cluster landed with.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File tools\extern_surface.ps1
#        powershell ... -File tools\extern_surface.ps1 -Shapes runtime
#
# Exit code 0 when every referenced symbol is ported, 1 when any is missing.

param([ValidateSet("all", "battery", "examples", "runtime", "self")][string]$Shapes = "all")

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "battery_common.ps1")
$repoAbs = (Resolve-Path ".").Path

$srcs = @()
switch ($Shapes) {
    "battery"  { $srcs += Get-ChildItem "tests\pickle\*.pkl" -File }
    "examples" { $srcs += Get-ChildItem "examples" -Recurse -Filter *.pkl -File }
    "runtime"  { $srcs += Get-ChildItem "pickle\runtime\*.pkl" -File }
    "self"     { $srcs += Get-ChildItem "pickle\compiler\*.pkl" -File; $srcs += Get-ChildItem "pickle\cli\*.pkl" -File }
    "all" {
        $srcs += Get-ChildItem "tests\pickle\*.pkl" -File
        $srcs += Get-ChildItem "examples" -Recurse -Filter *.pkl -File
        $srcs += Get-ChildItem "pickle\compiler\*.pkl" -File
        $srcs += Get-ChildItem "pickle\cli\*.pkl" -File
        $srcs += Get-ChildItem "pickle\runtime\*.pkl" -File
    }
}
# Drop scratch/debug sources that are not part of the real surface. These are
# developer probes (_tmp_dbg*, probe_build, byteprobe, f64probe, …) that do not
# ship and already contribute 0 rows (they fail to compile standalone), so
# excluding them is numerically neutral and only removes report noise.
$srcs = $srcs |
    Where-Object { $_.BaseName -notmatch '^(_tmp_|_probe|probe_|byteprobe|f64probe|static_probe)' } |
    Sort-Object FullName -Unique

# symbol -> set of shape names that reference it
$need = @{}
$failed = @()
foreach ($f in $srcs) {
    # A program that fails to compile writes a diagnostic to stderr, which under
    # $ErrorActionPreference="Stop" becomes a terminating error before $LASTEXITCODE
    # is ever read. Drop to Continue for the call, exactly as Invoke-NativeQuiet does.
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $text = & $batteryCommonCli "irx" $f.FullName 2>&1 | Out-String
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    if ($code -ne 0) { $failed += $f.FullName; continue }
    foreach ($m in [regex]::Matches($text, '(?m)^extern\s+\d+\s+([0-9a-fA-F]+)\s*$')) {
        $hex = $m.Groups[1].Value
        if ($hex.Length % 2 -ne 0) { continue }
        $bytes = New-Object byte[] ($hex.Length / 2)
        for ($i = 0; $i -lt $bytes.Length; $i++) { $bytes[$i] = [Convert]::ToByte($hex.Substring($i * 2, 2), 16) }
        $name = [System.Text.Encoding]::ASCII.GetString($bytes)
        if ($name -notmatch '^pickle_') { continue }
        if (-not $need.ContainsKey($name)) { $need[$name] = @{} }
        $need[$name][$f.BaseName] = $true
    }
}

# ported symbol -> declaring file, from the #[symbol("...")] convention
$ported = @{}
foreach ($rf in Get-ChildItem "pickle\runtime\*.pkl" -File) {
    $rt = [System.IO.File]::ReadAllText($rf.FullName)
    foreach ($m in [regex]::Matches($rt, '#\[symbol\("([^"]+)"\)\]')) {
        $ported[$m.Groups[1].Value] = $rf.Name
    }
}

$missing = @()
$rows = @()
foreach ($s in ($need.Keys | Sort-Object)) {
    $shapeList = ($need[$s].Keys | Sort-Object)
    if ($ported.ContainsKey($s)) {
        $rows += [pscustomobject]@{ symbol = $s; status = "ported"; where = $ported[$s]; shapes = $shapeList.Count }
    } else {
        $missing += $s
        $rows += [pscustomobject]@{ symbol = $s; status = "MISSING"; where = "-"; shapes = $shapeList.Count }
    }
}

$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Output
Write-Output ""
Write-Output "SURFACE shapes scanned : $($srcs.Count) sources ($Shapes)"
Write-Output "SURFACE irx failures    : $($failed.Count)"
Write-Output "SURFACE distinct pickle_* symbols referenced: $($need.Count)"
Write-Output "SURFACE already ported  : $($need.Count - $missing.Count)"
Write-Output "SURFACE MISSING         : $($missing.Count)"
if ($missing.Count -gt 0) {
    Write-Output ""
    Write-Output "SURFACE missing list (this is the 17.1 work order, worst first):"
    foreach ($m in ($missing | Sort-Object { -$need[$_].Count })) {
        $where = ($need[$m].Keys | Sort-Object | Select-Object -First 6) -join ", "
        if ($need[$m].Count -gt 6) { $where += ", +$($need[$m].Count - 6) more" }
        Write-Output ("  {0,-28} {1,4} shapes   {2}" -f $m, $need[$m].Count, $where)
    }
}
if ($failed.Count -gt 0) {
    Write-Output ""
    Write-Output "SURFACE sources that failed to compile to IRX (not counted):"
    foreach ($x in $failed) {
        $note = ""
        if ((Split-Path $x -Leaf) -eq "fs.pkl") {
            $note = "  # pure-Pickle library; its Fs interface seam is bound by the production program, so standalone IRX is expected to fail (no in-file implementation)"
        }
        Write-Output "  $x$note"
    }
}

$report = "tools\diff\extern_surface.txt"
New-Item -ItemType Directory -Path "tools\diff" -Force | Out-Null
($rows | ForEach-Object { "{0,-8} {1,-30} {2,4} shapes" -f $_.status, $_.symbol, $_.shapes }) | Set-Content -LiteralPath $report
Write-Output ""
Write-Output "SURFACE report written to $report"

if ($missing.Count -gt 0) { exit 1 }
exit 0