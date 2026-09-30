# One-off verification for "no console window in the release exe":
#   1. snapshot conhost PIDs, 2. start audioserver.exe, 3. see if a new console appeared,
#   4. confirm the GUI window exists, 5. confirm audioserver.log still got the startup lines.
# ASCII only (PS 5.1 decodes .ps1 as ANSI).
param(
    [string]$Exe = 'D:\code\AudioServer\target\release\audioserver.exe',
    [switch]$WithConsole
)
$log = Join-Path (Split-Path $Exe -Parent) 'audioserver.log'
$beforeLen = 0L
if (Test-Path $log) { $beforeLen = (Get-Item $log).Length }
$hostsBefore = @(Get-Process conhost -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$procsBefore = @(Get-Process audioserver -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })

if ($WithConsole) { $env:PCSPEAKER_CONSOLE = '1' }
Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe -Parent)
Start-Sleep -Seconds 4

$p = @(Get-Process audioserver -ErrorAction SilentlyContinue | Where-Object { $procsBefore -notcontains $_.Id })
$hostsAfter = @(Get-Process conhost -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$newHosts = @($hostsAfter | Where-Object { $hostsBefore -notcontains $_ })
$afterLen = 0L
if (Test-Path $log) { $afterLen = (Get-Item $log).Length }

Write-Output ("new_process_pid      = " + ($p | Select-Object -First 1).Id)
Write-Output ("has_gui_window       = " + ((Get-Process -Id ($p | Select-Object -First 1).Id).MainWindowHandle -ne 0))
Write-Output ("new_console_hosts    = " + $newHosts.Count + "  " + ($newHosts -join ','))
Write-Output ("log_bytes_growth     = " + ($afterLen - $beforeLen))
if ($WithConsole) { Remove-Item Env:\PCSPEAKER_CONSOLE }
Write-Output '--- last log lines ---'
Get-Content $log -Tail 4 -Encoding UTF8 | ForEach-Object { Write-Output $_ }
