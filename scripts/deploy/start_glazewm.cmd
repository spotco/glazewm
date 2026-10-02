@echo off
setlocal EnableExtensions
REM Verbose z-order/native-op logging: set general.verbose_z_order
REM in config.yaml (preferred). Optional override: set GLAZEWM_VERBOSE_Z_ORDER=1
REM Always-quoted GlazeWM launcher. Unquoted
REM   start C:\Program Files\glzr.io\GlazeWM\glazewm.exe
REM splits at the space and pops "Windows cannot find C:\Program".
set "INSTALL=C:\Program Files\glzr.io\GlazeWM"
set "EXE=%INSTALL%\glazewm.exe"
if not exist "%EXE%" (
  echo ERROR: Missing "%EXE%"
  exit /b 1
)
start "" "%EXE%" %*
exit /b 0
