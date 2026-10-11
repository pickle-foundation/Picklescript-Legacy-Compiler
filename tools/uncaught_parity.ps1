<#
.SYNOPSIS
Compares how an uncaught `throw` is reported by the native toolchain and by a
self-host compiled executable.

.DESCRIPTION
`tools/selfhost_probe.ps1` covers the case where a program succeeds: it runs
the oracle under the JIT, throws away stderr, and requires exit 0. That is
exactly the wrong shape for an escape, because an escape is *supposed* to fail.
So this harness is the mirror image: it captures stdout, stderr and the exit
code separately, byte for byte, and requires all three to agree.

Byte-exactness is the point. The report is written verbatim with one trailing
newline appended, so a payload containing a newline stays on two lines, an empty
payload still produces the report, and a non-ASCII payload is not transliterated.
A harness that compared decoded strings through PowerShell would hide exactly
those differences, so every channel is captured with Start-Process redirection
and compared as bytes.

Each fixture also has its own expected outcome, so a parity match cannot be
achieved by both sides being wrong in the same way:

  exit 1, stderr beginning `uncaught exception: `   uncaught_<something>
  exit 0, stderr empty                              uncaught_none_clean

.PARAMETER SelfhostLink
Link with the self-hosted linker against the PICKLE runtime archive
(tools\diff\rtlib.ar, every pickle\runtime member lowered to COFF by
pickle\cli/rtlib.pkl) instead of `rustc` and a shim. Exercises
`pickle\compiler/linker.pkl` rather than the Rust toolchain — no cargo,
no combined.lib.

.PARAMETER Filter
Only run fixtures whose name contains this substring.
#>
param(
    [switch]$SelfhostLink,
    [string]$Filter = ""
)

$ErrorActionPreference = "Continue"
$Root = "C:\Users\vilni\Desktop\PickleScript"
$Cli  = "$Root\target\debug\pickle-cli.exe"
$Temp = "C:\Users\vilni\AppData\Local\Temp\opencode\uncaught"
$Work = Join-Path $Temp "work"

New-Item -ItemType Directory -Force -Path $Work | Out-Null

if (-not (Test-Path $Cli)) { Write-Output "CLI NOT FOUND: $Cli (run: cargo build -p pickle-cli)"; exit 1 }

# Run a program, capturing all three channels separately and byte-exactly.
# Start-Process redirection is used rather than PowerShell's own, which would
# re-encode the text and drop the difference this harness exists to find.
function Invoke-Captured {
    param([string]$Exe, [string[]]$ExeArgs, [string]$WorkingDir, [string]$Tag)
    $out = Join-Path $Work "$Tag.out"
    $err = Join-Path $Work "$Tag.err"
    # Start-Process rejects an empty -ArgumentList, so it is omitted for the
    # self-host binary, which is run with no arguments at all.
    $sp = @{
        FilePath               = $Exe
        NoNewWindow            = $true
        Wait                   = $true
        PassThru               = $true
        WorkingDirectory       = $WorkingDir
        RedirectStandardOutput = $out
        RedirectStandardError  = $err
    }
    if ($ExeArgs -and $ExeArgs.Count -gt 0) { $sp.ArgumentList = $ExeArgs }
    $p = Start-Process @sp
    return [pscustomobject]@{
        Exit   = $p.ExitCode
        StdOut = [System.IO.File]::ReadAllBytes($out)
        StdErr = [System.IO.File]::ReadAllBytes($err)
    }
}

function Same-Bytes([byte[]]$a, [byte[]]$b) {
    if ($a.Length -ne $b.Length) { return $false }
    for ($i = 0; $i -lt $a.Length; $i++) { if ($a[$i] -ne $b[$i]) { return $false } }
    return $true
}

function Show([byte[]]$b) {
    if ($b.Length -eq 0) { return "<empty>" }
    return (($b | ForEach-Object { if ($_ -ge 32 -and $_ -lt 127) { [char]$_ } else { "<0x{0:X2}>" -f $_ } }) -join '')
}

# ---- link inputs, built once for every fixture ------------------------------
$rtRlib = $null
$Combined = $null
if ($SelfhostLink) {
    $Combined = Join-Path $Root "tools\diff\rtlib.ar"
    if (-not (Test-Path $Combined)) { Write-Output "RUNTIME ARCHIVE NOT FOUND: $Combined (run tools\battery_full.ps1, which builds it via rtlib.pkl)"; exit 1 }
} else {
    $rtDir = Join-Path $Work "rt"
    Push-Location $Root
    try {
        & cargo build -p pickle-runtime --target-dir $rtDir *> $null
        if ($LASTEXITCODE -ne 0) { Write-Output "RUNTIME BUILD FAILED"; exit 1 }
    } finally { Pop-Location }
    $rtRlib = Get-ChildItem "$rtDir\debug\deps" -Filter "libpickle_runtime-*.rlib" |
        Select-Object -First 1 -ExpandProperty FullName
    if (-not $rtRlib) { Write-Output "RLIB NOT FOUND"; exit 1 }
    # The shim calls pickle_main and lets it do its own reporting, which is the
    # whole point: a self-host module is its own host, so nothing outside it may
    # print the escaped payload.
    $shim = Join-Path $Work "main_shim.rs"
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
}

$prefix = if ($SelfhostLink) { "selflink" } else { "rustc" }
$pass = 0; $fail = 0

$fixtures = Get-ChildItem "$Root\tests\uncaught\*.pkl" | Sort-Object Name |
    Where-Object { $_.BaseName.Contains($Filter) }

if (-not $fixtures) { Write-Output "NO FIXTURES MATCHED FILTER '$Filter'"; exit 1 }

foreach ($f in $fixtures) {
    $stem = $f.BaseName
    $probe = Join-Path $Work $stem
    New-Item -ItemType Directory -Force -Path $probe | Out-Null

    # ---- native ----
    $nat = Invoke-Captured -Exe $Cli -ExeArgs @("run", $f.FullName) -WorkingDir $Root -Tag "nat_$stem"

    # ---- self-host: source -> IR -> x64 -> COFF ----
    $drv = & $Cli run pickle\cli\x64_real_run.pkl -- $f.FullName $probe 2>&1
    if ($LASTEXITCODE -ne 0 -or (($drv -join "`n") -notmatch "X64REAL OK")) {
        Write-Output "FAIL $stem : lowering failed"
        Write-Output ($drv -join "`n")
        $fail++
        continue
    }
    $obj = Join-Path $probe "funcs.obj"
    if (-not (Test-Path $obj)) { Write-Output "FAIL $stem : no funcs.obj"; $fail++; continue }

    # ---- link ----
    $exe = Join-Path $probe "$prefix.exe"
    if ($SelfhostLink) {
        $lk = & $Cli run pickle\compiler\linker.pkl -- $obj $Combined $exe 2>&1
        if ($LASTEXITCODE -ne 0 -or (($lk -join "`n") -notmatch "LINKER WROTE")) {
            Write-Output "FAIL $stem : selfhost link failed"
            Write-Output ($lk -join "`n")
            $fail++
            continue
        }
    } else {
        & rustc (Join-Path $Work "main_shim.rs") --edition 2021 -C opt-level=2 -C panic=abort `
            "--extern=pickle_runtime=$rtRlib" "-C" "link-arg=$obj" -o $exe *> $null
        if ($LASTEXITCODE -ne 0) { Write-Output "FAIL $stem : rustc link failed"; $fail++; continue }
    }

    $self = Invoke-Captured -Exe $exe -ExeArgs @() -WorkingDir $Root -Tag "self_$stem"

    # ---- the fixture's own expected outcome, so parity cannot be wrong twice ----
    $problems = @()
    if ($stem -eq "uncaught_none_clean") {
        if ($nat.Exit -ne 0) { $problems += "native should exit 0, got $($nat.Exit)" }
        if ($nat.StdErr.Length -ne 0) { $problems += "native wrote stderr: $(Show $nat.StdErr)" }
        if ($self.Exit -ne 0) { $problems += "self-host should exit 0, got $($self.Exit)" }
        if ($self.StdErr.Length -ne 0) { $problems += "self-host wrote stderr: $(Show $self.StdErr)" }
    } else {
        $marker = [System.Text.Encoding]::UTF8.GetBytes("uncaught exception: ")
        $markerOk = $nat.StdErr.Length -gt $marker.Length
        if ($markerOk) { for ($i = 0; $i -lt $marker.Length; $i++) { if ($nat.StdErr[$i] -ne $marker[$i]) { $markerOk = $false; break } } }
        if ($nat.Exit -ne 1) { $problems += "native should exit 1, got $($nat.Exit)" }
        if (-not $markerOk) { $problems += "native stderr lacks the marker: $(Show $nat.StdErr)" }
        $selfMarkerOk = $self.StdErr.Length -gt $marker.Length
        if ($selfMarkerOk) { for ($i = 0; $i -lt $marker.Length; $i++) { if ($self.StdErr[$i] -ne $marker[$i]) { $selfMarkerOk = $false; break } } }
        if ($self.Exit -ne 1) { $problems += "self-host should exit 1, got $($self.Exit)" }
        if (-not $selfMarkerOk) { $problems += "self-host stderr lacks the marker: $(Show $self.StdErr)" }
    }

    # ---- all three channels must agree ----
    if ($nat.Exit -ne $self.Exit) { $problems += "exit differs: native=$($nat.Exit) self=$($self.Exit)" }
    if (-not (Same-Bytes $nat.StdOut $self.StdOut)) {
        $problems += "stdout differs:`n      native: $(Show $nat.StdOut)`n      self  : $(Show $self.StdOut)"
    }
    if (-not (Same-Bytes $nat.StdErr $self.StdErr)) {
        $problems += "stderr differs:`n      native: $(Show $nat.StdErr)`n      self  : $(Show $self.StdErr)"
    }

    if ($problems.Count -eq 0) {
        Write-Output ("PASS {0,-30} exit={1} stdout={2}b stderr={3}b" -f $stem, $nat.Exit, $nat.StdOut.Length, $nat.StdErr.Length)
        $pass++
    } else {
        Write-Output "FAIL $stem"
        $problems | ForEach-Object { Write-Output "      $_" }
        $fail++
    }
}

Write-Output ""
Write-Output "UNCAUGHT PARITY: $pass passed, $fail failed ($prefix link)"
if ($fail -gt 0) { exit 1 }
exit 0
