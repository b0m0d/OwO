# M10 收尾验证：等资源门可过 → core 全量测试 → server 全量测试 → 运行态冒烟。
#
# 为什么需要"等"：§2.4 内存门要求可用内存 >= 6 GB 且已用 < 80%，而本机同时有其它
# 大型应用（实测 ~5 GB）与并行 Agent 占内存。链接期 link.exe 自身峰值约 +2 GB，
# 因此静态余量 6.9 GB 时仍会在链接中途跌破阈值、被门禁按设计终止（exit 137）。
# 本脚本**不绕过门禁**：它只在门禁能过的时刻才开始跑，并在每一步后记录真实退出码。
#
# 用法：
#   & "<repo>\agent-sdk\scripts\mk-m10-verify.ps1"
#   & "<repo>\agent-sdk\scripts\mk-m10-verify.ps1" -MinFreeGB 9 -MaxWaitMin 90 -Tag m10-mcp

param(
    [double]$MinFreeGB = 9,
    [double]$MaxUsedPct = 72,
    [int]$MaxWaitMin = 90,
    [string]$Tag = 'm10-mcp'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
. (Join-Path $PSScriptRoot 'resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet | Out-Null
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

function Get-ResSnapshot {
    $os = Get-CimInstance Win32_OperatingSystem
    $freeGB = $os.FreePhysicalMemory / 1MB
    $usedPct = (1 - $os.FreePhysicalMemory / $os.TotalVisibleMemorySize) * 100
    [pscustomobject]@{ FreeGB = $freeGB; UsedPct = $usedPct }
}

# --- 1) 等资源回落（只等，不改阈值） -----------------------------------------
$deadline = (Get-Date).AddMinutes($MaxWaitMin)
$waited = 0
while ($true) {
    $s = Get-ResSnapshot
    if ($s.FreeGB -ge $MinFreeGB -and $s.UsedPct -lt $MaxUsedPct) {
        Write-Host ("[wait] 资源可用：free={0:N2} GB used={1:N1}%（等待 {2} 分钟）" -f $s.FreeGB, $s.UsedPct, $waited)
        break
    }
    if ((Get-Date) -ge $deadline) {
        Write-Host ("[wait] 超时退出：free={0:N2} GB used={1:N1}%，已等 {2} 分钟（未做任何绕过）" -f $s.FreeGB, $s.UsedPct, $waited)
        exit 3
    }
    if ($waited % 5 -eq 0) {
        Write-Host ("[wait] free={0:N2} GB used={1:N1}% —— 继续等待（目标 free>={2} GB 且 used<{3}%）" -f $s.FreeGB, $s.UsedPct, $MinFreeGB, $MaxUsedPct)
    }
    Start-Sleep -Seconds 60
    $waited++
}

$logDir = Join-Path $root 'docs\qa\logs'
if (-not (Test-Path -LiteralPath $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$results = @()

# --- 2) core 全量测试（完整 core → 红线 2：-j 1） -----------------------------
$log = Join-Path $logDir ("mk-{0}-core-tests-{1}.log" -f $Tag, $stamp)
$code = Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--locked', '-j', '1', '--', '--test-threads=1') `
    -Cwd $root -Label "mk-$Tag-core" -PolicyMode strict -LogFile $log -HeartbeatSec 60 -PassThru
Write-Host "mk-${Tag}: core exit=$code log=$log"
$results += [pscustomobject]@{ step = 'core_tests'; exit = $code; log = $log }

# --- 3) server 全量测试 ------------------------------------------------------
if ($code -eq 0) {
    $log2 = Join-Path $logDir ("mk-{0}-server-tests-{1}.log" -f $Tag, $stamp)
    $code2 = Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-server', '--locked', '-j', '1', '--', '--test-threads=1') `
        -Cwd $root -Label "mk-$Tag-server" -PolicyMode strict -LogFile $log2 -HeartbeatSec 60 -PassThru
    Write-Host "mk-${Tag}: server exit=$code2 log=$log2"
    $results += [pscustomobject]@{ step = 'server_tests'; exit = $code2; log = $log2 }
    $code = $code2
} else {
    Write-Host "mk-${Tag}: core 未通过（exit=$code），跳过 server 测试与冒烟"
}

# --- 4) 运行态冒烟 ----------------------------------------------------------
if ($code -eq 0) {
    $smokeLog = Join-Path $logDir ("mk-{0}-smoke-{1}.log" -f $Tag, $stamp)
    $out = & (Join-Path $PSScriptRoot 'mk-smoke.ps1') -Tag $Tag 2>&1
    $out | Tee-Object -FilePath $smokeLog | Out-Null
    $code3 = $LASTEXITCODE
    Write-Host "mk-${Tag}: smoke exit=$code3 log=$smokeLog"
    $results += [pscustomobject]@{ step = 'smoke'; exit = $code3; log = $smokeLog }
    $code = $code3
}

"--- 汇总"
$results | ForEach-Object { "  {0,-14} exit={1}  {2}" -f $_.step, $_.exit, $_.log }
exit $code
