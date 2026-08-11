[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$SourceZip
)

$ErrorActionPreference = 'Stop'
$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$driverRoot = Join-Path $workspace 'third_party\simcom\windows10-x64-serial'
$manifestPath = Join-Path $driverRoot 'manifest.json'

if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "SIMCom driver manifest is missing: $manifestPath"
}

$resolvedZip = (Resolve-Path -LiteralPath $SourceZip -ErrorAction Stop).Path
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$sourceHash = (Get-FileHash -LiteralPath $resolvedZip -Algorithm SHA256).Hash
if ($sourceHash -ne $manifest.sourceArchiveSha256) {
    throw "SIMCom source ZIP hash mismatch. Expected $($manifest.sourceArchiveSha256), got $sourceHash."
}

$temporaryRoot = [System.IO.Path]::GetFullPath((Join-Path ([System.IO.Path]::GetTempPath()) ("a7670-simcom-driver-{0}" -f [Guid]::NewGuid().ToString('N'))))

try {
    New-Item -ItemType Directory -Path $temporaryRoot | Out-Null
    Expand-Archive -LiteralPath $resolvedZip -DestinationPath $temporaryRoot
    $sourceRoot = Join-Path $temporaryRoot 'Windows10'

    foreach ($entry in $manifest.files.PSObject.Properties) {
        $relativePath = $entry.Name -replace '/', '\'
        $source = Join-Path $sourceRoot $relativePath
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Required file is missing from the SIMCom source ZIP: Windows10\$relativePath"
        }
    }

    foreach ($entry in $manifest.files.PSObject.Properties) {
        $relativePath = $entry.Name -replace '/', '\'
        $source = Join-Path $sourceRoot $relativePath
        $destination = Join-Path $driverRoot $relativePath
        $destinationDirectory = Split-Path -Parent $destination
        New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
        Copy-Item -LiteralPath $source -Destination $destination -Force
    }

    & (Join-Path $PSScriptRoot 'validate-simcom-driver.ps1') -SourceZip $resolvedZip
    Write-Host "SIMCom driver payload is ready for bundling at: $driverRoot"
}
finally {
    $systemTemporaryRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    if ((Test-Path -LiteralPath $temporaryRoot) -and
        $temporaryRoot.StartsWith($systemTemporaryRoot, [System.StringComparison]::OrdinalIgnoreCase) -and
        ([System.IO.Path]::GetFileName($temporaryRoot) -match '^a7670-simcom-driver-[0-9a-f]{32}$')) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force
    }
}
