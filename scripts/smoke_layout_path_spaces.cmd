@echo off
setlocal EnableExtensions
REM Smoke: path-with-spaces for all path-bearing layout CLI/IPC commands.
REM Requires a running GlazeWM built from this branch.
REM Optional: set GLAZEWM_IPC_PORT if 6123 is ghost-bound.

set "DIR=%TEMP%\glazewm path test"
if not exist "%DIR%" mkdir "%DIR%"
set "SNAP=%DIR%\smoke-layout.json"
set "CLI=glazewm-cli"
where glazewm-cli >nul 2>&1
if errorlevel 1 (
  if exist "%~dp0..\target\release\glazewm-cli.exe" set "CLI=%~dp0..\target\release\glazewm-cli.exe"
  if exist "F:\dev\glazewm\target\release\glazewm-cli.exe" set "CLI=F:\dev\glazewm\target\release\glazewm-cli.exe"
)

echo [smoke] Using CLI: %CLI%
echo [smoke] Snapshot path: %SNAP%

"%CLI%" save-layout "%SNAP%"
if errorlevel 1 (
  echo [smoke] FAIL: save-layout
  exit /b 1
)

"%CLI%" inspect-layout "%SNAP%"
if errorlevel 1 (
  echo [smoke] FAIL: inspect-layout with spaces
  exit /b 1
)

"%CLI%" query layout-match "%SNAP%"
if errorlevel 1 (
  echo [smoke] FAIL: query layout-match with spaces
  exit /b 1
)

"%CLI%" load-layout "%SNAP%"
if errorlevel 1 (
  echo [smoke] FAIL: load-layout with spaces
  exit /b 1
)

"%CLI%" command load-layout "%SNAP%"
if errorlevel 1 (
  echo [smoke] FAIL: command load-layout with spaces
  exit /b 1
)

echo [smoke] OK: all path-bearing commands accepted a path with spaces.
exit /b 0
