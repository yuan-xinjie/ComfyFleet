; ComfyFleet 的数据目录默认在 $INSTDIR\data（便携模式）。
; Tauri 模板只会删除 %APPDATA%/%LOCALAPPDATA%，所以卸载时勾选“删除应用数据”
; 还必须显式清掉安装目录下的 data，否则模型/实例/日志全部残留。
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    RmDir /r "$INSTDIR\data"
  ${EndIf}
!macroend
