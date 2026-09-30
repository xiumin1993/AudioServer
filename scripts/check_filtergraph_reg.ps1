# 检查 DirectShow Filter Graph Manager 的 COM 注册情况
# 用途：dshow_consumer_probe 里 CoCreateInstance(CLSID_FilterGraph) 报 REGDB_E_CLASSNOTREG，
#       需要确认是 CLSID 写错、注册表缺失，还是 32/64 位视图问题。
$ErrorActionPreference = 'SilentlyContinue'

$ids = [ordered]@{
    'CLSID_FilterGraph (quartz)'   = 'E5F188C1-B7BA-11CF-BA53-0020AF0BA770'
    'CLSID_SystemDeviceEnum (已验证可用)' = '62BE5D10-60EB-11D0-BD3B-00A0C911CE86'
}

foreach ($kv in $ids.GetEnumerator()) {
    $guid = $kv.Value
    foreach ($hive in 'HKLM:\SOFTWARE\Classes\CLSID', 'HKLM:\SOFTWARE\Classes\Wow6432Node\CLSID') {
        $p = Join-Path $hive "{$guid}"
        if (Test-Path $p) {
            $server = (Get-ItemProperty -Path (Join-Path $p 'InprocServer32')).'(default)'
            Write-Host ("[存在] {0}  {1}`n        -> {2}" -f $kv.Key, $p, $server)
        } else {
            Write-Host ("[缺失] {0}  {1}" -f $kv.Key, $p)
        }
    }
    Write-Host ''
}

Write-Host '--- 系统 DLL 存在性 ---'
foreach ($f in 'C:\Windows\System32\quartz.dll', 'C:\Windows\SysWOW64\quartz.dll') {
    Write-Host ("{0} : {1}" -f $f, (Test-Path $f))
}

Write-Host ''
Write-Host '--- OBS Virtual Camera 滤镜的 InprocServer32 指向 ---'
# 从设备枚举的友好名反查滤镜 CLSID：直接扫 quartz 相关注册不现实，
# 这里改为搜注册表里含 "OBS" 且带 InprocServer32 的 CLSID
$hits = Get-ChildItem 'HKLM:\SOFTWARE\Classes\CLSID' | Where-Object {
    $def = (Get-ItemProperty $_.PSPath).'(default)'
    $def -like '*OBS*Camera*' -or $def -like '*Virtual Camera*'
}
if (-not $hits) {
    # 64 位系统上 DShow 滤镜常注册在 Wow6432Node
    $hits = Get-ChildItem 'HKLM:\SOFTWARE\Classes\Wow6432Node\CLSID' | Where-Object {
        $def = (Get-ItemProperty $_.PSPath).'(default)'
        $def -like '*OBS*Camera*' -or $def -like '*Virtual Camera*'
    }
    $scope = 'Wow6432Node'
} else {
    $scope = 'CLSID'
}
foreach ($h in $hits) {
    $name = (Get-ItemProperty $h.PSPath).'(default)'
    $srv = (Get-ItemProperty (Join-Path $h.PSPath 'InprocServer32')).'(default)'
    $th = (Get-ItemProperty (Join-Path $h.PSPath 'InprocServer32')).'ThreadingModel'
    Write-Host ("  CLSID {0}`n      名称: {1}`n      DLL : {2}  (ThreadingModel={3})" -f $h.PSChildName, $name, $srv, $th)
}
Write-Host "(搜索范围：$scope，命中 $($hits.Count) 个)"
