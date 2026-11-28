param([Parameter(Mandatory=$true)][string]$WorkDir)
# Download official packages into an explicitly chosen developer directory.
# This script does not change Developer Mode, drivers, or machine settings.
$ErrorActionPreference = 'Stop'
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
New-Item -ItemType Directory -Force $WorkDir | Out-Null
$dxcZip = Join-Path $WorkDir 'dxc.zip'
$sdkZip = Join-Path $WorkDir 'agility.zip'
Invoke-WebRequest 'https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.10.2605.37/dxc_2026_08_11.zip' -OutFile $dxcZip
Invoke-WebRequest 'https://api.nuget.org/v3-flatcontainer/microsoft.direct3d.d3d12/1.722.0-preview/microsoft.direct3d.d3d12.1.722.0-preview.nupkg' -OutFile $sdkZip
Expand-Archive $dxcZip (Join-Path $WorkDir 'dxc') -Force
Expand-Archive $sdkZip (Join-Path $WorkDir 'agility') -Force
$env:ERIKA_SR_D3D12_LINALG = '1'
$env:ERIKA_D3D12_AGILITY_SDK_PATH = Join-Path $WorkDir 'agility\build\native\bin\x64'
$env:ERIKA_D3D12_DXCOMPILER_PATH = Join-Path $WorkDir 'dxc\bin\x64\dxcompiler.dll'
$env:ERIKA_D3D12_DXC_INCLUDE_PATH = Join-Path $WorkDir 'dxc\inc\hlsl'
Write-Output 'Preview paths configured for this PowerShell process. Run Erika or cargo from this process.'
& (Join-Path $WorkDir 'dxc\bin\x64\dxc.exe') --version
