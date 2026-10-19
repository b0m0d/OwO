# verify-release-pairing.ps1 - 十期 · §11.3 发布核心进程配对复测（核心侧半程）
#
# 报告遗留项之一："`/auth/token` 仍是公开引导接口，尚未实现桌面进程配对/一次性
# 引导证明"。代码侧已完成（Tauri 壳生成随机证明 → 注入 OWO_DESKTOP_PAIRING_SECRET
# → WebView 经 IPC 取证明 → API 客户端带 x-owo-desktop-pairing 头 → 引导端点校验）。
#
# 本脚本复测**发布核心侧**的配对行为（GUI 壳侧的自行拉起需要整套 Tauri 工具链与
# 桌面会话，不在本受控环境执行）：
#   1) 启动 release `owo-agent serve`，注入 OWO_DESKTOP_RELEASE=1 +
#      OWO_DESKTOP_PAIRING_SECRET=<32+ 随机串>；
#   2) 断言 `GET /auth/token` 无配对头 → 403（不泄露 bearer token）；
#   3) 断言带正确配对头 → 200 + token（引导成功）；
#   4) 断言带错误配对头 → 403。
# 开发模式对照（不注入秘密）：无头也 200——浏览器调试不被误锁。
#
# Usage:
#   pwsh -File agent-sdk\scripts\verify-release-pairing.ps1 [-CliPath ...\owo-agent.exe] [-Port 0]
# Exit code: 0 = 配对门控行为正确, 1 = 行为不符, 2 = 前置错误。

param(
    [int]$Port = 0,
    [string]$CliPath = ""
)

$ErrorActionPreference = "Stop"
$sdkRoot = Split-Path -Parent $PSScriptRoot
if ($CliPath -eq "") { $CliPath = Join-Path $sdkRoot "target\debug\owo-agent.exe" }
$cli = $CliPath
if (-not (Test-Path $cli)) {
    Write-Host "缺少二进制：$cli" -ForegroundColor Red
    exit 2
}

if ($Port -eq 0) {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $Port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    $listener.Stop()
}

$wsRoot = Join-Path $env:TEMP ("owo-pairing-" + [guid]::NewGuid().ToString("N"))
$dataRoot = Join-Path $wsRoot "data"
New-Item -ItemType Directory -Path $wsRoot, $dataRoot -Force | Out-Null

$secret = "pair-" + ([guid]::NewGuid().ToString("N").Replace("-", "")) + ([guid]::NewGuid().ToString("N").Replace("-", ""))
$secret = $secret.Substring(0, 40)

$env:OWO_AGENT_DATA = $dataRoot
$env:OWO_CLOUD_ENABLED = "false"
$env:OWO_DESKTOP_RELEASE = "1"
$env:OWO_DESKTOP_PAIRING_SECRET = $secret
$env:RUST_LOG = "info"
$regKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not [string]::IsNullOrEmpty($regKey)) { $env:OPENAI_API_KEY = $regKey }
if (-not $env:OPENAI_API_KEY) {
    $env:OPENAI_BASE_URL = "http://127.0.0.1:9"
}

$proc = Start-Process -FilePath $cli -ArgumentList @("serve", "--workspace", $wsRoot, "--port", "$Port") -PassThru -RedirectStandardOutput (Join-Path $wsRoot "out.log") -RedirectStandardError (Join-Path $wsRoot "err.log") -NoNewWindow

try {
    # 等待 /health 就绪
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $ready = $false
    while ($sw.Elapsed.TotalSeconds -lt 20 -and -not $proc.HasExited) {
        try { $null = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 2; $ready = $true; break }
        catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $ready) {
        Write-Host "服务未在 20s 内就绪（可能启动失败）" -ForegroundColor Red
        Get-Content (Join-Path $wsRoot "err.log") -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    | $_" }
        exit 1
    }

    $base = "http://127.0.0.1:$Port/auth/token"

    # 1) 无配对头 → 403
    $noHeaderStatus = 0
    try { $null = Invoke-RestMethod -Uri $base -TimeoutSec 5; $noHeaderStatus = 200 }
    catch { $noHeaderStatus = [int]$_.Exception.Response.StatusCode }

    # 2) 正确配对头 → 200 + token
    $okResult = Invoke-RestMethod -Uri $base -Headers @{ "x-owo-desktop-pairing" = $secret } -TimeoutSec 5

    # 3) 错误配对头 → 403
    $badHeaderStatus = 0
    try { $null = Invoke-RestMethod -Uri $base -Headers @{ "x-owo-desktop-pairing" = "wrong-secret-wrong-secret-wrong" } -TimeoutSec 5; $badHeaderStatus = 200 }
    catch { $badHeaderStatus = [int]$_.Exception.Response.StatusCode }

    Write-Host "== 发布核心配对复测 ==" -ForegroundColor Cyan
    Write-Host ("    无配对头       : HTTP {0} (期望 403)" -f $noHeaderStatus)
    Write-Host ("    正确配对头     : HTTP 200, token 长度 {0} (期望 200 + 长 token)" -f ($okResult.token.Length))
    Write-Host ("    错误配对头     : HTTP {0} (期望 403)" -f $badHeaderStatus)

    $pass = ($noHeaderStatus -eq 403) -and ($okResult.token.Length -ge 64) -and ($badHeaderStatus -eq 403)
    if ($pass) {
        Write-Host "    结论: 发布核心配对门控行为正确（§11.3 /auth/token 核心侧复测通过）" -ForegroundColor Green
    } else {
        Write-Host "    结论: 配对门控行为不符预期" -ForegroundColor Red
        Get-Content (Join-Path $wsRoot "out.log") -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    | $_" }
    }
    if ($pass) { exit 0 } else { exit 1 }
}
finally {
    if ($proc -and -not $proc.HasExited) { try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { } }
    Remove-Item Env:OWO_AGENT_DATA -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_DESKTOP_RELEASE -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_DESKTOP_PAIRING_SECRET -ErrorAction SilentlyContinue
    Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    Write-Host "== 清理完成（临时目录：$wsRoot）==" -ForegroundColor DarkGray
}