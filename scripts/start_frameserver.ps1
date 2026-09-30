# Start FrameServer with elevation
Start-Process -FilePath "net.exe" -ArgumentList "start FrameServer" -Verb RunAs -Wait
Start-Sleep -Seconds 2
# Verify
$svc = Get-Service FrameServer
Write-Host "FrameServer status: $($svc.Status)"
