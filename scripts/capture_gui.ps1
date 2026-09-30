# One-off diagnostic: bring the audioserver GUI window to the front and screenshot it.
# Touches only our own process's window. ASCII only (PS 5.1 parses .ps1 as ANSI).
param(
    [string]$Name = 'audioserver',
    [string]$Out = 'D:\temp\pcs_diag\gui.png',
    # Capture the maximised window: the settings page is taller than the default
    # 420x540 client area, so this is how you see every card at once.
    [switch]$Maximize
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
$p = Get-Process -Name $Name -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($null -eq $p) { Write-Output 'NO WINDOW'; exit 1 }
$sw = if ($Maximize) { 3 } else { 9 }   # 3 = SW_MAXIMIZE, 9 = SW_RESTORE
[void][Win]::ShowWindowAsync($p.MainWindowHandle, $sw)
Start-Sleep -Milliseconds 900
[void][Win]::SetForegroundWindow($p.MainWindowHandle)
Start-Sleep -Milliseconds 700
$rect = New-Object Win+RECT
[void][Win]::GetWindowRect($p.MainWindowHandle, [ref]$rect)
$w = $rect.R - $rect.L
$h = $rect.B - $rect.T
if ($w -le 0 -or $h -le 0) { Write-Output 'BAD RECT'; exit 1 }
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.L, $rect.T, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$dir = Split-Path $Out -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
Write-Output "saved $Out (${w}x${h})"
