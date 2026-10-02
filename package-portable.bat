@echo off
setlocal EnableExtensions EnableDelayedExpansion
cd /d "%~dp0"

set "VERSION=%~1"
if not defined VERSION (
  for /f "delims=" %%I in ('powershell -NoProfile -Command "git rev-parse --short=8 HEAD" 2^>nul') do set "VERSION=spotcobuild-%%I"
)
if not defined VERSION set "VERSION=spotcobuild-local"

for /f "delims=" %%I in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss"') do set "STAMP=%%I"
set "STAGE=%CD%\Temp\portable-%VERSION%-%STAMP%"
set "ZIP=%CD%\Temp\glazewm-%VERSION%-%STAMP%.zip"

echo Building the current checkout...
set "BUILD_BAT_NOPAUSE=1"
call build.bat
if errorlevel 1 (
  echo ERROR: build.bat failed.
  exit /b 1
)

set "RELEASE_DIR=%CD%\target\release"
set "REQUIRED_EXES=glazewm.exe glazewm-cli.exe glazewm-watcher.exe"
for %%F in (%REQUIRED_EXES%) do (
  if not exist "%RELEASE_DIR%\%%F" (
    echo ERROR: missing release artifact: %RELEASE_DIR%\%%F
    exit /b 1
  )
)
if not exist "%CD%\resources\assets\sample-config.yaml" (
  echo ERROR: missing default config: %CD%\resources\assets\sample-config.yaml
  exit /b 1
)

mkdir "%STAGE%"
if errorlevel 1 (
  echo ERROR: could not create staging directory: %STAGE%
  exit /b 1
)

for %%F in (%REQUIRED_EXES%) do copy /y "%RELEASE_DIR%\%%F" "%STAGE%\%%F" >nul
REM Repo sample config (starter deploy), not the live user config under %%USERPROFILE%%\.glzr\glazewm\
copy /y "%CD%\resources\assets\sample-config.yaml" "%STAGE%\config.yaml" >nul
REM GlazeWM has no settings.json (Zebar-only); config.yaml is the sole starter config.

powershell -NoProfile -Command "Compress-Archive -Path '%STAGE%\*' -DestinationPath '%ZIP%' -CompressionLevel Optimal"
if errorlevel 1 (
  echo ERROR: failed to create ZIP: %ZIP%
  exit /b 1
)

echo.
echo Portable release created:
echo   %ZIP%
echo Contents:
for %%F in (%REQUIRED_EXES%) do echo   %%F
echo   config.yaml
echo.
echo Install destinations on a stock GlazeWM machine:
echo   glazewm*.exe  -^> C:\Program Files\glzr.io\GlazeWM\
echo   config.yaml   -^> %%USERPROFILE%%\.glzr\glazewm\config.yaml
exit /b 0
