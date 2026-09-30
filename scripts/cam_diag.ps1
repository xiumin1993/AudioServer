# 摄像头链路诊断：把连接边界、摄像头消息、帧流量按时间顺序对齐打印
# 目的：判断视频通话时"显示过期照片"到底是
#   (a) 手机没登记摄像头会话（没 cam_start）
#   (b) 登记了但手机没推帧
#   (c) 手机推帧了但 OBS 引擎没写进共享内存
$ErrorActionPreference = 'Stop'
$log = 'D:\code\AudioServer\target\release\audioserver.log'
$lines = Get-Content $log

Write-Host "=== 消息类型统计（cam/mic 相关）==="
$msgs = $lines | Select-String 'Client message:'
$msgs | ForEach-Object {
    $t = $_.Line -replace '.*Client message:\s*', ''
    if ($t -match '"type":"([a-z_]+)"') { $Matches[1] } else { '?' }
} | Group-Object | Sort-Object Count -Descending | Format-Table Count, Name -AutoSize

Write-Host "=== 连接 / 摄像头事件 时间线（最后 45 条）==="
$mark = $lines | Select-String -Pattern 'New connection:|Downlink task exited|Client requested close|"type":"cam_start"|"type":"cam_stop"|"type":"mic_start"|\[Cam\]|VcamObs\] OBS camera|handle count|First frame written|First JPEG from phone|mapping created'
$mark | Select-Object -Last 45 | ForEach-Object {
    $t = $_.Line -replace '^\[(\w+) [\w:]+ [^\]]+\] ', ''
    "L{0,6}  {1}" -f $_.LineNumber, $t
}

Write-Host ""
Write-Host "=== 摄像头帧流量：手机推了多少字节 ==="
$flow = $lines | Select-String -Pattern 'cam|Cam' | Where-Object { $_.Line -match 'bytes|KB|packets|fps' }
if ($flow) { $flow | Select-Object -Last 12 | ForEach-Object { "L{0,6}  {1}" -f $_.LineNumber, ($_.Line -replace '^\[\w+ [\w:]+ [^\]]+\] ', '') } }
else { Write-Host "  (日志里没有任何摄像头帧流量记录 → 手机一帧都没推)" }

Write-Host ""
Write-Host "=== 最后一次 New connection 之后出现过哪些消息 ==="
$lastConn = ($lines | Select-String 'New connection:' | Select-Object -Last 1).LineNumber
Write-Host "  最后连接起始行: L$lastConn"
$lines | Select-String 'Client message:' | Where-Object { $_.LineNumber -ge $lastConn } | ForEach-Object {
    $t = $_.Line -replace '.*Client message:\s*', ''
    if ($t.Length -gt 110) { $t = $t.Substring(0, 110) + '...' }
    "  L{0,6}  {1}" -f $_.LineNumber, $t
}
