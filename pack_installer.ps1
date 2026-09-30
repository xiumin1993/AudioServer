# pack_installer.ps1 - build the Windows INSTALLER for AudioServer.
#
# ASCII-only on purpose: PowerShell 5.1 decodes .ps1 files with the active ANSI
# codepage, and a Chinese comment can eat the newline that follows it.
#
# Pipeline:
#   1. cargo build --release
#   2. stage dist\stage-installer\ : audioserver.exe + server.exe + docs + VERSION.txt
#   3. generate config.default.json by asking the BUILT binary for it
#      (server.exe --print-default-config). That is the whole point of step 3: the
#      defaults shipped to users and the defaults compiled into the exe come from the
#      same Default impls in src/config.rs, so they cannot drift apart.
#   4. refuse to continue if any driver payload (.msi/.inf/.sys/.dll/.bat/.ps1) or any
#      unexpected exe is in the stage tree - the product must never install drivers.
#   5. ISCC.exe installer\setup.iss  ->  dist\AudioServer-<ver>-win-x64-setup.exe
#   6. print size + SHA-256.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\pack_installer.ps1
#   ... add -SkipBuild if the release build is already fresh
#
# NOTE: close the running AudioServer window first, otherwise cargo cannot overwrite
# target/release/audioserver.exe ("Access is denied", os error 5).

param([switch]$SkipBuild)

$ErrorActionPreference = 'Continue'
$root = 'D:\code\AudioServer'
Set-Location $root

# ---- 0. find Inno Setup -----------------------------------------------------
# winget installs it either machine-wide or into the current user's profile, so look
# in both places. Nothing here installs it: the tool has to be there already.
$iscc = $null
foreach ($dir in @(
    "${env:ProgramFiles(x86)}\Inno Setup 6",
    "${env:ProgramFiles}\Inno Setup 6",
    "${env:LOCALAPPDATA}\Programs\Inno Setup 6"
)) {
    $cand = Join-Path $dir 'ISCC.exe'
    if (Test-Path $cand) { $iscc = $cand; break }
}
if (-not $iscc) {
    Write-Host 'ISCC.exe not found. Install Inno Setup 6 first:'
    Write-Host '    winget install -e --id JRSoftware.InnoSetup'
    exit 1
}
Write-Host ('==> Inno Setup  : ' + $iscc)

# ---- 1. release build -------------------------------------------------------
if ($SkipBuild) {
    Write-Host '==> cargo build --release  (SKIPPED by -SkipBuild)'
} else {
    Write-Host '==> cargo build --release'
    # cargo writes warnings to stderr; through cmd.exe they stay plain text instead of
    # turning into PowerShell NativeCommandError records.
    cmd /c 'cargo build --release 2>&1' | Select-Object -Last 4 | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) {
        Write-Host 'BUILD FAILED. If it said "failed to remove ... audioserver.exe",'
        Write-Host 'the GUI is still running - close it and re-run this script.'
        exit 1
    }
}

$guiExe = Join-Path $root 'target\release\audioserver.exe'
$cliExe = Join-Path $root 'target\release\server.exe'
foreach ($e in @($guiExe, $cliExe)) {
    if (-not (Test-Path $e)) { Write-Host ('release exe missing: ' + $e); exit 1 }
}

# ---- 2. version from git ----------------------------------------------------
$describe = (& git describe --tags --always).Trim()
$commit   = (& git rev-parse --short HEAD).Trim()
# "v3.4.11-1-g8a1c5c3" -> "v3.4.11". File name keeps the readable v-prefix; the
# Windows file-version fields need digits only.
$version  = ($describe -split '-')[0]
$verNum   = $version -replace '^v', ''
$builtAt  = (Get-Item $guiExe).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
Write-Host ('==> version     : ' + $version + '  (commit ' + $commit + ', built ' + $builtAt + ')')

# ---- 3. stage ---------------------------------------------------------------
$stage = Join-Path $root 'dist\stage-installer'
if (Test-Path $stage) {
    # Files we just copied are often still held by an antivirus scan, so one delete
    # attempt can fail. Retry, and give up loudly rather than packaging a dirty tree.
    for ($i = 0; $i -lt 5 -and (Test-Path $stage); $i++) {
        Remove-Item $stage -Recurse -Force -ErrorAction Continue
        Start-Sleep -Milliseconds 600
    }
}
if (Test-Path $stage) {
    Write-Host ('STAGING DIR COULD NOT BE CLEANED: ' + $stage)
    Write-Host 'Close any Explorer window / terminal whose cwd is inside it, then re-run.'
    exit 1
}
New-Item -ItemType Directory -Path $stage | Out-Null

Copy-Item $guiExe (Join-Path $stage 'audioserver.exe')
Copy-Item $cliExe (Join-Path $stage 'server.exe')
Copy-Item (Join-Path $root 'README.md')          (Join-Path $stage 'README.md')
Copy-Item (Join-Path $root 'docs\quick-start.md') (Join-Path $stage 'quick-start.md')

@"
package    : AudioServer $version installer
commit     : $commit
git ref    : $describe
built at   : $builtAt
profile    : release (opt-level=3, lto=true, codegen-units=1, strip=true)
sha256(gui): $( (Get-FileHash $guiExe -Algorithm SHA256).Hash )
sha256(cli): $( (Get-FileHash $cliExe -Algorithm SHA256).Hash )
drivers    : NONE installed or bundled - see installer\setup.iss header comment
"@ | Set-Content -Path (Join-Path $stage 'VERSION.txt') -Encoding ASCII

# ---- 4. config.default.json, generated BY the built binary ------------------
# Decoding: the exe prints UTF-8. PowerShell would decode that with the OEM codepage,
# which is wrong on a non-Chinese machine, so force the console reader to UTF-8 first
# and then write the file as UTF-8 WITHOUT a BOM (serde_json refuses a leading BOM).
Write-Host '==> generating config.default.json from the built binary'
$prevEnc = [Console]::OutputEncoding
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$json = (& $cliExe '--print-default-config' | Out-String)
[Console]::OutputEncoding = $prevEnc
if ([string]::IsNullOrWhiteSpace($json)) {
    Write-Host 'server.exe --print-default-config produced nothing - is the exe too old?'
    exit 1
}
try {
    $parsed = $json | ConvertFrom-Json
} catch {
    Write-Host ('generated config is not valid JSON: ' + $_.Exception.Message)
    exit 1
}
$cfgPath = Join-Path $stage 'config.default.json'
[System.IO.File]::WriteAllText($cfgPath, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Host ('    sections: ' + (($parsed.PSObject.Properties.Name) -join ', '))

# ---- 5. hard guard: no driver payload, no unexpected files ------------------
$expected = @('audioserver.exe', 'server.exe', 'README.md', 'quick-start.md',
              'VERSION.txt', 'config.default.json')
foreach ($f in $expected) {
    if (-not (Test-Path (Join-Path $stage $f))) { Write-Host ('MISSING in stage: ' + $f); exit 1 }
}
$all = Get-ChildItem $stage -Recurse -File
$extra = $all | Where-Object { $expected -notcontains $_.Name }
# -Include with -Recurse is unreliable on PS 5.1, so filter by extension ourselves.
$driverish = $all | Where-Object {
    '.msi', '.inf', '.sys', '.dll', '.bat', '.ps1', '.reg' -contains $_.Extension.ToLower()
}
if ($extra -or $driverish) {
    Write-Host 'INSTALLER MUST CONTAIN ONLY THE APP (no driver, no install helper):'
    if ($extra)      { $extra      | ForEach-Object { Write-Host ('  unexpected : ' + $_.Name) } }
    if ($driverish)  { $driverish  | ForEach-Object { Write-Host ('  driver-ish : ' + $_.Name) } }
    exit 1
}
Write-Host ('==> staged tree ok (' + $expected.Count + ' files, no driver payload)')

# ---- 6. compile the installer ----------------------------------------------
$outDir = Join-Path $root 'dist'
if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }
$iss = Join-Path $root 'installer\setup.iss'
$innoDir = Split-Path $iscc -Parent
# /D... are the preprocessor symbols setup.iss documents at the top.
& $iscc ('/DMyAppVersion=' + $version) ('/DMyVerNum=' + $verNum) ('/DMyCommit=' + $commit) `
       ('/DStageDir=' + $stage) ('/DInnoDir=' + $innoDir) ('/O' + $outDir) $iss
if ($LASTEXITCODE -ne 0) { Write-Host 'ISCC failed'; exit 1 }

$setup = Join-Path $outDir ('AudioServer-' + $version + '-win-x64-setup.exe')
if (-not (Test-Path $setup)) { Write-Host ('setup exe not found: ' + $setup); exit 1 }

# ---- 7. result --------------------------------------------------------------
Write-Host '==> result'
Write-Host ('setup exe : ' + $setup)
Write-Host ('size      : ' + ('{0:N1} MB' -f ((Get-Item $setup).Length / 1MB)))
Write-Host ('sha256    : ' + (Get-FileHash $setup -Algorithm SHA256).Hash)
Write-Host ''
Write-Host 'Verify before handing it out:'
Write-Host ('  ' + $setup + ' /HELP            (or start it and pick a TEMP folder)')
