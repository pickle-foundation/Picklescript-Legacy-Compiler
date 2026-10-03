# Generates the Rust oracle for float-literal parsing and captures exact bits.
# Output: float_oracle.tsv  ->  <literal text> <TAB> <i64 bit pattern>
#
# Rust `str::parse::<f64>()` is the ONLY authority here. An earlier version of
# this script validated each candidate through .NET `[double]::Parse` first,
# which silently dropped 1e309 / 1e400 / -1e400: .NET throws OverflowException on
# those, while Rust returns Ok(inf). That removed exactly the saturation cases
# the table most needs to pin down.
$ErrorActionPreference = 'Stop'
$out = "$env:TEMP\opencode\float_oracle"
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force -Path $out | Out-Null

$specs = @()
# subnormal band, both ends and interior
foreach ($e in -324..-308) { $specs += "1e$e"; $specs += "5e$e"; $specs += "9e$e" }
foreach ($e in -323..-320) { foreach ($m in 1..9) { $specs += "${m}e$e" } }
# many-digit mantissas with deep exponents (exercises guard digits + rounding)
foreach ($e in -307..-300) { $specs += "1.23456789e$e" }
# the subnormal/normal boundary and its immediate neighbours
$specs += @("2.2250738585072009e-308", "2.2250738585072011e-308", "2.2250738585072014e-308")
# 2^53 and neighbours: integers above 2^53 are not all representable
$specs += @("9007199254740992", "9007199254740993", "9007199254740994", "9007199254740995")
# long digit runs that must round
$specs += @("123456789012345678901234567890", "0.1000000000000000000000001")
$specs += @("0.1", "0.2", "0.3", "0.5", "1.5", "2.5", "3.5", "123.456", "3.14159265358979")
$specs += @("1e23", "1e290", "1e295", "1e300", "1e305", "1e307", "1e308")
$specs += @("1.7976931348623157e308", "1.7976931348623156e308", "8.98846567431158e307")
# saturation edges: Rust returns Ok(inf) / Ok(0.0) here, it does not error
$specs += @("1e309", "1e400", "2e308", "1.8e308", "1.7976931348623159e308")
$specs += @("1e-325", "1e-330", "1e-400")
$specs += @("-1e309", "-1e400", "-2e308", "-1.8e308", "-1.7976931348623159e308")
$specs += @("-1e-325", "-1e-330", "-1e-400")
# negative variants across the whole range, including the smallest subnormal
foreach ($t in @("1e-320", "5e-324", "1.23456789e-305", "1e300", "1.7976931348623157e308", "0.1", "9007199254740993")) {
    $specs += "-$t"
}
$specs += @("-0.0", "0", "1", "1.0")
# syntax-form probes: Rust's f64 FromStr accepts these; the port must agree
$specs += @(".5", "1.", "1E5", "1e+5", "0.0000000000000000000000001")

$rows = @(); $seen = @{}
foreach ($s in $specs) {
    if ($seen.ContainsKey($s)) { continue }
    $seen[$s] = $true
    $rows += $s
}

$rs = @()
$rs += 'fn main() {'
$rs += '    let lits: &[&str] = &['
foreach ($s in $rows) {
    $esc = $s.Replace('\', '\\').Replace('"', '\"')
    $rs += "        `"$esc`","
}
$rs += '    ];'
$rs += '    let mut rows: Vec<(f64, &str)> = Vec::new();'
$rs += '    for t in lits {'
$rs += '        match t.parse::<f64>() {'
$rs += '            Ok(v) => rows.push((v, t)),'
$rs += '            Err(e) => println!("{}\tERR:{}", t, e),'
$rs += '        }'
$rs += '    }'
$rs += '    // total_cmp gives a total order that places -0.0 before 0.0, which'
$rs += '    // matters for the ascending-chain differential main.'
$rs += '    rows.sort_by(|a, b| a.0.total_cmp(&b.0));'
$rs += '    for (v, t) in rows {'
$rs += '        println!("{}\t{}", t, v.to_bits() as i64);'
$rs += '    }'
$rs += '}'
Set-Content -Path "$out\oracle.rs" -Value ($rs -join "`n") -Encoding ASCII

$exe = "$out\oracle.exe"
& rustc -O -o $exe "$out\oracle.rs" 2>&1 | Out-String | Write-Host
if ($LASTEXITCODE -ne 0) { throw "rustc failed" }
& $exe | Set-Content -Path "$out\float_oracle.tsv" -Encoding ASCII

$lines = @(Get-Content "$out\float_oracle.tsv")
$errs = @($lines | Where-Object { $_ -match "`tERR:" })
$inf = @($lines | Where-Object { $_ -match "`t9218868437227405312$" })
$ninf = @($lines | Where-Object { $_ -match "`t-4503599627370496$" })
$zero = @($lines | Where-Object { $_ -match "`t0$" })
"literals        : $($rows.Count)"
"oracle lines    : $($lines.Count)"
"parse errors    : $($errs.Count)"
"+inf results    : $($inf.Count)"
"-inf results    : $($ninf.Count)"
"exact 0.0       : $($zero.Count)"
"oracle dir      : $out"