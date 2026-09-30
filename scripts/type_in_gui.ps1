# type_in_gui.ps1 - drive the AudioServer GUI with real mouse/keyboard input.
#
# Why: the settings page is supposed to write config.json when an input box is
# "finished with" (focus left / Enter). That behaviour cannot be proven by reading
# code or by unit tests over config::update() alone - it needs an actual click in an
# actual TextEdit. This script does the click + typing, then prints the config file so
# the result can be compared with what was typed.
#
# Coordinates are WINDOW-relative (top-left of the window = 0,0) because that is what a
# screenshot of the window gives us; the script adds the window rect to get screen coords.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\type_in_gui.ps1 `
#     -X 385 -Y 404 -SelectAll -Text 8081 -FinalKey Tab
#
# ASCII-only literals (PowerShell 5.1 decodes .ps1 without BOM as ANSI).

param(
    # Click point inside the window, in window-relative pixels
    [int]$X = 0,
    [int]$Y = 0,
    # Text to type after the click
    [string]$Text = '',
    # Select existing contents before typing: Tab / Enter / None
    [string]$FinalKey = 'Tab',
    [switch]$SelectAll,
    # Print the config file afterwards
    [switch]$ShowConfig
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win2 {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowRect(IntPtr h, out RECT2 r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT2 { public int L, T, R, B; }
}
"@

$MOUSEEVENTF_LEFTDOWN = 0x0002
$MOUSEEVENTF_LEFTUP = 0x0004

$p = Get-Process -Name audioserver -ErrorAction SilentlyContinue |
    Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if ($null -eq $p) { Write-Output 'NO WINDOW'; exit 1 }

# NOTE: deliberately no ShowWindowAsync here. SW_RESTORE would un-maximise the window
# and every window-relative coordinate we computed from a maximised screenshot would
# then point somewhere else on the desktop (that is exactly what happened on the first run).
[void][Win2]::SetForegroundWindow($p.MainWindowHandle)
Start-Sleep -Milliseconds 600

$rect = New-Object Win2+RECT2
[void][Win2]::GetWindowRect($p.MainWindowHandle, [ref]$rect)
$sx = $rect.L + $X
$sy = $rect.T + $Y
Write-Output ("window: " + $rect.L + ',' + $rect.T + ' size ' + ($rect.R - $rect.L) + 'x' + ($rect.B - $rect.T))
Write-Output ("click : screen $sx,$sy (window-relative $X,$Y)")

[void][Win2]::SetCursorPos($sx, $sy)
Start-Sleep -Milliseconds 250
[Win2]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
[Win2]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 400

if ($SelectAll) {
    [System.Windows.Forms.SendKeys]::SendWait('^a')
    Start-Sleep -Milliseconds 200
}
if ($Text -ne '') {
    # SendKeys needs braces around characters that are also its own control syntax
    $safe = $Text -replace '([+^%~(){}])', '{$1}'
    [System.Windows.Forms.SendKeys]::SendWait($safe)
    Start-Sleep -Milliseconds 300
}
switch ($FinalKey) {
    'Tab' { [System.Windows.Forms.SendKeys]::SendWait('{TAB}') }
    'Enter' { [System.Windows.Forms.SendKeys]::SendWait('{ENTER}') }
}
Start-Sleep -Milliseconds 700

if ($ShowConfig) {
    $cfg = Join-Path $env:APPDATA 'PCAssistant\config.json'
    Write-Output "--- $cfg ---"
    if (Test-Path $cfg) {
        Get-Content $cfg -Raw -Encoding UTF8 | Write-Output
    } else {
        Write-Output 'MISSING'
    }
}

$log = 'D:\code\AudioServer\target\release\audioserver.log'
if (Test-Path $log) {
    Write-Output '--- last 6 log lines ---'
    Get-Content $log -Tail 6 | ForEach-Object { Write-Output $_ }
}
