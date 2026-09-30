# One-off diagnostic: save a full-screen PNG so the audioserver GUI can be inspected.
# Read-only, changes nothing. ASCII only: PowerShell 5.1 parses .ps1 as ANSI unless a
# BOM is present, so non-ASCII comments break the script body.
param(
    [string]$Out = 'D:\code\AudioServer\design\shots\screen.png'
)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$b = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap($b.Width, $b.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
$dir = Split-Path $Out -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose()
$bmp.Dispose()
Write-Output "saved $Out ($($b.Width)x$($b.Height))"
