#requires -Version 5.1
<#
verify-cross-client.ps1 — P1 增量 5：跨客户端一致性验收（指南 §8 P1 第 7 条）。

验证目标：**同一 Daemon** 被 CLI 与 HTTP/Rust 客户端共享——
  1. 启动一个隔离 Daemon（真实进程 + 发现文件）；
  2. HTTP 客户端创建会话 A；
  3. CLI `turn` **复用**同一 Daemon 并创建会话 B；
  4. `GET /sessions` 同时看到 A 与 B（会话数据同源）；
  5. `owo-agent-client` 示例（Rust 共享客户端）也看到同一批会话；
  6. `daemon status` 报出的 pid/port 与 core_ready 一致；
  7. `daemon stop` 优雅停止。

脚本自带清理：无论成败都会停止自己启动的 Daemon（避免孤儿进程）。
用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-cross-client.ps1
#>
[CmdletBinding()]
param(
    [string]$Tag = 'cross-client',
    [int]$ReadyTimeoutSec = 60,
    # 追加真实模型 turn（经 CLI 共享客户端路径），需要 OPENAI_API_KEY。
    [switch]$RealTurn
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root

$exe = Join-Path $root 'target\debug\owo-agent.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "缺少产物：$exe（先构建 owo-agent-cli）" }

$ts = Get-Date -Format 'yyyyMMdd-HHmmss'
$work = Join-Path $env:TEMP ("owo-$Tag-$ts")
$ws = Join-Path $work 'ws'
$data = Join-Path $work 'data'
New-Item -ItemType Directory -Force -Path $ws, $data | Out-Null
$out = Join-Path $work 'serve.out.log'
$err = Join-Path $work 'serve.err.log'
$evidence = Join-Path $root ("docs\qa\evidence\{0}-{1}" -f $Tag, $ts)
New-Item -ItemType Directory -Force -Path $evidence | Out-Null

$results = @()
function Add-Check([string]$name, [bool]$pass, [string]$detail) {
    $script:results += [ordered]@{ check = $name; pass = $pass; detail = $detail }
    Write-Host ("  [{0}] {1} — {2}" -f $(if ($pass) { 'PASS' } else { 'FAIL' }), $name, $detail) `
        -ForegroundColor $(if ($pass) { 'Green' } else { 'Red' })
}

$proc = $null
try {
    # 0) 自动启动（指南 §2.3 规则 3）：全新数据根下 CLI `turn` 必须自行拉起 Daemon。
    #    用 Start-Process -Wait 捕获 turn 自身退出（daemon 是分离进程，另行停止）。
    $data2 = Join-Path $work 'data-autostart'
    New-Item -ItemType Directory -Force -Path $data2 | Out-Null
    $turnOut = Join-Path $work 'autostart-turn.out.log'
    $turnErr = Join-Path $work 'autostart-turn.err.log'
    $turnProc = Start-Process -FilePath $exe `
        -ArgumentList @('turn', '--workspace', $ws, '--prompt', 'hello', '--no-approval', '--output', 'jsonl', '--data-dir', $data2) `
        -WorkingDirectory $root -RedirectStandardOutput $turnOut -RedirectStandardError $turnErr -PassThru -WindowStyle Hidden
    $null = $turnProc.WaitForExit(120000)
    $desc2Path = Join-Path $data2 'runtime\daemon.json'
    $autoOk = $false
    $autoDetail = '未生成 discovery（turn 未启动 Daemon）'
    $autoPid = 0
    if (Test-Path -LiteralPath $desc2Path) {
        $desc2 = Get-Content -LiteralPath $desc2Path -Raw | ConvertFrom-Json
        $autoPid = [int]$desc2.pid
        $autoOk = [bool](Get-Process -Id $autoPid -ErrorAction SilentlyContinue)
        $autoDetail = "spawned pid=$autoPid port=$($desc2.port)（turn exit=$($turnProc.ExitCode)）"
    }
    Add-Check 'cli.autostart_daemon' $autoOk $autoDetail
    if ($autoPid -gt 0) {
        $savedEap0 = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & $exe daemon stop --data-dir $data2 2>&1 | Out-Null
        $ErrorActionPreference = $savedEap0
        Start-Sleep -Seconds 1
        Add-Check 'cli.autostart_cleanup' (-not [bool](Get-Process -Id $autoPid -ErrorAction SilentlyContinue)) "pid=$autoPid 已停止"
    }

    $env:OWO_AGENT_DATA = $data
    if (-not $RealTurn) {
        $env:OWO_CLOUD_ENABLED = 'false'
    } else {
        $key = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
        if (-not $key) { $key = $env:OPENAI_API_KEY }
        if ($key) { $env:OPENAI_API_KEY = $key }
        Add-Check 'real_turn.credential_present' ([bool]$key) 'OPENAI_API_KEY 已注入（不回显）'
    }
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
    Add-Check 'daemon.core_ready' ([bool]$ready) $(if ($ready) { 'core_ready 已输出' } else { '未就绪' })
    if (-not $ready) { throw 'no core_ready' }
    $info = $ready | ConvertFrom-Json
    $base = "http://127.0.0.1:$($info.port)"
    $token = (Get-Content -LiteralPath (Join-Path $data 'auth\token') -Raw).Trim()
    $headers = @{ Authorization = "Bearer $token" }

    # 1) HTTP 客户端创建会话 A
    $bodyA = @{ workspace = $ws } | ConvertTo-Json -Compress
    $respA = Invoke-WebRequest -Uri "$base/session" -Method Post -Headers $headers -ContentType 'application/json' -Body $bodyA -UseBasicParsing -TimeoutSec 20
    $sessionA = $respA.Content | ConvertFrom-Json
    Add-Check 'http.create_session_A' ($respA.StatusCode -eq 200 -and $sessionA.id) "id=$($sessionA.id)"

    # 2) CLI turn 复用同一 Daemon（--data-dir 指向同一数据根）并创建会话 B
    #    注意：ConvertFrom-Json 的顶层数组必须**先赋值**再 @(...).Count，内联管道会
    #    把整个数组当单个对象（PS 5.1 实测），会把数量恒算成 1。
    $beforeList = (Invoke-WebRequest -Uri "$base/sessions" -Headers $headers -UseBasicParsing -TimeoutSec 15).Content | ConvertFrom-Json
    $before = @($beforeList).Count
    $savedEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'   # 原生命令的 stderr（弃用告警等）不得中断验收
    $prompt = if ($RealTurn) { '请只回答两个字：你好' } else { 'hello' }
    $turnLog = Join-Path $work 'cli-turn.log'
    & $exe turn --workspace $ws --prompt $prompt --no-approval --output jsonl --data-dir $data 2>&1 |
        Out-File -LiteralPath $turnLog -Encoding utf8
    $turnExit = $LASTEXITCODE
    $ErrorActionPreference = $savedEap
    $afterList = (Invoke-WebRequest -Uri "$base/sessions" -Headers $headers -UseBasicParsing -TimeoutSec 15).Content | ConvertFrom-Json
    $after = @($afterList).Count
    Add-Check 'cli.turn_reused_same_daemon' ($after -ge ($before + 1)) "sessions $before -> $after（CLI turn exit=$turnExit，模型未配置不影响会话创建）"

    if ($RealTurn) {
        $turnText = Get-Content -LiteralPath $turnLog -Raw -ErrorAction SilentlyContinue
        $resultLine = @($turnText -split "`n" | Where-Object { $_ -match '"type"\s*:\s*"turn_result"' } | Select-Object -Last 1)
        $finalText = $null
        if ($resultLine.Count -gt 0) {
            try { $finalText = ($resultLine[0] | ConvertFrom-Json).final_text } catch { $finalText = $null }
        }
        Add-Check 'cli.real_turn_final_text' ([bool]$finalText) "final_text=$finalText（CLI 经共享客户端消费 Daemon SSE）"
    }

    # 3) 同一 /sessions 同时包含 A（HTTP 建）与新会话（CLI 建）
    $ids = @($afterList | ForEach-Object { $_.id })
    Add-Check 'shared.sessions_visible_to_both' ($ids -contains $sessionA.id) "HTTP 会话 $($sessionA.id) 在共享会话列表中"

    # 3b) daemon 模式 REPL：管道输入 /new → /status → /exit，复用同一 Daemon
    $replOut = Join-Path $work 'repl-daemon.log'
    $replInput = "/new`n/status`n/audit`n/capabilities`n/exit`n"
    $savedEap3 = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $replInput | & $exe repl --workspace $ws --data-dir $data *> $replOut
    $ErrorActionPreference = $savedEap3
    $replText = Get-Content -LiteralPath $replOut -Raw -ErrorAction SilentlyContinue
    $replNew = $replText -match '新会话：'
    $replStatus = $replText -match '工作区：'
    $replAudit = $replText -match '\[审计\]'
    $replCaps = $replText -match '\[能力\]'
    Add-Check 'repl.default_daemon_mode' ($replNew -and $replStatus) "默认 REPL 含 新会话/工作区（经共享客户端，无 --local）"
    Add-Check 'repl.command_parity_get' ($replAudit -and $replCaps) "默认 REPL /audit /capabilities 经服务端读取"
    $replSessionsRaw = (Invoke-WebRequest -Uri "$base/sessions" -Headers $headers -UseBasicParsing -TimeoutSec 15).Content
    $replList = $replSessionsRaw | ConvertFrom-Json
    $afterRepl = @($replList).Count
    Add-Check 'repl.daemon_session_shared' ($afterRepl -ge ($after + 1)) "REPL /new 后共享会话数 $after -> $afterRepl"

    # 4) Rust 共享客户端也看到同一批会话
    $savedEap2 = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $clientOut = & cargo run -q -p owo-agent-client --example discover -- $data 2>&1 | Out-String
    $ErrorActionPreference = $savedEap2
    $clientOk = $clientOut -match 'healthy=true'
    $clientSessions = if ($clientOut -match 'sessions=(\d+)') { [int]$Matches[1] } else { -1 }
    Add-Check 'client.example_connects' $clientOk ("client 输出：" + (($clientOut.Trim() -split "`n") | Select-Object -First 1))
    Add-Check 'client.sees_same_sessions' ($clientSessions -eq $afterRepl) "client sessions=$clientSessions daemon sessions=$afterRepl"

    # 5) daemon status 与 core_ready 一致
    $statusOut = & $exe daemon status --data-dir $data 2>&1 | Out-String
    $statusOk = ($statusOut -match "pid=$($info.pid)") -and ($statusOut -match "port=$($info.port)")
    Add-Check 'daemon.status_matches' $statusOk ("status 行：" + (($statusOut.Trim() -split "`n") | Select-Object -First 1))

    # 6) daemon stop 优雅停止
    & $exe daemon stop --data-dir $data 2>&1 | Out-Null
    Start-Sleep -Seconds 2
    $alive = [bool](Get-Process -Id $info.pid -ErrorAction SilentlyContinue)
    Add-Check 'daemon.stop_graceful' (-not $alive) "pid=$($info.pid) 存活=$alive"
} catch {
    Add-Check 'verify.aborted' $false $_.Exception.Message
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    # 兜底：确保没有残留 daemon 占用该数据根。
    if (Test-Path -LiteralPath (Join-Path $data 'runtime\daemon.json')) {
        try {
            $desc = Get-Content -LiteralPath (Join-Path $data 'runtime\daemon.json') -Raw | ConvertFrom-Json
            Stop-Process -Id $desc.pid -Force -ErrorAction SilentlyContinue
        } catch { }
    }
    Remove-Item Env:\OWO_AGENT_DATA, Env:\OWO_CLOUD_ENABLED -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $out -Destination (Join-Path $evidence 'serve.stdout.log') -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $err -Destination (Join-Path $evidence 'serve.stderr.log') -ErrorAction SilentlyContinue
}

$report = [ordered]@{
    schema      = 'owo-cross-client/1'
    tag         = $Tag
    finished_at = (Get-Date).ToUniversalTime().ToString('o')
    work_dir    = $work
    evidence    = $evidence
    checks      = $results
    ok          = -not (@($results | Where-Object { -not $_.pass }).Count)
}
$json = ConvertTo-Json -InputObject $report -Depth 6
[System.IO.File]::WriteAllText((Join-Path $evidence 'report.json'), $json + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
$fail = @($results | Where-Object { -not $_.pass }).Count
Write-Host ("verify-cross-client[{0}] {1}/{2} 通过 evidence={3}" -f $Tag, (@($results).Count - $fail), @($results).Count, $evidence)
if ($fail -gt 0) { exit 1 }
exit 0
