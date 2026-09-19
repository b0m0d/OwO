# 微内核重构运行态验收（每拆一个 crate 后必须跑一次）。
#
# 验收内容（不是"TCP 能连"，而是真实进程 + 真实 HTTP + 真实落盘）：
#   1. 用隔离的 OWO_AGENT_DATA 启动 `owo-agent serve --port 0`
#   2. 从 stdout 读到 core_ready（桌面壳判定就绪的唯一依据）并取得真实端口
#   3. 未鉴权请求被拒（401）——权限面没有被拆分削弱
#   4. 带 auth/token 的 GET /health、/server/status、/sessions 返回 200
#   5. 真实创建一条会话并读回（走 SqliteSessionStore + 迁移后的 storage_crypto/audit）
#   6. 数据目录出现 index.db / auth/token / server.pid（内核原语真实参与运行）
#   7. 停止进程且无残留（不留下孤儿 daemon）
#
# 用法：
#   & "<repo>\agent-sdk\scripts\mk-smoke.ps1" -Tag m0-kernel
#
# 结果写入 docs/qa/evidence/mk-smoke-<Tag>-<ts>/report.json，退出码 0/1。
param(
    [string]$Tag = 'smoke',
    [int]$ReadyTimeoutSec = 90
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\debug\owo-agent.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "缺少 $exe；请先构建：cargo build -p owo-agent-cli" -ForegroundColor Red
    exit 1
}

$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path $env:TEMP ("owo-mk-smoke-{0}-{1}" -f $Tag, $ts)
$ws = Join-Path $work 'ws'
$data = Join-Path $work 'data'
$out = Join-Path $work 'serve.stdout.log'
$err = Join-Path $work 'serve.stderr.log'
New-Item -ItemType Directory -Force -Path $ws, $data, (Join-Path $data 'logs') | Out-Null

$evidence = Join-Path $root ("docs\qa\evidence\mk-smoke-{0}-{1}" -f $Tag, $ts)
New-Item -ItemType Directory -Force -Path $evidence | Out-Null

$report = [ordered]@{
    tag = $Tag; started_at = (Get-Date).ToUniversalTime().ToString('o')
    work_dir = $work; evidence_dir = $evidence
    checks = @(); ok = $false
}
function Add-Check([string]$name, [bool]$pass, [string]$detail) {
    $script:report.checks += [ordered]@{ check = $name; pass = $pass; detail = $detail }
    $color = if ($pass) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1} — {2}" -f $(if ($pass) { 'PASS' } else { 'FAIL' }), $name, $detail) -ForegroundColor $color
}

$env:OWO_AGENT_DATA = $data
$env:OWO_CLOUD_ENABLED = 'false'
$proc = $null
try {
    $proc = Start-Process -FilePath $exe -ArgumentList @('serve', '--port', '0', '--output', 'jsonl', '--workspace', $ws) `
        -WorkingDirectory $root -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -WindowStyle Hidden

    $deadline = (Get-Date).AddSeconds($ReadyTimeoutSec)
    $ready = $null
    while ((Get-Date) -lt $deadline -and -not $ready) {
        Start-Sleep -Milliseconds 300
        if (Test-Path -LiteralPath $out) {
            foreach ($line in (Get-Content -LiteralPath $out -ErrorAction SilentlyContinue)) {
                if ($line -match '"event"\s*:\s*"core_ready"') { $ready = $line; break }
            }
        }
        if ($proc.HasExited) { break }
    }
    Add-Check 'daemon.core_ready' ([bool]$ready) $(if ($ready) { 'core_ready 行已输出' } else { "未在 ${ReadyTimeoutSec}s 内输出 core_ready（exited=$($proc.HasExited)）" })
    if (-not $ready) { throw 'no core_ready' }

    $json = $ready | ConvertFrom-Json
    $base = "http://127.0.0.1:$($json.port)"
    $report.port = $json.port
    $report.pid = $json.pid
    $report.api_version = $json.api_version
    $report.build_id = $json.build_id
    Write-Host ("  core_ready: port={0} pid={1} api_version={2}" -f $json.port, $json.pid, $json.api_version)

    # 1) /health 公开可读且 healthy
    $health = Invoke-WebRequest -Uri "$base/health" -UseBasicParsing -TimeoutSec 15
    $hb = $health.Content | ConvertFrom-Json
    Add-Check 'http.health_public' (($health.StatusCode -eq 200) -and $hb.healthy) ("status=$($health.StatusCode) healthy=$($hb.healthy) stage=$($hb.stage)")

    # 2) 未鉴权受保护路由必须 401（权限面未被削弱）
    $unauth = 0
    try { $null = Invoke-WebRequest -Uri "$base/sessions" -UseBasicParsing -TimeoutSec 15 } catch { $unauth = [int]$_.Exception.Response.StatusCode }
    Add-Check 'http.unauthenticated_401' ($unauth -eq 401) "GET /sessions without token -> $unauth"

    # 3) 带 token 的只读路由
    $tokenFile = Join-Path $data 'auth\token'
    $hasToken = Test-Path -LiteralPath $tokenFile
    Add-Check 'data.auth_token_written' $hasToken ("auth/token 存在=$hasToken（未打印内容）")
    $token = if ($hasToken) { (Get-Content -LiteralPath $tokenFile -Raw).Trim() } else { '' }
    $h = @{ Authorization = "Bearer $token" }
    $sessions = Invoke-WebRequest -Uri "$base/sessions" -Headers $h -UseBasicParsing -TimeoutSec 15
    Add-Check 'http.sessions_list' ($sessions.StatusCode -eq 200) "status=$($sessions.StatusCode) body=$($sessions.Content)"
    $status = Invoke-WebRequest -Uri "$base/server/status" -Headers $h -UseBasicParsing -TimeoutSec 15
    $sb = $status.Content | ConvertFrom-Json
    Add-Check 'http.server_status_readable' ($status.StatusCode -eq 200) ("read_only=$($sb.storage.read_only) warn=$($sb.storage.migration_warning)")

    # 4) 真实创建会话并读回
    #    CreateSessionRequest.workspace 是必填字段（owo-agent-protocol）；缺它服务端
    #    返回 422 而不是 500，说明 schema 校验在正常工作（首次写这个脚本时踩到过）。
    $created = $null
    try {
        $body = @{ workspace = $ws } | ConvertTo-Json -Compress
        $r = Invoke-WebRequest -Uri "$base/session" -Method Post -Headers $h -ContentType 'application/json' -Body $body -UseBasicParsing -TimeoutSec 20
        $created = $r.Content | ConvertFrom-Json
        Add-Check 'session.create' ($r.StatusCode -eq 200 -or $r.StatusCode -eq 201) "status=$($r.StatusCode) id=$($created.id)"
    } catch {
        Add-Check 'session.create' $false $_.Exception.Message
    }
    if ($created -and $created.id) {
        $got = Invoke-WebRequest -Uri "$base/session/$($created.id)" -Headers $h -UseBasicParsing -TimeoutSec 15
        Add-Check 'session.read_back' ($got.StatusCode -eq 200) "status=$($got.StatusCode) len=$($got.Content.Length)"
        $list2 = (Invoke-WebRequest -Uri "$base/sessions" -Headers $h -UseBasicParsing -TimeoutSec 15).Content | ConvertFrom-Json
        $found = @($list2 | Where-Object { $_.id -eq $created.id }).Count
        Add-Check 'session.persisted_in_list' ($found -ge 1) "list 中匹配 $found 条"
    }

    # 5) 内核原语真实参与的落盘证据
    $indexDb = Test-Path -LiteralPath (Join-Path $data 'index.db')
    $pidFile = Test-Path -LiteralPath (Join-Path $data 'server.pid')
    Add-Check 'data.index_db' $indexDb 'index.db 已建立（SqliteSessionStore/WAL）'
    Add-Check 'data.server_pid' $pidFile 'server.pid 已建立'
    $auditSeen = [bool](Select-String -LiteralPath $err -Pattern 'audit_event' -Quiet -ErrorAction SilentlyContinue)
    Add-Check 'audit.event_emitted' $auditSeen 'server_start 审计事件已写入（owo-agent-kernel::audit 参与运行）'

    $report.ok = -not (@($report.checks | Where-Object { -not $_.pass }).Count)
} catch {
    Add-Check 'smoke.aborted' $false $_.Exception.Message
    $report.ok = $false
} finally {
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
    $leftover = if ($proc) { [bool](Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) } else { $false }
    Add-Check 'process.no_orphan' (-not $leftover) ("残留进程=$leftover")
    $report.finished_at = (Get-Date).ToUniversalTime().ToString('o')
    $report.ok = -not (@($report.checks | Where-Object { -not $_.pass }).Count)

    Copy-Item -LiteralPath $out -Destination (Join-Path $evidence 'serve.stdout.log') -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $err -Destination (Join-Path $evidence 'serve.stderr.log') -ErrorAction SilentlyContinue
    Remove-Item Env:\OWO_AGENT_DATA -ErrorAction SilentlyContinue
    Remove-Item Env:\OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
}

$report | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $evidence 'report.json') -Encoding UTF8
Write-Host ("mk-smoke[{0}] ok={1} evidence={2}" -f $Tag, $report.ok, $evidence)
if ($report.ok) { exit 0 } else { exit 1 }
