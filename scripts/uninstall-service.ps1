param([switch]$PurgeData)
$ErrorActionPreference = "Stop"
if (Get-Service A7670ModemService -ErrorAction SilentlyContinue) {
    Stop-Service A7670ModemService -Force
    sc.exe delete A7670ModemService | Out-Null
}
$install = Join-Path $env:ProgramFiles "A7670 Modem"
if (Test-Path -LiteralPath $install) { Remove-Item -LiteralPath $install -Recurse -Force }
if ($PurgeData) {
    $data = Join-Path $env:ProgramData "A7670 Modem"
    if (Test-Path -LiteralPath $data) { Remove-Item -LiteralPath $data -Recurse -Force }
}

