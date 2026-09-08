# verify-desktop-handshake.ps1 - 审计方案 2026-09-05 §4.2 桌面壳-核心实例握手复测（核心侧自动化半程）
#
# §4 根因：桌面壳固定 4096 + 盲复用旧服务 + 每次新生成配对密钥 → 旧服务 403 → 空壳。
# 修复契约（核心侧）：
#   1) `serve --port 0` 支持系统分配端口；listener 绑定后向 stdout 打印一行
#      `{"event":"core_ready","pid":N,"port":N,"api_version":"...","build_id":"...","instance_id":"..."}`；
#   2) 注入 OWO_DESKTOP_INSTANCE_ID 后：
#      - /health 返回 instance_id/pid/stage/build_id（非秘密字段）；
#      - /auth/token 在配对头之外还要求 x-owo-desktop-instance 头精确匹配
#        （缺失/错值 → 403 auth/instance_mismatch/not_retryable）；
#      - /server/shutdown 同样校验实例头（confirm 校验之前）。
#   3) 开发模式对照（不注入任何环境变量）：/health.instance_id 为 null，
#      /auth/token 无头仍 200（浏览器调试不被误锁）。
#
# GUI 壳侧（窗口可见、单实例、重启退避、日志落盘）需要真实桌面会话，另行人工/Playwright 验收。
#
# Usage:
#   pwsh -File agent-sdk\scripts\verify-desktop-handshake.ps1 [-CliPath ...\owo-agent.exe] [-SkipDevControl]
# Exit code: 0 = 握手契约全部通过, 1 = 行为不符, 2 = 前置错误。

param(
    [string]$CliPath = "",
    [switch]$SkipDevControl
)

$ErrorActionPreference = "Stop"
$sdkRoot = Split-Path -Parent $PSScriptRoot
if ($CliPath -eq "") { $CliPath = Join-Path $sdkRoot "target\debug\owo-agent.exe" }
$cli = $CliPath
if (-not (Test-Path $cli)) {
    Write-Host "缺少二进制：$cli" -ForegroundColor Red
    exit 2
}

function New-SessionRoot {
    return Join-Path $env:TEMP ("owo-handshake-" + [guid]::NewGuid().ToString("N"))
}

function Get-CoreReadyLine {
    param([string]$LogPath, [System.Diagnostics.Stopwatch]$Sw, [double]$TimeoutSec, [object]$Proc)
    while ($Sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        if ($Proc.HasExited) { break }
        if (Test-Path $LogPath) {
            $lines = Get-Content $LogPath -ErrorAction SilentlyContinue
            foreach ($line in $lines) {
                if ($line -like '*"event":"core_ready"*') { return $line }
            }
        }
        Start-Sleep -Milliseconds 150
    }
    return $null
}

# ---------- 会话 1：注入配对 + 实例身份（发布桌面等价环境） ----------
$wsRoot = New-SessionRoot
$dataRoot = Join-Path $wsRoot "data"
New-Item -ItemType Directory -Path $wsRoot, $dataRoot -Force | Out-Null
$outLog = Join-Path $wsRoot "out.log"
$errLog = Join-Path $wsRoot "err.log"

$secret = ("pair-" + [guid]::NewGuid().ToString("N") + [guid]::NewGuid().ToString("N")).Substring(0, 48)
$instance = [guid]::NewGuid().ToString("N")

$env:OWO_AGENT_DATA = $dataRoot
$env:OWO_CLOUD_ENABLED = "false"
$env:OWO_DESKTOP_RELEASE = "1"
$env:OWO_DESKTOP_PAIRING_SECRET = $secret
$env:OWO_DESKTOP_INSTANCE_ID = $instance
$env:RUST_LOG = "info"
$regKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not [string]::IsNullOrEmpty($regKey)) { $env:OPENAI_API_KEY = $regKey }
if (-not $env:OPENAI_API_KEY) { $env:OPENAI_BASE_URL = "http://127.0.0.1:9" }

$proc = Start-Process -FilePath $cli -ArgumentList @("serve", "--workspace", $wsRoot, "--port", "0") -PassThru -RedirectStandardOutput $outLog -RedirectStandardError $errLog -NoNewWindow

$failures = New-Object System.Collections.Generic.List[string]
$assert = {
    param($name, $cond, $detail)
    if ($cond) { Write-Host ("    [通过] {0}" -f $name) -ForegroundColor Green }
    else { Write-Host ("    [失败] {0} —— {1}" -f $name, $detail) -ForegroundColor Red; $failures.Add($name) }
}

try {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $readyLine = Get-CoreReadyLine -LogPath $outLog -Sw $sw -TimeoutSec 25 -Proc $proc
    if ($null -eq $readyLine) {
        Write-Host "25s 内未在 stdout 观察到 core_ready 行" -ForegroundColor Red
        Get-Content $outLog, $errLog -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    | $_" }
        exit 1
    }
    $ready = $readyLine | ConvertFrom-Json

    Write-Host "== §4.2 实例握手复测（注入身份） ==" -ForegroundColor Cyan
    Write-Host ("    ready 行: port={0} pid={1} api={2} instance={3}" -f $ready.port, $ready.pid, $ready.api_version, $ready.instance_id)
    & $assert "ready.port 为系统分配端口(>0)" ($ready.port -gt 0) "port=$($ready.port)"
    & $assert "ready.instance_id == 注入值" ($ready.instance_id -eq $instance) "ready=$($ready.instance_id) 期望=$instance"
    & $assert "ready.api_version 非空" (-not [string]::IsNullOrEmpty($ready.api_version)) "api_version 空"
    $port = [int]$ready.port

    # /health：非秘密身份字段
    $health = Invoke-RestMethod -Uri "http://127.0.0.1:$port/health" -TimeoutSec 5
    & $assert "/health.instance_id 匹配" ($health.instance_id -eq $instance) "got=$($health.instance_id)"
    & $assert "/health.pid > 0" ($health.pid -gt 0) "pid=$($health.pid)"
    & $assert "/health.stage == ready" ($health.stage -eq "ready") "stage=$($health.stage)"
    & $assert "/health.healthy == true" ($health.healthy -eq $true) "healthy=$($health.healthy)"

    $tokenBase = "http://127.0.0.1:$port/auth/token"

    # 实例门控矩阵（配对头始终正确，仅实例头变化）
    $stNoInstance = 0
    try { $null = Invoke-RestMethod -Uri $tokenBase -Headers @{ "x-owo-desktop-pairing" = $secret } -TimeoutSec 5; $stNoInstance = 200 }
    catch { $stNoInstance = [int]$_.Exception.Response.StatusCode }
    & $assert "/auth/token 缺实例头 → 403" ($stNoInstance -eq 403) "got=$stNoInstance"

    $stBadInstance = 0
    try { $null = Invoke-RestMethod -Uri $tokenBase -Headers @{ "x-owo-desktop-pairing" = $secret; "x-owo-desktop-instance" = "00000000000000000000000000000000" } -TimeoutSec 5; $stBadInstance = 200 }
    catch { $stBadInstance = [int]$_.Exception.Response.StatusCode }
    & $assert "/auth/token 错实例头 → 403" ($stBadInstance -eq 403) "got=$stBadInstance"

    $ok = Invoke-RestMethod -Uri $tokenBase -Headers @{ "x-owo-desktop-pairing" = $secret; "x-owo-desktop-instance" = $instance } -TimeoutSec 5
    & $assert "/auth/token 正确配对+实例 → 200 + token" ($ok.token.Length -ge 64) "token 长度 $($ok.token.Length)"
    $token = $ok.token

    # 关闭门控：错误实例 → 403（服务应存活）；正确实例 → 200 并优雅退出
    # （/server/shutdown 是 Bearer 保护面：壳先经 /auth/token 引导取得 token 再关闭。）
    $authHeaders = @{ "Authorization" = "Bearer $token" }
    $stShutdownBad = 0
    $shutdownBody = '{"confirm":true}'
    try {
        $null = Invoke-RestMethod -Uri "http://127.0.0.1:$port/server/shutdown" -Method Post -Body $shutdownBody -ContentType "application/json" -Headers ($authHeaders + @{ "x-owo-desktop-instance" = "00000000000000000000000000000000" }) -TimeoutSec 5
        $stShutdownBad = 200
    } catch { $stShutdownBad = [int]$_.Exception.Response.StatusCode }
    & $assert "/server/shutdown 错实例头 → 403" ($stShutdownBad -eq 403) "got=$stShutdownBad"
    Start-Sleep -Milliseconds 300
    & $assert "拒绝关闭后服务仍存活" (-not $proc.HasExited) "进程已退出（不应被错误实例头关闭）"

    if (-not $proc.HasExited) {
        $null = Invoke-RestMethod -Uri "http://127.0.0.1:$port/server/shutdown" -Method Post -Body $shutdownBody -ContentType "application/json" -Headers ($authHeaders + @{ "x-owo-desktop-instance" = $instance }) -TimeoutSec 5
        $exited = $false
        $sw.Restart()
        while ($sw.Elapsed.TotalSeconds -lt 15) {
            if ($proc.HasExited) { $exited = $true; break }
            Start-Sleep -Milliseconds 250
        }
        & $assert "正确实例头关闭 → 进程 15s 内优雅退出" $exited "进程未退出"
    }
}
catch {
    Write-Host "会话 1 异常：$($_.Exception.Message)" -ForegroundColor Red
    Get-Content $outLog, $errLog -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "    | $_" }
    $failures.Add("session1-exception")
}
finally {
    if ($proc -and -not $proc.HasExited) { try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch {} }
}

# ---------- 会话 2：开发模式对照（无实例/配对注入，浏览器调试兼容） ----------
if (-not $SkipDevControl) {
    Remove-Item Env:OWO_DESKTOP_RELEASE -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_DESKTOP_PAIRING_SECRET -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_DESKTOP_INSTANCE_ID -ErrorAction SilentlyContinue

    $ws2 = New-SessionRoot
    $data2 = Join-Path $ws2 "data"
    New-Item -ItemType Directory -Path $ws2, $data2 -Force | Out-Null
    $out2 = Join-Path $ws2 "out.log"
    $err2 = Join-Path $ws2 "err.log"
    $env:OWO_AGENT_DATA = $data2
    $proc2 = Start-Process -FilePath $cli -ArgumentList @("serve", "--workspace", $ws2, "--port", "0") -PassThru -RedirectStandardOutput $out2 -RedirectStandardError $err2 -NoNewWindow
    try {
        $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
        $line2 = Get-CoreReadyLine -LogPath $out2 -Sw $sw2 -TimeoutSec 25 -Proc $proc2
        Write-Host "== 开发模式对照（无注入） ==" -ForegroundColor Cyan
        if ($null -eq $line2) {
            & $assert "开发模式 ready 行仍输出" $false "25s 未观察到 core_ready"
        } else {
            $ready2 = $line2 | ConvertFrom-Json
            & $assert "开发模式 ready.instance_id 为空" ([string]::IsNullOrEmpty($ready2.instance_id)) "instance=$($ready2.instance_id)"
            $h2 = Invoke-RestMethod -Uri "http://127.0.0.1:$($ready2.port)/health" -TimeoutSec 5
            & $assert "开发模式 /health.instance_id 为 null" ($null -eq $h2.instance_id) "got=$($h2.instance_id)"
            $st2 = 0
            try { $null = Invoke-RestMethod -Uri "http://127.0.0.1:$($ready2.port)/auth/token" -TimeoutSec 5; $st2 = 200 }
            catch { $st2 = [int]$_.Exception.Response.StatusCode }
            & $assert "开发模式 /auth/token 无头 → 200（浏览器调试兼容）" ($st2 -eq 200) "got=$st2"
        }
    }
    catch {
        Write-Host "会话 2 异常：$($_.Exception.Message)" -ForegroundColor Red
        $failures.Add("session2-exception")
    }
    finally {
        if ($proc2 -and -not $proc2.HasExited) { try { Stop-Process -Id $proc2.Id -Force -ErrorAction SilentlyContinue } catch {} }
    }
}

Remove-Item Env:OWO_AGENT_DATA -ErrorAction SilentlyContinue
Remove-Item Env:OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
Remove-Item Env:OPENAI_BASE_URL -ErrorAction SilentlyContinue

Write-Host "== 清理完成 ==" -ForegroundColor DarkGray
if ($failures.Count -gt 0) {
    Write-Host ("结论：{0} 项未通过：{1}" -f $failures.Count, ($failures -join "; ")) -ForegroundColor Red
    exit 1
}
Write-Host "结论：§4.2 实例握手核心侧契约全部通过" -ForegroundColor Green
exit 0
