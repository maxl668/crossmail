# CrossMail Windows 侧 C# 绑定断言（仿 scripts/build-bindings.sh 的 expect 风格）。
# 断言条数单独统计，不混入 self-hosted server 侧 81 项与 539 项测试口径。
param(
    [Parameter(Mandatory = $true)][string]$ExportsFile,
    [string]$BindingsDir = "$PSScriptRoot\..\bindings\CrossMail.Bindings",
    [string]$Manifest = "$PSScriptRoot\..\bindings\CrossMail.Bindings\abi-symbols.json"
)
$ErrorActionPreference = 'Stop'

$script:fail = 0
$script:checks = 0
function Expect([bool]$ok, [string]$label) {
    $script:checks++
    if ($ok) { "  [OK]   $label" }
    else { "  [FAIL] $label"; $script:fail = 1 }
}
function ToPascal([string]$s) {
    (($s -split '_') | ForEach-Object {
        if ($_.Length -eq 0) { '' }
        else { $_.Substring(0, 1).ToUpper() + $_.Substring(1) }
    }) -join ''
}

if (-not (Test-Path $ExportsFile)) { throw "export truth table not found: $ExportsFile" }
$exports = @{}
foreach ($line in Get-Content $ExportsFile) {
    $s = $line.Trim()
    if ($s) { $exports[$s] = $true }
}
"==> 导出符号真值表：$($exports.Count) 个"

$native = Get-Content "$BindingsDir\NativeSymbols.g.cs" -Raw
$models = Get-Content "$BindingsDir\Models.g.cs" -Raw
$errors = Get-Content "$BindingsDir\Errors.g.cs" -Raw
$engine = Get-Content "$BindingsDir\Engine.g.cs" -Raw
$manifestObj = Get-Content $Manifest -Raw | ConvertFrom-Json

$entrypoints = @([regex]::Matches($native, 'EntryPoint\s*=\s*"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
$declared = @{}
foreach ($ep in $entrypoints) { $declared[$ep] = $true }

"==> 逐项核对 C# EntryPoint 是否存在于真实导出表（$($entrypoints.Count) 项）"
foreach ($ep in $entrypoints) {
    Expect $exports.ContainsKey($ep) "export:$ep"
}

"==> 核对 UDL 全部函数在 C# 绑定层有对应 EntryPoint"
$expected = @()
foreach ($m in $manifestObj.udl_engine_methods) { $expected += "uniffi_crossmail_ffi_fn_method_crossmailengine_$m" }
foreach ($f in $manifestObj.udl_functions) { $expected += "uniffi_crossmail_ffi_fn_func_$f" }
foreach ($s in $expected) { Expect $declared.ContainsKey($s) "udl-symbol:$s" }

"==> 核对 checksum 契约符号"
foreach ($p in $manifestObj.checksums.PSObject.Properties) {
    Expect $declared.ContainsKey($p.Name) "checksum:$($p.Name)"
}

"==> 核对 C# 源码类型 / 方法名"
foreach ($d in $manifestObj.udl_dictionaries) {
    Expect ($models.Contains("public sealed class $d")) "class:$d"
}
foreach ($e in $manifestObj.udl_enums) {
    Expect ($models.Contains("public enum $e")) "enum:$e"
}
foreach ($m in $manifestObj.udl_engine_methods) {
    Expect ($engine -match ("\b" + (ToPascal $m) + "\s*\(")) "method:$m"
}
foreach ($f in $manifestObj.udl_functions) {
    Expect ($engine -match ("\b" + (ToPascal $f) + "\s*\(")) "func:$f"
}
foreach ($v in $manifestObj.udl_error_variants) {
    Expect ($errors.Contains("$v = ")) "error:$v"
}
Expect ($models.Contains('public abstract class FlagDto')) 'FlagDto(sealed-variants)'
Expect ((Get-Content "$BindingsDir\PushObserver.cs" -Raw).Contains('init_callback_vtable_pushobserver')) 'PushObserver(vtable)'
Expect ($engine.Contains('PushObserverRegistry.Register')) 'PushObserver(register)'
Expect ($engine.Contains('InvokeScalar')) 'scalar-return-path'
Expect ($engine.Contains('InvokeVoid')) 'void-return-path'

""
# 机器可读计数行（ASCII only）：验证链用它汇总，不解析中文摘要行——
# 控制台代码页会把中文变成乱码，按文本匹配会静默失败。
"COUNT_CHECKS=$($script:checks)"
"COUNT_ENTRYPOINTS=$($entrypoints.Count)"
if ($script:fail -ne 0) {
    "[FAIL] C# 绑定断言未通过（共 $($script:checks) 项）"
    exit 1
}
"[OK] C# 绑定断言全部通过：$($script:checks) 项，其中 EntryPoint $($entrypoints.Count) 项"
