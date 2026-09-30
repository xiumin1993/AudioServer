# pack_release.ps1 - build a distributable Windows package for AudioServer.
#
# What it does (all ASCII literals on purpose: PowerShell 5.x decodes .ps1 files
# without a BOM using the active ANSI codepage, which garbles Chinese strings and
# would produce corrupted file names inside the package):
#   1. cargo build --release (optimized + stripped, see [profile.release] in Cargo.toml)
#   2. stage dist/AudioServer-<version>-win-x64/ : GUI exe + CLI exe + docs + drivers
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
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage 'drivers\UnityCapture') | Out-Null

Copy-Item $exe (Join-Path $stage 'audioserver.exe')
Copy-Item (Join-Path $root 'target\release\server.exe') (Join-Path $stage 'server.exe')
Copy-Item (Join-Path $root 'README.md') (Join-Path $stage 'README.md')
Copy-Item (Join-Path $root 'docs\quick-start.md') (Join-Path $stage 'quick-start.md')

# Virtual camera driver payload (MIT-licensed source we ship alongside the server)
$installSrc = Join-Path $root 'third_party\UnityCapture-master\Install'
Copy-Item "$installSrc\*" (Join-Path $stage 'drivers\UnityCapture') -Recurse

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
          'start-audioserver.bat', 'VERSION.txt',
          'drivers\UnityCapture\Install.bat', 'drivers\UnityCapture\UnityCaptureFilter64.dll')
foreach ($m in $must) {
    if (-not (Test-Path (Join-Path $stage $m))) {
        Write-Host "MISSING in package: $m"
        exit 1
    }
}
Write-Host ("==> staged tree ok (" + $must.Count + " required entries present)")

if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

Write-Host '==> result'
Get-ChildItem $zip | ForEach-Object {
    Write-Host ("zip     : " + $_.FullName)
    Write-Host ("zip size: {0:N1} MB" -f ($_.Length / 1MB))
}
Write-Host ("zip sha256: " + (Get-FileHash $zip -Algorithm SHA256).Hash)
Write-Host ("staged dir: $stage")
