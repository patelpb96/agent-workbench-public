@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=x64 -host_arch=x64
if errorlevel 1 exit /b %errorlevel%
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
rem This VS install keeps two CRT compatibility libraries outside the default desktop LIB path.
rem Append only as fallbacks so the standard desktop libraries remain preferred.
if exist "%VCToolsInstallDir%lib\onecore\x64\legacy_stdio_definitions.lib" set "LIB=%LIB%;%VCToolsInstallDir%lib\onecore\x64"
if exist "%VCToolsInstallDir%lib\x64\uwp\oldnames.lib" set "LIB=%LIB%;%VCToolsInstallDir%lib\x64\uwp"
cd /d "%~dp0"
cargo %*
