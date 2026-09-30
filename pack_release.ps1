# pack_release.ps1 - build a distributable Windows package for AudioServer.
#
# What it does (all ASCII literals on purpose: PowerShell 5.x decodes .ps1 files
# without a BOM using the active ANSI codepage, which garbles Chinese strings and
# would produce corrupted file names inside the package):
#   1. cargo build --release (optimized + stripped, see [profile.release] in Cargo.toml)
#   2. stage dist/AudioServer-<version>-win-x64/ : GUI exe + CLI exe + docs
#   3. zip it up and print size + SHA-256 so the receiver can verify the download
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\pack_release.ps1
#
# NOTE: close the running AudioServer window first, otherwise cargo cannot
# overwrite target/release/audioserver.exe ("Access is denied", os error 5).

$ErrorActionPreference = 'Continue'
$root = 'D:\code\AudioServer'
Set-Location $root

# ---- 1. release build -------------------------------------------------------
Write-Host '==> cargo build --release'
# cargo prints warnings on stderr; with 2>&1 PowerShell would turn each line into a
# NativeCommandError record. Going through cmd.exe keeps plain text output instead.
cmd /c 'cargo build --release 2>&1' | Select-Object -Last 6 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) {
    Write-Host 'BUILD FAILED. If the message was "failed to remove ... audioserver.exe",'
    Write-Host 'the GUI is still running - close it and re-run this script.'
    exit 1
}

# ---- 2. version string from git --------------------------------------------
$describe = (& git describe --tags --always).Trim()
$commit   = (& git rev-parse --short HEAD).Trim()
# "v3.4.11-1-g8a1c5c3" -> "v3.4.11": the file name carries the release tag only,
# the exact commit lives in VERSION.txt (that is what a bug report needs).
$version  = ($describe -split '-')[0]
$exe      = Join-Path $root 'target\release\audioserver.exe'
if (-not (Test-Path $exe)) { Write-Host 'release exe missing after build'; exit 1 }
$builtAt  = (Get-Item $exe).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
$pkgName  = "AudioServer-$version-win-x64"
$stage    = Join-Path $root "dist\$pkgName"
$zip      = Join-Path $root "dist\$pkgName.zip"

Write-Host "==> packaging $pkgName (commit $commit, built $builtAt)"

# ---- 3. stage the tree ------------------------------------------------------
# NO driver files and NO driver-install scripts go into the package (by design):
# the exe only detects + tells the user where to download them. Keeps the package
# small and means the program never touches the user's system.
# ASCII comments only: PS 5.1 decodes this file as ANSI, and a Chinese comment
# ending in a dangling lead byte swallows the newline and comments out the next line.
if (Test-Path $stage) {
    # The just-copied exes are often still being scanned (Defender) or held by an open
    # Explorer window, so one Remove-Item attempt can fail transiently: retry a few times.
    for ($i = 0; $i -lt 5 -and (Test-Path $stage); $i++) {
        Remove-Item $stage -Recurse -Force -ErrorAction Continue
        Start-Sleep -Milliseconds 600
    }
}
if (Test-Path $stage) {
    # A leftover dir would silently mix the previous build's files into this package
    # (that is exactly how an old drivers\ folder once survived into a "clean" tree).
    Write-Host "STAGING DIR COULD NOT BE CLEANED: $stage"
    Write-Host 'Close any Explorer window / terminal whose cwd is inside it, then re-run.'
    exit 1
}
New-Item -ItemType Directory -Path $stage | Out-Null

Copy-Item $exe (Join-Path $stage 'audioserver.exe')
Copy-Item (Join-Path $root 'target\release\server.exe') (Join-Path $stage 'server.exe')
Copy-Item (Join-Path $root 'README.md') (Join-Path $stage 'README.md')
Copy-Item (Join-Path $root 'docs\quick-start.md') (Join-Path $stage 'quick-start.md')

# Launcher: keeps the window visible (a background process is confusing to users)
$bat = @"
@echo off
cd /d "%~dp0"
start "" "audioserver.exe"
"@
Set-Content -Path (Join-Path $stage 'start-audioserver.bat') -Value $bat -Encoding ASCII

# Build provenance, so a bug report can be tied to an exact commit
@"
package   : $pkgName
commit    : $commit
git ref   : $describe
built at  : $builtAt
profile   : release (opt-level=3, lto=true, codegen-units=1, strip=true)
sha256(exe): $( (Get-FileHash $exe -Algorithm SHA256).Hash )
"@ | Set-Content -Path (Join-Path $stage 'VERSION.txt') -Encoding ASCII

# ---- 4. verify the staged tree, then zip ------------------------------------
$must = @('audioserver.exe', 'server.exe', 'README.md', 'quick-start.md',
          'start-audioserver.bat', 'VERSION.txt')
foreach ($m in $must) {
    if (-not (Test-Path (Join-Path $stage $m))) {
        Write-Host "MISSING in package: $m"
        exit 1
    }
}
# Guard: no driver / driver-install payload may ever end up in the package.
$forbidden = Get-ChildItem $stage -Recurse -Include '*.bat', '*.msi', '*.dll', '*.inf' -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -ne 'start-audioserver.bat' }
if ($forbidden) {
    Write-Host 'PACKAGE MUST NOT CONTAIN DRIVER FILES:'
    $forbidden | ForEach-Object { Write-Host ('  ' + $_.FullName) }
    exit 1
}
Write-Host ("==> staged tree ok (" + $must.Count + " required entries, no driver payload)")

if (Test-Path $zip) { Remove-Item $zip -Force -ErrorAction Continue }
# -Force: overwrite a zip the previous delete could not release (Defender scan)
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal -Force
# The staged folder is deliberately kept next to the zip so it can be browsed/inspected.

Write-Host '==> result'
Get-ChildItem $zip | ForEach-Object {
    Write-Host ("zip     : " + $_.FullName)
    Write-Host ("zip size: {0:N1} MB" -f ($_.Length / 1MB))
}
Write-Host ("zip sha256: " + (Get-FileHash $zip -Algorithm SHA256).Hash)
Write-Host ("staged dir: $stage")
