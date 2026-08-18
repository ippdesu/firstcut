# 生成 M0 测试样例：合成 JPG（无 EXIF）+ ARW 占位文件
# 用法: pwsh samples/make_samples.ps1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$out = Join-Path $PSScriptRoot 'generated'
New-Item -ItemType Directory -Force -Path $out | Out-Null

function New-TestJpg($name, $colorFn) {
  $bmp = New-Object System.Drawing.Bitmap 800, 600
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.Clear([System.Drawing.Color]::FromArgb(255, 0, 0, 0))
  for ($y = 0; $y -lt 600; $y += 1) {
    $c = & $colorFn $y
    $brush = New-Object System.Drawing.SolidBrush $c
    $g.FillRectangle($brush, 0, $y, 800, 1)
    $brush.Dispose()
  }
  $path = Join-Path $out $name
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Jpeg)
  $g.Dispose(); $bmp.Dispose()
  Write-Host "  created $name"
}

# 1) 纯黑（欠曝）
New-TestJpg 'DSC00001.JPG' { param($y) [System.Drawing.Color]::FromArgb(255, 5, 5, 5) }
# 2) 纯白（过曝）
New-TestJpg 'DSC00002.JPG' { param($y) [System.Drawing.Color]::FromArgb(255, 250, 250, 250) }
# 3) 灰阶渐变（正常曝光）
New-TestJpg 'DSC00003.JPG' { param($y) [System.Drawing.Color]::FromArgb(255, [int]($y * 255 / 600), [int]($y * 255 / 600), [int]($y * 255 / 600)) }

# ARW 占位：与 DSC00001/02 配对 + 一个孤立的 DSC00004
foreach ($n in @('DSC00001.ARW', 'DSC00002.ARW', 'DSC00004.ARW')) {
  Set-Content -Path (Join-Path $out $n) -Value 'placeholder' -NoNewline
  Write-Host "  created $n (placeholder)"
}
Write-Host "done -> $out"
