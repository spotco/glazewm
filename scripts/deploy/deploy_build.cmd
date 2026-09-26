@echo off
setlocal EnableExtensions
set "INSTALL=C:\Program Files\glzr.io\GlazeWM"
set "REL=F:\dev\glazewm\target\release"
set "REPO=F:\dev\glazewm"

if /I "%~1"=="--help" goto :usage
if /I "%~1"=="-h" goto :usage

if not exist "%REL%\glazewm.exe" (
  echo ERROR: Missing "%REL%\glazewm.exe" - run build.bat first.
  exit /b 1
)
if not exist "%INSTALL%\" (
  echo ERROR: Install dir missing: "%INSTALL%"
  exit /b 1
)

echo Soft-exiting GlazeWM (wm-exit) so IPC listen socket is Dropped...
REM Prefer soft exit over taskkill /F — hard kill leaves ghost LISTENING ports.
if exist "%INSTALL%\glazewm.exe" (
  "%INSTALL%\glazewm.exe" command wm-exit >nul 2>&1
  timeout /t 2 /nobreak >nul
)
echo Stopping any remaining GlazeWM processes...
taskkill /IM glazewm.exe /F >nul 2>&1
taskkill /IM glazewm-watcher.exe /F >nul 2>&1
timeout /t 1 /nobreak >nul

echo Copying artifacts -^> "%INSTALL%"
copy /Y "%REL%\glazewm.exe" "%INSTALL%\glazewm.exe"
if errorlevel 1 goto :copyfail
if exist "%REL%\glazewm-cli.exe" copy /Y "%REL%\glazewm-cli.exe" "%INSTALL%\glazewm-cli.exe"

REM Watcher may stay locked by a zombie PID (taskkill says not running).
REM Rename-replace so deploy can still land the new binary.
if exist "%REL%\glazewm-watcher.exe" (
  if exist "%INSTALL%\glazewm-watcher.exe" (
    del /F /Q "%INSTALL%\glazewm-watcher.exe.bak_deploy" >nul 2>&1
    move /Y "%INSTALL%\glazewm-watcher.exe" "%INSTALL%\glazewm-watcher.exe.bak_deploy" >nul 2>&1
  )
  copy /Y "%REL%\glazewm-watcher.exe" "%INSTALL%\glazewm-watcher.exe"
  if errorlevel 1 (
    echo WARNING: watcher copy failed even after rename; binary may still be locked.
  )
)

echo.
echo Deployed:
dir "%INSTALL%\glazewm.exe" "%INSTALL%\glazewm-cli.exe" "%INSTALL%\glazewm-watcher.exe" 2>nul
echo.
echo Tip: start GlazeWM ONLY with a quoted path (or start_glazewm.cmd):
echo   "%INSTALL%\glazewm.exe"
echo   "%REPO%\scripts\deploy\start_glazewm.cmd"
echo Never: start C:\Program Files\...  (unquoted -^> C:\Program popup)
exit /b 0

:copyfail
echo.
echo COPY FAILED - likely need ACL grant ^(UAC once^):
echo   Right-click grant_install_write_access.cmd -^> Run as administrator
echo Then re-run deploy_build.cmd
exit /b 1

:usage
echo Usage: deploy_build.cmd
echo Copies release binaries from %REL% into %INSTALL%
echo Requires prior backup_install.cmd and one-time grant_install_write_access.cmd
exit /b 0
