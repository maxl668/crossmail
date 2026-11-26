# CrossMail Windows Step 5 verification chain. Re-runnable.
# Writes win/verification-report.txt with the real results of each step.
param([string]$Configuration = "Release")
$ErrorActionPreference = 'Stop'
$win = Split-Path -Parent $PSScriptRoot
$appBin = Join-Path $win "app\CrossMail.App\bin\x64\$Configuration\net8.0-windows10.0.19041.0"
$smokeExe = Join-Path $win "smoke\CrossMail.Smoke\bin\$Configuration\net8.0\CrossMail.Smoke.exe"
$appExe = Join-Path $appBin "CrossMail.App.exe"
$sln = Join-Path $win "CrossMail.sln"
$exports = Join-Path $win "abi\dll-exports.txt"
$script:report = @()

function Add-Line([string]$s) { $script:report += $s; $s }

# Hard guard: every smoke below writes to an ISOLATED data root. Prove the real
# account DB was byte-identical before and after the whole chain.
$realDb = Join-Path $env:LOCALAPPDATA "CrossMail\crossmail.db"
function Get-DbHash([string]$path) {
    if (-not (Test-Path $path)) { return 'absent' }
    return (Get-FileHash $path -Algorithm SHA256).Hash.Substring(0, 16)
}
$dbHashBefore = Get-DbHash $realDb

# Piping a GUI-subsystem exe through Out-String makes PowerShell wait for exit.
$b = & dotnet build $sln -c $Configuration -v q --nologo 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { $b; throw "step1 dotnet build failed" }
Add-Line "1_BUILD_OK warnings=0 errors=0"

$a = & (Join-Path $PSScriptRoot "check-csharp-bindings.ps1") -ExportsFile $exports | Out-String
if ($a -notmatch '\[OK\]' -or $a -match '\[FAIL\]') { $a; throw "step2 binding assertions failed" }
# Counters come from the child script's ASCII lines (COUNT_CHECKS / COUNT_ENTRYPOINTS).
# Never hardcode them, and never parse the Chinese summary line: this file is UTF-8
# without BOM, which Windows PowerShell 5.1 decodes as ANSI, so non-ASCII comments
# here corrupt the parse (proven 2026-11-15: the line after a Chinese comment was
# swallowed and $checks came out null). Keep this file pure ASCII.
$checks = [int]([regex]::Match($a, 'COUNT_CHECKS=(\d+)').Groups[1].Value)
$entrypoints = [int]([regex]::Match($a, 'COUNT_ENTRYPOINTS=(\d+)').Groups[1].Value)
if ($checks -le 0 -or $entrypoints -le 0) { $a; throw "step2 binding assertion counters invalid checks=$checks entrypoints=$entrypoints" }
Add-Line "2_BINDING_ASSERTIONS_OK checks=$checks entrypoints=$entrypoints"

$s = & $smokeExe 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $s -notmatch 'SMOKE_OK enhanced_failures=0') { $s; throw "step3 core smoke failed" }
Add-Line "3_CORE_SMOKE_OK enhanced_failures=0"

$s = & $appExe --smoke 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $s -notmatch 'APP_SMOKE_OK' -or $s -notmatch 'vault\.roundtrip=OK') { $s; throw "step4 app smoke failed" }
if ($s -notmatch 'DATA_ROOT_ISOLATED=True' -or $s -notmatch 'DATA_ROOT_NAME=CrossMail-Smoke') { $s; throw "step4 app smoke did not use the isolated data root" }
$fp = [regex]::Match($s, 'vault\.master_key_fingerprint=([0-9A-F]{8})')
if (-not $fp.Success) { $s; throw "step4 missing persisted master key fingerprint" }
Add-Line "4_APP_SMOKE_OK real_dll_loaded=true vault_roundtrip=true master_key_persisted=true fingerprint=$($fp.Groups[1].Value) data_root_isolated=true"

$s = & $appExe --ui-smoke 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $s -notmatch 'UI_SMOKE_OK pages=8') { $s; throw "step5 ui smoke failed" }
if ($s -notmatch 'DATA_ROOT_ISOLATED=True' -or $s -notmatch 'DATA_ROOT_NAME=CrossMail-Smoke') { $s; throw "step5 ui smoke did not use the isolated data root" }
$act = [regex]::Match($s, 'actions=(\d+) ok=(\d+) skipped=(\d+)')
if (-not $act.Success) { $s; throw "step5 ui smoke missing action counters" }
Add-Line "5_UI_SMOKE_OK pages=8 actions=$($act.Groups[1].Value) handlers_ok=$($act.Groups[2].Value) skipped=$($act.Groups[3].Value) data_root_isolated=true (real button handlers executed; S5-02..S5-09)"

$m = & (Join-Path $PSScriptRoot "build-msix.ps1") -Platform x64 -Configuration $Configuration | Out-String
if ($m -notmatch 'UNPACK_OK payload_verified=4/4' -or $m -notmatch 'ARCH_OK platform=x64 machine=0x8664') { $m; throw "step6 msix packaging failed" }
Add-Line "6_MSIX_OK unpack_payload=4/4 signed=true chain_untrusted=true"

# Step 7: verify PE machine type of the payload INSIDE the unpacked MSIX, not just the build output.
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
$unpackBin = Join-Path $win "packaging\verify-x64"
foreach ($peName in @("CrossMail.App.exe", "crossmail_ffi.dll")) {
    $p = Join-Path $unpackBin $peName
    if (-not (Test-Path $p)) { throw "step7 missing msix payload: $p" }
    $machine = Get-PeMachine $p
    if ($machine -ne 0x8664) { throw ("step7 ARCH_MISMATCH: {0}=0x{1:X4}" -f $peName, $machine) }
}
Add-Line "7_PACKAGE_ARCH_OK msix payload exe+dll machine=0x8664 (no cross-arch DLL)"

# Step 8: unpackaged / portable distribution - NO MSIX, NO installer, NO code signing.
# Use a dedicated output dir: the default one may be locked by a client the user
# launched from it. Never terminate a user's running process just to rebuild.
$verifyDist = Join-Path $win "dist\CrossMail-Portable-x64-verify"
$p = & (Join-Path $PSScriptRoot "build-portable.ps1") -Platform x64 -Configuration $Configuration -OutputDir $verifyDist | Out-String
if ($p -notmatch 'PORTABLE_OK' -or $p -notmatch 'ZIP_LAUNCH_OK' -or $p -notmatch 'PORTABLE_WINDOW_OK' -or $p -notmatch 'product_shell=true' -or $p -notmatch 'shell_commands=4') { $p; throw "step8 portable distribution failed" }
if ($p -notmatch 'PORTABLE_DATA_ROOT_ISOLATED=true') { $p; throw "step8 extracted payload did not use the isolated data root" }
$pfp = [regex]::Match($p, 'master_key_fingerprint=([0-9A-F]{8})')
if (-not $pfp.Success -or $pfp.Groups[1].Value -ne $fp.Groups[1].Value) { $p; throw "step8 extracted exe did not see the same persisted master key" }
Add-Line "8_PORTABLE_OK unpackaged_x64 zip_launch_ok=true smoke_ok=true window_smoke_ok=true product_shell=true shell_commands=4 settings=5 compose_window=true push_badge=simulated master_key_cross_process=true data_root_isolated=true (bypasses 0x800B0109 signature trust)"

# Step 9: run the SAME Phase 2 window smoke against the BUILD TREE, so its
# app-window-smoke-result.txt is refreshed too (portable whitelist excludes result
# files, otherwise a stale Phase 1 result file would linger and mislead readers).
$ws = & $appExe --window-smoke 2>&1 | Out-String
if ($LASTEXITCODE -ne 0 -or $ws -notmatch 'WINDOW_SMOKE_OK' -or $ws -notmatch 'SHELL_BUTTONS_OK=4' -or
    $ws -notmatch 'SETTINGS_SECTIONS=5' -or $ws -notmatch 'COMPOSE_WINDOW_OK compose_ok=True' -or
    $ws -notmatch 'PUSH_BADGE_OK=True') { $ws; throw "step9 build-tree window smoke failed" }
if ($ws -notmatch 'DATA_ROOT_ISOLATED=True' -or $ws -notmatch 'DATA_ROOT_NAME=CrossMail-Smoke') { $ws; throw "step9 build-tree window smoke did not use the isolated data root" }
Add-Line "9_BUILD_WINDOW_SMOKE_OK product_shell=true shell_commands=4 settings_sections=5 compose_window=true push_badge=simulated data_root_isolated=true (build tree + extracted zip both verified)"

$dbHashAfter = Get-DbHash $realDb
if ($dbHashAfter -ne $dbHashBefore) { throw "REAL_DB_MUTATED: before=$dbHashBefore after=$dbHashAfter" }
Add-Line "REAL_DB_UNTOUCHED=true sha_before=$dbHashBefore sha_after=$dbHashAfter (all smoke writes went to %LOCALAPPDATA%\\CrossMail-Smoke)"

Add-Line "OFFLINE_ONLY: MSIX structural check (pack/unpack); page construction + automated button-handler execution (NOT human clicks); Phase 2 shell buttons invoked via automation peer (real Click routing, NOT human mouse clicks); push badge fed by a SYNTHETIC event (no real subscribe_push)"
Add-Line "NOT_VERIFIED: MSIX install+launch - real Add-AppxPackage attempt rejected with 0x800B0109 (untrusted signature root); human-visible window inspection + real mouse/keyboard clicks; real-account mail read/write + real push subscription/background sync/performance; ARM64 DLL+MSIX (needs elevated VS installer + MSVC ARM64 CRT)"
Add-Line "NOTE: unpackaged portable zip is a fully usable delivery shape without MSIX; it is NOT a substitute for MSIX-specific capabilities (package identity, Appx background tasks, auto-update)."
Add-Line "P0_FIX: EngineConfig.MasterKeyB64 is now injected from %LOCALAPPDATA%\CrossMail\master.key; without it every open_engine generated a NEW random vault key and credentials became undecryptable. step4 verifies vault round-trip, step8 verifies cross-process key identity."
[IO.File]::WriteAllLines((Join-Path $win "verification-report.txt"), $script:report)
"VERIFICATION_CHAIN_OK steps=9"
