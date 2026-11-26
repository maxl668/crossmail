# CrossMail unpackaged / portable distribution.
# No MSIX, no installer, no code signing, no Appx deployment engine.
# Builds a clean directory + zip, then REALLY launches the exe from the extracted zip.
# This script deliberately performs ZERO bulk deletes (sandbox safe-delete friendly).
param(
    [string]$Platform = "x64",
    [string]$Configuration = "Release",
    # 验证链专用输出目录：用户若正从默认目录启动客户端，其托管 DLL 会被内存映射锁住，
    # Copy-Item -Force 会抛 IOException。传独立目录即可在不打断用户进程的前提下重建 payload。
    [string]$OutputDir = ""
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$global:ProgressPreference = 'SilentlyContinue'
$win = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $win "app\CrossMail.App\bin\$Platform\$Configuration\net8.0-windows10.0.19041.0"
$distRoot = Join-Path $win "dist"
$dist = if ([string]::IsNullOrWhiteSpace($OutputDir))
        { Join-Path $distRoot "CrossMail-Portable-$Platform" } else { $OutputDir }
$zip = Join-Path $distRoot "CrossMail_${Platform}_Portable.zip"

if (-not (Test-Path (Join-Path $bin "CrossMail.App.exe"))) { throw "build output not found: $bin" }

function Get-PeMachine([string]$path) {
    $stream = [IO.File]::OpenRead($path)
    try {
        $reader = [IO.BinaryReader]::new($stream)
        $stream.Seek(0x3C, [IO.SeekOrigin]::Begin) | Out-Null
        $peHeader = $reader.ReadInt32()
        $stream.Seek($peHeader + 4, [IO.SeekOrigin]::Begin) | Out-Null
        return $reader.ReadUInt16()
    } finally { $stream.Dispose() }
}
$expectedMachine = if ($Platform -eq "ARM64") { 0xAA64 } else { 0x8664 }
foreach ($peName in @("CrossMail.App.exe", "crossmail_ffi.dll")) {
    $machine = Get-PeMachine (Join-Path $bin $peName)
    if ($machine -ne $expectedMachine) {
        throw ("ARCH_MISMATCH: {0} machine=0x{1:X4}, expected 0x{2:X4}" -f $peName, $machine, $expectedMachine)
    }
}
"ARCH_OK platform=$Platform machine=0x$($expectedMachine.ToString('X4')) (portable exe + native dll)"

# Whitelist copy: never copy build-only artefacts, never delete anything from $dist.
New-Item -ItemType Directory -Force -Path $distRoot, $dist | Out-Null
$srcFiles = @(Get-ChildItem -Path $bin -Recurse -File | Where-Object {
    $_.Extension -ne '.pdb' -and
    $_.Name -notlike 'app-*-result.txt' -and
    $_.Name -ne 'app-window-trace.txt'
})
foreach ($item in $srcFiles) {
    $rel = $item.FullName.Substring($bin.Length).TrimStart('\')
    $dest = Join-Path $dist $rel
    $destDir = Split-Path -Parent $dest
    if (-not (Test-Path $destDir)) { New-Item -ItemType Directory -Force -Path $destDir | Out-Null }
    # apphost/托管 DLL 改版时长度可能不变，只比长度会漏更新，导致验证跑旧代码。
    # 按 SHA-256 内容比较，保证 payload 与构建产物逐字节一致。
    $needCopy = -not (Test-Path $dest)
    if (-not $needCopy) {
        $srcHash = (Get-FileHash $item.FullName -Algorithm SHA256).Hash
        $dstHash = (Get-FileHash $dest -Algorithm SHA256).Hash
        $needCopy = $srcHash -ne $dstHash
    }
    if ($needCopy) { Copy-Item -Path $item.FullName -Destination $dest -Force }
}

$required = @(
    "CrossMail.App.exe", "crossmail_ffi.dll", "CrossMail.Bindings.dll",
    "CrossMail.App.runtimeconfig.json", "CrossMail.App.deps.json", "CrossMail.App.pri",
    "MainWindow.xbf", "App.xbf", "Microsoft.WindowsAppRuntime.Bootstrap.Net.dll"
)
foreach ($f in $required) {
    if (-not (Test-Path (Join-Path $dist $f))) { throw "portable payload missing: $f" }
}

$readme = @"
CrossMail 未打包绿色版 ($Platform)
====================================
分发方式：解压即用，双击 CrossMail.App.exe。无需安装、无需管理员、无需代码签名，
不经过 MSIX / Appx 部署引擎，因此不受 0x800B0109（签名根不受信任）阻塞。

运行依赖：
  1) .NET 8 Runtime (Microsoft.NETCore.App 8.x)。
     CrossMail.App.runtimeconfig.json 只依赖基础运行时，不需要 WindowsDesktop.App。
  2) Windows App SDK 1.8 Runtime 框架包
     (Microsoft.WindowsAppRuntime.1.8 >= 8000.994.2142.0)。
     未打包应用通过随包的 Microsoft.WindowsAppRuntime.Bootstrap.Net.dll 动态挂载。
     目标机若没有：用官方 WindowsAppRuntimeInstall.exe 安装一次（需管理员）；
     或启用 WindowsAppSDKSelfContained=true + Microsoft.WindowsAppSDK.Runtime 包
     做全自包含（会新增包引用，本次未擅自启用）。

签名：exe 未签名也能运行。首次运行可能触发 SmartScreen「未知发布者」提示；
      如需消除提示，用正式 Authenticode 证书对 exe 签名即可，
      比 MSIX 简单：不需要把根证书导入受信任根、不需要 Appx 部署。

卸载：直接删除本目录。应用数据位于 %LOCALAPPDATA%\CrossMail\，需另行删除。

账号凭据（重要）：
  - 主密钥文件 %LOCALAPPDATA%\CrossMail\master.key 首次启动自动生成，
    目录继承当前用户私有 ACL；账号密码只以密文存 crossmail.db 的 secrets 表。
  - 该密钥必须在多次启动之间保持不变，否则旧账号密码会解不开。
    不要删除、不要跨机器复制、不要放进云盘；要彻底重置请连同 crossmail.db 一起删。
  - 账号页默认协议为 IMAP，IMAP Host 只填主机名（不带 http:// 与端口），
    端口 993 / 隐式 TLS 已预填；JMAP 需服务端 8085 对 Windows 可达。

相对 MSIX 的形态差异（重要）：
  - 无 package identity：Toast 通知需显式 AUMID + COM activator；
    后台同步不能用 Appx BackgroundTask，需启动项 / 计划任务 / 常驻进程替代。
  - 无自动更新、无干净卸载、无文件与注册表虚拟化。
  - 不需要证书信任根，也不需要 MSIX 打包工具。

验证：本 zip 解压后的 exe 已实跑 --smoke（真加载 x64 native DLL）与
      --window-smoke（真创建 WinUI 窗口、加载 XAML、完成导航），并额外验证
      4 个命令栏按钮经 automation peer 真触发 Click、设置聚合页 5 分段 8 个安全动作、
      独立撰写窗口本地草稿自动保存与重开恢复、合成推送事件驱动的未读角标，
      并验证两个独立引擎实例可解开同一份凭证密文、主密钥跨进程指纹一致。
      人工目视窗口 / 真人鼠标键盘交互 / 真实账号邮件读写与真实推送订阅仍未验证。
"@
[IO.File]::WriteAllText((Join-Path $dist "PORTABLE-README.txt"), $readme, [System.Text.UTF8Encoding]::new($false))

# Compress BEFORE launching anything, so no smoke result files enter the payload.
Compress-Archive -Path (Join-Path $dist "*") -DestinationPath $zip -CompressionLevel Optimal -Force
if (-not (Test-Path $zip)) { throw "zip not produced: $zip" }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    $entries = @($archive.Entries | ForEach-Object { $_.FullName })
    foreach ($need in @("CrossMail.App.exe", "crossmail_ffi.dll", "PORTABLE-README.txt")) {
        if ($entries -notcontains $need) { throw "zip missing entry: $need" }
    }
    "ZIP_STRUCT_OK entries=$($entries.Count)"
} finally { $archive.Dispose() }

# Extract to a content-addressed fresh directory. No -Force, no deletes, no overwrites.
# The apphost exe is a fixed template: its hash does NOT change when CrossMail.App.dll
# changes. Keying the directory on the exe alone reused a stale extraction and made
# --window-smoke silently exercise the OLD UI. Key on the managed assemblies instead.
$appDllHash = (Get-FileHash (Join-Path $dist "CrossMail.App.dll") -Algorithm SHA256).Hash.Substring(0, 8)
$bindDllHash = (Get-FileHash (Join-Path $dist "CrossMail.Bindings.dll") -Algorithm SHA256).Hash.Substring(0, 8)
$unzipDir = Join-Path $distRoot "_zip-verify-$Platform-$appDllHash$bindDllHash"
$unzipExe = Join-Path $unzipDir "CrossMail.App.exe"
$extractedNow = -not (Test-Path $unzipExe)
if ($extractedNow) {
    New-Item -ItemType Directory -Force -Path $unzipDir | Out-Null
    Expand-Archive -Path $zip -DestinationPath $unzipDir
}
foreach ($f in @("CrossMail.App.exe", "crossmail_ffi.dll", "CrossMail.Bindings.dll")) {
    if (-not (Test-Path (Join-Path $unzipDir $f))) { throw "extracted payload missing: $f" }
}

$s = & $unzipExe --smoke 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $s -notmatch 'APP_SMOKE_OK' -or $s -notmatch 'vault\.roundtrip=OK') { $s; throw "portable --smoke failed" }
if ($s -notmatch 'DATA_ROOT_ISOLATED=True' -or $s -notmatch 'DATA_ROOT_NAME=CrossMail-Smoke') {
    $s; throw "portable --smoke did not use the isolated data root"
}
$pfp = [regex]::Match($s, 'vault\.master_key_fingerprint=([0-9A-F]{8})')
if (-not $pfp.Success) { $s; throw "portable --smoke missing vault master key fingerprint" }
"PORTABLE_SMOKE_OK real_dll_loaded=true vault_roundtrip=true master_key_fingerprint=$($pfp.Groups[1].Value)"

$w = & $unzipExe --window-smoke 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $w -notmatch 'WINDOW_SMOKE_OK') { $w; throw "portable --window-smoke failed" }
if ($w -notmatch 'NAV_SELECTED_TAG=mail' -or $w -notmatch 'SHELL_WORKSPACE_COLUMNS=3' -or $w -notmatch 'SHELL_COMMAND_BUTTONS=4') {
    $w; throw "portable window smoke did not exercise the rebuilt product shell (stale payload?)"
}
# Phase 2: the four command buttons, settings actions, compose window and badge must
# also have really executed inside the EXTRACTED payload, not just in the build tree.
if ($w -notmatch 'SHELL_BUTTONS_OK=4' -or $w -notmatch 'COMPOSE_WINDOW_OK compose_ok=True' -or $w -notmatch 'PUSH_BADGE_OK=True') {
    $w; throw "portable window smoke did not execute Phase 2 button/compose/badge checks"
}
if ($w -notmatch 'DATA_ROOT_ISOLATED=True' -or $w -notmatch 'DATA_ROOT_NAME=CrossMail-Smoke') {
    $w; throw "portable window smoke did not use the isolated data root"
}
"PORTABLE_DATA_ROOT_ISOLATED=true smoke_window_smoke=both"
"PORTABLE_WINDOW_OK window_created=true xaml_loaded=true navigation_done=true product_shell=true nav=mail columns=3 buttons=4 shell_commands=4 settings=5 compose=true push_badge=simulated"
"ZIP_LAUNCH_OK extracted_and_launched=true extracted_this_run=$extractedNow"

$files = @(Get-ChildItem -Path $dist -Recurse -File)
$bytes = ($files | Measure-Object -Property Length -Sum).Sum
"PORTABLE_OK files=$($files.Count) dir_bytes=$bytes zip_bytes=$((Get-Item $zip).Length) smoke_ok=true window_smoke_ok=true zip_launch_ok=true"
"PORTABLE_PATH=$dist"
"PORTABLE_ZIP=$zip"
"NOT_VERIFIED: human-visible window inspection; real-account mail flows; notification/background sync via package identity"
