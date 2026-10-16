<#
.SYNOPSIS
Byte-compares the fatal `pickle_panic_*` / `fatalMsg` paths between the JIT
oracle and a self-host linked executable.

.DESCRIPTION
`tools/selfhost_probe.ps1` requires a program to succeed; `tools/
uncaught_parity.ps1` handles uncaught `throw`. This harness is the third
shape: a program that is SUPPOSED to die with `fatal: pickle: ...` on stderr
and exit 1.

Each pair is (self, oracle):

  self   tests/tmp/<stem>.pkl        - imports the ported module, so the
                                        self-host linked image resolves the
                                        panic/cast symbol to the ported body
  oracle tests/tmp/<stem>_oracle.pkl - no imports, so the JIT runs the
                                        native body (sl_castpanic_iface and
                                        sl_ifacemiss are the exceptions: their
                                        by-name call needs the import, so
                                        those twins are byte-identical copies
                                        and compare ported-in-JIT against
                                        ported-in-linked-image)

All three channels - stdout, stderr, exit code - are captured byte for byte
and must agree, and stderr must carry the pair's expected marker, so both
sides being wrong in the same way cannot pass.

.PARAMETER Filter
Only run pairs whose stem contains this substring.
#>
param(
    [string]$Filter = ""
)

$ErrorActionPreference = "Continue"
$Root = "C:\Users\vilni\Desktop\PickleScript"
$Cli  = "$Root\target\debug\pickle-cli.exe"
$Temp = "C:\Users\vilni\AppData\Local\Temp\opencode\fatalparity"
$Work = Join-Path $Temp "work"

New-Item -ItemType Directory -Force -Path $Work | Out-Null

if (-not (Test-Path $Cli)) { Write-Output "CLI NOT FOUND: $Cli (run: cargo build -p pickle-cli)"; exit 1 }

function Invoke-Captured {
    param([string]$Exe, [string[]]$ExeArgs, [string]$WorkingDir, [string]$Tag)
    $out = Join-Path $Work "$Tag.out"
    $err = Join-Path $Work "$Tag.err"
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

function StartsWith([byte[]]$b, [byte[]]$prefix) {
    if ($b.Length -lt $prefix.Length) { return $false }
    for ($i = 0; $i -lt $prefix.Length; $i++) { if ($b[$i] -ne $prefix[$i]) { return $false } }
    return $true
}

function Show([byte[]]$b) {
    if ($b.Length -eq 0) { return "<empty>" }
    return (($b | ForEach-Object { if ($_ -ge 32 -and $_ -lt 127) { [char]$_ } else { "<0x{0:X2}>" -f $_ } }) -join '')
}

$Combined = @("C:\Users\vilni\AppData\Local\Temp\opencode\x64real\rarc\combined.lib", "$Env:TEMP\opencode\x64real\rarc\combined.lib", "$Env:TEMP\opencode\rarc\combined.lib") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $Combined) { Write-Output "COMBINED LIB NOT FOUND (rebuild via tools/build_combined_lib.ps1)"; exit 1 }

# stem -> expected stderr marker (byte prefix, trailing newline excluded)
$pairs = @(
    @{ stem = "sl_panic";          marker = "fatal: pickle: match is not exhaustive" }
    @{ stem = "sl_castpanic";      marker = "fatal: pickle: invalid class cast" }
    @{ stem = "sl_castpanic_iface"; marker = "fatal: pickle: invalid interface cast" }
    @{ stem = "sl_noneunwrap";     marker = "fatal: pickle: unwrapped none value" }
    @{ stem = "sl_aritydiff";      marker = "fatal: pickle: spread list does not match the expected number of arguments" }
    @{ stem = "sl_ifacemiss";      marker = "fatal: pickle: interface method dispatch found no implementation" }
    @{ stem = "sl_sync_kindmutex";   marker = "fatal: pickle: handle 1 is not a mutex" }
    @{ stem = "sl_sync_kindcond";    marker = "fatal: pickle: handle 1 is not a cond" }
    @{ stem = "sl_sync_kindsem";     marker = "fatal: pickle: handle 1 is not a semaphore" }
    @{ stem = "sl_sync_kindatomic";  marker = "fatal: pickle: handle 1 is not an atomic" }
    @{ stem = "sl_sync_kindchannel"; marker = "fatal: pickle: handle 1 is not a channel" }
    @{ stem = "sl_sync_sendclosed";  marker = "fatal: pickle: send on closed channel" }
    @{ stem = "sl_sync_trysendclosed"; marker = "fatal: pickle: send on closed channel" }
    @{ stem = "sl_sync_recvclosed";  marker = "fatal: pickle: receive on a closed, empty channel" }
    @{ stem = "sl_sync_capzero";     marker = "fatal: pickle: channel capacity must be at least 1, got 0" }
    @{ stem = "sl_sync_capneg";      marker = "fatal: pickle: channel capacity must be at least 1, got -3" }
    @{ stem = "sl_once_kindonce";    marker = "fatal: pickle: handle 1 is not a once" }
)
$pairs = $pairs | Where-Object { $_.stem.Contains($Filter) }

if (-not $pairs) { Write-Output "NO PAIRS MATCHED FILTER '$Filter'"; exit 1 }

$pass = 0; $fail = 0

foreach ($pr in $pairs) {
    $stem = $pr.stem
    $selfSrc  = Join-Path $Root "tests\tmp\$stem.pkl"
    $oracleSrc = Join-Path $Root "tests\tmp\${stem}_oracle.pkl"
    if (-not (Test-Path $selfSrc))  { Write-Output "FAIL $stem : missing $selfSrc"; $fail++; continue }
    if (-not (Test-Path $oracleSrc)) { Write-Output "FAIL $stem : missing $oracleSrc"; $fail++; continue }

    # ---- oracle: JIT, native panic bodies ----
    $orc = Invoke-Captured -Exe $Cli -ExeArgs @("run", $oracleSrc) -WorkingDir $Root -Tag "orc_$stem"

    # ---- self: source -> IR -> x64 -> COFF -> self-host link ----
    $probe = Join-Path $Work $stem
    New-Item -ItemType Directory -Force -Path $probe | Out-Null
    $drv = & $Cli run pickle\cli\x64_real_run.pkl -- $selfSrc $probe 2>&1
    if ($LASTEXITCODE -ne 0 -or (($drv -join "`n") -notmatch "X64REAL OK")) {
        Write-Output "FAIL $stem : lowering failed"
        Write-Output ($drv -join "`n")
        $fail++
        continue
    }
    $obj = Join-Path $probe "funcs.obj"
    if (-not (Test-Path $obj)) { Write-Output "FAIL $stem : no funcs.obj"; $fail++; continue }
    $exe = Join-Path $probe "probe.exe"
    $lk = & $Cli run pickle\compiler\linker.pkl -- $obj $Combined $exe 2>&1
    if ($LASTEXITCODE -ne 0 -or (($lk -join "`n") -notmatch "LINKER WROTE")) {
        Write-Output "FAIL $stem : selfhost link failed"
        Write-Output ($lk -join "`n")
        $fail++
        continue
    }
    $self = Invoke-Captured -Exe $exe -ExeArgs @() -WorkingDir $Root -Tag "self_$stem"

    $problems = @()
    if ($orc.Exit -ne 1) { $problems += "oracle should exit 1, got $($orc.Exit)" }
    if ($self.Exit -ne 1) { $problems += "self should exit 1, got $($self.Exit)" }
    $marker = [System.Text.Encoding]::UTF8.GetBytes($pr.marker)
    if (-not (StartsWith $orc.StdErr $marker)) { $problems += "oracle stderr lacks marker: $(Show $orc.StdErr)" }
    if (-not (StartsWith $self.StdErr $marker)) { $problems += "self stderr lacks marker: $(Show $self.StdErr)" }
    if (-not (Same-Bytes $orc.StdOut $self.StdOut)) {
        $problems += "stdout differs:`n      oracle: $(Show $orc.StdOut)`n      self  : $(Show $self.StdOut)"
    }
    if (-not (Same-Bytes $orc.StdErr $self.StdErr)) {
        $problems += "stderr differs:`n      oracle: $(Show $orc.StdErr)`n      self  : $(Show $self.StdErr)"
    }

    if ($problems.Count -eq 0) {
        Write-Output ("PASS {0,-24} exit=1 stdout={1}b stderr={2}b" -f $stem, $orc.StdOut.Length, $orc.StdErr.Length)
        $pass++
    } else {
        Write-Output "FAIL $stem"
        $problems | ForEach-Object { Write-Output "      $_" }
        $fail++
    }
}

Write-Output ""
Write-Output "FATAL PARITY: $pass passed, $fail failed (selfhost link)"
if ($fail -gt 0) { exit 1 }
exit 0
