param(
    [string]$Out = "$Env:TEMP\opencode\rarc\combined.lib",
    [switch]$Install,
    [string]$TargetDir = "$Env:TEMP\opencode\combo\rt"
)

$ErrorActionPreference = "Stop"

$Root = "C:\Users\vilni\Desktop\PickleScript"
$Ar   = "C:\Users\vilni\Tools\w64devkit\w64devkit\bin\ar.exe"
$Work = "$Env:TEMP\opencode\combo\work"

if (-not (Test-Path $Ar)) { throw "ar not found: $Ar" }

# ---- 1. build the nostd runtime rlib -------------------------------------
# Use a dedicated target dir: the shared target\debug\deps also holds a *std*
# build of pickle-runtime, and picking the newest rlib there silently yields
# the std objects (77) instead of the freestanding ones (44).
Push-Location $Root
try {
    $env:RUSTFLAGS = ""
    cargo build -p pickle-runtime --features nostd --target-dir $TargetDir | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "cargo build -p pickle-runtime --features nostd failed" }
} finally {
    Pop-Location
}

$deps = Join-Path $TargetDir "debug\deps"
$rlibs = @(Get-ChildItem $deps -Filter "libpickle_runtime-*.rlib")
if ($rlibs.Count -eq 0) { throw "no libpickle_runtime-*.rlib in $deps" }
if ($rlibs.Count -gt 1) {
    $names = ($rlibs | ForEach-Object { $_.Name }) -join ", "
    throw "expected exactly one pickle_runtime rlib in $deps, found $($rlibs.Count): $names"
}
$rlib = $rlibs[0]

# ---- 2. locate the sysroot core/alloc rlibs -----------------------------
$sysroot = (rustc --print sysroot).Trim()
$triple  = (rustc -vV | Select-String '^host:' | ForEach-Object { ($_ -split ':')[1].Trim() })
$sysLib  = Join-Path $sysroot "lib\rustlib\$triple\lib"
if (-not (Test-Path $sysLib)) { throw "sysroot lib dir not found: $sysLib" }

$sysRlibs = @()
foreach ($crate in @("core", "alloc")) {
    $f = Get-ChildItem $sysLib -Filter "lib$crate-*.rlib" | Select-Object -First 1
    if (-not $f) { throw "lib$crate-*.rlib not found in $sysLib" }
    $sysRlibs += $f.FullName
}

# ---- 3. extract object members (no .rmeta) into a scratch dir ------------
Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Work | Out-Null

$srcs = @($rlib.FullName) + $sysRlibs
Push-Location $Work
try {
    foreach ($src in $srcs) {
        $objs = @(& $Ar t $src 2>$null | Where-Object { $_ -like '*.o' })
        if ($objs.Count -eq 0) { throw "no object members in $src" }
        # pass an explicit list: w64devkit ar does not expand globs
        & $Ar x $src @($objs) 2>&1 | Out-Null
        Write-Host ("extracted {0,3} objects from {1}" -f $objs.Count, (Split-Path $src -Leaf))
    }
    $all = @(Get-ChildItem -Path $Work -Filter *.o | ForEach-Object { $_.FullName })
    if ($all.Count -lt 2) { throw "expected runtime + core + alloc objects, got $($all.Count)" }

    # ---- 4. build the combined archive ----------------------------------
    New-Item -ItemType Directory -Force -Path (Split-Path $Out) | Out-Null
    Remove-Item -Force $Out -ErrorAction SilentlyContinue
    & $Ar rcs $Out @($all) 2>&1 | Out-Null
    if (-not (Test-Path $Out)) { throw "ar failed to produce $Out" }
    $count = @(& $Ar t $Out 2>$null).Count
    Write-Host ("combined.lib: {0} members, {1} bytes" -f $count, (Get-Item $Out).Length)
} finally {
    Pop-Location
}

# ---- 5. optionally install into the selfhost probe's first-choice slot ---
if ($Install) {
    $slot = "$Env:TEMP\opencode\x64real\rarc"
    New-Item -ItemType Directory -Force -Path $slot | Out-Null
    Copy-Item $Out (Join-Path $slot "combined.lib") -Force
    Write-Host "installed -> $slot\combined.lib"
}

Write-Host "wrote $Out"
