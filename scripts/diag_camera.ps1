# Camera diagnostic script
Write-Host "=== 1. Camera-related services ==="
Get-Service | Where-Object { $_.DisplayName -match 'camera|frame|video|capture' } | Format-Table Name, DisplayName, Status, StartType -AutoSize

Write-Host "`n=== 2. PnP Camera devices ==="
Get-PnpDevice -Class Camera -ErrorAction SilentlyContinue | Format-Table Status, Class, FriendlyName, InstanceId -AutoSize

Write-Host "`n=== 3. PnP Image devices ==="
Get-PnpDevice -Class Image -ErrorAction SilentlyContinue | Format-Table Status, Class, FriendlyName, InstanceId -AutoSize

Write-Host "`n=== 4. Camera privacy setting ==="
$val = Get-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\webcam' -Name Value -ErrorAction SilentlyContinue
if ($val) { Write-Host "Camera access: $($val.Value)" } else { Write-Host "NOT FOUND" }

Write-Host "`n=== 5. FrameServer registry config ==="
$fs = Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Services\FrameServer' -ErrorAction SilentlyContinue
if ($fs) {
    Write-Host "Start: $($fs.Start)  (0=boot 1=system 2=auto 3=demand 4=disabled)"
    Write-Host "ImagePath: $($fs.ImagePath)"
}

Write-Host "`n=== 6. DirectShow filters (obs-virtualcam / UnityCapture) ==="
# Check registered DirectShow filters
$dsPaths = @(
    'HKLM:\SOFTWARE\Microsoft\DirectShow\Filters',
    'HKLM:\SOFTWARE\Classes\CLSID'
)
# Look for OBS Virtual Camera filter CLSID
$obsClsid = Get-ChildItem 'HKLM:\SOFTWARE\Classes\CLSID' -ErrorAction SilentlyContinue | ForEach-Object {
    $name = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).'(default)'
    if ($name -match 'OBS|Virtual') {
        Write-Host "Found: $($_.PSChildName) = $name"
    }
}

Write-Host "`n=== 7. OBS VirtualCam registry entries ==="
Get-ChildItem 'HKLM:\SOFTWARE\OBSVirtualCam' -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host $_.PSPath
    Get-ItemProperty $_.PSPath | Format-List
}
Get-ChildItem 'HKLM:\SOFTWARE\Classes\CLSID' -ErrorAction SilentlyContinue | Where-Object {
    (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).'(default)' -match 'OBS'
} | ForEach-Object {
    Write-Host "OBS CLSID: $($_.PSChildName)"
    $inproc = Join-Path $_.PSPath 'InprocServer32'
    if (Test-Path $inproc) {
        Get-ItemProperty $inproc | Format-List
    }
}

Write-Host "`n=== 8. Media Foundation camera enumeration test ==="
# Try to enumerate cameras via WinRT
try {
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    $null = [Windows.Devices.Enumeration.DeviceInformation,Windows.Devices.Enumeration,ContentType=WindowsRuntime]
    $asyncOp = [Windows.Devices.Enumeration.DeviceInformation]::FindAllAsync([Windows.Devices.Enumeration.DeviceInformation]::GetDeviceSelector('VideoCapture'))
    # Wait for completion
    $task = $asyncOp.AsTask()
    $task.Wait(5000) | Out-Null
    $result = $task.Result
    Write-Host "WinRT VideoCapture devices found: $($result.Count)"
    foreach ($dev in $result) {
        Write-Host "  - $($dev.Name) [$($dev.Id)]"
    }
} catch {
    Write-Host "WinRT enumeration failed: $($_.Exception.Message)"
}
