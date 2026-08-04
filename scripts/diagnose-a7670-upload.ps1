<#
.SYNOPSIS
Diagnoses A7670 CFTRANRX behavior directly on an explicit USB AT port.

.DESCRIPTION
The A7670ModemService must already be stopped. The script never stops or
reconfigures the service. It writes two uniquely named diagnostic files to the
modem EFS, records only metadata and sanitized modem responses, and never logs
payload bytes. Diagnostic files are intentionally retained for inspection.

.PARAMETER Port
Dedicated A7670 USB AT port, for example COM6.

.PARAMETER AmrPath
AMR acceptance fixture. Defaults to output_test.amr in the repository root.

.PARAMETER ServiceUploadTimedOut
Set only when the same AMR upload has timed out through the Windows service.
This lets the final classification distinguish a service-only failure.
#>
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^COM\d+$')]
    [string]$Port,
    [string]$AmrPath = (Join-Path $PSScriptRoot '..\output_test.amr'),
    [ValidateRange(1200, 4000000)]
    [int]$Baud = 115200,
    [string]$LogPath = (Join-Path $env:TEMP ("a7670-upload-diagnostic-{0}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))),
    [switch]$PlayLocal,
    [switch]$ServiceUploadTimedOut,
    [switch]$DtrEnable,
    [switch]$RtsEnable
)

$ErrorActionPreference = 'Stop'
$ChunkBytes = 256
$PacingMs = 50
$PromptTimeoutMs = 5000
$ResultTimeoutMs = 30000

function Format-ControlBytes {
    param(
        [AllowNull()]
        [AllowEmptyCollection()]
        [byte[]]$Bytes
    )
    if ($null -eq $Bytes -or $Bytes.Length -eq 0) {
        return '<none>'
    }
    $builder = [System.Text.StringBuilder]::new()
    foreach ($byte in $Bytes) {
        switch ($byte) {
            9 { [void]$builder.Append('<TAB>') }
            10 { [void]$builder.Append('<LF>') }
            13 { [void]$builder.Append('<CR>') }
            default {
                if ($byte -ge 32 -and $byte -le 126) {
                    [void]$builder.Append([char]$byte)
                } else {
                    [void]$builder.Append(('<0x{0:X2}>' -f $byte))
                }
            }
        }
    }
    $builder.ToString()
}

function Write-Diagnostic {
    param([string]$Phase, [string]$Detail)
    $line = '{0} phase={1} {2}' -f (Get-Date).ToUniversalTime().ToString('o'), $Phase, $Detail
    Write-Host $line
    Add-Content -LiteralPath $LogPath -Value $line -Encoding utf8
}

function Read-ModemResponse {
    param(
        [System.IO.Ports.SerialPort]$Serial,
        [int]$TimeoutMs,
        [switch]$Prompt
    )
    $received = [System.Collections.Generic.List[byte]]::new()
    $deadline = [DateTime]::UtcNow.AddMilliseconds($TimeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        try {
            $value = $Serial.ReadByte()
            if ($value -ge 0) {
                $received.Add([byte]$value)
            }
        } catch [System.TimeoutException] {
            continue
        }
        $ascii = [System.Text.Encoding]::ASCII.GetString($received.ToArray())
        if ($Prompt -and $received.Contains([byte][char]'>')) {
            break
        }
        if ($ascii -match '(?m)(^|\r|\n)(OK|ERROR|\+CME ERROR:.*|\+CMS ERROR:.*)(\r|\n|$)') {
            break
        }
    }
    # PowerShell unwraps an empty array returned from a function into $null.
    # Return a stable object so a no-response timeout remains distinguishable
    # from an implementation failure in the diagnostic itself.
    $bytes = [byte[]]$received.ToArray()
    [pscustomobject]@{
        Text = [System.Text.Encoding]::ASCII.GetString($bytes)
        Sanitized = Format-ControlBytes -Bytes $bytes
        ByteCount = $bytes.Length
        PromptReceived = $received.Contains([byte][char]'>')
    }
}

function Invoke-AtCommand {
    param(
        [System.IO.Ports.SerialPort]$Serial,
        [string]$Command,
        [int]$TimeoutMs = 5000
    )
    $Serial.DiscardInBuffer()
    $bytes = [System.Text.Encoding]::ASCII.GetBytes("$Command`r")
    $Serial.Write($bytes, 0, $bytes.Length)
    Write-Diagnostic 'command_sent' ("command={0}" -f $Command)
    $response = Read-ModemResponse -Serial $Serial -TimeoutMs $TimeoutMs
    Write-Diagnostic 'command_response' ("command={0} byte_count={1} bytes={2}" -f $Command, $response.ByteCount, $response.Sanitized)
    if ($response.Text -notmatch '(?m)(^|\r|\n)OK(\r|\n|$)') {
        if ($response.ByteCount -eq 0) {
            throw "Command timed out with no response: $Command. Verify that $Port is the dedicated USB AT port; optionally retry with -DtrEnable or -RtsEnable."
        }
        throw "Command failed or timed out: $Command"
    }
    $response.Text
}

function Send-DiagnosticFile {
    param(
        [System.IO.Ports.SerialPort]$Serial,
        [string]$Name,
        [byte[]]$Data
    )
    $path = "C:/$Name"
    $command = 'AT+CFTRANRX="{0}",{1}' -f $path, $Data.Length
    $Serial.DiscardInBuffer()
    $commandBytes = [System.Text.Encoding]::ASCII.GetBytes("$command`r")
    $started = [System.Diagnostics.Stopwatch]::StartNew()
    $Serial.Write($commandBytes, 0, $commandBytes.Length)
    Write-Diagnostic 'upload_command' ("declared_bytes={0}" -f $Data.Length)

    $prompt = Read-ModemResponse -Serial $Serial -TimeoutMs $PromptTimeoutMs -Prompt
    Write-Diagnostic 'upload_prompt' ("elapsed_ms={0} byte_count={1} bytes={2}" -f $started.ElapsedMilliseconds, $prompt.ByteCount, $prompt.Sanitized)
    if (-not $prompt.PromptReceived) {
        return [pscustomobject]@{ Name = $Name; Success = $false; Stage = 'prompt'; Size = $Data.Length }
    }

    $chunks = 0
    for ($offset = 0; $offset -lt $Data.Length; $offset += $ChunkBytes) {
        $count = [Math]::Min($ChunkBytes, $Data.Length - $offset)
        $Serial.Write($Data, $offset, $count)
        $chunks++
        if ($offset + $count -lt $Data.Length) {
            Start-Sleep -Milliseconds $PacingMs
        }
    }
    Write-Diagnostic 'upload_payload' ("bytes={0} chunks={1} chunk_bytes={2} pacing_ms={3} elapsed_ms={4}" -f $Data.Length, $chunks, $ChunkBytes, $PacingMs, $started.ElapsedMilliseconds)

    $result = Read-ModemResponse -Serial $Serial -TimeoutMs $ResultTimeoutMs
    Write-Diagnostic 'upload_result' ("elapsed_ms={0} byte_count={1} bytes={2}" -f $started.ElapsedMilliseconds, $result.ByteCount, $result.Sanitized)
    if ($result.Text -notmatch '(?m)(^|\r|\n)OK(\r|\n|$)') {
        return [pscustomobject]@{ Name = $Name; Success = $false; Stage = 'result'; Size = $Data.Length }
    }

    [void](Invoke-AtCommand -Serial $Serial -Command 'AT+FSCD=C:')
    $attributes = Invoke-AtCommand -Serial $Serial -Command ('AT+FSATTRI="{0}"' -f $Name)
    $match = [regex]::Match($attributes, '\+FSATTRI:\s*(\d+)')
    $verified = $match.Success -and [int64]$match.Groups[1].Value -eq $Data.Length
    Write-Diagnostic 'size_verification' ("expected_bytes={0} reported_bytes={1} status={2}" -f $Data.Length, $(if ($match.Success) { $match.Groups[1].Value } else { 'missing' }), $(if ($verified) { 'ok' } else { 'mismatch' }))
    [pscustomobject]@{ Name = $Name; Success = $verified; Stage = $(if ($verified) { 'complete' } else { 'verification' }); Size = $Data.Length }
}

$service = Get-Service -Name 'A7670ModemService' -ErrorAction SilentlyContinue
if ($service -and $service.Status -ne 'Stopped') {
    throw 'A7670ModemService must be stopped before opening the dedicated AT port.'
}
$AmrPath = [System.IO.Path]::GetFullPath($AmrPath)
if (-not (Test-Path -LiteralPath $AmrPath -PathType Leaf)) {
    throw "AMR fixture not found: $AmrPath"
}
$LogPath = [System.IO.Path]::GetFullPath($LogPath)
$logDirectory = Split-Path -Parent $LogPath
if (-not (Test-Path -LiteralPath $logDirectory -PathType Container)) {
    New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
}

$serial = [System.IO.Ports.SerialPort]::new($Port, $Baud, 'None', 8, 'One')
$serial.Handshake = 'None'
$serial.ReadTimeout = 100
$serial.WriteTimeout = 5000
$serial.DtrEnable = $DtrEnable.IsPresent
$serial.RtsEnable = $RtsEnable.IsPresent

try {
    $serial.Open()
    Write-Diagnostic 'session' ("port={0} baud={1} chunk_bytes={2} pacing_ms={3} dtr={4} rts={5}" -f $Port, $Baud, $ChunkBytes, $PacingMs, $serial.DtrEnable, $serial.RtsEnable)
    foreach ($command in @('ATI', 'AT+CGMM', 'AT+CGMR', 'AT+CFTRANRX=?', 'AT+CCMXPLAY=?', 'AT+FSCD?', 'AT+FSMEM')) {
        [void](Invoke-AtCommand -Serial $serial -Command $command)
    }

    $suffix = [guid]::NewGuid().ToString('N').Substring(0, 12)
    $probeName = "diag_probe_$suffix.txt"
    $amrName = "diag_audio_$suffix.amr"
    $probeBytes = [System.Text.Encoding]::ASCII.GetBytes([guid]::NewGuid().ToString('N').Substring(0, 10))
    $amrBytes = [System.IO.File]::ReadAllBytes($AmrPath)
    $probeResult = Send-DiagnosticFile -Serial $serial -Name $probeName -Data $probeBytes
    $amrResult = Send-DiagnosticFile -Serial $serial -Name $amrName -Data $amrBytes

    if ($PlayLocal -and $amrResult.Success) {
        [void](Invoke-AtCommand -Serial $serial -Command ('AT+CCMXPLAY="C:/{0}",0,0' -f $amrName))
        Write-Diagnostic 'local_playback' 'status=started'
    }

    $probeTimedOut = $probeResult.Stage -in @('prompt', 'result')
    $amrTimedOut = $amrResult.Stage -in @('prompt', 'result')
    $classification = if ($probeTimedOut -and $amrTimedOut) {
        'firmware_or_usb_transfer_behavior'
    } elseif ($probeResult.Success -and $amrResult.Success -and $ServiceUploadTimedOut) {
        'rust_transport_or_parser_defect'
    } elseif ($probeResult.Success -and $amrResult.Success) {
        'direct_transport_passed'
    } else {
        'size_dependent_or_inconclusive'
    }
    Write-Diagnostic 'classification' ("result={0} probe_stage={1} amr_stage={2} amr_bytes={3}" -f $classification, $probeResult.Stage, $amrResult.Stage, $amrBytes.Length)
    Write-Host "Diagnostic log: $LogPath"
} finally {
    if ($serial.IsOpen) {
        $serial.Close()
    }
    $serial.Dispose()
}
