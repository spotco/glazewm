@echo off
setlocal EnableExtensions
set "INSTALL=C:\Program Files\glzr.io\GlazeWM"
set "BACKUP_ROOT=C:\Users\mooto\glazewm-install-backups"
for /f %%I in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss"') do set "STAMP=%%I"
set "DEST=%BACKUP_ROOT%\%STAMP%"

if not exist "%INSTALL%\glazewm.exe" (
  echo ERROR: No glazewm.exe at "%INSTALL%"
  exit /b 1
)

mkdir "%DEST%" 2>nul
echo Backing up "%INSTALL%" -^> "%DEST%"
robocopy "%INSTALL%" "%DEST%" /E /COPY:DAT /R:1 /W:1 /NFL /NDL /NJH /NJS /NP
set "RC=%ERRORLEVEL%"
if %RC% GEQ 8 (
  echo robocopy failed with %RC%
  exit /b %RC%
)

echo %STAMP%> "%BACKUP_ROOT%\LATEST.txt"
echo OK backup at "%DEST%"
dir /b "%DEST%"
exit /b 0
