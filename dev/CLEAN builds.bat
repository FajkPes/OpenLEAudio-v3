@echo off
 powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0CLEAN builds.ps1" %*
if errorlevel 1 exit /b 1
pause
