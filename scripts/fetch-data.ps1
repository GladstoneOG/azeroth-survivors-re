# PowerShell fetch-data script for Windows
param(
    [string]$DataDir = "wow-data"
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path "$PSScriptRoot\..").ProviderPath
$targetDir = if ([System.IO.Path]::IsPathRooted($DataDir)) { $DataDir } else { Join-Path $root $DataDir }
$dataTarget = Join-Path $targetDir "Data"

# Check if MPQs are already present
if ((Test-Path "$dataTarget\terrain.MPQ") -and (Test-Path "$dataTarget\patch.MPQ")) {
    Write-Host "Game data already present at $dataTarget" -ForegroundColor Green
    exit 0
}

if (-not (Test-Path $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}

$defaultDirectUrl = "https://ia600708.us.archive.org/32/items/WoW_1.12.1-5875/1.12.1-5875_enUS.zip"
$backupUrl = "https://archive.org/download/WoW_1.12.1-5875/1.12.1-5875_enUS.zip"
$url = if ($env:WOW_DATA_URL) { $env:WOW_DATA_URL } else { $defaultDirectUrl }
$zipName = "1.12.1-5875_enUS.zip"
$zipFile = Join-Path $targetDir $zipName

# Check for aria2c for fast multi-connection concurrent download (IDM-style acceleration)
$aria2 = $null
$localAria2 = Join-Path $PSScriptRoot "aria2c.exe"
if (Test-Path $localAria2) {
    $aria2 = $localAria2
} else {
    $ariaCmd = Get-Command aria2c.exe -ErrorAction SilentlyContinue
    if ($ariaCmd) {
        $aria2 = $ariaCmd.Source
    }
}

# If aria2c is not found, try quickly downloading portable aria2c (~2.4 MB)
if (-not $aria2) {
    try {
        Write-Host "Fetching fast multi-connection downloader (aria2c)..." -ForegroundColor Cyan
        $ariaZip = Join-Path $targetDir "aria2_temp.zip"
        & curl.exe -sL -o $ariaZip "https://github.com/aria2/aria2/releases/download/release-1.37.0/aria2-1.37.0-win-64bit-build1.zip"
        if (Test-Path $ariaZip) {
            & tar.exe -xf $ariaZip -C $targetDir --strip-components=1 "*aria2c.exe"
            Remove-Item $ariaZip -Force -ErrorAction SilentlyContinue
            $extractedAria = Join-Path $targetDir "aria2c.exe"
            if (Test-Path $extractedAria) {
                Move-Item $extractedAria $localAria2 -Force -ErrorAction SilentlyContinue
                if (Test-Path $localAria2) {
                    $aria2 = $localAria2
                } else {
                    $aria2 = $extractedAria
                }
            }
        }
    } catch {
        # Fallback to curl if download fails
    }
}

if ($aria2) {
    Write-Host "Downloading WoW 1.12.1 client (~5.1 GB) using 16 concurrent connections (IDM-style acceleration)..." -ForegroundColor Cyan
    Write-Host "Source: $url" -ForegroundColor DarkCyan
    & $aria2 -c -x 16 -s 16 -k 1M --file-allocation=none --summary-interval=2 -d $targetDir -o $zipName $url
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "aria2 download returned code $LASTEXITCODE. Retrying with curl.exe..."
        & curl.exe -L -C - --retry 3 -o $zipFile $url
    }
} else {
    Write-Host "Downloading WoW 1.12.1 client (~5.1 GB) from $url..." -ForegroundColor Cyan
    & curl.exe -L -C - --retry 3 -o $zipFile $url
}

if ($LASTEXITCODE -ne 0 -and -not (Test-Path $zipFile)) {
    Write-Host "Download failed or interrupted with exit code $LASTEXITCODE." -ForegroundColor Red
    exit $LASTEXITCODE
}

Write-Host "Download complete. Extracting MPQ data files..." -ForegroundColor Cyan
if (-not (Test-Path $dataTarget)) {
    New-Item -ItemType Directory -Path $dataTarget -Force | Out-Null
}

# Detect 7-Zip if available in PATH or default Program Files locations
$7zCmd = Get-Command 7z.exe -ErrorAction SilentlyContinue
$7z = if ($7zCmd) { $7zCmd.Source } else { $null }
if (-not $7z) {
    if (Test-Path "$env:ProgramFiles\7-Zip\7z.exe") {
        $7z = "$env:ProgramFiles\7-Zip\7z.exe"
    } elseif (Test-Path "${env:ProgramFiles(x86)}\7-Zip\7z.exe") {
        $7z = "${env:ProgramFiles(x86)}\7-Zip\7z.exe"
    }
}

$extractSuccess = $false

if ($7z) {
    Write-Host "Using 7-Zip: $7z" -ForegroundColor Cyan
    & $7z e $zipFile "-o$dataTarget" "*.[mM][pP][qQ]" -r -y
    if ($LASTEXITCODE -eq 0) {
        $extractSuccess = $true
    }
}

if (-not $extractSuccess) {
    $tarCmd = Get-Command tar.exe -ErrorAction SilentlyContinue
    if ($tarCmd) {
        Write-Host "7-Zip not found, using Windows tar.exe..." -ForegroundColor Cyan
        & tar.exe -xf $zipFile -C $targetDir "*.[mM][pP][qQ]"
        if ($LASTEXITCODE -eq 0) {
            $extractSuccess = $true
        }
    }
}

if (-not $extractSuccess -and -not (Test-Path "$dataTarget\terrain.MPQ")) {
    Write-Host "Extracting via .NET ZipFile (fallback)..." -ForegroundColor Cyan
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipFile)
    try {
        foreach ($entry in $zip.Entries) {
            if ($entry.FullName -match '\.(?i:mpq)$') {
                $destName = [System.IO.Path]::GetFileName($entry.FullName)
                $destFile = Join-Path $dataTarget $destName
                Write-Host "  Extracting $destName..."
                [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $destFile, $true)
            }
        }
        $extractSuccess = $true
    } catch {
        Write-Host "Extraction error: $_" -ForegroundColor Red
    } finally {
        $zip.Dispose()
    }
}

# If files were extracted into a different folder under $targetDir, relocate them to $dataTarget
if (-not (Test-Path "$dataTarget\terrain.MPQ")) {
    $foundTerrain = Get-ChildItem -Path $targetDir -Filter "*terrain.mpq" -Recurse -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($foundTerrain -and $foundTerrain.DirectoryName -ne $dataTarget) {
        $sourceDir = $foundTerrain.DirectoryName
        Write-Host "Moving extracted MPQ files from $sourceDir to $dataTarget..." -ForegroundColor Cyan
        Get-ChildItem -Path $sourceDir -File | Where-Object { $_.Extension -match '^\.mpq$' } | ForEach-Object {
            Move-Item -Path $_.FullName -Destination $dataTarget -Force
        }
    }
}

if ((Test-Path "$dataTarget\terrain.MPQ") -and (Test-Path "$dataTarget\patch.MPQ")) {
    Write-Host "Extraction verified! Removing temporary zip file to save ~5 GB disk space..." -ForegroundColor Green
    Remove-Item -Path $zipFile -Force -ErrorAction SilentlyContinue
    # Also clean up aria2 control file if left behind
    Remove-Item -Path "$zipFile.aria2" -Force -ErrorAction SilentlyContinue

    # Optional: Create junction link 'Data' in project root if it does not already exist
    $rootData = Join-Path $root "Data"
    if (-not (Test-Path $rootData)) {
        try {
            cmd.exe /c "mklink /J `"$rootData`" `"$dataTarget`"" *>$null
        } catch {}
    }

    Write-Host "Game data ready at: $dataTarget" -ForegroundColor Green
    exit 0
} else {
    Write-Host "Could not find terrain.MPQ and patch.MPQ in $dataTarget. Please check extracted files." -ForegroundColor Red
    exit 1
}
