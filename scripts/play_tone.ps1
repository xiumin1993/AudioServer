# One-off diagnostic: play a very quiet sine tone so WASAPI loopback has real data to
# capture, letting us measure CPU + streaming behaviour under active audio.
# Amplitude is ~2% of full scale on purpose: enough for non-zero packets, nearly silent
# so it does not blast the speakers. Writes one temp WAV and deletes it. ASCII only.
param(
    [int]$Seconds = 12,
    [int]$Amplitude = 650
)
Add-Type -AssemblyName System.Windows.Forms
$rate = 48000
$ch = 2
$n = $rate * $Seconds
$bytesPer = $n * $ch * 2
$ms = New-Object IO.MemoryStream
$bw = New-Object IO.BinaryWriter($ms)
$bw.Write([byte[]](0x52,0x49,0x46,0x46)); $bw.Write([int](36 + $bytesPer))
$bw.Write([byte[]](0x57,0x41,0x56,0x45))
$bw.Write([byte[]](0x66,0x6d,0x74,0x20)); $bw.Write([int]16)
$bw.Write([int16]1); $bw.Write([int16]$ch); $bw.Write([int]$rate)
$bw.Write([int]($rate * $ch * 2)); $bw.Write([int16]($ch * 2)); $bw.Write([int16]16)
$bw.Write([byte[]](0x64,0x61,0x74,0x61)); $bw.Write([int]$bytesPer)
for ($i = 0; $i -lt $n; $i++) {
  $v = [int]($Amplitude * [Math]::Sin(2 * [Math]::PI * 440 * ($i / $rate)))
  $s = [int16]$v
  $bw.Write($s); $bw.Write($s)
}
$bw.Flush()
$path = Join-Path $env:TEMP 'pcs_tone.wav'
[IO.File]::WriteAllBytes($path, $ms.ToArray())
$bw.Close(); $ms.Close()
$sp = New-Object Media.SoundPlayer($path)
$sp.PlayLooping()
Write-Output "playing $Seconds s of 440Hz at amplitude $Amplitude (path $path)"
Start-Sleep -Seconds $Seconds
$sp.Stop()
Remove-Item $path -ErrorAction SilentlyContinue
Write-Output 'done'
