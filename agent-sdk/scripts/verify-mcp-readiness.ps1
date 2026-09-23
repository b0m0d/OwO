#requires -Version 5.1
<#
verify-mcp-readiness.ps1 — P2 §6.2：坏 MCP 不得拖慢本地服务就绪。

构造 N 个"会挂住"的 stdio MCP（命令 sleep 30s），启动 Daemon，测量从进程启动到
`core_ready` 的墙钟时间。验收：并发 + 全局 3s 上限下，就绪时间不随 N 线性增长，
且 < MaxReadySec（默认 6s，含进程启动与其它初始化开销）。

脚本自带清理（停止自己启动的 Daemon）。
用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-mcp-readiness.ps1
#>
[CmdletBinding()]
param(
    [int]$BadServers = 3,
    [double]$MaxReadySec = 6.0
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
$exe = Join-Path $root 'target\debug\owo-agent.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "缺少产物：$exe（先构建 owo-agent-cli）" }

$work = Join-Path $env:TEMP ("owo-mcp-ready-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
$ws = Join-Path $work 'ws'
$data = Join-Path $work 'data'
New-Item -ItemType Directory -Force -Path $ws, $data | Out-Null

$configs = @()
for ($i = 1; $i -le $BadServers; $i++) {
    $configs += @{
        name      = "bad$i"
        transport = 'stdio'
        command   = 'powershell'
        args      = @('-NoProfile', '-Command', 'Start-Sleep -Seconds 30')
    }
}
[System.IO.File]::WriteAllText(
    (Join-Path $data 'mcp-servers.json'),
    (ConvertTo-Json -InputObject $configs -Depth 5),
    (New-Object System.Text.UTF8Encoding($false)))

$out = Join-Path $work 'serve.out.log'
$err = Join-Path $work 'serve.err.log'
$env:OWO_AGENT_DATA = $data
$env:OWO_CLOUD_ENABLED = 'false'
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$proc = Start-Process -FilePath $exe -ArgumentList @('serve', '--port', '0', '--output', 'jsonl', '--workspace', $ws) `
    -WorkingDirectory $root -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -WindowStyle Hidden

$ready = $null
$deadline = (Get-Date).AddSeconds(30)
try {
    while ((Get-Date) -lt $deadline -and -not $ready) {
        Start-Sleep -Milliseconds 100
        if (Test-Path -LiteralPath $out) {
            foreach ($line in (Get-Content -LiteralPath $out -ErrorAction SilentlyContinue)) {
                if ($line -match '"event"\s*:\s*"core_ready"') { $ready = $line; break }
            }
        }
        if ($proc.HasExited) { break }
    }
    $sw.Stop()
    $elapsed = [Math]::Round($sw.Elapsed.TotalSeconds, 2)
    $pass = ($null -ne $ready) -and ($elapsed -lt $MaxReadySec)
    Write-Host ("[mcp-readiness] bad_servers={0} core_ready={1}s（上限 {2}s）" -f $BadServers, $elapsed, $MaxReadySec) `
        -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' })
    if (-not $pass) { Write-Host "未就绪或超时：$ready" -ForegroundColor Red }
    exit $(if ($pass) { 0 } else { 1 })
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path -LiteralPath (Join-Path $data 'runtime\daemon.json')) {
        try {
            $desc = Get-Content -LiteralPath (Join-Path $data 'runtime\daemon.json') -Raw | ConvertFrom-Json
            Stop-Process -Id $desc.pid -Force -ErrorAction SilentlyContinue
        } catch { }
    }
    Get-Process owo-agent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\OWO_AGENT_DATA, Env:\OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
}
