# verify_i18n.ps1 - prove the v3.6 internationalization on this PC.
#
# Matrix: language (en / zh) x surface (guide page, speaker, mic, camera).
# Language is forced with PCSPEAKER_LANG (highest priority, beats settings.txt),
# the surface is forced with PCSPEAKER_DEMO_MODE / PCSPEAKER_DEMO_TAB, so no
# fragile mouse simulation is needed.
#
# Each run is screenshotted and the log tail printed. At the end ONE normal
# instance (system language, no overrides) is left running, like before.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\verify_i18n.ps1
#
# NOTE: comments are ASCII on purpose - PowerShell 5.1 decodes a BOM-less UTF-8
#       .ps1 as GBK, and a Chinese comment ending in a dangling lead byte can
#       swallow the next newline and comment out a code line.

$ErrorActionPreference = 'Continue'
$root  = 'D:\code\AudioServer'
$shots = Join-Path $root 'dist\shots\i18n'
Set-Location $root
if (Test-Path $shots) { Remove-Item (Join-Path $shots '*.png') -Force -ErrorAction SilentlyContinue }
if (-not (Test-Path $shots)) { New-Item -ItemType Directory -Path $shots | Out-Null }

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace Win32 -Name W -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool SetForegroundWindow(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool ShowWindow(System.IntPtr h, int n);
'@

function Stop-AudioServer {
    cmd /c 'taskkill /IM audioserver.exe /F >NUL 2>&1'
    for ($i = 0; $i -lt 10; $i++) {
        Start-Sleep -Milliseconds 500
        $left = (cmd /c 'tasklist /FI "IMAGENAME eq audioserver.exe" /NH' | Select-String 'audioserver').Count
        if ($left -eq 0) { return }
    }
    Write-Host '  WARNING: audioserver still running after 5s'
}

function Port-Listening {
    param([int]$Port)
    $c = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
    if ($c) { return ($c | Measure-Object).Count } else { return 0 }
}

function Shot-Window {
    param([string]$Name)
    $p = Get-Process -Name 'audioserver' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $p) { Write-Host "  no audioserver window to shoot ($Name)"; return }
    $h = $p.MainWindowHandle
    if ($h -ne [System.IntPtr]::Zero) {
        [Win32.W]::ShowWindow($h, 9) | Out-Null
        [Win32.W]::SetForegroundWindow($h) | Out-Null
    }
    Start-Sleep -Milliseconds 900
    $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object System.Drawing.Bitmap($b.Width, $b.Height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
    $out = Join-Path $shots ("$Name.png")
    $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Host "  screenshot -> $out"
}

function Log-Grep {
    param([string]$Pattern)
    $log = Join-Path $root 'target\release\audioserver.log'
    if (Test-Path $log) {
        Get-Content $log | Select-String $Pattern | Select-Object -Last 3 | ForEach-Object { Write-Host "  | $_" }
    }
}

function Run-Case {
    param([string]$Name, [hashtable]$Env)
    Stop-AudioServer
    Remove-Item (Join-Path $root 'target\release\audioserver.log') -Force -ErrorAction SilentlyContinue
    foreach ($k in $Env.Keys) { Set-Item -Path ("Env:\$k") -Value $Env[$k] }
    Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe)
    Start-Sleep -Seconds 5
    Shot-Window -Name $Name
    # the language actually applied is printed by apply_startup_locale()
    Log-Grep -Pattern 'UI language|UI locale|demo override'
    Write-Host ('    8080 listeners = ' + (Port-Listening -Port 8080))
    foreach ($k in $Env.Keys) { Remove-Item ("Env:\$k") -ErrorAction SilentlyContinue }
}

# ---- 1. release build ---------------------------------------------------------
Stop-AudioServer
Write-Host '==> cargo build --release'
cmd /c 'cargo build --release 2>&1' | Select-Object -Last 4 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) { Write-Host 'BUILD FAILED'; exit 1 }
$exe = Join-Path $root 'target\release\audioserver.exe'
Write-Host ('exe mtime : ' + (Get-Item $exe).LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))

# ---- 2. the matrix ------------------------------------------------------------
Write-Host ''
Write-Host '==> [1/7] guide page, ENGLISH'
Run-Case -Name '01_guide_en' -Env @{ PCSPEAKER_LANG = 'en'; PCSPEAKER_FORCE_ENV_GUIDE = '1' }

Write-Host ''
Write-Host '==> [2/7] guide page, CHINESE'
Run-Case -Name '02_guide_zh' -Env @{ PCSPEAKER_LANG = 'zh'; PCSPEAKER_FORCE_ENV_GUIDE = '1' }

Write-Host ''
Write-Host '==> [3/7] speaker mode / settings tab (LANGUAGE card), ENGLISH'
Run-Case -Name '03_speaker_settings_en' -Env @{ PCSPEAKER_LANG = 'en'; PCSPEAKER_DEMO_TAB = 'settings' }

Write-Host ''
Write-Host '==> [4/7] speaker mode / settings tab (LANGUAGE card), CHINESE'
Run-Case -Name '04_speaker_settings_zh' -Env @{ PCSPEAKER_LANG = 'zh'; PCSPEAKER_DEMO_TAB = 'settings' }

Write-Host ''
Write-Host '==> [5/7] mic mode / connection tab, ENGLISH'
Run-Case -Name '05_mic_conn_en' -Env @{ PCSPEAKER_LANG = 'en'; PCSPEAKER_DEMO_MODE = 'mic' }

Write-Host ''
Write-Host '==> [6/7] mic mode / connection tab, CHINESE'
Run-Case -Name '06_mic_conn_zh' -Env @{ PCSPEAKER_LANG = 'zh'; PCSPEAKER_DEMO_MODE = 'mic' }

Write-Host ''
Write-Host '==> [7/7] camera mode / connection tab, CHINESE'
Run-Case -Name '07_cam_conn_zh' -Env @{ PCSPEAKER_LANG = 'zh'; PCSPEAKER_DEMO_MODE = 'camera' }

# ---- 3. leave the normal instance running (system language, no overrides) ------
Write-Host ''
Write-Host '==> final: normal start (no PCSPEAKER_* overrides)'
Run-Case -Name '08_normal' -Env @{}
Write-Host '==> done. Screenshots in dist\shots\i18n'
