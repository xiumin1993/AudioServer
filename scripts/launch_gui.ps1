# launch_gui.ps1 - start the release GUI visibly and report what it touched.
#
# Why a script instead of an inline command: this session's shell is Git Bash, which
# mangles "$env:NAME" and "$_" when PowerShell is invoked inline. Everything that
# needs a variable lives in this file instead. ASCII-only literals: PowerShell 5.1
# decodes .ps1 files without a BOM using the ANSI codepage.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\launch_gui.ps1
#   ... -DemoTab settings
#   ... -DemoMode mic -Lang zh

param(
    # Which page the first frame should land on: connection | settings | log
    [string]$DemoTab = 'connection',
    # Which mode card to show: speaker | mic | camera
    [string]$DemoMode = 'speaker',
    # Extra language for the first frame: '' (follow config/system) | en | zh
    [string]$Lang = '',
    # Wait this many milliseconds before reporting
    [int]$SettleMs = 2500
)

$exe = 'D:\code\AudioServer\target\release\audioserver.exe'
if (-not (Test-Path $exe)) { Write-Host "NOT BUILT: $exe"; exit 1 }

# A previous instance would (a) hold the exe open and (b) already own port 8080,
# making "is my new build listening?" impossible to answer.
$stale = Get-Process -Name audioserver -ErrorAction SilentlyContinue
if ($stale) {
    Write-Host ('stopping stale pid(s): ' + ($stale.Id -join ','))
    $stale | Stop-Process -Force
    Start-Sleep -Milliseconds 800
}

# Environment variables are inherited by the child process. These three are the
# documented developer switches (see main.rs); a normal user never sets them.
$env:PCSPEAKER_DEMO_TAB = $DemoTab
$env:PCSPEAKER_DEMO_MODE = $DemoMode
if ($Lang -ne '') { $env:PCSPEAKER_LANG = $Lang }

$before = (Get-Item $exe).LastWriteTime
$p = Start-Process -FilePath $exe -PassThru
Write-Host ("pid     : " + $p.Id)
Write-Host ("exe mtime: " + $before.ToString('yyyy-MM-dd HH:mm:ss'))
Start-Sleep -Milliseconds $SettleMs

$alive = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
if ($alive) {
    Write-Host ("running : yes (window title: '" + $alive.MainWindowTitle + "')")
} else {
    Write-Host 'running : NO - the process exited. Log tail:'
}

# The config file the app is supposed to create on first launch
$cfg = Join-Path $env:APPDATA 'PCAssistant\config.json'
if (Test-Path $cfg) {
    Write-Host ("config  : $cfg (" + (Get-Item $cfg).Length + ' bytes)')
} else {
    Write-Host "config  : MISSING at $cfg"
}

# Proof of life for the WebSocket listener: which PID owns the default port
Start-Sleep -Milliseconds 500
$conns = Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
    Where-Object { $_.LocalPort -in 8080, 8081 }
if ($conns) {
    $conns | ForEach-Object { Write-Host ("listen  : " + $_.LocalAddress + ':' + $_.LocalPort + ' pid=' + $_.OwningProcess) }
} else {
    Write-Host 'listen  : nothing on 8080/8081'
}

$log = Join-Path (Split-Path $exe) 'audioserver.log'
if (Test-Path $log) {
    Write-Host '--- last 12 log lines ---'
    Get-Content $log -Tail 12 | ForEach-Object { Write-Host $_ }
}
