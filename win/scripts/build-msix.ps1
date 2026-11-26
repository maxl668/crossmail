# CrossMail MSIX packaging for Windows (x64 / ARM64).
# Produces a real .msix with makeappx.exe and signs it with a dev self-signed cert.
# This script does NOT claim installation or launch on a real device.
param(
    [string]$Platform = "x64",
    [string]$Configuration = "Release"
)
$ErrorActionPreference = 'Stop'
$win = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $win "app\CrossMail.App\bin\$Platform\$Configuration\net8.0-windows10.0.19041.0"
$pkgDir = Join-Path $win "packaging"
$stage = Join-Path $pkgDir "stage-$Platform"
$msix = Join-Path $pkgDir "CrossMail_$Platform.msix"
$pfx = Join-Path $pkgDir "CrossMail-Dev.pfx"
$sdkBin = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64"
$makeappx = Join-Path $sdkBin "makeappx.exe"
$signtool = Join-Path $sdkBin "signtool.exe"

if (-not (Test-Path (Join-Path $bin "CrossMail.App.exe"))) { throw "exe not found: $bin" }
if (-not (Test-Path (Join-Path $bin "crossmail_ffi.dll"))) { throw "native dll not found in: $bin" }
if (-not (Test-Path $makeappx)) { throw "makeappx.exe not found" }

# Guard against cross-architecture leaks: apphost and native DLL must both match $Platform.
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
        throw ("ARCH_MISMATCH: {0} machine=0x{1:X4}, expected 0x{2:X4} for {3}" -f $peName, $machine, $expectedMachine, $Platform)
    }
}
"ARCH_OK platform=$Platform machine=0x$($expectedMachine.ToString('X4')) (exe + native dll)"

New-Item -ItemType Directory -Force -Path $pkgDir, $stage, (Join-Path $stage "Assets") | Out-Null
# Overlay the current build output; makeappx /o overwrites the package. No destructive cleanup here.
Copy-Item -Path (Join-Path $bin "*") -Destination $stage -Recurse -Force

# Minimal real PNG bytes so the manifest asset references resolve inside the package.
$png = [Convert]::FromBase64String("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==")
foreach ($assetName in @("StoreLogo.png", "Square150x150Logo.png", "Square44x44Logo.png")) {
    [IO.File]::WriteAllBytes((Join-Path $stage "Assets\$assetName"), $png)
}

$arch = if ($Platform -eq "ARM64") { "arm64" } else { "x64" }
$manifest = @"
<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  IgnorableNamespaces="uap rescap">
  <Identity Name="CrossMail.Windows" Publisher="CN=CrossMail Dev" Version="0.1.0.0" ProcessorArchitecture="$arch" />
  <Properties>
    <DisplayName>CrossMail</DisplayName>
    <PublisherDisplayName>CrossMail</PublisherDisplayName>
    <Logo>Assets\StoreLogo.png</Logo>
    <Description>CrossMail Windows client</Description>
  </Properties>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.17763.0" MaxVersionTested="10.0.26100.0" />
    <PackageDependency Name="Microsoft.WindowsAppRuntime.1.8" MinVersion="8000.994.2142.0" Publisher="CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US" />
  </Dependencies>
  <Resources><Resource Language="zh-CN" /></Resources>
  <Applications>
    <Application Id="App" Executable="CrossMail.App.exe" EntryPoint="Windows.FullTrustApplication">
      <uap:VisualElements DisplayName="CrossMail" Description="CrossMail Windows client"
        Square150x150Logo="Assets\Square150x150Logo.png" Square44x44Logo="Assets\Square44x44Logo.png" BackgroundColor="transparent" />
    </Application>
  </Applications>
  <Capabilities><rescap:Capability Name="runFullTrust" /></Capabilities>
</Package>
"@
[IO.File]::WriteAllText((Join-Path $stage "AppxManifest.xml"), $manifest, [System.Text.UTF8Encoding]::new($false))

$packOut = & $makeappx pack /d $stage /p $msix /o 2>&1
if ($LASTEXITCODE -ne 0) { $packOut; throw "makeappx pack failed: $LASTEXITCODE" }
"PACK_OK $((Get-Item $msix).Length) bytes -> $msix"

$cert = Get-ChildItem "Cert:\CurrentUser\My" | Where-Object { $_.Subject -eq "CN=CrossMail Dev" } | Select-Object -First 1
if ($null -eq $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=CrossMail Dev" `
        -CertStoreLocation "Cert:\CurrentUser\My" -KeyAlgorithm RSA -KeyLength 2048 `
        -HashAlgorithm SHA256 -KeyExportPolicy Exportable
}
$pwd = ConvertTo-SecureString -String "CrossMailDevOnly" -Force -AsPlainText
if ([IO.File]::Exists($pfx)) { [IO.File]::Delete($pfx) }
Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $pwd | Out-Null
& $signtool sign /fd SHA256 /f $pfx /p "CrossMailDevOnly" $msix
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed: $LASTEXITCODE" }
"SIGN_OK"

# A dev self-signed root is not OS-trusted, so /pa verify is expected to fail.
# Record the chain status honestly instead of treating it as a build error.
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$verifyOut = & $signtool verify /pa $msix 2>&1
$chainOk = ($LASTEXITCODE -eq 0)
$ErrorActionPreference = $prevEap
if ($chainOk) { "SIGN_CHAIN_OK" }
else { "SIGN_CHAIN_UNTRUSTED expected for dev self-signed cert" }

# Structural verification: unpack the real msix and check payload entries exist.
$unpack = Join-Path $pkgDir "verify-$Platform"
& $makeappx unpack /p $msix /d $unpack /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx unpack failed: $LASTEXITCODE" }
foreach ($payload in @("AppxManifest.xml", "CrossMail.App.exe", "crossmail_ffi.dll", "CrossMail.Bindings.dll")) {
    if (-not (Test-Path (Join-Path $unpack $payload))) { throw "missing payload in msix: $payload" }
}
"UNPACK_OK payload_verified=4/4"
"NOT_VERIFIED: MSIX install + launch on a real device (self-signed cert is not OS-trusted)"
