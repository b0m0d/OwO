# verify-mcp-bind-speedup.ps1 - 十期 · §11.3 MCP 绑定提速复测
#
# 报告原始缺陷：`owo-agent serve` 曾在绑定监听端口前**串行等待全部 MCP 连接**，
# 一个失联/卡死的 MCP 服务器会让本地 HTTP 监听与桌面壳健康检查无限期阻塞。
# 修复：`connect_mcp_clients` 对每个 MCP 用 3 秒上限（tokio::time::timeout）包裹，
# 超时即降级跳过、继续启动。
#
# 本脚本做"同目录监听复测"：
#   1) 临时工作区 + 临时数据根，写入一个**永不完成握手**的 MCP 配置
#      （stdio 子进程 `Start-Sleep 3600`：不读 stdin、不写 stdout，连接必然卡死）；
#   2) 启动 `owo-agent serve`（debug 二进制，与 release 同一份 serve 路径代码）；
#   3) 轮询 127.0.0.1:<port> 建连成功的时间点；
#   4) 断言：建连等待远小于"旧行为无限阻塞"——≤ 3s MCP 上限 + 启动余量（本脚本放宽到 15s，
#      避免慢机器误报；实测偏差大时按实际测量收敛）；随后 /health 应返回 200。
#
# Usage:
#   pwsh -File agent-sdk\scripts\verify-mcp-bind-speedup.ps1 [-Port 4317] [-TimeoutSec 15]
# Exit code: 0 = 监听未被卡死 MCP 阻塞（复测通过）, 1 = 失败, 2 = 前置错误。

param(
    [int]$Port = 0,          # 0 = 自动挑选空闲端口
    [int]$TimeoutSec = 15,   # 建连等待上限（秒），含 3s MCP 上限 + 启动余量
    [string]$CliPath = "",   # 二进制路径；空 = 默认 debug（未指定时用 target\debug\owo-agent.exe）
    [switch]$Check           # 只检查二进制与端口可用性，不启动
)

$ErrorActionPreference = "Stop"
$sdkRoot = Split-Path -Parent $PSScriptRoot
if ($CliPath -eq "") { $CliPath = Join-Path $sdkRoot "target\debug\owo-agent.exe" }
$cli = $CliPath
if (-not (Test-Path $cli)) {
    Write-Host "缺少二进制：$cli（先 cargo build -p owo-agent-cli）" -ForegroundColor Red
    exit 2
}

# 自动挑选空闲端口
if ($Port -eq 0) {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $Port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    $listener.Stop()
}

if ($Check) {
    Write-Host "CLI: $cli"
    Write-Host "Port: $Port（可建连）"
    exit 0
}

$wsRoot = Join-Path $env:TEMP ("owo-mcp-bind-" + [guid]::NewGuid().ToString("N"))
$dataRoot = Join-Path $wsRoot "data"
New-Item -ItemType Directory -Path $wsRoot, $dataRoot -Force | Out-Null

# 挂死的 MCP 配置：子进程不读 stdin、不写 stdout → 握手永远不完成。
$mcp = @'
[
  {
    "name": "hang-forever",
    "transport": "stdio",
    "command": "powershell.exe",
    "args": ["-NoProfile", "-Command", "Start-Sleep 3600"]
  }
]
'@
[System.IO.File]::WriteAllText((Join-Path $dataRoot "mcp-servers.json"), $mcp, (New-Object System.Text.UTF8Encoding($false)))

$env:OWO_AGENT_DATA = $dataRoot
$env:OWO_CLOUD_ENABLED = "false"   # 与模型无关；serve 仅解析模型名，不发请求
$env:RUST_LOG = "info"
# serve 启动即构造模型 Provider（build_agent_with_mcp），需要凭据或本地端点；
# 按 AGENTS.md 标准从 Windows 用户级环境变量注入（不回显值本体）。
$regKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not [string]::IsNullOrEmpty($regKey)) { $env:OPENAI_API_KEY = $regKey }
if (-not $env:OPENAI_API_KEY) {
    $env:OPENAI_BASE_URL = "http://127.0.0.1:9"  # 本地端点允许空 key；serve 不实际发模型请求
}

$proc = Start-Process -FilePath $cli -ArgumentList @("serve", "--workspace", $wsRoot, "--port", "$Port") -PassThru -RedirectStandardOutput (Join-Path $wsRoot "out.log") -RedirectStandardError (Join-Path $wsRoot "err.log") -NoNewWindow

try {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $connected = $false
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec -and -not $proc.HasExited) {
        $tcp = [System.Net.Sockets.TcpClient]::new()
        try {
            $iar = $tcp.BeginConnect("127.0.0.1", $Port, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(300)) {
                $tcp.EndConnect($iar)
                $connected = $true
                break
            }
        } catch { }
        finally { $tcp.Dispose() }
        Start-Sleep -Milliseconds 200
    }
    $elapsed = [Math]::Round($sw.Elapsed.TotalSeconds, 2)

    Write-Host "== MCP 绑定复测 ==" -ForegroundColor Cyan
    Write-Host ("    监听 127.0.0.1:{0} 建连耗时: {1}s (上限 3s/MCP + 启动余量, 断言 {2}s)" -f $Port, $elapsed, $TimeoutSec)
    Write-Host ("    进程存活: {0}" -f (-not $proc.HasExited))

    if (-not $connected) {
        Write-Host "    服务未在规定时间内建连（可能仍被 MCP 阻塞或启动失败）" -ForegroundColor Red
        Get-Content (Join-Path $wsRoot "err.log") -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    | $_" }
        exit 1
    }

    # /health 应为 200（公开端点，无 token）
    $health = $null
    for ($i = 0; $i -lt 10; $i++) {
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/health" -TimeoutSec 2
            break
        } catch { Start-Sleep -Milliseconds 200 }
    }
    if ($null -eq $health) {
        Write-Host "    /health 未返回：服务建连成功但健康端点异常" -ForegroundColor Red
        exit 1
    }
    Write-Host ("    /health: {0}" -f ($health | ConvertTo-Json -Compress)) -ForegroundColor Green
    Write-Host "    结论: 监听未被卡死 MCP 阻塞（§11.3 MCP 绑定提速复测通过）" -ForegroundColor Green

    # 日志中应有"已跳过"降级证据
    $outLog = Get-Content (Join-Path $wsRoot "out.log") -Raw -ErrorAction SilentlyContinue
    if ($outLog -match "已跳过") {
        Write-Host "    降级证据: serve 日志包含 MCP 跳过记录" -ForegroundColor Green
    }
    exit 0
}
finally {
    if ($proc -and -not $proc.HasExited) {
        try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { }
        # 等服务进程退出并等待清理（强杀恢复逻辑自身处理 pid 文件）
        $proc.WaitForExit(3000)
    }
    Remove-Item Env:OWO_AGENT_DATA -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
    Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    Write-Host "== 清理完成（临时目录：$wsRoot）==" -ForegroundColor DarkGray
}