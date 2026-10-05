Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen(0, 0, 0, 0, $b.Size)
$out = "C:\Users\Public\Documents\ESTsoft\CreatorTemp\gui1.png"
$bmp.Save($out)
$g.Dispose()
$bmp.Dispose()
Write-Host "saved $out"
