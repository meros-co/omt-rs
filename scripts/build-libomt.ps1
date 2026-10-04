<#
    Builds the official OMT libraries (libvmx, libomtnet, libomt) from source
    for the interop tests, on Windows x64.

        powershell -ExecutionPolicy Bypass -File scripts\build-libomt.ps1 [-Out dir]
        $env:OMT_LIB_DIR = "target\libomt-ref"; cargo test --features interop --test interop

    Needs git, the .NET 8 SDK (NativeAOT) and Visual Studio's C++ tools.
    Upstream publishes no binaries, so this is the only route.
#>
param([string]$Out = (Join-Path (Split-Path -Parent $PSScriptRoot) "target\libomt-ref"))
$ErrorActionPreference = "Stop"
$work = Join-Path $Out "_src"
New-Item -ItemType Directory -Force $work | Out-Null

# NativeAOT's ILCompiler finds the MSVC linker through vswhere, by bare name.
$installer = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"
if (Test-Path (Join-Path $installer "vswhere.exe")) { $env:PATH = "$installer;$env:PATH" }

# Some machines only have Visual Studio's offline NuGet feed configured.
@'
<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" protocolVersion="3" />
  </packageSources>
</configuration>
'@ | Set-Content -Encoding utf8 (Join-Path $work "nuget.config")

foreach ($repo in "libvmx", "libomtnet", "libomt") {
    $dir = Join-Path $work $repo
    if (Test-Path (Join-Path $dir ".git")) { git -C $dir pull --ff-only }
    else { git clone --depth 1 "https://github.com/openmediatransport/$repo.git" $dir }
    if ($LASTEXITCODE -ne 0) { throw "git failed for $repo" }
}

# libvmx: cl inside a VS developer environment, as upstream's buildwinx64.cmd.
$vswhere = Join-Path $installer "vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw "Visual Studio with the C++ tools is required." }
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
Push-Location (Join-Path $work "libvmx")
try {
    $cl = "cl /nologo /O2 /std:c++17 /EHsc /arch:AVX2 /LD src\vmxcodec_x86.cpp src\vmxcodec_avx2.cpp src\vmxcodec.cpp /Fe:libvmx.dll /link /DEF:exports.def"
    cmd /c "`"$vcvars`" && $cl"
    if (-not (Test-Path "libvmx.dll")) { throw "libvmx.dll was not produced" }
    Copy-Item "libvmx.dll" $Out -Force
} finally { Pop-Location }

dotnet build (Join-Path $work "libomtnet\libomtnet.sln") -c Release
if ($LASTEXITCODE -ne 0) { throw "libomtnet build failed" }
dotnet publish (Join-Path $work "libomt\libomt.sln") -r win-x64 -c Release
if ($LASTEXITCODE -ne 0) { throw "libomt publish failed" }
$dll = Get-ChildItem -Path (Join-Path $work "libomt") -Recurse -Filter "libomt.dll" |
    Where-Object { $_.FullName -match "publish" } | Select-Object -First 1
if (-not $dll) { throw "libomt.dll was not produced" }
Copy-Item $dll.FullName $Out -Force
Write-Host "libomt + libvmx in $Out"
