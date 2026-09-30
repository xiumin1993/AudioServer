param([string]$Name = 'audioserver', [int]$Seconds = 6)
$p = @(Get-Process -Name $Name -ErrorAction SilentlyContinue)
if ($p.Count -eq 0) { Write-Output 'NOT FOUND'; exit 1 }
$proc = $p[0]
$procStart = $proc.StartTime

function Snap($proc) {
  $m = @{}
  foreach ($t in $proc.Threads) { $m[[int]$t.Id] = $t.TotalProcessorTime.TotalMilliseconds }
  return $m
}
$b = Snap $proc
Start-Sleep -Seconds $Seconds
$a = Snap $proc
$wall = $Seconds * 1000

$rows = @()
foreach ($t in $a.Keys) {
  if (-not $b.Contains($t)) { continue }
  $delta = $a[$t] - $b[$t]
  $pct = [math]::Round(($delta / $wall) * 100, 1)
  $th = $null
  foreach ($x in $proc.Threads) { if ([int]$x.Id -eq [int]$t) { $th = $x; break } }
  $isMain = $false
  $st = $null
  if ($null -ne $th) {
    try { $st = $th.StartTime } catch { $st = $null }
    if ($null -ne $st) { $isMain = ([math]::Abs(($st - $procStart).TotalSeconds) -lt 2) }
  }
  if ($pct -ge 0.3) {
    $rows += [pscustomobject]@{
      Thread = $t
      PctOfCore = $pct
      IsMainThread = $isMain
      ThreadStart = $st
      State = if ($null -ne $th) { $th.ThreadState } else { '?' }
      Wait = if ($null -ne $th) { $th.WaitReason } else { '?' }
    }
  }
}
Write-Output ("Process start = {0}" -f $procStart)
$rows | Sort-Object -Descending PctOfCore | Format-Table -AutoSize | Out-String -Width 220
