# run-v1-acceptance.ps1 - V1-R1 formal acceptance runner (Lane 2: single-agent baseline
# and paired multi-agent comparison for the same frozen suite).
#
# Pipeline:
#   0) Env init: prefer scripts/init-dev-env.ps1 (Lane 1 unified entry) when present,
#      otherwise fall back to the internal ORT cache probe + user-level credential
#      passthrough (process scope only; never persisted).
#   1) preflight gate (exit 2 when blocked - no reference substitution).
#   2) freeze gate (formal mode): evals/v1/freeze.json must match the current suite
#      (task inputs / checkers / permissions / budgets / model / version hashes).
#   3) Formal batch (10 tasks x N reps, default 20):
#        - AgentMode::Single            -> exec "agent"      (real single Agent)
#        - AgentMode::Multi             -> exec "workswarm"  (real TeamRun, Lane 3 executor)
#      Batch isolation: each batch writes under <OutRoot>/batch-<id>; re-test after
#      any fix MUST create a NEW batch, old failure records are never overwritten.
#   4) Paired report (PairedStats JSON for Lane 3) + evidence copy under evals/v1/results.
#
# Dev smoke (-Smoke) is a separate lane: 3 tasks x 1 rep into smoke-<ts> dirs that are
# NEVER mixed into formal statistics.
#
# Usage:
#   pwsh -File agent-sdk\scripts\run-v1-acceptance.ps1 [-Smoke] [-EstimateOnly]
#        [-Batch <id>] [-Reps 20] [-Only <task-id>] [-TeamMode auto|team|single]
#        [-Model <id>] [-PriceInPerMTok $x] [-PriceOutPerMTok $x]
#        [-SkipSingle] [-SkipMulti] [-Suite <path>] [-OutRoot <dir>]
# Exit code: 0 = phases done, 1 = at least one run failed, 2 = blocked (preflight/freeze).

param(
    [string]$Suite,
    [string]$OutRoot = "scratch-eval-runs\v1-acceptance",
    [string]$Batch,
    [string]$Exe,
    [switch]$Smoke,
    [switch]$EstimateOnly,
    [int]$Reps = 20,
    [string]$Only,
    [string]$TeamMode = "auto",
    [string]$Model,
    [string]$StrategyVersion = "ten-3-default",
    [double]$PriceInPerMTok = 0,
    [double]$PriceOutPerMTok = 0,
    [switch]$SkipSingle,
    [switch]$SkipMulti
)

$ErrorActionPreference = "Continue"
$sdkRoot = Split-Path -Parent $PSScriptRoot
Set-Location $sdkRoot
Write-Host ("V1 acceptance - root: {0}  out: {1}" -f $sdkRoot, $OutRoot) -ForegroundColor Cyan

# 预构建二进制复用：并行路线的 WIP 编辑可能让 cargo run 在启动瞬间碰到中段编译错误；
# 传入 -Exe <path> 后全部 CLI 调用走固定二进制，批量全程不受并行编辑影响（版本 = 冻结时刻）。
# 注意：函数只执行不返回值（原生 stdout 会污染返回值）；退出码一律由调用方读 $LASTEXITCODE。
function Invoke-EvalCli {
    param([string]$Verb, [string[]]$RestArgs)
    $cli = @("product-eval", $Verb) + @($RestArgs)
    if ($Exe) {
        & $Exe @cli
    } else {
        cargo run -q -p owo-agent-cli -- @cli
    }
}

# ---------------------------------------------------------------------------
# 0) Env init: prefer Lane 1 unified entry; fallback internal probe + credentials
# ---------------------------------------------------------------------------
$initScript = Join-Path $PSScriptRoot "init-dev-env.ps1"
if (Test-Path $initScript) {
    Write-Host "[init] dot-sourcing scripts/init-dev-env.ps1 (Lane 1 unified init)" -ForegroundColor Cyan
    . $initScript
}
# Internal ORT probe (process scope only; no persistence).
if (-not $env:ORT_LIB_PATH) {
    $probeRoot = Join-Path $sdkRoot "target\sherpa-onnx-prebuilt"
    $hit = $null
    if (Test-Path $probeRoot) {
        $hit = Get-ChildItem -Path $probeRoot -Recurse -Filter "onnxruntime.lib" -ErrorAction SilentlyContinue |
            Select-Object -First 1
    }
    if ($null -ne $hit) {
        $env:ORT_LIB_PATH = $hit.DirectoryName
    }
}
if (-not $env:SHERPA_ONNX_LIB_DIR) {
    $probeRoot = Join-Path $sdkRoot "target\sherpa-onnx-prebuilt"
    if (Test-Path $probeRoot) {
        $libHit = Get-ChildItem -Path $probeRoot -Recurse -Filter "sherpa-onnx-c-api.lib" -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($libHit) { $env:SHERPA_ONNX_LIB_DIR = $libHit.DirectoryName }
    }
}
# Credential passthrough: fresh shells do not inherit user-level registry env.
if (-not $env:OPENAI_API_KEY) {
    $userKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if ($userKey) { $env:OPENAI_API_KEY = $userKey }
}
if ($PriceInPerMTok -gt 0) { $env:OWO_EVAL_PRICE_IN_PER_MTOK = "$PriceInPerMTok" }
if ($PriceOutPerMTok -gt 0) { $env:OWO_EVAL_PRICE_OUT_PER_MTOK = "$PriceOutPerMTok" }

$suiteArgs = @()
if ($Suite) { $suiteArgs += @("--suite", $Suite) }
$modelArgs = @()
if ($Model) { $modelArgs = @("--model", $Model) }

$script:failedRuns = 0
$script:skippedCount = 0

function Invoke-EvalRun {
    param([string]$Label, [string[]]$RunArgs)
    Write-Host ""
    Write-Host ("== [acceptance] {0} ==" -f $Label) -ForegroundColor Cyan
    $allArgs = @($suiteArgs) + @($modelArgs) + @($RunArgs)
    Invoke-EvalCli "run" $allArgs
    $rc = $LASTEXITCODE
    if ($rc -ne 0) {
        Write-Host ("    run FAILED (exit={0}): {1}" -f $rc, $Label) -ForegroundColor Red
        $script:failedRuns++
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# 1) preflight gate - blocked means NO live run and NO reference substitution
# ---------------------------------------------------------------------------
Write-Host "== [acceptance] preflight gate ==" -ForegroundColor Cyan
$preflightOut = Join-Path $sdkRoot $OutRoot
$preArgs = @($suiteArgs) + @("--out", $preflightOut)
Invoke-EvalCli "preflight" $preArgs
$preRc = $LASTEXITCODE
if ($preRc -ne 0) {
    Write-Host "PREFLIGHT BLOCKED (exit=$preRc): acceptance NOT executed." -ForegroundColor Red
    exit 2
}

# ---------------------------------------------------------------------------
# 2) freeze gate (formal only) - suite must match evals/v1/freeze.json
# ---------------------------------------------------------------------------
if (-not $Smoke -and -not $EstimateOnly) {
    $freezePath = Join-Path $sdkRoot "evals\v1\freeze.json"
    if (-not (Test-Path $freezePath)) {
        Write-Host "FREEZE GATE: evals/v1/freeze.json missing - run 'product-eval freeze' first." -ForegroundColor Red
        exit 2
    }
    Write-Host "== [acceptance] freeze gate ==" -ForegroundColor Cyan
    $frArgs = @($suiteArgs) + @("--freeze", $freezePath)
    Invoke-EvalCli "validate" $frArgs
    $frRc = $LASTEXITCODE
    if ($frRc -ne 0) {
        Write-Host "FREEZE GATE BLOCKED: suite drifted from freeze.json. Regenerate freeze (new batch) or fix drift." -ForegroundColor Red
        exit 2
    }
}

# ---------------------------------------------------------------------------
# Smoke / estimate mode: separate dirs, never mixed into formal statistics
# ---------------------------------------------------------------------------
$smokeTasks = @("code-bug-fix", "research-technical-brief", "document-revise")

if ($Smoke) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $smokeRoot = Join-Path $OutRoot "smoke-$stamp"
    Write-Host "== [smoke] development smoke (separate dirs; not counted in stats) ==" -ForegroundColor Cyan
    foreach ($task in $smokeTasks) {
        Invoke-EvalRun ("agent single x1 - {0}" -f $task) @(
            "--exec", "agent", "--agents", "single", "--reps", "1",
            "--only", $task, "--out", (Join-Path $smokeRoot "agent-single")
        ) | Out-Null
    }
    if ($script:failedRuns -gt 0) {
        Write-Host ("SMOKE: {0} run(s) FAILED" -f $script:failedRuns) -ForegroundColor Red
        exit 1
    }
    Write-Host ("SMOKE: DONE (out: {0})" -f $smokeRoot) -ForegroundColor Green
    exit 0
}

if ($EstimateOnly) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $estRoot = Join-Path $OutRoot "estimate-$stamp"
    $calibOut = Join-Path $estRoot "calib"
    Write-Host "== [estimate] calibration: 3 tasks x 2 reps single-agent (real model) ==" -ForegroundColor Cyan
    foreach ($task in $smokeTasks) {
        Invoke-EvalRun ("calibration agent single x2 - {0}" -f $task) @(
            "--exec", "agent", "--agents", "single", "--reps", "2",
            "--only", $task, "--out", $calibOut
        ) | Out-Null
    }
    $report = Join-Path $calibOut "report.json"
    if (Test-Path $report) {
        $data = Get-Content $report -Raw | ConvertFrom-Json
        $total = $data.metrics.runs_total
        $calls = $data.metrics.total_model_calls
        $tokens = $data.metrics.total_tokens
        $meanWs = $data.metrics.mean_wall_ms
        Write-Host ""
        Write-Host "=== CALIBRATION (n=$total, tokens=$tokens, calls=$calls, mean_wall_ms=$meanWs) ===" -ForegroundColor Cyan
        $perRun = if ($total -gt 0) { $calls / $total } else { 0 }
        $calibCells = 6.0            # 3 tasks x 2 reps
        $allTasksScale = 200.0 / [Math]::Max(1.0, $calibCells)
        $projCalls = [Math]::Ceiling($calls * $allTasksScale)
        $projTokens = if ($tokens) { [Math]::Ceiling($tokens * $allTasksScale) } else { $null }
        $estInput = $null
        $estOutput = $null
        if ($projTokens -and $env:OWO_EVAL_PRICE_IN_PER_MTOK -and $env:OWO_EVAL_PRICE_OUT_PER_MTOK) {
            $inMtok = $env:OWO_EVAL_PRICE_IN_PER_MTOK
            $outMtok = $env:OWO_EVAL_PRICE_OUT_PER_MTOK
            $estInput = $projTokens * 0.8 * $inMtok / 1e6
            $estOutput = $projTokens * 0.2 * $outMtok / 1e6
        }
        Write-Host ("Projected single-agent formal batch (200 cells): calls ~{0}  tokens ~{1}  (per-run calls {2:N1})" -f $projCalls, $projTokens, $perRun) -ForegroundColor Yellow
        if ($estInput -and $estOutput) {
            Write-Host ("Est. cost (single 200): input {0:N4} + output {1:N4} = {2:N4} (按 OWO_EVAL_PRICE_* 单价 0.4/0.8 元/MTok 计)" -f $estInput, $estOutput, ($estInput + $estOutput)) -ForegroundColor Yellow
        } else {
            Write-Host "Est. cost: set -PriceInPerMTok/-PriceOutPerMTok to get USD estimate (tokens are recorded regardless)." -ForegroundColor Yellow
        }
        Write-Host ("Concurrency cap: 1 (sequential cells; shared journal per out dir).") -ForegroundColor Yellow
        Write-Host ("Multi-agent side (workswarm 200 cells) will use its own worker budget; calibrate separately with -SkipSingle after approve.") -ForegroundColor Yellow
    } else {
        Write-Host "CALIBRATION FAILED: report.json missing." -ForegroundColor Red
        exit 1
    }
    if ($script:failedRuns -gt 0) { exit 1 }
    exit 0
}

# ---------------------------------------------------------------------------
# 3) formal batch: batch isolation + journal resume
# ---------------------------------------------------------------------------
if (-not $Batch) {
    $Batch = "accept-" + (Get-Date -Format "yyyyMMdd-HHmmss")
}
$batchRoot = Join-Path $OutRoot $Batch
$singleOut = Join-Path $batchRoot "agent-single"
$multiOut  = Join-Path $batchRoot "workswarm-multi"
Write-Host ("== [formal] batch {0} (reps {1}, team_mode {2}, strategy_version {3}) ==" -f $Batch, $Reps, $TeamMode, $StrategyVersion) -ForegroundColor Cyan

if (-not $SkipSingle) {
    Invoke-EvalRun "agent single - ALL tasks x $Reps reps" @(
        "--exec", "agent", "--agents", "single", "--reps", "$Reps",
        "--label", $Batch, "--tag", "acceptance", "--tag", "batch=$Batch",
        "--out", $singleOut
    ) | Out-Null
}
if (-not $SkipMulti) {
    Invoke-EvalRun "workswarm multi - ALL tasks x $Reps reps (TeamRun)" @(
        "--exec", "workswarm", "--agents", "multi", "--reps", "$Reps", "--team-mode", $TeamMode,
        "--label", $Batch, "--tag", "acceptance", "--tag", "batch=$Batch",
        "--out", $multiOut
    ) | Out-Null
}

# ---------------------------------------------------------------------------
# 4) paired report (Lane 3 input) + evidence copy
# ---------------------------------------------------------------------------
$singleReport = Join-Path $singleOut "report.json"
$multiReport  = Join-Path $multiOut "report.json"
$pairedOut    = Join-Path $batchRoot "paired.json"
if ((Test-Path $singleReport) -and (Test-Path $multiReport) -and -not $SkipSingle -and -not $SkipMulti) {
    Write-Host "== [formal] generate paired report ==" -ForegroundColor Cyan
    Invoke-EvalCli "paired" @($singleReport, $multiReport, "--out", $pairedOut, "--strategy-version", $StrategyVersion)
    if ($LASTEXITCODE -ne 0) {
        Write-Host "PAIRED REPORT generation FAILED." -ForegroundColor Red
        $script:failedRuns++
    }
}

# Evidence copy (checked-in location for Lane 3 and the final report).
$resultsDir = Join-Path $sdkRoot "evals\v1\results\$Batch"
New-Item -ItemType Directory -Force -Path $resultsDir | Out-Null
foreach ($src in @($singleReport, $multiReport, $pairedOut)) {
    if (Test-Path $src) {
        Copy-Item $src (Join-Path $resultsDir (Split-Path $src -Leaf)) -Force
    }
}
$gitCommit = git -C $sdkRoot rev-parse HEAD 2>$null
$gitDirty = (git -C $sdkRoot status --porcelain 2>$null | Measure-Object).Count -gt 0
$info = @{
    batch = $Batch
    generated_at = (Get-Date).ToString("o")
    model = $Model
    strategy_version = $StrategyVersion
    team_mode = $TeamMode
    reps = $Reps
    git_commit = ($gitCommit -join "").Trim()
    git_dirty = $gitDirty
    suite = "v1-r1-product-suite"
    concurrency_cap = 1
    note = "paired reports feed Lane 3 benefit gate (PairedStats contract)."
}
($info | ConvertTo-Json) | Set-Content (Join-Path $resultsDir "batch-info.json") -Encoding UTF8

Write-Host ""
Write-Host "==================== V1 ACCEPTANCE SUMMARY ====================" -ForegroundColor Cyan
Write-Host ("batch      : {0}" -f $Batch)
Write-Host ("single out : {0}" -f $singleOut)
Write-Host ("multi  out : {0}" -f $multiOut)
if (Test-Path $pairedOut) { Write-Host ("paired     : {0}" -f $pairedOut) }
Write-Host ("evidence   : {0}" -f $resultsDir)
if (Test-Path $singleReport) {
    Write-Host "---- single-agent ----"
    Invoke-EvalCli "compare" @($singleReport, $singleReport)
    $data = Get-Content $singleReport -Raw | ConvertFrom-Json
    Write-Host ("    success {0}/{1} ({2}%) tokens={3} cost={4}" -f `
        $data.metrics.passed, $data.metrics.runs_total, [Math]::Round($data.metrics.success_rate * 100, 1), `
        $data.metrics.total_tokens, $data.metrics.total_cost_usd)
}
if ($script:failedRuns -gt 0) {
    Write-Host ("V1 ACCEPTANCE: {0} run(s) FAILED" -f $script:failedRuns) -ForegroundColor Red
    exit 1
}
Write-Host "V1 ACCEPTANCE: DONE" -ForegroundColor Green
exit 0