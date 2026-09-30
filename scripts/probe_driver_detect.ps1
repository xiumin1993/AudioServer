$ErrorActionPreference = 'Continue'

Write-Host '=== MMDevices friendly names containing CABLE ==='
foreach ($root in @('HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render',
                    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture')) {
    Get-ChildItem $root -ErrorAction SilentlyContinue | ForEach-Object {
        $prof = Join-Path $_.PSPath 'Properties'
        $fp = Join-Path $prof '{a45c254e-df1c-4efa-8020-16d0371820f8},2'
        $name = (Get-ItemProperty $prof -ErrorAction SilentlyContinue).$fp
        if ($name -match 'CABLE|VB-Audio') { Write-Host ("  HIT " + $name + "   [" + $_.PSChildName + "]  " + $root.Split('\')[-1]) }
    }
}

Write-Host ''
Write-Host '=== where is the cable driver sys file ==='
Get-ChildItem 'C:\Windows\System32\drivers' -Filter 'vb*' -ErrorAction SilentlyContinue | ForEach-Object { Write-Host ('  ' + $_.Name) }
Write-Host '  --- DriverStore entries for vbcable:'
Get-ChildItem 'C:\Windows\System32\DriverStore\FileRepository' -Filter '*cable*' -ErrorAction SilentlyContinue | ForEach-Object { Write-Host ('  ' + $_.Name) }

Write-Host ''
Write-Host '=== HKLM\SOFTWARE\VB-Audio content ==='
Get-ChildItem 'HKLM:\SOFTWARE\VB-Audio' -ErrorAction SilentlyContinue | ForEach-Object { Write-Host ('  subkey: ' + $_.PSChildName) }
Get-ItemProperty 'HKLM:\SOFTWARE\VB-Audio' -ErrorAction SilentlyContinue | Format-List | Out-String -Stream | Select-Object -First 10
