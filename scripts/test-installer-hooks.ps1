[CmdletBinding()]
param(
    [string]$Makensis = "$PSScriptRoot\..\target\.tauri\NSIS\makensis.exe",
    [string]$OutputDirectory = "$PSScriptRoot\..\target\installer-hook-tests"
)

$ErrorActionPreference = 'Stop'
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force $OutputDirectory | Out-Null
$fixtureSource = Join-Path $OutputDirectory 'fixture.rs'
@'
use std::{env, fs::{self, OpenOptions}, io::Write, path::PathBuf};
fn main() {
    let root = env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let args: Vec<String> = env::args().skip(1).collect();
    let mut log = OpenOptions::new().create(true).append(true).open(root.join("commands.log")).unwrap();
    writeln!(log, "{}", args.join("\t")).unwrap();
    let marker: PathBuf = root.join("registered");
    let mut code = 0;
    if args.first().map(String::as_str) == Some("sc") {
        match args.get(1).map(String::as_str) {
            Some("query") if !marker.exists() => code = 1060,
            Some("create") => { fs::write(marker, b"registered").unwrap(); },
            Some("delete") => { fs::remove_file(marker).unwrap(); },
            Some("stop") => code = 1062,
            _ => {}
        }
    }
    std::process::exit(code);
}
'@ | Set-Content -LiteralPath $fixtureSource
& rustc $fixtureSource -o (Join-Path $OutputDirectory 'fixture.exe')
if ($LASTEXITCODE -ne 0) { throw 'Failed to build installer command fixture.' }

# Compile the real hooks, replacing only external effects with argument-recording fixtures.
$hooks = Get-Content -LiteralPath "$PSScriptRoot\..\modem-app\src-tauri\nsis\installer-hooks.nsh" -Raw
$hooks = $hooks.Replace('"$SYSDIR\sc.exe"', '"$EXEDIR\fixture.exe" sc')
$hooks = $hooks.Replace('"$SYSDIR\icacls.exe"', '"$EXEDIR\fixture.exe" acl')
$hooks = $hooks.Replace('"$SYSDIR\pnputil.exe"', '"$EXEDIR\fixture.exe" driver')
$hooks = $hooks.Replace('CreateDirectory ', 'DetailPrint ')
$hooks = [regex]::Replace($hooks, 'nsExec::ExecToStack [^\r\n]*WaitForStatus[^\r\n]*', 'nsExec::ExecToStack ''"$EXEDIR\fixture.exe" wait''')
$hookPath = Join-Path $OutputDirectory 'fixture-hooks.nsh'
$hooks | Set-Content -LiteralPath $hookPath
$testScript = @'
Unicode true
Name "Installer hooks regression"
OutFile "@OUTPUT@\hooks-test.exe"
RequestExecutionLevel user
!include "@OUTPUT@\fixture-hooks.nsh"
Section Install
  SetShellVarContext current
  StrCpy $INSTDIR "$EXEDIR\Program Files\A7670 Modem"
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro NSIS_HOOK_POSTINSTALL
  WriteUninstaller "$EXEDIR\hooks-uninstall.exe"
SectionEnd
Section Uninstall
  !insertmacro NSIS_HOOK_PREUNINSTALL
SectionEnd
'@
$testPath = Join-Path $OutputDirectory 'hooks-test.nsi'
$testScript.Replace('@OUTPUT@', $OutputDirectory) | Set-Content -LiteralPath $testPath
& $Makensis /WX $testPath
if ($LASTEXITCODE -ne 0) { throw 'Installer hooks failed warnings-as-errors compilation.' }

# Only these test-owned files are reset; the harness never calls SCM, icacls or pnputil.
foreach ($name in @('registered', 'commands.log')) {
    $path = Join-Path $OutputDirectory $name
    if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
}
foreach ($attempt in 1..2) {
    $process = Start-Process -FilePath (Join-Path $OutputDirectory 'hooks-test.exe') -ArgumentList '/S' -WindowStyle Hidden -PassThru
    if (-not $process.WaitForExit(60000)) { $process.Kill(); throw "Installer harness timed out on attempt $attempt." }
    if ($process.ExitCode -ne 0) { throw "Installer harness failed on attempt $attempt ($($process.ExitCode))." }
}
$commands = @(Get-Content -LiteralPath (Join-Path $OutputDirectory 'commands.log'))
$expectedPath = '"' + (Join-Path $OutputDirectory 'Program Files\A7670 Modem\modemd.exe') + '"'
foreach ($action in @('create', 'config')) {
    $line = @($commands | Where-Object { $_.StartsWith("sc`t$action`t") })
    if ($line.Count -ne 1 -or -not $line[0].Contains("binPath=`t$expectedPath`tstart=`tdelayed-auto`tobj=`tNT AUTHORITY\LocalService")) {
        throw "Service $action did not preserve quoted executable path, startup or account."
    }
}
$expectedData = Join-Path $env:ProgramData 'A7670 Modem'
if (@($commands | Where-Object { $_.StartsWith("acl`t$expectedData`t/grant`t*S-1-5-19:(OI)(CI)M") }).Count -ne 2) {
    throw 'Install and reinstall must grant LocalService access to the actual ProgramData directory.'
}
$drivers = @($commands | Where-Object { $_.StartsWith("driver`t") })
if ($drivers.Count -ne 4 -or $drivers[0] -notmatch 'simfilter\.inf' -or $drivers[1] -notmatch 'simser\.inf' -or $drivers[2] -notmatch 'simfilter\.inf' -or $drivers[3] -notmatch 'simser\.inf') {
    throw 'Driver order changed between install and reinstall.'
}
if (@($commands | Where-Object { $_.StartsWith("sc`tstart`t") }).Count -ne 2 -or @($commands | Where-Object { $_.StartsWith("sc`tstop`t") }).Count -ne 1) {
    throw 'Install must start the service; reinstall must stop and restart it.'
}
Write-Output 'Installer hook regression passed: fresh install, reinstall, ProgramData ACL, quoted binary path, LocalService, delayed start and driver ordering.'
