# Emits a generated Pickle test block from the Rust oracle table.
# i64::MIN is not writable as a Pickle literal, so it is spelled out.
$ErrorActionPreference = 'Stop'
$out = "$env:TEMP\opencode\float_oracle"
$tsv = "$out\float_oracle.tsv"
$block = "$out\block.pkl"

$lines = @(Get-Content $tsv)
$body = @()
$i = 0
foreach ($ln in $lines) {
    $parts = $ln -split "`t"
    if ($parts.Count -lt 2) { continue }
    $text = $parts[0]
    $bits = $parts[1]
    if ($bits -like 'ERR:*') { throw "oracle parse error for '$text': $bits" }
    if ($bits -eq '-9223372036854775808') { $bits = '0 - 9223372036854775807 - 1' }
    $body += "    expect(f64ToBits(floatVal(`"$text`"))).toBe($bits)"
    $i++
}

$hdr = @()
$hdr += ''
$hdr += '// GENERATED from the Rust `str::parse::<f64>()` oracle by'
$hdr += '// tools\gen_float_float_oracle.ps1 + gen_float_emit_block.ps1. Do not hand-edit.'
$hdr += '//'
$hdr += '// The two float bugs fixed in this file were both invisible to every other'
$hdr += '// test, because each had exactly one hand-picked assertion guarding it. This'
$hdr += '// table is the generalisation: every literal below is asserted bit-for-bit'
$hdr += '// against what Rust actually parses it to, so any future divergence in'
$hdr += '// irxParseFloat / floatVal shows up as a named failing literal instead of a'
$hdr += '// silent value change somewhere downstream.'
$hdr += '//'
$hdr += '// Coverage: the subnormal band (1e-324 .. 9e-308, 72 values), the'
$hdr += '// subnormal/normal boundary and its neighbours, 2^53 and neighbours where'
$hdr += '// not every integer is representable, 25-digit digit runs, deep exponents'
$hdr += '// with many mantissa digits, both saturation tails (Rust returns Ok(inf) and'
$hdr += '// Ok(0.0) rather than erroring), signed zero, negative values across the'
$hdr += '// whole range, and syntax forms Rust accepts (".5", "1.", "1E5", "1e+5").'
$hdr += "// Total: $i literals."
$hdr += 'test fn float_val_matches_rust_oracle_table() {'
$hdr += $body
$hdr += '}'

Set-Content -Path $block -Value ($hdr -join "`n") -Encoding ASCII
"assertions generated : $i"
"block written        : $block"
"block lines          : $(@(Get-Content $block).Count)"
''
'--- first 6 assertion lines ---'
$body | Select-Object -First 6 | ForEach-Object { "  $_" }
''
'--- last 4 assertion lines ---'
$body | Select-Object -Last 4 | ForEach-Object { "  $_" }