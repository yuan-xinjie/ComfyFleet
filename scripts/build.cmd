@echo off
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" %*
set "BUILD_EXIT=%ERRORLEVEL%"
echo.
if not "%BUILD_EXIT%"=="0" (
  echo 构建失败，退出码 %BUILD_EXIT%
) else (
  echo 构建完成
)
rem 双击运行时保留窗口；带参数在终端运行时直接退出。
if "%~1"=="" pause
exit /b %BUILD_EXIT%
