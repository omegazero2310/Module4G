[CmdletBinding()]
param(
    [string]$SourceZip
)

$ErrorActionPreference = 'Stop'
$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$driverRoot = Join-Path $workspace 'third_party\simcom\windows10-x64-serial'
$manifestPath = Join-Path $driverRoot 'manifest.json'

if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "SIMCom driver manifest is missing: $manifestPath"
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$expectedPayload = @($manifest.files.PSObject.Properties.Name | ForEach-Object { $_ -replace '/', '\' })
$actualPayload = @(Get-ChildItem -LiteralPath $driverRoot -Recurse -File | Where-Object {
    $_.Extension -in '.inf', '.cat', '.sys'
} | ForEach-Object {
    $_.FullName.Substring($driverRoot.Length).TrimStart('\')
})

$unexpected = @($actualPayload | Where-Object { $_ -notin $expectedPayload })
if ($unexpected.Count -ne 0) {
    throw "Unexpected SIMCom driver payload file(s): $($unexpected -join ', ')"
}

foreach ($entry in $manifest.files.PSObject.Properties) {
    $relativePath = $entry.Name -replace '/', '\'
    $path = Join-Path $driverRoot $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required SIMCom driver file is missing: $relativePath"
    }

    $actualHash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    if ($actualHash -ne $entry.Value) {
        throw "SIMCom driver hash mismatch for $relativePath. Expected $($entry.Value), got $actualHash. Restore the unmodified vendor file."
    }
}

if ($actualPayload.Count -ne $expectedPayload.Count) {
    throw 'The SIMCom driver payload is incomplete.'
}

foreach ($catalog in 'simlteusbfilter.cat', 'simlteusbser.cat') {
    $signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $driverRoot $catalog)
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "SIMCom catalog signature is not valid for $catalog ($($signature.Status)): $($signature.StatusMessage)"
    }
    if ($signature.SignerCertificate.Subject -notmatch 'Microsoft Windows Hardware Compatibility Publisher') {
        throw "SIMCom catalog $catalog is not signed by Microsoft Windows Hardware Compatibility Publisher."
    }
}

$filterInf = Get-Content -LiteralPath (Join-Path $driverRoot 'simfilter.inf') -Raw
$serialInf = Get-Content -LiteralPath (Join-Path $driverRoot 'simser.inf') -Raw
if ($filterInf -notmatch 'USB\\VID_1E0E&PID_9011') {
    throw 'simfilter.inf does not cover USB\VID_1E0E&PID_9011.'
}
if ($serialInf -notmatch 'USB\\VID_1E0E&PID_9011&MI_04') {
    throw 'simser.inf does not cover the USB\VID_1E0E&PID_9011 AT-port interface (MI_04).'
}

if ($SourceZip) {
    $resolvedZip = (Resolve-Path -LiteralPath $SourceZip).Path
    $sourceHash = (Get-FileHash -LiteralPath $resolvedZip -Algorithm SHA256).Hash
    if ($sourceHash -ne $manifest.sourceArchiveSha256) {
        throw "SIMCom source ZIP hash mismatch. Expected $($manifest.sourceArchiveSha256), got $sourceHash."
    }
}

Write-Host "SIMCom Windows 10 x64 serial driver payload verified ($($expectedPayload.Count) vendor files)."
