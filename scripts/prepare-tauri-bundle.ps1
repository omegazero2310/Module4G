[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$manifest = Join-Path $workspace 'Cargo.toml'
$source = Join-Path $workspace 'target\release\modemd.exe'
$bundleDirectory = Join-Path $workspace 'modem-app\src-tauri\binaries'
$bundleBinary = Join-Path $bundleDirectory 'modemd-x86_64-pc-windows-msvc.exe'

& (Join-Path $PSScriptRoot 'validate-simcom-driver.ps1')

& cargo build --manifest-path $manifest -p modemd --release
if ($LASTEXITCODE -ne 0) {
    throw "Failed to build modemd.exe (cargo exit code $LASTEXITCODE)."
}
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "Cargo completed without producing the required daemon: $source"
}

New-Item -ItemType Directory -Path $bundleDirectory -Force | Out-Null
Copy-Item -LiteralPath $source -Destination $bundleBinary -Force
if (-not (Test-Path -LiteralPath $bundleBinary -PathType Leaf)) {
    throw "Failed to stage the daemon for the Tauri bundle: $bundleBinary"
}
