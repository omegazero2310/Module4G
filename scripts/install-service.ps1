param([string]$Binary = "$PSScriptRoot\..\target\release\modemd.exe")
$ErrorActionPreference = "Stop"
$Binary = [System.IO.Path]::GetFullPath($Binary)
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "Service binary not found: $Binary`nBuild it first with: cargo build -p modemd --release`nNote: modem-sim.exe is the development simulator and cannot be installed as A7670ModemService."
}
$target = Join-Path $env:ProgramFiles "A7670 Modem\modemd.exe"
$data = Join-Path $env:ProgramData "A7670 Modem"
New-Item -ItemType Directory -Force (Split-Path $target), $data | Out-Null
$existing = Get-Service A7670ModemService -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.Status -ne 'Stopped') {
        Stop-Service A7670ModemService -Force
        $existing.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
    }
}
Copy-Item -LiteralPath $Binary -Destination $target -Force
if ($existing) {
    sc.exe config A7670ModemService binPath= "`"$target`"" start= delayed-auto obj= "NT AUTHORITY\LocalService" | Out-Null
} else {
    sc.exe create A7670ModemService binPath= "`"$target`"" start= delayed-auto obj= "NT AUTHORITY\LocalService" | Out-Null
}
sc.exe failure A7670ModemService reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
Start-Service A7670ModemService
