# PowerShell fetch-data script for Windows
param(
    [string]$DataDir = "wow-data"
)

$ErrorActionPreference = "Stop"
$root = Resolve-Path "$PSScriptRoot\.."
$targetDir = Join-Path $root $DataDir
$dataTarget = Join-Path $targetDir "Data"

# Check if MPQs are already present
if ((Test-Path "$dataTarget\terrain.MPQ") -and (Test-Path "$dataTarget\patch.MPQ")) {
    Write-Host "Game data already present at $dataTarget" -ForegroundColor Green
    exit 0
}

if (-not (Test-Path $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}

$url = if ($env:WOW_DATA_URL) { $env:WOW_DATA_URL } else { "https://archive.org/download/WoW_1.12.1-5875/1.12.1-5875_enUS.zip" }
$zipFile = Join-Path $targetDir "1.12.1-5875_enUS.zip"

Write-Host "Downloading WoW 1.12.1 client (~5.1 GB) from $url..." -ForegroundColor Cyan
& curl.exe -L -C - -o $zipFile $url

if ($LASTEXITCODE -ne 0) {
    Write-Error "Download failed or interrupted with exit code $LASTEXITCODE."
    exit $LASTEXITCODE
}

Write-Host "Download complete. Extracting MPQ data files..." -ForegroundColor Cyan
if (-not (Test-Path $dataTarget)) {
    New-Item -ItemType Directory -Path $dataTarget -Force | Out-Null
}

$7z = (Get-Command 7z.exe -ErrorAction SilentlyContinue)?.Source
if ($7z) {
    Write-Host "Using 7z: $7z"
    # Extract only .MPQ files from any subfolder matching *Data/*.MPQ or *.MPQ into $dataTarget
    & $7z e $zipFile "-o$dataTarget" "*.MPQ" -r -y
} else {
    Write-Host "7z not found, using tar.exe..."
    & tar.exe -xf $zipFile -C $targetDir --wildcards "*Data/*.MPQ" "*data/*.mpq"
}

if ((Test-Path "$dataTarget\terrain.MPQ") -and (Test-Path "$dataTarget\patch.MPQ")) {
    Write-Host "Extraction verified! Removing temporary zip file to save ~5 GB disk space..." -ForegroundColor Green
    Remove-Item -Path $zipFile -Force -ErrorAction SilentlyContinue
    Write-Host "Game data ready at: $dataTarget" -ForegroundColor Green
} else {
    Write-Warning "Could not find terrain.MPQ and patch.MPQ in $dataTarget. Please check extracted files."
}
