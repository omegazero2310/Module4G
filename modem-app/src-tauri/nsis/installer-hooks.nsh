!include "LogicLib.nsh"

!define MODEMD_SERVICE "A7670ModemService"
!define MODEMD_WAIT_SECONDS 30

!macro ModemdQueryService PREFIX
  nsExec::ExecToStack '"$SYSDIR\sc.exe" query "${MODEMD_SERVICE}"'
  Pop $0
  Pop $1
  ${If} $0 == 0
    Push 1
  ${ElseIf} $0 == 1060
    Push 0
  ${Else}
    MessageBox MB_OK|MB_ICONSTOP "Unable to query ${MODEMD_SERVICE} (sc.exe exit code $0).$\r$\n$1"
    Abort
  ${EndIf}
!macroend

!macro ModemdWaitForStatus STATUS CONTEXT
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "try { (Get-Service -Name ${MODEMD_SERVICE} -ErrorAction Stop).WaitForStatus('${STATUS}', [TimeSpan]::FromSeconds(${MODEMD_WAIT_SECONDS})); exit 0 } catch { exit 1 }"'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "${CONTEXT} did not complete within ${MODEMD_WAIT_SECONDS} seconds. Setup cannot safely continue."
    Abort
  ${EndIf}
!macroend

Function ModemdStopBeforeInstall
  !insertmacro ModemdQueryService ""
  Pop $2
  ${If} $2 == 0
    Return
  ${EndIf}

  nsExec::ExecToStack '"$SYSDIR\sc.exe" stop "${MODEMD_SERVICE}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
  ${AndIf} $0 != 1062
    MessageBox MB_OK|MB_ICONSTOP "Unable to stop ${MODEMD_SERVICE} (sc.exe exit code $0). Close any service-management tools and try again.$\r$\n$1"
    Abort
  ${EndIf}
  !insertmacro ModemdWaitForStatus "Stopped" "Stopping ${MODEMD_SERVICE}"
FunctionEnd

Function ModemdInstallAndStart
  !insertmacro ModemdQueryService ""
  Pop $2
  ${If} $2 == 1
    nsExec::ExecToStack '"$SYSDIR\sc.exe" config "${MODEMD_SERVICE}" binPath= $\"$INSTDIR\modemd.exe$\" start= delayed-auto obj= $\"NT AUTHORITY\LocalService$\"'
  ${Else}
    nsExec::ExecToStack '"$SYSDIR\sc.exe" create "${MODEMD_SERVICE}" binPath= $\"$INSTDIR\modemd.exe$\" start= delayed-auto obj= $\"NT AUTHORITY\LocalService$\" DisplayName= $\"A7670 Modem Service$\"'
  ${EndIf}
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Unable to register ${MODEMD_SERVICE} (sc.exe exit code $0).$\r$\n$1"
    Abort
  ${EndIf}

  nsExec::ExecToStack '"$SYSDIR\sc.exe" failure "${MODEMD_SERVICE}" reset= 86400 actions= restart/5000/restart/15000/restart/60000'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Unable to configure recovery for ${MODEMD_SERVICE} (sc.exe exit code $0).$\r$\n$1"
    Abort
  ${EndIf}

  nsExec::ExecToStack '"$SYSDIR\sc.exe" start "${MODEMD_SERVICE}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Unable to start ${MODEMD_SERVICE} (sc.exe exit code $0). Check the Windows Event Log for service startup details.$\r$\n$1"
    Abort
  ${EndIf}
  !insertmacro ModemdWaitForStatus "Running" "Starting ${MODEMD_SERVICE}"
FunctionEnd

Function un.ModemdServiceExists
  !insertmacro ModemdQueryService "un."
FunctionEnd

Function un.ModemdStopAndDelete
  Call un.ModemdServiceExists
  Pop $2
  ${If} $2 == 0
    Return
  ${EndIf}

  nsExec::ExecToStack '"$SYSDIR\sc.exe" stop "${MODEMD_SERVICE}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
  ${AndIf} $0 != 1062
    MessageBox MB_OK|MB_ICONSTOP "Unable to stop ${MODEMD_SERVICE} (sc.exe exit code $0). Uninstall cannot safely remove the daemon.$\r$\n$1"
    Abort
  ${EndIf}
  !insertmacro ModemdWaitForStatus "Stopped" "Stopping ${MODEMD_SERVICE}"

  nsExec::ExecToStack '"$SYSDIR\sc.exe" delete "${MODEMD_SERVICE}"'
  Pop $0
  Pop $1
  ${If} $0 != 0
  ${AndIf} $0 != 1060
    MessageBox MB_OK|MB_ICONSTOP "Unable to delete ${MODEMD_SERVICE} (sc.exe exit code $0).$\r$\n$1"
    Abort
  ${EndIf}

  StrCpy $3 0
  service_delete_wait:
    Sleep 1000
    Call un.ModemdServiceExists
    Pop $2
    ${If} $2 == 0
      Return
    ${EndIf}
    IntOp $3 $3 + 1
    IntCmp $3 ${MODEMD_WAIT_SECONDS} service_delete_timeout service_delete_wait service_delete_timeout
  service_delete_timeout:
    MessageBox MB_OK|MB_ICONSTOP "${MODEMD_SERVICE} is still registered after ${MODEMD_WAIT_SECONDS} seconds. Close any service-management tools and retry uninstall."
    Abort
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  Call ModemdStopBeforeInstall
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Call ModemdInstallAndStart
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  Call un.ModemdStopAndDelete
!macroend
