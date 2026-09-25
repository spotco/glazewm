@echo off
setlocal
set "INSTALL=C:\Program Files\glzr.io\GlazeWM"
set "REL=F:\dev\glazewm\target\release"
echo INSTALL_EXISTS=
if exist "%INSTALL%" (echo YES) else (echo NO)
echo.
echo --- INSTALL DIR ---
dir /b "%INSTALL%" 2>nul
echo.
echo --- ACL ---
icacls "%INSTALL%"
echo.
echo --- PROCESSES ---
tasklist /FI "IMAGENAME eq glazewm.exe" /FO LIST 2>nul
tasklist /FI "IMAGENAME eq glazewm-watcher.exe" /FO LIST 2>nul
echo.
echo --- ARTIFACTS ---
for %%F in (glazewm.exe glazewm-cli.exe glazewm-watcher.exe) do (
  if exist "%REL%\%%F" (dir "%REL%\%%F") else (echo MISSING %%F)
)
echo.
echo --- WHOAMI ---
whoami
whoami /groups | findstr /i "High Mandatory Admin"
