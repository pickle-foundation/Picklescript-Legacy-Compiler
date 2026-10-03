param(
    [Parameter(Mandatory = $true)][string]$Target,
    [switch]$SelfhostLink
)

$ErrorActionPreference = "Continue"
$Root   = "C:\Users\vilni\Desktop\PickleScript"
$Cli    = "$Root\target\debug\pickle-cli.exe"
$Gcc    = "C:\Users\vilni\Tools\w64devkit\w64devkit\bin\gcc.exe"
$Temp   = "C:\Users\vilni\AppData\Local\Temp\opencode\x64real"
$Stem   = [System.IO.Path]::GetFileNameWithoutExtension($Target)
$Probe  = Join-Path $Temp $Stem

New-Item -ItemType Directory -Force -Path $Probe | Out-Null

# ---- 1. oracle: JIT run of the real program ----
Push-Location $Root
try {
    $oracleOut = (& $Cli run $Target 2>$null)
    if ($LASTEXITCODE -ne 0) { Write-Output "ORACLE FAIL exit=$LASTEXITCODE"; exit 1 }
    [System.IO.File]::WriteAllBytes((Join-Path $Probe "oracle.txt"), [System.Text.Encoding]::UTF8.GetBytes(($oracleOut -join "`n")))
} finally { Pop-Location }

# ---- 2. selfhost driver: source -> PickleIR -> x64 lower -> funcs.s ----
Push-Location $Root
try {
    $drvOut = & $Cli run pickle\cli\x64_real_run.pkl -- $Target $Probe 2>&1
    $drvOutTxt = $drvOut -join "`n"
    if ($LASTEXITCODE -ne 0) { Write-Output "DRIVER FAIL exit=$LASTEXITCODE"; Write-Output $drvOutTxt; exit 1 }
    if ($drvOutTxt -notmatch "X64REAL OK") { Write-Output $drvOutTxt; Write-Output "DRIVER: lowering failed"; exit 1 }
} finally { Pop-Location }

# ---- 3. COFF object already written by the driver (funcs.obj): no gcc -c ----
$obj = Join-Path $Probe "funcs.obj"
if (-not (Test-Path $obj)) { Write-Output "OBJ NOT FOUND: $obj"; exit 1 }

# ---- 4. link: selfhost pelink (nostd combined.lib) or rustc + shim ----
$exe = Join-Path $Probe "probe.exe"
if ($SelfhostLink) {
    $Combined = @(
        "$Temp\rarc\combined.lib"
        "$Env:TEMP\opencode\rarc\combined.lib"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $Combined) { Write-Output "COMBINED LIB NOT FOUND: $Temp\rarc\combined.lib (rebuild via cargo build -p pickle-runtime --features nostd + ar)"; exit 1 }
    $drvOut = & $Cli run pickle\compiler\linker.pkl -- $obj $Combined $exe 2>&1
    $drvOutTxt = $drvOut -join "`n"
    if ($LASTEXITCODE -ne 0 -or $drvOutTxt -notmatch "LINKER WROTE") { Write-Output "SELFHOST LINK FAIL exit=$LASTEXITCODE"; Write-Output $drvOutTxt; exit 1 }
    Write-Output "SELFHOST LINKED: $($drvOutTxt -replace '[\r\n]+','; ')"
} else {
    $rtDir = Join-Path $Probe "rt"
    Push-Location $Root
    try {
        & cargo build -p pickle-runtime --target-dir $rtDir *> $null
        if ($LASTEXITCODE -ne 0) { Write-Output "RT BUILD FAIL exit=$LASTEXITCODE"; exit 1 }
    } finally { Pop-Location }
    $deps = Join-Path $rtDir "debug\deps"
    $rlib = Get-ChildItem $deps -Filter "libpickle_runtime-*.rlib" | Select-Object -First 1 -ExpandProperty FullName
    if (-not $rlib) { Write-Output "RLIB NOT FOUND"; exit 1 }

    $shim = Join-Path $Probe "main_shim.rs"
    @'
unsafe extern "C" {
    fn pickle_main();
    fn pickle_args_set(argc: usize, argv: *const *const u8);
    fn pickle_runtime_init() -> u32;
    fn pickle_runtime_shutdown();
    fn pickle_flush();
}
#[allow(unused_extern_crates)]
extern crate pickle_runtime;
fn main() {
    let args: Vec<std::ffi::CString> = {
        let mut v = vec![std::ffi::CString::new("pickle").unwrap()];
        let mut skip_dashdash = true;
        for a in std::env::args_os().skip(1) {
            if skip_dashdash && a == "--" { skip_dashdash = false; continue; }
            skip_dashdash = false;
            match a.into_string() {
                Ok(s) => match std::ffi::CString::new(s) {
                    Ok(c) => v.push(c),
                    Err(_) => v.push(std::ffi::CString::new("").unwrap()),
                },
                Err(_) => v.push(std::ffi::CString::new("").unwrap()),
            }
        }
        v
    };
    let ptrs: Vec<*const u8> = args.iter().map(|c| c.as_ptr() as *const u8).collect();
    unsafe {
        pickle_args_set(ptrs.len(), ptrs.as_ptr());
        pickle_runtime_init();
        pickle_main();
        pickle_flush();
        pickle_runtime_shutdown();
    }
}
'@ | Set-Content -Encoding utf8 $shim

    $exePath = $exe
    & rustc $shim --edition 2021 -C opt-level=2 -C panic=abort "--extern=pickle_runtime=$rlib" "-C" "link-arg=$(Join-Path $Probe 'funcs.obj')" -o $exe *> $null
    if ($LASTEXITCODE -ne 0) { Write-Output "LINK FAIL exit=$LASTEXITCODE"; exit 1 }
}

# ---- 6. run the probe and compare to the oracle ----
# Run with the same working directory the oracle used, so that programs relying
# on relative paths (file/stream builtins) are not compared across two CWDs.
Push-Location $Root
try { $selfOut = (& $exe 2>$null) } finally { Pop-Location }
[System.IO.File]::WriteAllBytes((Join-Path $Probe "self.txt"), [System.Text.Encoding]::UTF8.GetBytes(($selfOut -join "`n")))
$self = [System.IO.File]::ReadAllBytes((Join-Path $Probe "self.txt"))
$orch = [System.IO.File]::ReadAllBytes((Join-Path $Probe "oracle.txt"))
$same = ([Convert]::ToBase64String($self) -eq [Convert]::ToBase64String($orch))
if ($same) {
    Write-Output "PROBE PASS ($($self.Length) bytes identical)"
    Get-Content -Raw (Join-Path $Probe "self.txt")
    exit 0
} else {
    Write-Output "PROBE MISMATCH: self=$($self.Length) oracle=$($orch.Length)"
    Write-Output "--- oracle ---"; Get-Content -Raw (Join-Path $Probe "oracle.txt")
    Write-Output "--- self ---"; Get-Content -Raw (Join-Path $Probe "self.txt")
    exit 1
}
