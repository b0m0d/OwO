# render-v1-acceptance-report.ps1 - Render the V1 acceptance markdown report from a batch
# directory produced by run-v1-acceptance.ps1 (agent-single/report.json [+ workswarm-multi/report.json]).
#
# Output (default): <BatchDir>\acceptance-report.md
#   - header/provenance (suite, model, execution, batch label, freeze suite_hash)
#   - per-case table: runs / passed / success rate / Wilson 95% CI / mean wall / mean calls /
#     total tokens / quality (checker pass rate)
#   - per-category + overall rollups for each agent mode
#   - failure manifest: every non-passed run with mode, cell, status, failed step (failure
#     position), model calls, wall, tokens, artifact refs, archived sandbox rel path
#   - paired single-vs-multi comparison table (when both reports exist)
#
# Usage:
#   pwsh -File agent-sdk\scripts\render-v1-acceptance-report.ps1 -BatchDir <dir> [-Out <path>]

param(
    [Parameter(Mandatory = $true)][string]$BatchDir,
    [string]$Out
)

$ErrorActionPreference = "Stop"
$singleJson = Join-Path $BatchDir "agent-single\report.json"
$multiJson  = Join-Path $BatchDir "workswarm-multi\report.json"

function Wilson-CI {
    param([int]$Passed, [int]$Total)
    if ($Total -eq 0) { return @(0.0, 0.0) }
    $z = 1.959963985 # 95%
    $p = $Passed / $Total
    $den = 1 + [Math]::Pow($z, 2) / $Total
    $center = $p + [Math]::Pow($z, 2) / (2 * $Total)
    $margin = $z * [Math]::Sqrt(($p * (1 - $p) + [Math]::Pow($z, 2) / (4 * $Total)) / $Total)
    return @([Math]::Max(0.0, ($center - $margin) / $den), [Math]::Min(1.0, ($center + $margin) / $den))
}

function Load-Report {
    param([string]$Path, [string]$Label)
    if (-not (Test-Path $Path)) { return $null }
    $r = Get-Content $Path -Raw | ConvertFrom-Json
    Write-Host ("[load] {0}: runs={1} passed={2} failed={3} error={4} timeout={5} cancel={6}" -f `
        $Label, $r.metrics.runs_total, $r.metrics.passed, $r.metrics.failed, $r.metrics.error, `
        $r.metrics.timed_out, $r.metrics.cancelled)
    return $r
}

$single = Load-Report $singleJson "single"
$multi  = Load-Report $multiJson "multi"
if (-not $single -and -not $multi) {
    Write-Host "NO REPORTS found under $BatchDir (agent-single/report.json or workswarm-multi/report.json)." -ForegroundColor Red
    exit 2
}

$sb = New-Object System.Text.StringBuilder
function H1($t)   { [void]$sb.AppendLine("`n# $t`n") }
function H2($t)   { [void]$sb.AppendLine("`n## $t`n") }
function H3($t)   { [void]$sb.AppendLine("`n### $t`n") }
function Row($t)  { [void]$sb.AppendLine($t) }

H1 "V1-R1 产品评测验收报告"
foreach ($tag in @("single", "multi")) {
    $r = if ($tag -eq "single") { $single } else { $multi }
    if (-not $r) { continue }
    $mode = $tag
    H2 "Agent 模式：$mode"
    Row ("| 项 | 值 |")
    Row ("| --- | --- |")
    Row ("| 批次标签 | {0} |" -f $r.batch_label)
    Row ("| 标签 | {0} |" -f ($r.tags -join ", "))
    Row ("| 套件 | {0}（修订 hash 前 12 位 `{1}`） |" -f $r.suite_name, $r.suite_hash.Substring(0, [Math]::Min(12, $r.suite_hash.Length)))
    Row ("| 执行面 | {0} |" -f $r.execution)
    Row ("| 模型 | {0} |" -f $r.model)
    Row ("| 生成时间 | {0} |" -f $r.generated_at)
    $ci = Wilson-CI $r.metrics.passed $r.metrics.runs_total
    Row ("| 总成功率 | {0}/{1} = {2:P1}（Wilson 95% CI [{3:P1}, {4:P1}]） |" -f $r.metrics.passed, $r.metrics.runs_total, `
        $r.metrics.success_rate, $ci[0], $ci[1])
    Row ("| 总调用量 | {0}（均值 {1:N1}/run） |" -f $r.metrics.total_model_calls, ($r.metrics.total_model_calls / $r.metrics.runs_total))
    Row ("| 总 tokens | {0} |" -f $r.metrics.total_tokens)
    Row ("| 费用 | {0}（按 OWO_EVAL_PRICE_* 单价；null=未计费，tokens 仍真实） |" -f $r.metrics.estimated_cost_usd)
    Row ("| 平均墙钟 | {0:N0} ms |" -f $r.metrics.mean_wall_ms)
    Row ("| 失败分类 | failed={0} error={1} timeout={2} cancelled={3} |" -f $r.metrics.failed, $r.metrics.error, `
        $r.metrics.timed_out, $r.metrics.cancelled)
    Row ("| 未执行（pending） | {0} |" -f $r.pending.Count)

    $runsPerCase = if ($r.per_case.Count -gt 0) { $r.metrics.runs_total / $r.per_case.Count } else { 0 }
    H3 ("逐任务（每任务 {0} runs）" -f $runsPerCase)
    Row ("| 任务 | 类别 | 通过/总数 | 成功率 | CI95 低 | CI95 高 | 均值墙钟 ms | 均值调用 | tokens | 质量分 |")
    Row ("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    foreach ($c in ($r.per_case | Sort-Object case_id)) {
        $cci = Wilson-CI $c.passed $c.runs_total
        Row ("| {0} | {1} | {2}/{3} | {4:P0} | {5:P1} | {6:P1} | {7:N0} | {8:N1} | {9} | {10} |" -f `
            $c.case_id, $c.category, $c.passed, $c.runs_total, $c.success_rate, $cci[0], $cci[1], `
            $c.mean_wall_ms, $c.mean_model_calls, $c.total_tokens, `
            $(if ($null -eq $c.quality) { "-" } else { "{0:P1}" -f $c.quality }))
    }

    # 失败清单（含失败位置/Artifact/失败沙盒）
    $bad = @($r.runs | Where-Object { $_.status -ne "passed" })
    if ($bad.Count -gt 0) {
        H3 "失败清单（{0} 条；修复后重测=新批次，历史失败不重算不覆盖）" -f $bad.Count
        Row ("| 单元格 | 状态 | 失败位置(failed_step) | 调用量 | 墙钟 ms | tokens | Artifact | 失败沙盒 |")
        Row ("| --- | --- | --- | --- | --- | --- | --- | --- |")
        foreach ($f in $bad) {
            Row ("| {0} | {1} | {2} | {3} | {4:N0} | {5} | {6} | {7} |" -f `
                $f.cell, $f.status, $f.failed_step, $f.model_calls, $f.wall_ms, $f.total_tokens, `
                ($f.artifact_refs -join ";"), $f.sandbox_rel)
        }
    } else {
        Row ("`n_no failures in mode {0}_" -f $mode)
    }
    Row ""
}

# 配对对照（single vs multi 同口径）
if ($single -and $multi) {
    H2 "配对对照（PairedStats：供三路收益策略消费；bs=bindings 四元组）"
    Row ("| 任务组 | single 成功率 | multi 成功率 | 差值pp | multi 更优 | single wall ms | multi wall ms | single 质量 | multi 质量 |")
    Row ("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    $smap = @{}; foreach ($c in $single.per_case) { $smap[$c.case_id] = $c }
    foreach ($m in ($multi.per_case | Sort-Object case_id)) {
        $s = $smap[$m.case_id]
        if (-not $s) { continue }
        $diff = ($m.success_rate - $s.success_rate) * 100
        $better = if ($diff -ge 5) { "是(+$([Math]::Round($diff,1))pp)" } else { "否" }
        Row ("| {0} | {1:P0} | {2:P0} | {3:+0.0;-0.0}pp | {4} | {5:N0} | {6:N0} | {7} | {8} |" -f `
            $m.case_id, $s.success_rate, $m.success_rate, $diff, $better, `
            $s.mean_wall_ms, $m.mean_wall_ms, `
            $(if ($null -eq $s.quality) { "-" } else { "{0:P1}" -f $s.quality }), `
            $(if ($null -eq $m.quality) { "-" } else { "{0:P1}" -f $m.quality }))
    }
    Row ""
    Row "说明：多 Agent 启用判定（success_rate_pp≥5 或 quality_pct≥10 或 wall_rel_save≤-30%，且样本充分 ≥30）由三路 team_benefit 依据 paired.json 裁决，本表仅呈现证据。"
}

if (-not $Out) { $Out = Join-Path $BatchDir "acceptance-report.md" }
[System.IO.File]::WriteAllText($Out, $sb.ToString(), [System.Text.UTF8Encoding]::new($false))
Write-Host ("REPORT WRITTEN: {0}" -f $Out) -ForegroundColor Green
exit 0