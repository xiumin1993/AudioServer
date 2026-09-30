# One-off diagnostic: list our server processes + every console host window, so we can
# tell which window is the audioserver console. ASCII only (PS 5.1 decodes .ps1 as ANSI).
$names = @('audioserver', 'server', 'conhost', 'powershell', 'cmd')
Get-Process -Name $names -ErrorAction SilentlyContinue |
    Select-Object Name, Id, StartTime, @{n='Win';e={$_.MainWindowTitle}} |
    Sort-Object Name, Id |
    Format-Table -AutoSize |
    Out-String -Width 200 | Write-Output
