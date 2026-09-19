@echo off
setlocal EnableExtensions EnableDelayedExpansion
cd /d "%~dp0"

set "LOG=%~dp0build.log"
echo ========================================
echo  GlazeWM Windows build.bat
echo  Repo: %CD%
echo  Log:  %LOG%
echo ========================================
echo.

echo GlazeWM build started %DATE% %TIME%> "%LOG%"
echo Repo=%CD%>> "%LOG%"

where cargo >nul 2>&1
if errorlevel 1 (
  echo ERROR: cargo not found on PATH. Install Rust from https://rustup.rs and reopen the shell.
  echo ERROR: cargo missing>> "%LOG%"
  echo.
  if not defined BUILD_BAT_NOPAUSE pause
  exit /b 1
)

where rustc >nul 2>&1
if errorlevel 1 (
  echo ERROR: rustc not found on PATH.
  echo ERROR: rustc missing>> "%LOG%"
  echo.
  if not defined BUILD_BAT_NOPAUSE pause
  exit /b 1
)

set "VSDEV="
call :FindVsDevCmd
if defined VSDEV (
  echo Loading MSVC env:
  echo   !VSDEV!
  call "!VSDEV!" -arch=x64 -host_arch=x64
  set "EC=!ERRORLEVEL!"
  if not "!EC!"=="0" (
    echo ERROR: VsDevCmd.bat failed.
    echo ERROR: VsDevCmd failed>> "%LOG%"
    echo.
    if not defined BUILD_BAT_NOPAUSE pause
    exit /b 1
  )
) else (
  echo WARNING: VsDevCmd.bat not found. Continuing with current PATH.
  echo WARNING: VsDevCmd not found>> "%LOG%"
)

where link >nul 2>&1
if errorlevel 1 (
  echo ERROR: MSVC link.exe not on PATH. Install VS Build Tools with the C++ workload, then re-run.
  echo ERROR: link.exe missing>> "%LOG%"
  echo.
  if not defined BUILD_BAT_NOPAUSE pause
  exit /b 1
)

echo.
echo [1/2] cargo build --release
echo -----
echo [1/2] cargo build --release>> "%LOG%"
call cargo build --release
set "EC=!ERRORLEVEL!"
if not "!EC!"=="0" (
  echo ERROR: cargo build --release failed.
  echo ERROR: cargo build --release failed>> "%LOG%"
  echo See also: %LOG%
  echo.
  if not defined BUILD_BAT_NOPAUSE pause
  exit /b 1
)
echo OK: cargo build --release>> "%LOG%"

echo.
echo [2/2] cargo build --release -p wm-watcher
echo -----
echo [2/2] cargo build --release -p wm-watcher>> "%LOG%"
call cargo build --release -p wm-watcher
set "EC=!ERRORLEVEL!"
if not "!EC!"=="0" (
  echo ERROR: cargo build --release -p wm-watcher failed.
  echo ERROR: wm-watcher build failed>> "%LOG%"
  echo See also: %LOG%
  echo.
  if not defined BUILD_BAT_NOPAUSE pause
  exit /b 1
)
echo OK: wm-watcher>> "%LOG%"

set "OUT=%CD%\target\release"
echo.
echo ========================================
echo  BUILD SUCCEEDED
echo ========================================
echo.
echo Artifacts directory:
echo   %OUT%
echo.
echo Key binaries:
if exist "%OUT%\glazewm.exe" (echo   %OUT%\glazewm.exe) else echo   MISSING glazewm.exe
if exist "%OUT%\glazewm-watcher.exe" (echo   %OUT%\glazewm-watcher.exe) else echo   MISSING glazewm-watcher.exe
if exist "%OUT%\glazewm-cli.exe" (echo   %OUT%\glazewm-cli.exe) else echo   MISSING glazewm-cli.exe
echo.
echo Full log: %LOG%
echo Install drop-in: C:\Program Files\glzr.io\GlazeWM\
echo ========================================
echo BUILD SUCCEEDED OUT=%OUT%>> "%LOG%"
echo.
if not defined BUILD_BAT_NOPAUSE pause
exit /b 0

:FindVsDevCmd
set "CAND=%ProgramFiles(x86)%\Microsoft Visual Studio\18\BuildTools\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
set "CAND=%ProgramFiles%\Microsoft Visual Studio\18\BuildTools\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
set "CAND=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
set "CAND=%ProgramFiles%\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
set "CAND=%ProgramFiles(x86)%\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
set "CAND=%ProgramFiles%\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat"
if exist "!CAND!" set "VSDEV=!CAND!" & goto :eof
goto :eof
