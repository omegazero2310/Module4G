[CmdletBinding()]
param([switch]$PurgeData)

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

$service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($service) {
    Invoke-Sc -Arguments @('stop', $serviceName) -AllowedExitCodes @(0, 1062)
    $service.WaitForStatus('Stopped', $timeout)
    Invoke-Sc -Arguments @('delete', $serviceName) -AllowedExitCodes @(0, 1060)

    $deadline = [DateTime]::UtcNow.Add($timeout)
    while ((Get-Service -Name $serviceName -ErrorAction SilentlyContinue) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
        throw "$serviceName is still registered after $($timeout.TotalSeconds) seconds. Close service-management tools and retry."
    }
}

# The combined NSIS package owns the application directory and its binaries.
# This fallback removes only the SCM registration unless data purge is explicit.
if ($PurgeData) {
    $data = Join-Path $env:ProgramData 'A7670 Modem'
    if (Test-Path -LiteralPath $data) {
        Remove-Item -LiteralPath $data -Recurse -Force
    }
}
