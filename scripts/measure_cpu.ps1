param(
    [string]$Name = 'audioserver',
    [int]$Seconds = 8
)
$cpuCount = (Get-CimInstance Win32_ComputerSystem).NumberOfLogicalProcessors

function Snap($n) {
  $p = @(Get-Process -Name $n -ErrorAction SilentlyContinue)
  if ($p.Count -eq 0) { return $null }
  $total = 0.0
  $ws = 0
  foreach ($proc in $p) {
    $total += $proc.TotalProcessorTime.TotalMilliseconds
    $ws += $proc.WorkingSet64
  }
  return @{ t = $total; ws = $ws; count = $p.Count }
}

$a = Snap $Name
if ($null -eq $a) { Write-Output 'PROCESS NOT FOUND'; exit 1 }
Start-Sleep -Seconds $Seconds
$b = Snap $Name

$dt = $b.t - $a.t
$wall = $Seconds * 1000
$pctOneCore = ($dt / $wall) * 100
$pctTotal = ($dt / ($wall * $cpuCount)) * 100

Write-Output ("logical_cpus      = {0}" -f $cpuCount)
Write-Output ("window_ms         = {0}" -f $wall)
Write-Output ("cpu_ms_consumed   = {0:N0}" -f $dt)
Write-Output ("percent_of_1_core = {0:N1}%" -f $pctOneCore)
Write-Output ("percent_of_total  = {0:N1}%" -f $pctTotal)
Write-Output ("working_set_MB    = {0:N1}" -f ($b.ws / 1MB))
