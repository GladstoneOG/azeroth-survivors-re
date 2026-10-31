@echo off
setlocal
cd /d "%~dp0"

echo ========================================================
echo               Warcraft Survivors Launcher
echo ========================================================
echo.

:: 1. Check if WoW 1.12.1 MPQ data exists in any standard location
if exist "wow-data\Data\terrain.MPQ" goto SET_ENV_AND_START
if exist "WoW\Data\terrain.MPQ" goto SET_ENV_AND_START
if exist "Data\terrain.MPQ" goto SET_ENV_AND_START
if exist "wow-data\Data\terrain.mpq" goto SET_ENV_AND_START
if exist "WoW\Data\terrain.mpq" goto SET_ENV_AND_START
if exist "Data\terrain.mpq" goto SET_ENV_AND_START

:: 2. Auto-fetch MPQs using scripts\fetch-data.ps1
echo [INFO] WoW 1.12.1 MPQ assets not found.
echo [INFO] Performing one-time automated setup (downloading ~5.1 GB MPQs from archive.org)...
echo.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\fetch-data.ps1"
if errorlevel 1 (
    echo.
    echo [ERROR] Asset download/extraction failed.
    echo Please make sure you have internet access and at least 6 GB of free disk space.
    pause
    exit /b 1
)

:SET_ENV_AND_START
:: 3. Configure WoW data directory environment variable so game finds MPQs
if not defined WOW_DATA (
    if exist "%~dp0wow-data\Data\terrain.MPQ" set "WOW_DATA=%~dp0wow-data"
    if exist "%~dp0wow-data\Data\terrain.mpq" set "WOW_DATA=%~dp0wow-data"
    if exist "%~dp0WoW\Data\terrain.MPQ" set "WOW_DATA=%~dp0WoW"
    if exist "%~dp0WoW\Data\terrain.mpq" set "WOW_DATA=%~dp0WoW"
    if exist "%~dp0Data\terrain.MPQ" set "WOW_DATA=%~dp0"
    if exist "%~dp0Data\terrain.mpq" set "WOW_DATA=%~dp0"
)

:: 4. Launch Warcraft Survivors
echo.
echo [INFO] Launching Warcraft Survivors...
if exist "target\release\warcraft-survivors.exe" (
    start "" "target\release\warcraft-survivors.exe" %*
) else if exist "warcraft-survivors.exe" (
    start "" "warcraft-survivors.exe" %*
) else (
    echo [ERROR] warcraft-survivors.exe not found!
    echo Please ensure warcraft-survivors.exe is present in this folder or in target\release\.
    pause
    exit /b 1
)
