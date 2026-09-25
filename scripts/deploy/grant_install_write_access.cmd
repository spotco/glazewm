@echo off
REM One-time elevated: grant current user Modify on GlazeWM install dir
REM so later deploy_build.cmd does not need UAC.
setlocal EnableExtensions
set "INSTALL=C:\Program Files\glzr.io\GlazeWM"
set "PARENT=C:\Program Files\glzr.io"

net session >nul 2>&1
if errorlevel 1 (
  echo This script must run elevated ^(UAC / Administrator^).
  echo Right-click -^> Run as administrator, or approve the UAC prompt.
  exit /b 1
)

if not exist "%INSTALL%" (
  echo ERROR: "%INSTALL%" not found
  exit /b 1
)

echo Granting Modify to %USERNAME% on:
echo   %INSTALL%
REM (OI)(CI) = object/container inherit; M = Modify
icacls "%INSTALL%" /grant "%USERNAME%:(OI)(CI)M" /T
if errorlevel 1 exit /b 1

REM Also allow writing new files if parent is locked down
icacls "%PARENT%" /grant "%USERNAME%:(OI)(CI)RX" >nul 2>&1

echo.
echo ACL after grant:
icacls "%INSTALL%"
echo.
echo DONE. Non-elevated deploy_build.cmd should now work.
exit /b 0
