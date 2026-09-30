# restart_server.ps1 - rebuild and relaunch the AudioServer GUI, visibly.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\restart_server.ps1
#
# Why this exists: a stale target/release/audioserver.exe (older than the last
# commit) has twice caused a false "the fix did nothing" diagnosis. Always run
# this after changing Rust source, and read the printed exe timestamp.
#
# IMPORTANT - keep this file pure ASCII. PowerShell 5.1 decodes a BOM-less .ps1
# with the ANSI codepage, so UTF-8 Chinese text both garbles and (worse) a
# trailing multi-byte lead byte can swallow the following newline, silently
# commenting out the next line of code. That is exactly how an earlier version
# of this script skipped its own "stop the old process" step.

$ErrorActionPreference = 'Continue'
$exePath = 'D:\code\AudioServer\target\release\audioserver.exe'
Set-Location 'D:\code\AudioServer'

# 1) stop whatever holds the exe (cargo cannot overwrite a running image)
cmd /c 'taskkill /IM audioserver.exe /F >NUL 2>&1'
for ($i = 0; $i -lt 10; $i++) {
    Start-Sleep -Milliseconds 500
    $left = (cmd /c 'tasklist /FI "IMAGENAME eq audioserver.exe" /NH' | Select-String 'audioserver').Count
    if ($left -eq 0) { Write-Host "old instance stopped (poll #$i)"; break }
}
if ($left -ne 0) { Write-Host 'WARNING: audioserver still running, build may fail'; }

# 2) release build (cargo warns on stderr; go through cmd to keep plain text)
Write-Host '==> cargo build --release'
cmd /c 'cargo build --release 2>&1' | Select-Object -Last 5 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) { Write-Host 'BUILD FAILED - run cargo build --release by hand'; exit 1 }

# 3) prove the binary is fresh
Write-Host ('exe mtime = ' + (Get-Item -LiteralPath $exePath).LastWriteTime)

# 4) start it in a visible window (the user wants to see the server running)
Start-Process -FilePath $exePath -WorkingDirectory (Split-Path -Parent $exePath)
Start-Sleep -Seconds 4
$listen = (Get-NetTCPConnection -LocalPort 8080 -State Listen -ErrorAction SilentlyContinue | Measure-Object).Count
$new = Get-Process -Name 'audioserver' -ErrorAction SilentlyContinue
Write-Host ('started PID=' + (($new.Id) -join ',') + '  8080 listeners=' + $listen)
