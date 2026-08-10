[CmdletBinding()]
param([string]$Binary = "$PSScriptRoot\..\target\release\modemd.exe")

$ErrorActionPreference = 'Stop'
$serviceName = 'A7670ModemService'
$timeout = [TimeSpan]::FromSeconds(30)

function Invoke-Sc {
    param(
        [Parameter(Mandatory)] [string[]]$Arguments,
        [int[]]$AllowedExitCodes = @(0)
    )

    $output = & sc.exe @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    if ($exitCode -notin $AllowedExitCodes) {
        throw "sc.exe $($Arguments[0]) failed with exit code $exitCode.`n$($output -join [Environment]::NewLine)"
    }
}

$Binary = [System.IO.Path]::GetFullPath($Binary)
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "Service binary not found: $Binary`nBuild it first with: cargo build -p modemd --release`nNote: modem-sim.exe is the development simulator and cannot be installed as A7670ModemService."
}

$target = Join-Path $env:ProgramFiles 'A7670 Modem\modemd.exe'
$data = Join-Path $env:ProgramData 'A7670 Modem'
New-Item -ItemType Directory -Force (Split-Path $target), $data | Out-Null
$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing) {
    Invoke-Sc -Arguments @('stop', $serviceName) -AllowedExitCodes @(0, 1062)
    $existing.WaitForStatus('Stopped', $timeout)
}

Copy-Item -LiteralPath $Binary -Destination $target -Force
$quotedTarget = '"{0}"' -f $target
if ($existing) {
    Invoke-Sc -Arguments @('config', $serviceName, "binPath=", $quotedTarget, 'start=', 'delayed-auto', 'obj=', 'NT AUTHORITY\LocalService')
} else {
    Invoke-Sc -Arguments @('create', $serviceName, "binPath=", $quotedTarget, 'start=', 'delayed-auto', 'obj=', 'NT AUTHORITY\LocalService', 'DisplayName=', 'A7670 Modem Service')
}
Invoke-Sc -Arguments @('failure', $serviceName, 'reset=', '86400', 'actions=', 'restart/5000/restart/15000/restart/60000')
Invoke-Sc -Arguments @('start', $serviceName)
(Get-Service -Name $serviceName -ErrorAction Stop).WaitForStatus('Running', $timeout)
