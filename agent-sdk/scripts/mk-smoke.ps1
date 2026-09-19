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

    # 5) 开发工具（M1）真实走通：ProductEval 路由 → owo-agent-eval-facade → devtools/product-eval
    #    reference 执行模式是确定性 dry 回放（零模型调用、零网络），适合做运行态门。
    #    suite 只能按注册名（v1 → <workspace>/evals/v1/suite.json），所以把仓库 evals 以目录
    #    联接挂进本轮隔离工作区。**不能另起第二个服务实例**：serve.rs 用 pid 文件做单实例
    #    闸门，实测第二次启动直接报“检测到运行中的服务（pid=…）：请先停止该进程再启动”。
    $repoEvals = Join-Path (Split-Path -Parent $PSScriptRoot) 'evals'
    $wsEvals = Join-Path $ws 'evals'
    $suiteVisible = $false
    if (Test-Path -LiteralPath (Join-Path $repoEvals 'v1\suite.json')) {
        try {
            if (-not (Test-Path -LiteralPath $wsEvals)) {
                $null = New-Item -ItemType Junction -Path $wsEvals -Target $repoEvals -ErrorAction Stop
            }
            $suiteVisible = Test-Path -LiteralPath (Join-Path $wsEvals 'v1\suite.json')
        } catch {
            try {
                Copy-Item -LiteralPath $repoEvals -Destination $wsEvals -Recurse -Force -ErrorAction Stop
                $suiteVisible = Test-Path -LiteralPath (Join-Path $wsEvals 'v1\suite.json')
            } catch { $suiteVisible = $false }
        }
    }
    if ($suiteVisible) {
        try {
            $peBody = @{ suite = 'v1'; execution = 'reference'; modes = @('single', 'workswarm'); repetitions = 1 } | ConvertTo-Json -Compress
            $peResp = Invoke-WebRequest -Uri "$base/product-eval/runs" -Method Post -Headers $h -ContentType 'application/json' -Body $peBody -UseBasicParsing -TimeoutSec 30
            $peJson = $peResp.Content | ConvertFrom-Json
            Add-Check 'product_eval.create_run' ($peResp.StatusCode -eq 202 -and $peJson.run_id) "status=$($peResp.StatusCode) run_id=$($peJson.run_id)"
            $done = $false; $detail = $null
            $wait = (Get-Date).AddSeconds(60)
            while ((Get-Date) -lt $wait) {
                Start-Sleep -Milliseconds 700
                $d = (Invoke-WebRequest -Uri "$base/product-eval/runs/$($peJson.run_id)" -Headers $h -UseBasicParsing -TimeoutSec 20).Content | ConvertFrom-Json
                if ($d.status -in @('completed', 'failed', 'cancelled', 'interrupted')) { $detail = $d; $done = $true; break }
            }
            if ($done) {
                Add-Check 'product_eval.run_completed' ($detail.status -eq 'completed') ("status=$($detail.status) progress=$($detail.progress.done)/$($detail.progress.total) metrics.runs_total=$($detail.report.metrics.runs_total)")
            } else {
                Add-Check 'product_eval.run_completed' $false '60s 内未到终态'
            }
        } catch {
            Add-Check 'product_eval.create_run' $false $_.Exception.Message
        }
    } else {
        Add-Check 'product_eval.suite_present' $false 'v1 套件未能在隔离工作区可见（跳过 ProductEval 运行态门）'
    }

    # 6) Daemon 扩展内核（M2）真实走通：notes 路由 → owo-agent-extensions::notes
    #    这条路由的服务端处理函数直接调用迁出的 notes 模块（create/list/search 全链），
    #    所以一次真实建笔记 + 搜索就同时证明了“别名 re-export 透明”与“运行时可用”。
    try {
        $noteTitle = "mk-smoke-$Tag"
        $noteBody = @{ title = $noteTitle } | ConvertTo-Json -Compress
        $nResp = Invoke-WebRequest -Uri "$base/notes" -Method Post -Headers $h -ContentType 'application/json' -Body $noteBody -UseBasicParsing -TimeoutSec 20
        $nJson = $nResp.Content | ConvertFrom-Json
        # create_note 的契约状态码是 201（CREATED），不是 200——首次写这个门时断言错成 200。
        Add-Check 'extensions.notes_create' ($nResp.StatusCode -eq 201 -and $nJson.id) "status=$($nResp.StatusCode) id=$($nJson.id)"
        # list_notes 返回 {count, notes:[...]} 对象信封，不是裸数组。
        $nList = (Invoke-WebRequest -Uri "$base/notes" -Headers $h -UseBasicParsing -TimeoutSec 20).Content | ConvertFrom-Json
        $nFound = @($nList.notes | Where-Object { $_.id -eq $nJson.id }).Count
        Add-Check 'extensions.notes_list' ($nFound -ge 1) "count=$($nList.count) 命中=$nFound（notes 已迁至 owo-agent-extensions）"
        $auto = Invoke-WebRequest -Uri "$base/automations" -Headers $h -UseBasicParsing -TimeoutSec 20
        Add-Check 'extensions.automations_list' ($auto.StatusCode -eq 200) "status=$($auto.StatusCode) body=$($auto.Content.Substring(0, [Math]::Min(80, $auto.Content.Length)))"
    } catch {
        Add-Check 'extensions.notes_create' $false $_.Exception.Message
    }

    # 7) 内核原语真实参与的落盘证据
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
