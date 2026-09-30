# One-off diagnostic: prove the server is quiet while idle - the log must not grow and
# the old per-frame "[Lang] settings.txt language =" spam must be frozen at its history.
# ASCII only (PS 5.1 decodes .ps1 as ANSI).
param(
    [string]$Log = 'D:\code\AudioServer\target\release\audioserver.log',
    [int]$Seconds = 12
)
$before = (Get-Item $Log).Length
Start-Sleep -Seconds $Seconds
$after = (Get-Item $Log).Length
$lines = (Select-String -Path $Log -Pattern 'settings.txt language' -AllMatches | Measure-Object).Count
Write-Output ("log_bytes_before       = {0}" -f $before)
Write-Output ("log_bytes_after        = {0}" -f $after)
Write-Output ("growth_in_{0}s         = {1}" -f $Seconds, ($after - $before))
Write-Output ("lang_line_total_in_log = {0}" -f $lines)
