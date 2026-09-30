# verify_env_gate.ps1 - check the v3.5 startup environment gate on this PC.
#
# It runs the freshly built GUI twice and proves the two branches:
#   A) PCSPEAKER_FORCE_ENV_GUIDE=1  -> guide page shows, TCP 8080 is NOT listening
#   B) normal start                 -> main UI shows,  TCP 8080 IS listening
# Each run is screenshotted (window brought to foreground first) and the log tail
# is printed, so "did the server really stay asleep" is answered by evidence.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\verify_env_gate.ps1
#
# NOTE: stops any running AudioServer at the start and leaves the NORMAL (ready)
#       instance running at the end, because that is what the user had open.

$ErrorActionPreference = 'Continue'
$root  = 'D:\code\AudioServer'
$shots = Join-Path $root 'dist\shots'
Set-Location $root
if (-not (Test-Path $shots)) { New-Item -ItemType Directory -Path $shots | Out-Null }

# ---- window foreground + screenshot helpers (only system APIs already present) --
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace Win32 -Name W -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool SetForegroundWindow(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool ShowWindow(System.IntPtr h, int n);
'@

function Stop-AudioServer {
    # taskkill by image name is more reliable here than a Get-Process pipeline
    # (the top-of-script Get-Process call returned nothing while the process was
    #  clearly alive - see notes in the commit). Then poll until it is really gone:
    #  cargo cannot overwrite target/release/audioserver.exe while it runs (os error 5).
    cmd /c 'taskkill /IM audioserver.exe /F >NUL 2>&1'
    for ($i = 0; $i -lt 10; $i++) {
        Start-Sleep -Milliseconds 500
        $left = (cmd /c 'tasklist /FI "IMAGENAME eq audioserver.exe" /NH' | Select-String 'audioserver').Count
        if ($left -eq 0) { Write-Host "  audioserver stopped (poll #$i)"; return }
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
    if ($null -eq $p) { Write-Host "no audioserver window to shoot ($Name)"; return }
    $h = $p.MainWindowHandle
    if ($h -ne [System.IntPtr]::Zero) {
        [Win32.W]::ShowWindow($h, 9) | Out-Null   # SW_RESTORE
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
    Write-Host "screenshot -> $out"
}

function Log-Tail {
    param([int]$Lines)
    $log = Join-Path $root 'target\release\audioserver.log'
    if (Test-Path $log) {
        Get-Content $log -Tail $Lines | ForEach-Object { Write-Host "  | $_" }
    } else { Write-Host "  (no log file yet)" }
}

# ---- 1. release build ---------------------------------------------------------
# Kill the running window FIRST: while the exe is running, cargo cannot overwrite
# it and the build dies with "failed to remove ... (os error 5)".
# (Kept in ASCII on purpose: a UTF-8 Chinese comment right before a code line can
#  swallow the newline under PowerShell 5.1's ANSI decoding and comment out the call.)
Stop-AudioServer
Write-Host '==> cargo build --release'
cmd /c 'cargo build --release 2>&1' | Select-Object -Last 4 | ForEach-Object { Write-Host $_ }
if ($LASTEXITCODE -ne 0) { Write-Host 'BUILD FAILED'; exit 1 }
$exe = Join-Path $root 'target\release\audioserver.exe'
Write-Host ('exe mtime : ' + (Get-Item $exe).LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))

Remove-Item (Join-Path $root 'target\release\audioserver.log') -Force -ErrorAction SilentlyContinue

# ---- 2A. forced guide page (simulates "driver missing") -----------------------
Write-Host ''
Write-Host '==> [A] PCSPEAKER_FORCE_ENV_GUIDE=1 : expect guide page, port 8080 CLOSED'
$env:PCSPEAKER_FORCE_ENV_GUIDE = '1'
Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe)
Start-Sleep -Seconds 5
Shot-Window 'A_guide'
Write-Host ('    8080 listeners = ' + (Port-Listening -Port 8080) + '  (must be 0)')
Write-Host '    log tail:'; Log-Tail -Lines 14
Stop-AudioServer
Remove-Item Env:\PCSPEAKER_FORCE_ENV_GUIDE

# ---- 2B. normal start (this PC has every driver) ------------------------------
Write-Host ''
Write-Host '==> [B] normal start : expect main UI, port 8080 LISTENING'
Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe)
Start-Sleep -Seconds 6
Shot-Window 'B_main'
Write-Host ('    8080 listeners = ' + (Port-Listening -Port 8080) + '  (must be >= 1)')
Write-Host '    log tail:'; Log-Tail -Lines 14
Write-Host ''
Write-Host '==> done. AudioServer (normal instance) left running for the user.'
