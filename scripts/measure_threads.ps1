param(
    [string]$Name = 'audioserver',
    [int]$Seconds = 8
)
# Per-thread CPU delta profiler: finds WHICH thread is burning a core.
$procs = @(Get-Process -Name $Name -ErrorAction SilentlyContinue)
if ($procs.Count -eq 0) { Write-Output 'PROCESS NOT FOUND'; exit 1 }

function SnapThreads($proc) {
  $map = @{}
  foreach ($t in $proc.Threads) {
    $map[[int]$t.Id] = $t.TotalProcessorTime.TotalMilliseconds
  }
  return $map
}

$before = @{}
foreach ($p in $procs) { $before[[int]$p.Id] = SnapThreads $p }
Start-Sleep -Seconds $Seconds
$wall = $Seconds * 1000

$rows = @()
foreach ($p in $procs) {
  $pid2 = [int]$p.Id
  $after = SnapThreads $p
  foreach ($tid in $after.Keys) {
    if (-not $before[$pid2].Contains($tid)) { continue }
    $delta = $after[$tid] - $before[$pid2][$tid]
    if ($delta -lt 0) { $delta = 0 }
    $pct = ($delta / $wall) * 100
    if ($pct -ge 0.5) {
      $rows += [pscustomobject]@{
        ProcId = $pid2
        ThreadId = $tid
        CpuMs = [math]::Round($delta, 0)
        PctOfOneCore = [math]::Round($pct, 1)
      }
    }
  }
}
$rows | Sort-Object -Descending PctOfOneCore | Format-Table -AutoSize | Out-String -Width 200
$total = ($rows | Measure-Object -Property PctOfOneCore -Sum).Sum
Write-Output ("TOTAL percent_of_one_core = {0:N1}" -f $total)
