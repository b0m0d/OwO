#requires -Version 5.1
<#
test-desktop-acceptance-common.ps1 — R3-A1 验收原语的负例自测（指南 §3.3.2 硬性要求）。

为什么存在：§2.2.2 的教训——"158×26 黑线截图"被旧实现判为通过。指南规定
「每次真实验收发现的假绿色都必须补一个负例测试」（§7.3），并要求：
  * 158×26 黑图必须失败；
  * 纯色 1280×720 图片必须失败；
  * 真实有效画面必须通过；
  * 失效 HWND / 非本 PID 句柄必须被 Get-OwoValidatedWindow 拒绝。
本脚本纯离线（不弹窗口、不启动壳），生成合成位图后调用原语断言，因此可以
进 ci-gate（无桌面会话依赖）。

用法（在 agent-sdk/ 目录）：
  .\scripts\test-desktop-acceptance-common.ps1
退出码：0 = 全部用例通过；1 = 存在失败用例。
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot "desktop-acceptance-common.ps1")
Add-OwoDesktopWin32

$work = Join-Path ([IO.Path]::GetTempPath()) ("owo-accept-selftest-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $work | Out-Null

$results = New-Object System.Collections.ArrayList
function Add-Case {
    param([string]$Name, [bool]$Pass, [string]$Detail)
    $null = $results.Add([pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail })
    $mark = if ($Pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} — {2}" -f $mark, $Name, $Detail)
}

function New-TestBitmap {
    param([string]$Path, [int]$Width, [int]$Height, [ValidateSet('black', 'solid', 'noise')][string]$Kind, [int]$Seed = 42)
    $bmp = New-Object System.Drawing.Bitmap($Width, $Height)
    try {
        $rnd = New-Object System.Random($Seed)
        for ($y = 0; $y -lt $Height; $y++) {
            for ($x = 0; $x -lt $Width; $x++) {
                $c = switch ($Kind) {
                    'black' { [System.Drawing.Color]::FromArgb(255, 0, 0, 0) }
                    'solid' { [System.Drawing.Color]::FromArgb(255, 200, 30, 30) }
                    'noise' {
                        # 低频渐变 + 细噪声：既高方差又保证非黑比例（模拟真实界面像素构成）
                        $r = (($x * 255 / $Width) + $rnd.Next(64)) -band 0xFF
                        $g = (($y * 255 / $Height) + $rnd.Next(64)) -band 0xFF
                        $b = ((($x + $y) % 256) + $rnd.Next(64)) -band 0xFF
                        [System.Drawing.Color]::FromArgb(255, $r, $g, $b)
                    }
                }
                $bmp.SetPixel($x, $y, $c)
            }
        }
        $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $bmp.Dispose()
    }
    return $Path
}

try {
    # ---- 负例 1：158×26 黑图（正是 §2.2.2 里被误判通过的现场尺寸） ----------
    $black = New-TestBitmap -Path (Join-Path $work 'black-158x26.png') -Width 158 -Height 26 -Kind black
    $m1 = Test-OwoScreenshot -Path $black -ExpectedWidth 1280 -ExpectedHeight 720
    Add-Case '负例1：158×26 黑图必须判失败' (-not $m1.ok) (Get-OwoScreenshotMetricLine $m1)
    Add-Case '负例1：失败原因含尺寸与黑屏双信号' `
        ((@($m1.fail_reasons | Where-Object { $_ -like 'undersized*' }).Count -gt 0) -and
         ((@($m1.fail_reasons | Where-Object { $_ -like 'bytes_lt*' }).Count -gt 0) -or
          (@($m1.fail_reasons | Where-Object { $_ -like 'mostly_black*' }).Count -gt 0))) `
        "reasons=$($m1.fail_reasons -join ';')"

    # ---- 负例 2：纯色 1280×720（方差≈0，防"纯色图假通过"） --------------------
    $solid = New-TestBitmap -Path (Join-Path $work 'solid-1280x720.png') -Width 1280 -Height 720 -Kind solid
    $m2 = Test-OwoScreenshot -Path $solid -ExpectedWidth 1280 -ExpectedHeight 720
    Add-Case '负例2：纯色 1280×720 必须判失败（方差门）' (-not $m2.ok) (Get-OwoScreenshotMetricLine $m2)
    Add-Case '负例2：失败原因必须是 flat_content' `
        (@($m2.fail_reasons | Where-Object { $_ -like 'flat_content*' }).Count -gt 0) `
        "reasons=$($m2.fail_reasons -join ';')"

    # ---- 正例：有效画面（渐变+噪声）必须通过 ----------------------------------
    $good = New-TestBitmap -Path (Join-Path $work 'noise-1280x720.png') -Width 1280 -Height 720 -Kind noise
    $m3 = Test-OwoScreenshot -Path $good -ExpectedWidth 1280 -ExpectedHeight 720
    Add-Case '正例：有效画面必须判通过' $m3.ok (Get-OwoScreenshotMetricLine $m3)

    # ---- 负例 3：尺寸不符（画面正常但与窗口矩形不一致，防"截错区域"） ----------
    $m4 = Test-OwoScreenshot -Path $good -ExpectedWidth 900 -ExpectedHeight 600
    Add-Case '负例3：与窗口矩形不符必须判失败' (-not $m4.ok) (Get-OwoScreenshotMetricLine $m4)

    # ---- 负例 4：缺失文件 --------------------------------------------------------
    $m5 = Test-OwoScreenshot -Path (Join-Path $work 'no-such.png')
    Add-Case '负例4：文件缺失必须判失败' ((-not $m5.ok) -and ($m5.fail_reasons -contains 'missing_file')) `
        "reasons=$($m5.fail_reasons -join ';')"

    # ---- 窗口验证负例：进程不存在 / 句柄非本 PID --------------------------------
    $w1 = Get-OwoValidatedWindow -ProcessId 999999 -TimeoutSec 1 -RequireVisible
    Add-Case '负例5：不存在的进程必须判失败' ((-not $w1.ok) -and ($w1.fail_reasons -contains 'process_exited')) `
        "reasons=$($w1.fail_reasons -join ';')"
    $w2 = Get-OwoValidatedWindow -ProcessId $PID -TimeoutSec 1 -RequireVisible
    Add-Case '负例6：无主窗口的本进程不得返回可见有效句柄' (-not $w2.ok) `
        "hwnd=$($w2.hwnd) ok=$($w2.ok) reasons=$($w2.fail_reasons -join ';')"

    $failed = @($results | Where-Object { -not $_.pass })
    $null = Save-OwoEvidenceJson -Dir $work -Name 'selftest-report.json' -InputObject ([ordered]@{
            stamp  = (Get-Date).ToUniversalTime().ToString('o')
            cases  = @($results)
            passed = @($results).Count - $failed.Count
            failed = $failed.Count
        })
    Write-Host ("[selftest] {0}/{1} 通过（证据：{2}）" -f (@($results).Count - $failed.Count), @($results).Count, $work)
    if ($failed.Count -gt 0) { exit 1 }
    exit 0
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
