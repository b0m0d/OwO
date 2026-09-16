# run-product-eval-live.ps1 - R1 live baseline runner (Lane 1, four-lane round).
#
# Execution order (per round plan):
#   0) preflight gate - Provider/credential/suite/out-dir/ORT checks.
#      Credentials missing => exit 2 with a blocked report. Reference (dry)
#      results are NEVER used as a substitute for live runs.
#   1) Phase A: real single-agent runs, one task per category (3 runs).
#   2) Phase B: real WorkSwarm TeamRun on the same 3 tasks (3 runs).
#   3) Phase C (-Full): expand to all 10 tasks, 3 repetitions each for the
#      single-agent engine; WorkSwarm expands to 10 tasks x 1 rep
#      (-FullWorkswarm raises WorkSwarm to 3 reps too).
#
# Usage (from anywhere):
#   pwsh -File agent-sdk\scripts\run-product-eval-live.ps1 [-Full] [-FullWorkswarm]
#        [-Suite path] [-OutRoot relative-or-absolute] [-Model id]
#        [-PriceInPerMTok 0.0] [-PriceOutPerMTok 0.0]
# Exit code: 0 = phases done, 1 = at least one run failed, 2 = preflight blocked.
#
# NOTE: ASCII-only on purpose (same reason as check-v1-r0.ps1).

param(
    [string]$Suite,
    [string]$OutRoot = "scratch-eval-runs\live-baseline",
    [switch]$Full,
    [switch]$FullWorkswarm,
    [string]$Model,
    [double]$PriceInPerMTok = 0,
    [double]$PriceOutPerMTok = 0
)

$ErrorActionPreference = "Continue"
$sdkRoot = Split-Path -Parent $PSScriptRoot
Set-Location $sdkRoot
Write-Host ("R1 live baseline - root: {0}  out: {1}" -f $sdkRoot, $OutRoot) -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# Env init: prefer Lane 1 unified entry (scripts/init-dev-env.ps1); fallback to
# the internal ORT auto-probe (process env only, no persist).
# ---------------------------------------------------------------------------
$initScript = Join-Path $PSScriptRoot "init-dev-env.ps1"
if (Test-Path $initScript) {
    Write-Host "[init] dot-sourcing scripts/init-dev-env.ps1 (Lane 1 unified init)" -ForegroundColor Cyan
    . $initScript
}
# ORT probe via the unified single implementation (audit 6.2; process scope only).
if (-not $env:ORT_LIB_PATH) {
    $probed = Get-OwoOrtLibDir
    if ($probed) {
        $env:ORT_LIB_PATH = $probed
        Write-Host ("[ORT] ORT_LIB_PATH auto-probed (process only): {0}" -f $probed) -ForegroundColor Cyan
    }
}

# ---------------------------------------------------------------------------
# Credential passthrough: fresh shells do not inherit user-level registry env,
# so read it into the process scope when missing (value is never printed).
# ---------------------------------------------------------------------------
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

function Invoke-EvalRun {
    param([string]$Label, [string[]]$RunArgs)
    Write-Host ""
    Write-Host ("== [live] {0} ==" -f $Label) -ForegroundColor Cyan
    cargo run -q -p owo-agent-cli -- product-eval run @suiteArgs @modelArgs @RunArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host ("    run FAILED (exit={0}): {1}" -f $LASTEXITCODE, $Label) -ForegroundColor Red
        $script:failedRuns++
        return $false
    }
    return $true
}

# ---------------------------------------------------------------------------
# 0) preflight gate - blocked means NO live run and NO reference substitution
# ---------------------------------------------------------------------------
Write-Host "== [live] preflight gate ==" -ForegroundColor Cyan
$preflightOut = Join-Path $sdkRoot $OutRoot
cargo run -q -p owo-agent-cli -- product-eval preflight @suiteArgs --out $preflightOut
if ($LASTEXITCODE -ne 0) {
    Write-Host "" 
    Write-Host ("PREFLIGHT BLOCKED (exit={0}): live baseline NOT executed." -f $LASTEXITCODE) -ForegroundColor Red
    Write-Host "Reference (dry) results are NOT a substitute for live runs - fix the blocking items above and re-run." -ForegroundColor Red
    exit 2
}

# ---------------------------------------------------------------------------
# Phase A: real single-agent, one task per category, 1 repetition each
# ---------------------------------------------------------------------------
$smokeTasks = @("code-bug-fix", "research-technical-brief", "document-revise")
foreach ($task in $smokeTasks) {
    Invoke-EvalRun ("agent single x1 - {0}" -f $task) @(
        "--exec", "agent", "--agents", "single", "--reps", "1",
        "--only", $task, "--out", (Join-Path $OutRoot "agent-single")
    ) | Out-Null
}

# ---------------------------------------------------------------------------
# Phase B: real WorkSwarm TeamRun on the same 3 tasks, 1 repetition each
# ---------------------------------------------------------------------------
foreach ($task in $smokeTasks) {
    Invoke-EvalRun ("workswarm single x1 - {0}" -f $task) @(
        "--exec", "workswarm", "--agents", "single", "--reps", "1",
        "--only", $task, "--out", (Join-Path $OutRoot "workswarm-single")
    ) | Out-Null
}

# ---------------------------------------------------------------------------
# Phase C: expansion to all 10 tasks (journal resume: finished cells skipped)
# ---------------------------------------------------------------------------
if ($Full) {
    $liveLabel = "live-" + (Get-Date -Format "yyyyMMdd-HHmmss")
    Invoke-EvalRun "agent single - ALL 10 tasks x 3 reps" @(
        "--exec", "agent", "--agents", "single", "--reps", "3",
        "--label", $liveLabel, "--tag", "live-baseline",
        "--out", (Join-Path $OutRoot "agent-single")
    ) | Out-Null
    $wsReps = if ($FullWorkswarm) { "3" } else { "1" }
    Invoke-EvalRun ("workswarm single - ALL 10 tasks x {0} rep(s)" -f $wsReps) @(
        "--exec", "workswarm", "--agents", "single", "--reps", $wsReps,
        "--team-mode", "auto",
        "--label", $liveLabel, "--tag", "live-baseline",
        "--out", (Join-Path $OutRoot "workswarm-single")
    ) | Out-Null
}

# ---------------------------------------------------------------------------
# Comparison: single agent vs WorkSwarm (same suite), plus pointers
# ---------------------------------------------------------------------------
$agentReport = Join-Path $sdkRoot (Join-Path $OutRoot "agent-single\report.json")
$wsReport = Join-Path $sdkRoot (Join-Path $OutRoot "workswarm-single\report.json")
Write-Host ""
Write-Host "==================== R1 LIVE BASELINE SUMMARY ====================" -ForegroundColor Cyan
if ((Test-Path $agentReport) -and (Test-Path $wsReport)) {
    cargo run -q -p owo-agent-cli -- product-eval compare $agentReport $wsReport
}
Write-Host ("single-agent journal : {0}" -f $agentReport.Replace("report.json", "state.jsonl"))
Write-Host ("workswarm journal    : {0}" -f $wsReport.Replace("report.json", "state.jsonl"))
if ($script:failedRuns -gt 0) {
    Write-Host ("R1 LIVE BASELINE: {0} run(s) FAILED" -f $script:failedRuns) -ForegroundColor Red
    exit 1
}
Write-Host "R1 LIVE BASELINE: DONE" -ForegroundColor Green
exit 0
