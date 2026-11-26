# ARM64 prerequisite installer. MUST run from an elevated (Administrator) PowerShell.
# Why: VS setup.exe refuses --quiet/--passive when not elevated ("iselevated: False", exit 5007).
# Full ARM64 unlock sequence (elevated VS x64 Native Tools prompt):
#   1. powershell -ExecutionPolicy Bypass -File win\scripts\install-arm64-tools.ps1
#   2. rustup target add aarch64-pc-windows-msvc
#   3. load vcvars for the x64_arm64 cross tools, and set INCLUDE/LIB to the
#      ARM64 MSVC CRT + Windows SDK ucrt/um arm64 directories. The FFI crate
#      has C dependencies (rusqlite/ring), so cl.exe must target arm64 too.
#   4. cargo build -p crossmail-ffi --release --target aarch64-pc-windows-msvc
#   5. dotnet build win\app\CrossMail.App\CrossMail.App.csproj -c Release -p:Platform=ARM64
#   6. powershell -File win\scripts\build-msix.ps1 -Platform ARM64 -Configuration Release
# Steps 5/6 refuse to proceed (MSB3030 / ARCH_MISMATCH) unless a real ARM64 DLL exists.
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
$isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    throw "Elevated PowerShell required. VS setup.exe --quiet returns 5007 when not elevated."
}

$setup = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\setup.exe"
$installPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
& $setup modify --installPath $installPath --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --quiet --norestart
"SETUP_EXIT=$LASTEXITCODE"

$arm64Cl = Get-ChildItem (Join-Path $installPath "VC\Tools\MSVC\*\bin\Hostx64\arm64\cl.exe") -ErrorAction SilentlyContinue
if ($arm64Cl) { "ARM64_CL_OK " + $arm64Cl[0].FullName }
else { "ARM64_CL_MISSING - installer did not add the component" }
$arm64Crt = Get-ChildItem (Join-Path $installPath "VC\Tools\MSVC\*\lib\arm64\vcruntime.lib") -ErrorAction SilentlyContinue
if ($arm64Crt) { "ARM64_CRT_OK " + $arm64Crt[0].FullName }
else { "ARM64_CRT_MISSING - linker will fail without the ARM64 CRT" }
