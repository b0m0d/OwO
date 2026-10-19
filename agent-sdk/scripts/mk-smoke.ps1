#requires -Version 5.1
<#
mk-smoke.ps1 — 微内核重构运行态验收（P0 证据链修复版）。

背景（指南 §1.3 / F-08）：旧版脚本直接运行 `target\debug\owo-agent.exe`，既不构建
当前源码、也不比较 HEAD 与产物 build id，导致 M0 的旧二进制拿到了 M15 的 18/18。
本版把"产物身份"变成**启动前的硬门**：

  1. `-ExePath` 可显式指定产物；缺省 target\debug\owo-agent.exe（绝不静默换路径）。
  2. 启动前读取：当前 HEAD / dirty、产物 SHA256 / mtime、产物 `--version` 自报的
     commit/dirty/built_at/api/version。commit 或 dirty 与源码不一致 → 立即失败
     （退出码 3），**不启动任何进程**。
  3. `-Build` 时先经 §2.4 资源门构建当前源码（Invoke-CiCargo，含内存/磁盘/构建空闲门），
     构建被资源门拒绝 → 退出码 4；编译失败 → 退出码 2。
  4. `-IdentityOnly` 只做身份门（离线、秒级），给负例测试做匹配态正例。
  5. 基础运行态门（真实进程 + 真实 HTTP + 真实落盘）与旧版一致。
  6. `-RealTurn` 追加**真实模型 turn 冒烟**：用进程环境里已配置的 Provider，验证收到
     多个逐 delta 实时事件（不是整轮缓存回放）。密钥只从环境变量读取，不打印。
  7. reference/mock 证据（ProductEval reference 回放）在报告里单列 `evidence=reference`,
     不与真实模型闭环混算。

退出码（便于 CI 区分，指南 §8 P0 第 8 条）：
  0 = 通过（含显式 skip 的 not_requested）；1 = 基础门失败；2 = 预检/构建失败；
  3 = 产物身份不符（旧二进制）；4 = 资源门拒绝；5 = 真实 turn 失败。

用法：
  & scripts\mk-smoke.ps1 -Tag p0-basic -Build
  & scripts\mk-smoke.ps1 -Tag p0-real -Build -RealTurn
  & scripts\mk-smoke.ps1 -Tag p0-identity -IdentityOnly
#>
[CmdletBinding()]
param(
    [string]$Tag = 'smoke',
    # 显式产物路径；缺省 target\debug\owo-agent.exe。禁止脚本静默改选其它产物。
    [string]$ExePath = '',
    # 先经资源门构建当前源码的 owo-agent-cli（保证产物与 HEAD 同源）。
    [switch]$Build,
    [ValidateSet('normal', 'strict')][string]$BuildPolicy = 'normal',
    # 追加真实模型 turn 冒烟（需要 OPENAI_API_KEY 等进程环境变量）。
    [switch]$RealTurn,
    # 只跑到身份门就结束（离线负例/正例测试用）。
    [switch]$IdentityOnly,
    [int]$ReadyTimeoutSec = 90,
    [int]$RealTurnTimeoutSec = 240
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

$EXIT_PASS = 0
$EXIT_SMOKE_FAIL = 1
$EXIT_PREFLIGHT = 2
$EXIT_IDENTITY = 3
$EXIT_RESOURCE = 4
$EXIT_REALTURN = 5

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
    schema       = 'owo-mk-smoke/2'
    tag          = $Tag
    mode         = $(if ($RealTurn) { 'real-turn' } else { 'basic' })
    identity_only = [bool]$IdentityOnly
    started_at   = (Get-Date).ToUniversalTime().ToString('o')
    finished_at  = ''
    source       = [ordered]@{ commit = ''; dirty = $null }
    binary       = $null
    build        = [ordered]@{ performed = [bool]$Build; policy = $BuildPolicy; exit_code = $null }
    identity     = [ordered]@{ ok = $false; reason = '' }
    checks       = @()
    real_turn    = [ordered]@{ requested = [bool]$RealTurn; status = 'not_requested' }
    started      = $false
    work_dir     = $work
    evidence_dir = $evidence
    ok           = $false
    exit_code    = $null
}

function Add-Check([string]$name, [bool]$pass, [string]$detail, [string]$evidenceKind = 'harness') {
    $script:report.checks += [ordered]@{ check = $name; pass = $pass; detail = $detail; evidence = $evidenceKind }
    $color = if ($pass) { 'Green' } else { 'Red' }
    Write-Host ("  [{0}] {1} — {2}" -f $(if ($pass) { 'PASS' } else { 'FAIL' }), $name, $detail) -ForegroundColor $color
}

function Save-Report {
    $script:report.finished_at = (Get-Date).ToUniversalTime().ToString('o')
    $json = ConvertTo-Json -InputObject $script:report -Depth 8
    [System.IO.File]::WriteAllText((Join-Path $script:evidence 'report.json'), $json + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
}

function Complete([int]$code) {
    $script:report.exit_code = $code
    $script:report.ok = ($code -eq $EXIT_PASS)
    Save-Report
    Write-Host ("mk-smoke[{0}] exit={1} ok={2} evidence={3}" -f $Tag, $code, $script:report.ok, $evidence)
    exit $code
}

# ---- 预检：源码身份 ---------------------------------------------------------------
$src = Get-CiGitIdentity -RepoRoot $root
$report.source = [ordered]@{ commit = $src.commit; dirty = [bool]$src.dirty }
Write-Host ("[mk-smoke] 源码 HEAD={0} dirty={1}" -f $src.commit, $src.dirty)

# ---- 可选：经资源门构建当前源码 ---------------------------------------------------
if ($Build) {
    . (Join-Path $PSScriptRoot 'resolve-ort.ps1')
    Resolve-OwoOrtEnv -Quiet | Out-Null
    Write-Host ("[mk-smoke] 经 §2.4 资源门构建当前源码（policy={0}，-j 1）…" -f $BuildPolicy)
    $buildArgs = @('build', '-p', 'owo-agent-cli', '--locked', '-j', '1')
    $buildCode = $null
    try {
        $buildCode = Invoke-CiCargo -Arguments $buildArgs -Cwd $root -Label 'mk-smoke-build' -PolicyMode $BuildPolicy -HeartbeatSec 60 -PassThru
    } catch {
        # 启动前被资源门拒绝（内存/磁盘）是抛异常；如实记入报告并按 4 退出。
        $report.build.exit_code = 137
        Add-Check 'build.resource_gate' $false $_.Exception.Message 'harness'
        Complete $EXIT_RESOURCE
    }
    $report.build.exit_code = $buildCode
    if ($buildCode -eq 137) {
        Add-Check 'build.resource_gate' $false '资源门运行中触发（resource_limited）' 'harness'
        Complete $EXIT_RESOURCE
    }
    if ($buildCode -ne 0) {
        Add-Check 'build.current_source' $false ("cargo build -p owo-agent-cli exit={0}" -f $buildCode) 'harness'
        Complete $EXIT_PREFLIGHT
    }
    Add-Check 'build.current_source' $true '当前源码构建成功（Invoke-CiCargo，资源门通过）' 'harness'
}

# ---- 预检：产物存在与身份 ---------------------------------------------------------
$exe = if ($ExePath) { $ExePath } else { Join-Path $root 'target\debug\owo-agent.exe' }
if (-not (Test-Path -LiteralPath $exe)) {
    Add-Check 'binary.present' $false ("缺少产物：{0}（用 -Build 构建当前源码，或 -ExePath 指定）" -f $exe) 'harness'
    Complete $EXIT_PREFLIGHT
}
try {
    $bin = Get-CiExeIdentity -ExePath $exe
} catch {
    Add-Check 'binary.readable' $false ("产物不可读/不可执行：{0}" -f $_.Exception.Message) 'harness'
    Complete $EXIT_PREFLIGHT
}
$report.binary = [ordered]@{
    path        = $bin.path
    sha256      = $bin.sha256
    bytes       = $bin.bytes
    mtime       = $bin.mtime
    version     = $bin.version
    commit      = $bin.commit
    dirty       = $bin.dirty
    built_at    = $bin.built_at
    api_version = $bin.api_version
    version_line = $bin.version_line
}
Write-Host ("[mk-smoke] 产物 {0}" -f $bin.path)
Write-Host ("[mk-smoke] sha256={0}" -f $bin.sha256)
Write-Host ("[mk-smoke] 产物身份：{0}" -f $bin.version_line)

$verdict = Test-CiBinaryIdentity -Source $src -Binary $bin
$report.identity.ok = $verdict.ok
$report.identity.reason = $verdict.reason
Add-Check 'binary.identity_matches_head' $verdict.ok $verdict.reason 'identity'
if (-not $verdict.ok) {
    # 关键：身份不符时**不得**启动任何进程（旧二进制必须在此失败）。
    Write-Host '[mk-smoke] 产物身份与当前 HEAD 不符——按 P0 证据链规则直接失败，不启动进程。' -ForegroundColor Red
    Complete $EXIT_IDENTITY
}

if ($IdentityOnly) {
    Write-Host '[mk-smoke] -IdentityOnly：身份门通过，未启动守护进程。' -ForegroundColor Green
    Complete $EXIT_PASS
}

# ---- 运行态验收（真实进程 + 真实 HTTP + 真实落盘） --------------------------------
$env:OWO_AGENT_DATA = $data
if (-not $RealTurn) {
    # 基础模式不需要出网；显式禁云，保证可复现、零外部依赖。
    $env:OWO_CLOUD_ENABLED = 'false'
}
if ($RealTurn) {
    # 真实模型需要 Provider 凭据：从用户级环境变量注入本进程（子进程继承），不打印。
    $key = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if (-not $key) { $key = $env:OPENAI_API_KEY }
    if ($key) { $env:OPENAI_API_KEY = $key }
    $report.real_turn.credential_present = [bool]$key
}

$proc = $null
try {
    $proc = Start-Process -FilePath $exe -ArgumentList @('serve', '--port', '0', '--output', 'jsonl', '--workspace', $ws) `
        -WorkingDirectory $root -RedirectStandardOutput $out -RedirectStandardError $err -PassThru -WindowStyle Hidden
    $report.started = $true

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
    Write-Host ("  core_ready: port={0} pid={1} api_version={2} build_id={3}" -f $json.port, $json.pid, $json.api_version, $json.build_id)

    # core_ready 自报的 build_id 必须再次与当前 HEAD 一致（运行进程身份，与产物文件身份交叉核对）。
    $runtimeIdOk = ($json.build_id -eq $src.commit)
    Add-Check 'daemon.runtime_build_id_matches_head' $runtimeIdOk ("core_ready.build_id={0} HEAD={1}" -f $json.build_id, $src.commit) 'identity'

    # §2.3：发现文件必须存在，且 pid/port/api_version 与 core_ready 自报值一致。
    # 这是 CLI/TUI/桌面壳发现并复用**同一** Daemon 的唯一通道（P1）。
    $discoveryFile = Join-Path $data 'runtime\daemon.json'
    $discoveryOk = $false
    $discoveryDetail = ''
    if (Test-Path -LiteralPath $discoveryFile) {
        try {
            $desc = Get-Content -LiteralPath $discoveryFile -Raw | ConvertFrom-Json
            $discoveryOk = ($desc.pid -eq $json.pid) -and ($desc.port -eq $json.port) -and ($desc.api_version -eq $json.api_version)
            $discoveryDetail = ("pid={0} port={1} api={2} instance='{3}' data_root={4}" -f $desc.pid, $desc.port, $desc.api_version, $desc.instance_id, $desc.data_root)
        } catch {
            $discoveryDetail = "解析失败：$($_.Exception.Message)"
        }
    } else {
        $discoveryDetail = "缺少 $discoveryFile"
    }
    Add-Check 'daemon.discovery_file' $discoveryOk $discoveryDetail

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
    #    reference 执行模式是确定性 dry 回放（零模型调用、零网络）——属于 mock 证据，
    #    报告里以 evidence=reference 单列，不计入真实模型闭环。
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
            Add-Check 'product_eval.create_run' ($peResp.StatusCode -eq 202 -and $peJson.run_id) "status=$($peResp.StatusCode) run_id=$($peJson.run_id)" 'reference'
            $done = $false; $detail = $null
            $wait = (Get-Date).AddSeconds(60)
            while ((Get-Date) -lt $wait) {
                Start-Sleep -Milliseconds 700
                $d = (Invoke-WebRequest -Uri "$base/product-eval/runs/$($peJson.run_id)" -Headers $h -UseBasicParsing -TimeoutSec 20).Content | ConvertFrom-Json
                if ($d.status -in @('completed', 'failed', 'cancelled', 'interrupted')) { $detail = $d; $done = $true; break }
            }
            if ($done) {
                Add-Check 'product_eval.run_completed' ($detail.status -eq 'completed') ("status=$($detail.status) progress=$($detail.progress.done)/$($detail.progress.total) metrics.runs_total=$($detail.report.metrics.runs_total)") 'reference'
            } else {
                Add-Check 'product_eval.run_completed' $false '60s 内未到终态' 'reference'
            }
        } catch {
            Add-Check 'product_eval.create_run' $false $_.Exception.Message 'reference'
        }
    } else {
        Add-Check 'product_eval.suite_present' $false 'v1 套件未能在隔离工作区可见（跳过 ProductEval 运行态门）' 'reference'
    }

    # 6) Daemon 扩展内核（M2）真实走通：notes 路由 → owo-agent-extensions::notes
    try {
        $noteTitle = "mk-smoke-$Tag"
        $noteBody = @{ title = $noteTitle } | ConvertTo-Json -Compress
        $nResp = Invoke-WebRequest -Uri "$base/notes" -Method Post -Headers $h -ContentType 'application/json' -Body $noteBody -UseBasicParsing -TimeoutSec 20
        $nJson = $nResp.Content | ConvertFrom-Json
        Add-Check 'extensions.notes_create' ($nResp.StatusCode -eq 201 -and $nJson.id) "status=$($nResp.StatusCode) id=$($nJson.id)"
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

    # 8) 真实模型 turn 冒烟（可选，-RealTurn）
    if ($RealTurn) {
        $rt = $report.real_turn
        if (-not $report.real_turn.credential_present) {
            $rt.status = 'failed_missing_credentials'
            Add-Check 'real_turn.multi_delta' $false '未找到 OPENAI_API_KEY（用户级环境变量或本进程环境）——真实 turn 无法执行' 'real-model'
                    } else {
            $helper = Join-Path $PSScriptRoot 'real-turn-smoke.py'
            $rtOut = Join-Path $work 'real-turn.json'
            $env:OWO_SMOKE_TOKEN = $token
            $env:OWO_SMOKE_BASE = $base
            $env:OWO_SMOKE_WORKSPACE = $ws
            $env:OWO_SMOKE_OUT = $rtOut
            Write-Host ("[mk-smoke] 真实模型 turn 冒烟（{0}s 上限，逐 delta）…" -f $RealTurnTimeoutSec)
            $pyCode = 1
            try {
                & python $helper --base $base --workspace $ws --timeout $RealTurnTimeoutSec --out $rtOut 2>&1 | ForEach-Object { Write-Host "    $_" }
                $pyCode = $LASTEXITCODE
            } catch {
                Add-Check 'real_turn.helper' $false $_.Exception.Message 'real-model'
            }
            if (Test-Path -LiteralPath $rtOut) {
                try { $rtJson = Get-Content -LiteralPath $rtOut -Raw | ConvertFrom-Json } catch { $rtJson = $null }
            } else { $rtJson = $null }
            if ($rtJson) {
                $rt.status = $rtJson.status
                $rt.delta_count = $rtJson.delta_count
                $rt.first_delta_latency_s = $rtJson.first_delta_latency_s
                $rt.stream_span_s = $rtJson.stream_span_s
                $rt.final_latency_s = $rtJson.final_latency_s
                $rt.buffered_suspected = $rtJson.buffered_suspected
                $rt.text_len = $rtJson.text_len
                $rt.error = $rtJson.error
                $rt.final_received = $rtJson.final_received
                Add-Check 'real_turn.multi_delta' ([int]$rtJson.delta_count -ge 2) ("delta_count={0}（要求 ≥2 的逐 delta 实时事件）" -f $rtJson.delta_count) 'real-model'
                Add-Check 'real_turn.first_delta_before_final' ([bool]$rtJson.first_delta_before_final) ("first_delta_latency={0}s stream_span={1}s final_latency={2}s" -f $rtJson.first_delta_latency_s, $rtJson.stream_span_s, $rtJson.final_latency_s) 'real-model'
                Add-Check 'real_turn.final_received' ([bool]$rtJson.final_received) ("final={0} text_len={1}" -f $rtJson.final_received, $rtJson.text_len) 'real-model'
                # P0 只负责"真实模型可达 + 收到多个 delta"；是否**逐 delta 实时**由 P2 修复。
                # 若检测到整轮缓存回放（F-02）则显式告警，不伪装成真流式，也不作为 P0 阻断项。
                if ($rtJson.buffered_suspected) {
                    Write-Host '  [WARN] real_turn.buffered_suspected=true：delta 集中在整轮结束时到达，疑似 F-02 假流式（P2 修复项，非 P0 阻断）' -ForegroundColor Yellow
                }
            } else {
                Add-Check 'real_turn.multi_delta' $false ("helper 未产出结果（exit={0}）" -f $pyCode) 'real-model'
                            }
            Remove-Item Env:\OWO_SMOKE_TOKEN, Env:\OWO_SMOKE_BASE, Env:\OWO_SMOKE_WORKSPACE, Env:\OWO_SMOKE_OUT -ErrorAction SilentlyContinue
        }
    }

} catch {
    Add-Check 'smoke.aborted' $false $_.Exception.Message 'harness'
} finally {
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 2
    }
    $leftover = if ($proc) { [bool](Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) } else { $false }
    Add-Check 'process.no_orphan' (-not $leftover) ("残留进程=$leftover")
    Copy-Item -LiteralPath $out -Destination (Join-Path $evidence 'serve.stdout.log') -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $err -Destination (Join-Path $evidence 'serve.stderr.log') -ErrorAction SilentlyContinue
    Remove-Item Env:\OWO_AGENT_DATA -ErrorAction SilentlyContinue
    Remove-Item Env:\OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
}

# 最终判定在所有检查（含 finally 里的 no_orphan）之后统一计算：
#   任一非真实模型检查失败 → 1；仅真实模型检查失败 → 5；否则 0。
$realFail = @($report.checks | Where-Object { -not $_.pass -and $_.evidence -eq 'real-model' }).Count
$otherFail = @($report.checks | Where-Object { -not $_.pass -and $_.evidence -ne 'real-model' }).Count
$finalCode = if ($otherFail -gt 0) { $EXIT_SMOKE_FAIL } elseif ($realFail -gt 0) { $EXIT_REALTURN } else { $EXIT_PASS }

Complete $finalCode
