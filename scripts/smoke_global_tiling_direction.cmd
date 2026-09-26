@echo off
setlocal EnableExtensions
REM CLI/runtime smoke for the global tiling direction feature.
REM Use -TestSoftExit to verify shutdown_commands kills and restarts Zebar.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0smoke_global_tiling_direction.ps1" %*
exit /b %ERRORLEVEL%
