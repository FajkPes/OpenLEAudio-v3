@echo off
setlocal
title OpenLEAudio - sign driver
set "OLEA_SIGN_SCRIPT=%~dp0scripts\sign-driver.ps1"
if not exist "%OLEA_SIGN_SCRIPT%" (
    echo ERROR: scripts\sign-driver.ps1 was not found beside this launcher.
    pause
    exit /b 1
)
echo.
echo OpenLEAudio driver signing
 echo Accept the Windows administrator prompt. Progress opens in PowerShell.
echo Wait for the FINISHED banner. If an error appears, keep its text.
echo.
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "try { $arguments = '-NoLogo -NoProfile -ExecutionPolicy Bypass -NoExit -File ' + [char]34 + $env:OLEA_SIGN_SCRIPT + [char]34 + ' -Sign'; $process = Start-Process powershell.exe -Verb RunAs -ArgumentList $arguments -PassThru -Wait; exit $process.ExitCode } catch { Write-Host $_.Exception.Message; exit 1 }"
if errorlevel 1 (
    echo Signing could not complete. See the PowerShell error above.
    pause
    exit /b 1
)
exit /b 0
