#requires -Version 5.1
<#
trace-perf.ps1 — P2 §6.4/§9.3：从回合 trace 计算 P50/P95 性能报告。

数据来源：`<traces>/ *.json`（TraceRecord，含 duration_ms 与 phase_timings[]）。
phase_timings 由 agent loop 在真实时间点埋入（phase / elapsed_ms / first_token_ms），
因此本脚本给出的是**真实回合**的模型首 token 与各阶段耗时分布，而不是 HTTP 探针延迟。

固定性能任务：每任务至少 20 个 trace；报告器识别 trace JSON 顶层可选字段 performance_task，
其值必须是下方固定任务 ID 之一。Core 仅在独立基准 Daemon 启动时设置 allowlisted
环境变量 OWO_PERF_TASK_ID 才写入标签；未标记 trace 只作为汇总样本，不会冒充任务覆盖。

用法：
  powershell -File scripts\trace-perf.ps1 -TracesDir <dir> -Out performance-report.json
  powershell -File scripts\trace-perf.ps1 -AllowUnclassifiedAggregate # 仅探索性汇总，不代表发布门通过

退出码：0 = 八项固定任务均达到 MinSamples；2 = 样本/任务标签不足（报告仍落盘）。
#>
[CmdletBinding()]
param(
    [string]$TracesDir = '',
    [ValidateRange(1, 100000)][int]$MinSamples = 20,
    [string]$Out = '',
    [string]$MarkdownOut = '',
    [switch]$AllowUnclassifiedAggregate
)
$ErrorActionPreference = 'Stop'

function Resolve-TracesDir {
    param([string]$Given)
    if ($Given) { return $Given }
    if ($env:OWO_AGENT_DATA) { return (Join-Path $env:OWO_AGENT_DATA 'traces') }
    if ($env:LOCALAPPDATA) { return (Join-Path $env:LOCALAPPDATA 'OwO\Agent\traces') }
    return (Join-Path (Get-Location) 'data\agent\traces')
}

function Get-Percentile {
    param([double[]]$Sorted, [double]$P)
    if (-not $Sorted -or $Sorted.Count -eq 0) { return 0 }
    $index = [Math]::Ceiling($Sorted.Count * $P) - 1
    if ($index -lt 0) { $index = 0 }
    if ($index -ge $Sorted.Count) { $index = $Sorted.Count - 1 }
    return [Math]::Round($Sorted[$index], 2)
}

$dir = Resolve-TracesDir -Given $TracesDir
if (-not (Test-Path -LiteralPath $dir)) {
    Write-Host "[trace-perf] traces 目录不存在：$dir" -ForegroundColor Red
    exit 2
}

$traces = @(Get-ChildItem -LiteralPath $dir -Filter '*.json' -File -ErrorAction SilentlyContinue)
$durations = New-Object System.Collections.Generic.List[double]
$phases = @{}   # phase -> @{ elapsed = List; first_token = List }
$fixedTasks = [ordered]@{
    daemon_start_session      = '启动 Daemon 并创建会话'
    short_text_conversation   = '纯文本短对话（首 token）'
    read_100kb_file           = '读取 100 KB 文件'
    search_1000_files         = '搜索 1,000 文件工作区'
    write_file_and_diff       = '写入小文件并生成 diff'
    approval_command          = '一次需要审批的命令'
    invalid_mcp_startup       = '坏 MCP 存在时启动并对话'
    disconnect_reconnect_cancel = '客户端断开、重连与取消'
}
$taskDurations = @{}
foreach ($taskId in $fixedTasks.Keys) {
    $taskDurations[$taskId] = New-Object System.Collections.Generic.List[double]
}
$unclassified = 0
$promptTokens = [long]0
$completionTokens = [long]0
$totalTokens = [long]0
$models = New-Object System.Collections.Generic.List[string]
foreach ($file in $traces) {
    try {
        $record = Get-Content -LiteralPath $file.FullName -Raw | ConvertFrom-Json
    } catch { continue }
    if ($null -ne $record.duration_ms) {
        $duration = [double]$record.duration_ms
        $durations.Add($duration)
        $taskId = [string]$record.performance_task
        if ($taskId -and $taskDurations.ContainsKey($taskId)) {
            $taskDurations[$taskId].Add($duration)
        } else {
            $unclassified++
        }
    }
    if ($record.model -and -not $models.Contains([string]$record.model)) { $models.Add([string]$record.model) }
    if ($null -ne $record.usage.prompt_tokens) { $promptTokens += [long]$record.usage.prompt_tokens }
    if ($null -ne $record.usage.completion_tokens) { $completionTokens += [long]$record.usage.completion_tokens }
    if ($null -ne $record.usage.total_tokens) { $totalTokens += [long]$record.usage.total_tokens }
    foreach ($phase in @($record.phase_timings)) {
        if (-not $phase -or -not $phase.phase) { continue }
        if (-not $phases.ContainsKey($phase.phase)) {
            $phases[$phase.phase] = @{
                elapsed = New-Object System.Collections.Generic.List[double]
                first_token = New-Object System.Collections.Generic.List[double]
            }
        }
        $phases[$phase.phase].elapsed.Add([double]$phase.elapsed_ms)
        if ($null -ne $phase.first_token_ms) {
            $phases[$phase.phase].first_token.Add([double]$phase.first_token_ms)
        }
    }
}

$sampleCount = $durations.Count
$sorted = @($durations | Sort-Object)
$taskResults = New-Object System.Collections.Generic.List[object]
$coverageComplete = $true
$belowMinTaskCount = 0
foreach ($taskId in $fixedTasks.Keys) {
    $taskSorted = @($taskDurations[$taskId] | Sort-Object)
    $taskSampleCount = $taskSorted.Count
    if ($taskSampleCount -lt $MinSamples) { $coverageComplete = $false; $belowMinTaskCount++ }
    $taskResults.Add([ordered]@{
        task_id = $taskId
        task = $fixedTasks[$taskId]
        samples = $taskSampleCount
        p50_ms = if ($taskSampleCount -gt 0) { Get-Percentile -Sorted $taskSorted -P 0.50 } else { $null }
        p95_ms = if ($taskSampleCount -gt 0) { Get-Percentile -Sorted $taskSorted -P 0.95 } else { $null }
        status = if ($taskSampleCount -ge $MinSamples) { 'measured' } elseif ($taskSampleCount -eq 0) { 'not_measured' } else { 'insufficient_samples' }
    })
}
$repoRoot = Split-Path -Parent $PSScriptRoot
$commit = 'unavailable'
$gitDirty = $null
try {
    $commitValue = & git -C $repoRoot rev-parse HEAD 2>$null
    if ($LASTEXITCODE -eq 0 -and $commitValue) { $commit = ([string]$commitValue).Trim() }
    $gitStatus = @(& git -C $repoRoot status --porcelain 2>$null)
    if ($LASTEXITCODE -eq 0) { $gitDirty = ($gitStatus.Count -gt 0) }
} catch { }
$osCaption = [Environment]::OSVersion.VersionString
try {
    $osInfo = Get-CimInstance Win32_OperatingSystem -ErrorAction Stop
    if ($osInfo.Caption) { $osCaption = [string]$osInfo.Caption }
} catch { }
$providerEndpoint = 'not_recorded'
$uniqueModels = [string[]]$models.ToArray()
[Array]::Sort($uniqueModels, [StringComparer]::OrdinalIgnoreCase)
$providerEndpointSource = 'unavailable'
$rawProviderEndpoint = $env:OPENAI_BASE_URL
if ($rawProviderEndpoint) {
    $providerEndpointSource = 'environment'
} else {
    # Read the active source default instead of duplicating a provider URL in this report tool.
    $gatewaySource = Join-Path $repoRoot 'crates\owo-agent-core\src\gateway.rs'
    if (Test-Path -LiteralPath $gatewaySource) {
        $gatewayText = Get-Content -LiteralPath $gatewaySource -Raw
        $defaultEndpointMatch = [regex]::Match($gatewayText, 'pub const DEFAULT_MODEL_BASE_URL:\s*&str\s*=\s*"([^"\r\n]+)"')
        if ($defaultEndpointMatch.Success) {
            $rawProviderEndpoint = $defaultEndpointMatch.Groups[1].Value
            $providerEndpointSource = 'source_default'
        }
    }
}
if ($rawProviderEndpoint) {
    try {
        $providerUri = [Uri]$rawProviderEndpoint
        $providerEndpoint = '{0}://{1}{2}' -f $providerUri.Scheme, $providerUri.Host, $providerUri.AbsolutePath
        if (-not $providerUri.IsDefaultPort) { $providerEndpoint = '{0}://{1}:{2}{3}' -f $providerUri.Scheme, $providerUri.Host, $providerUri.Port, $providerUri.AbsolutePath }
    } catch { $providerEndpoint = 'configured_unparseable'; $providerEndpointSource = 'configured' }
}
$reportStatus = if ($coverageComplete) { 'complete' } elseif ($sampleCount -lt $MinSamples) { 'insufficient_samples' } else { 'task_coverage_incomplete' }
$taskResultArray = $taskResults.ToArray()
$summary = [ordered]@{
    schema       = 'owo-performance-report/1'
    status       = $reportStatus
    traces_dir   = $dir
    generated_at = (Get-Date).ToUniversalTime().ToString('o')
    machine      = [ordered]@{
        name = [Environment]::MachineName
        os = $osCaption
        architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    }
    source       = [ordered]@{ git_commit = $commit; git_dirty = $gitDirty }
    provider     = [ordered]@{ endpoint = $providerEndpoint; endpoint_source = $providerEndpointSource; models = $uniqueModels }
    sample_policy = [ordered]@{ min_per_fixed_task = $MinSamples; unclassified_traces = $unclassified }
    samples      = $sampleCount
    fixed_tasks  = $taskResultArray
    token_usage  = [ordered]@{ prompt = $promptTokens; completion = $completionTokens; total = $totalTokens }
    turn_duration_ms = [ordered]@{
        p50 = Get-Percentile -Sorted $sorted -P 0.50
        p95 = Get-Percentile -Sorted $sorted -P 0.95
        max = if ($sorted.Count -gt 0) { [Math]::Round($sorted[-1], 2) } else { 0 }
    }
    phases       = [ordered]@{}
}

Write-Host "[trace-perf] traces=$sampleCount dir=$dir"
Write-Host ("  回合 duration_ms：p50={0} p95={1} max={2}" -f `
        $summary.turn_duration_ms.p50, $summary.turn_duration_ms.p95, $summary.turn_duration_ms.max)
foreach ($name in ($phases.Keys | Sort-Object)) {
    $elapsed = @($phases[$name].elapsed | Sort-Object)
    $first = @($phases[$name].first_token | Sort-Object)
    $entry = [ordered]@{
        samples            = $elapsed.Count
        elapsed_p50_ms     = Get-Percentile -Sorted $elapsed -P 0.50
        elapsed_p95_ms     = Get-Percentile -Sorted $elapsed -P 0.95
        first_token_p50_ms = Get-Percentile -Sorted $first -P 0.50
        first_token_p95_ms = Get-Percentile -Sorted $first -P 0.95
    }
        $summary.phases[$name] = $entry
    Write-Host ("  阶段 {0,-12} elapsed p50={1} p95={2}；first_token p50={3} p95={4}" -f `
            $name, $entry.elapsed_p50_ms, $entry.elapsed_p95_ms, $entry.first_token_p50_ms, $entry.first_token_p95_ms)
}

$markdownLines = New-Object System.Collections.Generic.List[string]
$markdownLines.Add('# Agent SDK 性能报告')
$markdownLines.Add('')
$markdownLines.Add(('- 状态：`{0}`；总 trace：{1}；每项任务最低样本：{2}' -f $reportStatus, $sampleCount, $MinSamples))
$markdownLines.Add(('- 生成时间：{0}' -f $summary.generated_at))
$markdownLines.Add(('- 机器：{0} / {1} / {2}' -f $summary.machine.name, $summary.machine.os, $summary.machine.architecture))
$markdownLines.Add(('- 源码：`{0}`；dirty：{1}' -f $summary.source.git_commit, $summary.source.git_dirty))
$markdownLines.Add(('- Provider：{0}（{1}）；模型：{2}' -f $summary.provider.endpoint, $summary.provider.endpoint_source, ($uniqueModels -join ', ')))
$markdownLines.Add(('- token：prompt {0} / completion {1} / total {2}；成本未在 trace 中记录' -f $promptTokens, $completionTokens, $totalTokens))
$markdownLines.Add('')
$markdownLines.Add('## 固定任务')
$markdownLines.Add('')
$markdownLines.Add('| 任务 | 样本 | P50 (ms) | P95 (ms) | 状态 |')
$markdownLines.Add('|---|---:|---:|---:|---|')
foreach ($task in $taskResults) {
    $p50 = if ($null -eq $task.p50_ms) { '—' } else { $task.p50_ms }
    $p95 = if ($null -eq $task.p95_ms) { '—' } else { $task.p95_ms }
    $markdownLines.Add(('| {0} (`{1}`) | {2} | {3} | {4} | {5} |' -f $task.task, $task.task_id, $task.samples, $p50, $p95, $task.status))
}
$markdownLines.Add('')
$markdownLines.Add('## 汇总分布')
$markdownLines.Add('')
$markdownLines.Add('| 指标 | P50 (ms) | P95 (ms) | 最大值 (ms) |')
$markdownLines.Add('|---|---:|---:|---:|')
$markdownLines.Add(('| Turn duration | {0} | {1} | {2} |' -f $summary.turn_duration_ms.p50, $summary.turn_duration_ms.p95, $summary.turn_duration_ms.max))
$markdownLines.Add('')
$markdownLines.Add('| 阶段 | 样本 | elapsed P50 | elapsed P95 | 首 token P50 | 首 token P95 |')
$markdownLines.Add('|---|---:|---:|---:|---:|---:|')
foreach ($name in ($phases.Keys | Sort-Object)) {
    $entry = $summary.phases[$name]
    $markdownLines.Add(('| {0} | {1} | {2} | {3} | {4} | {5} |' -f $name, $entry.samples, $entry.elapsed_p50_ms, $entry.elapsed_p95_ms, $entry.first_token_p50_ms, $entry.first_token_p95_ms))
}
$markdownLines.Add('')
$markdownLines.Add('## 口径与限制')
$markdownLines.Add('')
$markdownLines.Add('- 只有带 `performance_task` 固定任务标签的 trace 才计入任务行；未标记 trace 只进入总分布，不推断任务归属。')
$markdownLines.Add('- Core trace writer 仅在 Daemon 以 allowlisted `OWO_PERF_TASK_ID` 启动时写标签；固定任务门仍需逐项受控采集至少 20 次真实任务。')
$markdownLines.Add('- 本报告只汇总 turn traces；Daemon/Desktop 冷启动、HTTP 首 token 到 UI 显示、审批提交恢复、SSE 队列内存曲线等需独立探针。')
$markdownLines.Add('- Trace 不含 Provider 单价，因此只列 token 数，不估算成本。')
$markdownLines.Add('')
$markdownLines.Add('## 原始 trace')
$markdownLines.Add('')
foreach ($file in $traces) { $markdownLines.Add(('- `{0}`' -f $file.FullName)) }

$json = ConvertTo-Json -InputObject $summary -Depth 6
if (-not $Out) {
    $logDir = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs\qa\logs'
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    $Out = Join-Path $logDir ("trace-perf-{0}.json" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
$outDirectory = Split-Path -Parent $Out
if ($outDirectory -and -not (Test-Path -LiteralPath $outDirectory)) {
    New-Item -ItemType Directory -Force -Path $outDirectory | Out-Null
}
[System.IO.File]::WriteAllText($Out, $json + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "[trace-perf] 报告=$Out"
if (-not $MarkdownOut) { $MarkdownOut = [IO.Path]::ChangeExtension($Out, '.md') }
$markdownDirectory = Split-Path -Parent $MarkdownOut
if ($markdownDirectory -and -not (Test-Path -LiteralPath $markdownDirectory)) {
    New-Item -ItemType Directory -Force -Path $markdownDirectory | Out-Null
}
[System.IO.File]::WriteAllText($MarkdownOut, ($markdownLines -join [Environment]::NewLine) + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "[trace-perf] Markdown=$MarkdownOut"

if ($coverageComplete) {
    Write-Host "[trace-perf] 固定任务覆盖门通过。" -ForegroundColor Green
    exit 0
}
if ($AllowUnclassifiedAggregate -and $sampleCount -ge $MinSamples) {
    Write-Host "[trace-perf] 探索性汇总通过；固定任务覆盖仍未通过。" -ForegroundColor Yellow
    exit 0
}
Write-Host "[trace-perf] 固定任务覆盖不足：总样本=$sampleCount，最低/任务=$MinSamples，未达标任务=$belowMinTaskCount；报告已落盘。" -ForegroundColor Red
exit 2
