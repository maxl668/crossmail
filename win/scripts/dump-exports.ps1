# 用真实 dumpbin /exports 输出生成导出符号真值表（每行一个符号，排序去重）。
# 该表是 Step2 C# EntryPoint 断言的唯一真值来源，禁止手工编造符号名。
param(
    [Parameter(Mandatory = $true)][string]$Dll,
    [Parameter(Mandatory = $true)][string]$Out
)
$ErrorActionPreference = 'Stop'

$dumpbin = Get-ChildItem 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' -ErrorAction SilentlyContinue |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $dumpbin) { throw 'dumpbin.exe not found (VS2022 C++ tools required)' }
if (-not (Test-Path $Dll)) { throw "DLL not found: $Dll" }

$raw = & $dumpbin /exports $Dll 2>&1
if ($LASTEXITCODE -ne 0) { throw "dumpbin failed: $($raw -join [Environment]::NewLine)" }

$names = foreach ($line in $raw) {
    if ($line -match '^\s*\d+\s+\S+\s+[0-9A-Fa-f]+\s+(\S+)') { $Matches[1] }
}
$sorted = $names | Sort-Object -Unique
if ($sorted.Count -eq 0) { throw 'no exports parsed from dumpbin output' }

$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
Set-Content -Path $Out -Value $sorted -Encoding ascii
"[OK] $($sorted.Count) exports -> $Out"
$sorted | Select-Object -First 8 | ForEach-Object { "  $_" }
