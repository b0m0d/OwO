# long-task-e2e.ps1 — 真实模型长程任务 E2E 驱动
#
# 自包含：随机端口启动 owo-agent serve（隐藏窗口，隔离数据根 %TEMP%\owo-longtask-*）→
# 等待 /health → 运行 scripts/long-task-e2e.py（两轮任务 + 自动审批 + 文件/历史断言）→
# 结束后清理进程与临时目录（除非 -Keep）。
#
# 用法示例：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\long-task-e2e.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\long-task-e2e.ps1 -Port 4097 -TurnTimeout 900 -Keep
param(
    [int]$Port = 0,
    [int]$TurnTimeout = 900,
    [switch]$Keep
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if ($Port -eq 0) { $Port = Get-Random -Minimum 12000 -Maximum 32000 }
$tempRoot = Join-Path $env:TEMP "owo-longtask-$Port"
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
$workspace = Join-Path $tempRoot "ws"
New-Item -ItemType Directory -Path $workspace -Force | Out-Null
$dataDir = Join-Path $tempRoot "data"
New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
$outLog = Join-Path $tempRoot "serve.out.log"
$errLog = Join-Path $tempRoot "serve.err.log"

if (-not $env:OPENAI_API_KEY) {
    $userKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if ($userKey) { $env:OPENAI_API_KEY = $userKey }
}
if (-not $env:OPENAI_API_KEY) {
    Write-Error "缺少 OPENAI_API_KEY（进程环境或用户级环境变量），无法进行真实模型 E2E"
}
$env:OWO_AGENT_DATA = $dataDir
# E2E 边界：产品默认不设模型轮数/工具调用上限；验收脚本用显式上限保证
# "长程但收敛"（仍远超普通几轮任务），避免真实模型在无界探索上耗尽验收时间。
if (-not $env:OWO_AGENT_MAX_MODEL_TURNS) { $env:OWO_AGENT_MAX_MODEL_TURNS = '40' }
if (-not $env:OWO_AGENT_MAX_TOOL_CALLS) { $env:OWO_AGENT_MAX_TOOL_CALLS = '120' }

$binary = Join-Path $root "target\debug\owo-agent.exe"
if (-not (Test-Path -LiteralPath $binary)) {
    Write-Error "未找到 $binary；请先运行 cargo build -p owo-agent-cli -j 2"
}

Write-Host "==> 启动服务：port=$Port workspace=$workspace data=$dataDir"
$proc = Start-Process -FilePath $binary -ArgumentList @('serve', '--port', "$Port", '--workspace', $workspace) `
    -WorkingDirectory $root -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput $outLog -RedirectStandardError $errLog

$exitCode = 3
try {
    $base = "http://127.0.0.1:$Port"
    $up = $false
    for ($i = 0; $i -lt 120; $i++) {
        Start-Sleep -Milliseconds 500
        if ($proc.HasExited) { break }
        try {
            $r = Invoke-WebRequest -Uri "$base/health" -UseBasicParsing -TimeoutSec 2
            if ($r.StatusCode -eq 200) { $up = $true; break }
        } catch { }
    }
    if (-not $up) {
        Write-Host "==> 服务未就绪；stdout:"; Get-Content $outLog -ErrorAction SilentlyContinue | Select-Object -Last 20
        Write-Host "==> stderr:"; Get-Content $errLog -ErrorAction SilentlyContinue | Select-Object -Last 20
        throw "服务 $base 未就绪"
    }
    Write-Host "==> 服务就绪，运行长程 E2E"
    & python (Join-Path $root "scripts\long-task-e2e.py") --base $base --workspace $workspace --turn-timeout $TurnTimeout
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        Write-Host "==> E2E 退出码 $exitCode；服务日志尾部："
        Get-Content $outLog -ErrorAction SilentlyContinue | Select-Object -Last 30
        Get-Content $errLog -ErrorAction SilentlyContinue | Select-Object -Last 30
    }
} finally {
    if (-not $proc.HasExited) {
        & taskkill /PID $proc.Id /T /F 2>$null | Out-Null
    }
    if ($Keep) {
        Write-Host "==> 保留现场：$tempRoot"
    } else {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
exit $exitCode
