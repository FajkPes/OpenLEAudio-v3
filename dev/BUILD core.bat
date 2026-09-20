@echo off
REM Rust/MSVC needs the linker and a complete Windows SDK library path, exactly
REM as the main application build does. Without it the link step fails on
REM dbghelp.lib long before any of our own code is reached.
setlocal
set "VSWHEREDIR=C:\Program Files (x86)"
for /f "usebackq tokens=*" %%i in (`"%VSWHEREDIR%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -property installationPath`) do set "VSROOT=%%i"
set "WINSDK=10.0.26100.0"
if defined VSROOT call "%VSROOT%\VC\Auxiliary\Build\vcvarsall.bat" amd64 %WINSDK% >nul
pushd "%~dp0core"
cargo build %*
set "RC=%ERRORLEVEL%"
popd
exit /b %RC%

