@echo off
:: setup-env.bat — Run ONCE as Administrator to install the Windows SDK
:: required for cargo build on this machine.
::
:: What this script does:
::   1. Installs the Windows 11 SDK (build libs + headers) via VS installer
::   2. Verifies kernel32.lib is reachable
::   3. Runs cargo check to confirm the build environment works
::
:: Run with: Right-click → Run as Administrator

echo [1/3] Installing Windows 11 SDK (26100) via VS BuildTools installer...
"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vs_installer.exe" ^
    modify ^
    --installPath "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools" ^
    --add Microsoft.VisualStudio.Component.Windows11SDK.26100 ^
    --quiet --norestart
echo SDK install exit code: %ERRORLEVEL%

echo.
echo [2/3] Verifying kernel32.lib...
if exist "C:\Program Files (x86)\Windows Kits\10\Lib\10.0.26100.0\um\x64\kernel32.lib" (
    echo   OK — SDK lib found.
) else (
    echo   WARN — SDK lib not found at expected path.
    echo   Check: C:\Program Files (x86)\Windows Kits\10\Lib\
)

echo.
echo [3/3] Running cargo check in AeroSync-Lite...
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" -arch=x64
cd /d D:\Projects\AeroSync-Lite
cargo check
echo.
echo Done. Exit code: %ERRORLEVEL%
pause
