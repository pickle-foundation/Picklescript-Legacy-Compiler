# tools/selfhost.ps1 - pickle-only toolchain bootstrap.
#
# Closes the self-hosting loop so Rust is needed only to produce a seed, never to
# build the toolchain. Model:
#
#   stage0 (genesis, Rust)   pickle-cli build  ->  a single "seed" build driver
#   seed  (pickle-runtime)   seed <src> <lib> <out>  ->  every other ported driver
#
# The seed is a `pickle\cli\build.pkl` driver that runs on the PICKLE runtime
# (tools\diff\rtlib.ar), not the Rust runtime. Once it exists, every driver below
# is compiled by the seed from pickle source against the pickle runtime archive -
# no rustc, no cargo, no combined.lib, no Rust compiler in the loop.
#
# Two properties are asserted (unless -NoVerify):
#   * RT_ARCHIVE  the pickle-built rtlib driver regenerates rtlib.ar byte-identically
#   * FIXED POINT the pickle-built build driver rebuilds itself byte-identically
#
# Genesis is only needed once. On a tree that already ships a seed (a committed
# tools\bootstrap\seed\pklc_build.exe, or the output of a previous run), pass
# -Seed <path> and no Rust tool is required at all.

param(
    [string]$Seed,
    [string]$SeedLib = "tools\diff\rtlib.ar",
    [string]$OutDir  = "tools\diff\port",
    [string]$WorkDir = "tools\diff\selfhost",
    [string[]]$Only,
    [switch]$SeedOnly,
    [switch]$NoVerify
)

$ErrorActionPreference = "Stop"
# The ported runtime's automatic collector is not exercised by compile workloads;
# the battery pins the same flag. Genesis inherits it too.
$env:PKL_NO_AUTO_GC = "1"
Remove-Item Env:AUTO_GC -ErrorAction SilentlyContinue

$Root = (Resolve-Path ".").Path
$CliPath = Join-Path $Root "target\debug\pickle-cli.exe"
$RustCli = if (Test-Path -LiteralPath $CliPath) { $CliPath } else { $null }

function ByteEq([string]$a, [string]$b) {
    if (-not (Test-Path -LiteralPath $a) -or -not (Test-Path -LiteralPath $b)) { return $false }
    $fa = Get-Item -LiteralPath $a; $fb = Get-Item -LiteralPath $b
    if ($fa.Length -ne $fb.Length) { return $false }
    (Get-FileHash -LiteralPath $a -Algorithm SHA256).Hash -ieq (Get-FileHash -LiteralPath $b -Algorithm SHA256).Hash
}

function Get-Sha([string]$path) {
    if (Test-Path -LiteralPath $path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash } else { "" }
}

# Run a native command with every stream captured to a log, leaving the caller's
# $ErrorActionPreference untouched. Under "Stop", a native stderr line forwarded by
# the shell would otherwise become a terminating error before the exit code is read
# (the Rust cli prints a cargo progress line to stderr on every build).
function Invoke-Quiet([string]$exe, [string[]]$argv, [string]$log) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & $exe @argv *> $log
    $code = $LASTEXITCODE
    $ErrorActionPreference = $saved
    $code
}

# Hash of every input a driver build depends on: the ported source tree, the seed
# binary, and the runtime archive it links against.
function Get-SelfHostHash([string]$seed, [string]$lib) {
    $dirs = @("pickle\compiler", "pickle\cli", "pickle\runtime", "stdlib")
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
    foreach ($p in @($seed, $lib)) {
        if (Test-Path -LiteralPath $p) {
            $b = [System.IO.File]::ReadAllBytes($p)
            $null = $hasher.TransformBlock($b, 0, $b.Length, $null, 0)
        }
    }
    $null = $hasher.TransformFinalBlock([byte[]]@(), 0, 0)
    [BitConverter]::ToString($hasher.Hash).Replace("-", "").ToLowerInvariant()
}

New-Item -ItemType Directory -Path $OutDir  -Force | Out-Null
New-Item -ItemType Directory -Path $WorkDir -Force | Out-Null

# ---- Resolve the seed -----------------------------------------------------------
if ($Seed -and (Test-Path -LiteralPath $Seed)) {
    Write-Output "SELFHOST: using seed $Seed (no Rust required)"
} else {
    if (-not $RustCli) {
        throw "selfhost: no seed at '$Seed' and no Rust bootstrap ('target\debug\pickle-cli.exe'). Build the Rust cli once, or pass -Seed <path> to a pickle-built driver."
    }
    Write-Output "SELFHOST: GENESIS - building stage0 seed with the Rust cli ($RustCli)"
    $seed = Join-Path $WorkDir "seed.exe"
    # stage0 must exist as a working driver before it can compile the pickle seed.
    # Build it with the Rust cli (rustc-linked), then use it once to compile the
    # pickle-runtime seed from the very same source.
    $stage0Build = Join-Path $WorkDir "stage0_build.exe"
    $t0 = Get-Date
    Invoke-Quiet $RustCli @("build", "pickle\cli\build.pkl", "-o", $stage0Build) (Join-Path $WorkDir "stage0_build.log") | Out-Null
    if (-not (Test-Path -LiteralPath $stage0Build)) { throw "selfhost: genesis stage0 build failed; see $WorkDir\stage0_build.log" }
    Write-Output ("SELFHOST: stage0 build driver ready t={0}s" -f [math]::Round(((Get-Date) - $t0).TotalSeconds, 1))

    # The seed links against a runtime archive. Prefer the shipped one; otherwise
    # produce it once with a Rust-built rtlib driver (the last Rust build in the chain).
    if (-not (Test-Path -LiteralPath $SeedLib)) {
        $stage0Rtlib = Join-Path $WorkDir "stage0_rtlib.exe"
        Invoke-Quiet $RustCli @("build", "pickle\cli\rtlib.pkl", "-o", $stage0Rtlib) (Join-Path $WorkDir "stage0_rtlib.log") | Out-Null
        if (-not (Test-Path -LiteralPath $stage0Rtlib)) { throw "selfhost: genesis stage0 rtlib driver build failed" }
        Invoke-Quiet $stage0Rtlib @("pickle\runtime", $SeedLib) (Join-Path $WorkDir "stage0_rtlib_run.log") | Out-Null
        if (-not (Test-Path -LiteralPath $SeedLib)) { throw "selfhost: genesis runtime archive write failed" }
        Write-Output "SELFHOST: genesis runtime archive $SeedLib"
    }

    $Seed = Join-Path $WorkDir "seed.exe"
    $t0 = Get-Date
    Invoke-Quiet $stage0Build @("pickle\cli\build.pkl", $SeedLib, $Seed) (Join-Path $WorkDir "seed_build.log") | Out-Null
    if (-not (Test-Path -LiteralPath $Seed)) { throw "selfhost: seed build failed; see $WorkDir\seed_build.log" }
    Write-Output ("SELFHOST: pickle seed ready t={0}s" -f [math]::Round(((Get-Date) - $t0).TotalSeconds, 1))
}

if (-not (Test-Path -LiteralPath $SeedLib)) {
    throw "selfhost: runtime archive '$SeedLib' missing (pass -SeedLib, or run genesis with the Rust cli present)"
}
Write-Output "SELFHOST: seed=$Seed lib=$SeedLib out=$OutDir"

if ($SeedOnly) { Write-Output "SELFHOST: DONE (seed only)"; exit 0 }

# ---- Driver map: every ported driver the battery/sovereign gates build ----------
$drivers = @(
    @{ src = "pickle\cli\rtlib.pkl";         out = "pklc_rtlib.exe"    },
    @{ src = "pickle\cli\build.pkl";         out = "pklc_build.exe"    },
    @{ src = "pickle\cli\run.pkl";           out = "pklc_run.exe"      },
    @{ src = "pickle\cli\main.pkl";          out = "pklc_main.exe"     },
    @{ src = "pickle\cli\main.pkl";          out = "pklc_cli.exe"      },
    @{ src = "pickle\cli\test.pkl";          out = "pklc_test.exe"     },
    @{ src = "pickle\cli\gen2_run_one.pkl";  out = "pklc_gen2.exe"     },
    @{ src = "pickle\cli\run_irx.pkl";       out = "pklc_irx.exe"      },
    @{ src = "pickle\cli\run_one_self.pkl";  out = "pklc_runone.exe"   },
    @{ src = "pickle\cli\selflink.pkl";      out = "pklc_selflink.exe" }
)
if ($Only) {
    $Only = @($Only -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    $drivers = @($drivers | Where-Object { $Only -contains $_.out })
}

# Stamp gate: if the seed + lib + sources are unchanged since a previous
# selfhost run, the existing drivers are byte-stable and the rebuild is skipped.
$want = Get-SelfHostHash $Seed $SeedLib
$stamp = Join-Path $OutDir "selfhost.hash"
$allPresent = $true
foreach ($d in $drivers) { if (-not (Test-Path -LiteralPath (Join-Path $OutDir $d.out))) { $allPresent = $false } }
if ($allPresent -and (Test-Path -LiteralPath $stamp) -and ((Get-Content -LiteralPath $stamp -Raw).Trim() -eq $want)) {
    Write-Output "SELFHOST: DRIVERS UP TO DATE (stamp matches) - nothing to rebuild"
} else {
    $ok = 0; $fail = 0
    foreach ($d in $drivers) {
        $out = Join-Path $OutDir $d.out
        $t0 = Get-Date
        $rc = Invoke-Quiet $Seed @($d.src, $SeedLib, $out) (Join-Path $WorkDir "$($d.out).log")
        $dt = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
        $good = ($rc -eq 0) -and (Test-Path -LiteralPath $out)
        if ($good) { $ok++ } else { $fail++ }
        Write-Output ("SELFHOST: BUILD {0,-18} rc={1} ok={2} t={3}s" -f $d.out, $rc, $good, $dt)
    }
    Write-Output "SELFHOST: DRIVERS BUILT ok=$ok fail=$fail"
    if ($fail -eq 0) { Set-Content -LiteralPath $stamp -Value $want -NoNewline }
}

if ($NoVerify) { Write-Output "SELFHOST: DONE (verification skipped)"; exit 0 }

# ---- Verify: the pickle driver regenerates the runtime archive byte-identically --
$rtDriveOut = Join-Path $OutDir "pklc_rtlib.exe"
$regen = Join-Path $WorkDir "rtlib_regen.ar"
if (Test-Path -LiteralPath $rtDriveOut) {
    $t0 = Get-Date
    Invoke-Quiet $rtDriveOut @("pickle\runtime", $regen) (Join-Path $WorkDir "rtlib_regen.log") | Out-Null
    $same = ByteEq $regen $SeedLib
    Write-Output ("SELFHOST: RT_ARCHIVE byte_identical={0} t={1}s" -f $same, [math]::Round(((Get-Date) - $t0).TotalSeconds, 1))
    if (-not $same) { Write-Output "SELFHOST: FAIL - pickle-built rtlib driver does not reproduce $SeedLib" }
} else {
    Write-Output "SELFHOST: RT_ARCHIVE skipped (no $rtDriveOut)"
}

# ---- Verify: the pickle build driver rebuilds itself (fixed point) --------------
$buildOut = Join-Path $OutDir "pklc_build.exe"
$build2 = Join-Path $WorkDir "pklc_build_v2.exe"
if (Test-Path -LiteralPath $buildOut) {
    Invoke-Quiet $buildOut @("pickle\cli\build.pkl", $SeedLib, $build2) (Join-Path $WorkDir "build_v2.log") | Out-Null
    $fp = ByteEq $build2 $buildOut
    Write-Output ("SELFHOST: FIXED POINT byte_identical={0}" -f $fp)
    if (-not $fp) { Write-Output "SELFHOST: FAIL - pklc_build.exe does not reproduce itself" }
} else {
    Write-Output "SELFHOST: FIXED POINT skipped (no $buildOut)"
}

Write-Output "SELFHOST: DONE"
