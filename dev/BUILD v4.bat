@echo off
REM Compatibility entry point. Keep the build logic in one reviewed script.
call "%~dp0BUILD application.bat" --no-pause
exit /b %errorlevel%
