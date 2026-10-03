# raw_capture.ps1 - byte-exact child-process capture for PowerShell 5.1.
#
# Why this exists: `2>` redirection in Windows PowerShell decorates native
# stderr with a "NativeCommandError" wrapper, which corrupts byte comparisons,
# and `$LASTEXITCODE` can be masked. Several gates (uncaught parity, the
# activation probes) compare raw stderr bytes and the exit code, so the capture
# has to go through .NET directly.
#
# Note: ProcessStartInfo.ArgumentList does NOT exist on .NET Framework 4.x
# (PowerShell 5.1). Arguments must be pre-quoted into .Arguments, which is what
# Invoke-RawCapture does. Use it with positional paths only, or with values you
# have already quoted.
#
# Usage:
#   . tools\raw_capture.ps1
#   $r = Invoke-RawCapture -Exe $exe -Arguments "$src `"$lib`""
#   $r.ExitCode   # [int]
#   $r.StdOut     # [byte[]]
#   $r.StdErr     # [byte[]]
#   $r.StdErrHex  # [string] space-separated hex, for eyeballing
#   $r.StdErrText # [string] UTF8 decode, for reading only (never compare this)

function Invoke-RawCapture {
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string]$Arguments = '',
        [int]$TimeoutMs = 120000
    )

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $psi
    [void]$p.Start()

    # Read both streams before waiting: a child that fills one pipe buffer while
    # we block on the other would otherwise deadlock. CopyToAsync returns a void
    # Task, so wait on the task and then read the MemoryStream directly.
    $msOut = New-Object System.IO.MemoryStream
    $msErr = New-Object System.IO.MemoryStream
    $outTask = $p.StandardOutput.BaseStream.CopyToAsync($msOut)
    $errTask = $p.StandardError.BaseStream.CopyToAsync($msErr)

    if (-not $p.WaitForExit($TimeoutMs)) {
        try { $p.Kill() } catch { }
        throw "Invoke-RawCapture: timeout after ${TimeoutMs}ms running '$Exe $Arguments'"
    }
    $outTask.Wait()
    $errTask.Wait()

    $res = [pscustomobject]@{
        ExitCode   = $p.ExitCode
        StdOut     = $msOut.ToArray()
        StdErr     = $msErr.ToArray()
        StdErrHex  = (($msErr.ToArray() | ForEach-Object { $_.ToString('x2') }) -join ' ')
        StdOutHex  = (($msOut.ToArray() | ForEach-Object { $_.ToString('x2') }) -join ' ')
        StdErrText = [System.Text.Encoding]::UTF8.GetString($msErr.ToArray())
        StdOutText = [System.Text.Encoding]::UTF8.GetString($msOut.ToArray())
    }

    $p.Dispose()
    $msOut.Dispose()
    $msErr.Dispose()
    return $res
}