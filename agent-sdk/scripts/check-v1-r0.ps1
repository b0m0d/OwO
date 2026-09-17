# check-v1-r0.ps1 - One-click V1-R0 closeout gate (Lane 1, four-lane closeout day).
#
# Runs, in order: cargo fmt check -> core/server/cli cargo check ->
# targeted server/cli/core tests -> TS typecheck/build/unit tests -> desktop web checks.
# Recovery test suites are picked up automatically once Lane 3 lands the files.
#
# Usage (from anywhere):
#   pwsh -File agent-sdk\scripts\check-v1-r0.ps1 [-SkipCargo] [-SkipTs] [-SkipWeb]
# Exit code: 0 = all green, 1 = at least one step failed.
#
# NOTE: this file is intentionally ASCII-only so it behaves identically under
# Windows PowerShell 5.1 (ANSI codepage) and PowerShell 7+ (UTF-8).

param(
    [switch]$SkipCargo,
    [switch]$SkipTs,
    [switch]$SkipWeb
)

$ErrorActionPreference = "Continue"
$sdkRoot = Split-Path -Parent $PSScriptRoot
Set-Location $sdkRoot
Write-Host ("R0 closeout gate - root: {0}" -f $sdkRoot) -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# §7.2 ORT 统一解析入口（resolve-ort.ps1 → init-dev-env 单一实现）：
# 解析成功则进程级注入 SHERPA_ONNX_LIB_DIR/ORT_LIB_PATH/ORT_LIB_LOCATION
# 三个消费面（历史上只设 ORT_LIB_PATH 属不完整注入）；失败保持宽容
# （本脚本部分检查不需要链接），只给出可操作指引。
# ---------------------------------------------------------------------------
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
try {
    $ortMeta = Resolve-OwoOrtEnv -NoDownload -Quiet
    Write-Host ("[ORT] resolved (process only): {0} source={1} v{2}" -f $ortMeta.lib_dir, $ortMeta.source, $ortMeta.version) -ForegroundColor Cyan
}
catch {
    Write-Host "[ORT] 未解析（run: pwsh -File scripts\resolve-ort.ps1 -EnsureOrt）- 依赖链接的检查可能失败：$($_.Exception.Message)" -ForegroundColor Yellow
}

$script:results = New-Object System.Collections.Generic.List[object]
$script:failedSteps = 0

function Invoke-Step {
    param([string]$Name, [scriptblock]$Action)
    Write-Host ""
    Write-Host ("==> [R0] {0}" -f $Name) -ForegroundColor Cyan
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $out = & $Action 2>&1
    $exit = $LASTEXITCODE
    $sw.Stop()
    $ok = ($exit -eq 0)
    if (-not $ok) { $script:failedSteps++ }
    $tail = ($out | Select-Object -Last 4) -join " | "
    if ($tail.Length -gt 220) { $tail = $tail.Substring($tail.Length - 220) }
    $script:results.Add([pscustomobject]@{
        Step    = $Name
        Exit    = $exit
        Seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
        Ok      = $ok
        Tail    = $tail
    })
    $color = if ($ok) { "Green" } else { "Red" }
    Write-Host ("    exit={0}  {1}s" -f $exit, [math]::Round($sw.Elapsed.TotalSeconds, 1)) -ForegroundColor $color
}

# ---------------------------------------------------------------------------
# Cargo gates
# ---------------------------------------------------------------------------
if (-not $SkipCargo) {
    Invoke-Step "cargo fmt --all -- --check" { cargo fmt --all -- --check }

    Invoke-Step "cargo check core/server/cli" {
        cargo check -p owo-agent-core -p owo-agent-server -p owo-agent-cli
    }

    $serverSuites = @(
        "--test", "route_contract_tests",
        "--test", "desktop_world_api_tests",
        "--test", "workswarm_api_tests",
        "--test", "goal_api_tests"
    )
    # Lane 3 (R2) recovery suites are included as soon as they exist.
    if (Test-Path "crates/owo-agent-server/tests/workswarm_recovery_api_tests.rs") {
        $serverSuites += @("--test", "workswarm_recovery_api_tests")
    }
    # Lane 4 (V1 三日) ProductEval API suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-server/tests/product_eval_api_tests.rs") {
        $serverSuites += @("--test", "product_eval_api_tests")
    }
    # V1 四期 (Lane 3) Artifact review API suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-server/tests/artifact_review_api_tests.rs") {
        $serverSuites += @("--test", "artifact_review_api_tests")
    }
    # V1 四期 (Lane 2) WorkSwarm progress SSE API suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-server/tests/workswarm_progress_api_tests.rs") {
        $serverSuites += @("--test", "workswarm_progress_api_tests")
    }
    Invoke-Step "cargo test server targeted suites" {
        cargo test -p owo-agent-server @serverSuites
    }

    Invoke-Step "cargo test cli worker_child_tests" {
        cargo test -p owo-agent-cli --test worker_child_tests
    }

    Invoke-Step "cargo test core workswarm_tests" {
        cargo test -p owo-agent-core --test workswarm_tests
    }

    if (Test-Path "crates/owo-agent-core/tests/workswarm_recovery_tests.rs") {
        Invoke-Step "cargo test core workswarm_recovery_tests" {
            cargo test -p owo-agent-core --test workswarm_recovery_tests
        }
    }

    # V1 三日 ProductEval executor suites (Lane 1 single-agent / Lane 2 workswarm adapter).
    if (Test-Path "crates/owo-agent-core/tests/product_eval_single_agent_tests.rs") {
        Invoke-Step "cargo test core product_eval_single_agent_tests" {
            cargo test -p owo-agent-core --test product_eval_single_agent_tests
        }
    }
    if (Test-Path "crates/owo-agent-core/tests/product_eval_workswarm_tests.rs") {
        Invoke-Step "cargo test core product_eval_workswarm_tests" {
            cargo test -p owo-agent-core --test product_eval_workswarm_tests
        }
    }
    # R1 statistics suite (Lane 1): Wilson CI / percentiles / enablement edges.
    if (Test-Path "crates/owo-agent-core/tests/product_eval_statistics_tests.rs") {
        Invoke-Step "cargo test core product_eval_statistics_tests" {
            cargo test -p owo-agent-core --test product_eval_statistics_tests
        }
    }
    # V1 四期 (Lane 3) artifact review store/core suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-core/tests/artifact_review_tests.rs") {
        Invoke-Step "cargo test core artifact_review_tests" {
            cargo test -p owo-agent-core --test artifact_review_tests
        }
    }
    # V1 四期 (Lane 2) long-task responsiveness suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-core/tests/workswarm_responsiveness_tests.rs") {
        Invoke-Step "cargo test core workswarm_responsiveness_tests" {
            cargo test -p owo-agent-core --test workswarm_responsiveness_tests
        }
    }
}

# ---------------------------------------------------------------------------
# TypeScript gates
# ---------------------------------------------------------------------------
if (-not $SkipTs) {
    Push-Location "clients/ts"
    try {
        Invoke-Step "ts typecheck" { npm run typecheck }
        Invoke-Step "ts build" { npm run build }
        Invoke-Step "ts unit tests" { npm run test:unit }
    }
    finally {
        Pop-Location
    }
}

# ---------------------------------------------------------------------------
# Desktop web gates (syntax check all JS, run node tests when present)
# ---------------------------------------------------------------------------
if (-not $SkipWeb) {
    Invoke-Step "web node --check (app + panels)" {
        $bad = 0
        $files = @(Get-ChildItem "desktop/web/app.js" -File)
        $files += @(Get-ChildItem "desktop/web/panels" -Filter "*.js" -File -ErrorAction SilentlyContinue)
        foreach ($f in $files) {
            node --check $f.FullName
            if ($LASTEXITCODE -ne 0) { $bad++ }
        }
        Write-Host ("    checked {0} file(s), {1} failed" -f $files.Count, $bad)
        $global:LASTEXITCODE = $bad
    }

    if (Test-Path "desktop/web/tests") {
        Invoke-Step "web node --test tests/*.test.mjs" {
            node --test "desktop/web/tests/*.test.mjs"
        }
    }
    else {
        Write-Host ""
        Write-Host "==  [R0] web tests dir not present yet (Lane 4 pending) - skipped" -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "==================== R0 GATE SUMMARY ====================" -ForegroundColor Cyan
$script:results | Format-Table Step, Exit, Seconds, Ok -AutoSize | Out-Host
foreach ($r in $script:results) {
    if (-not $r.Ok) {
        Write-Host ("FAIL {0} (exit={1}) tail: {2}" -f $r.Step, $r.Exit, $r.Tail) -ForegroundColor Red
    }
}
$total = $script:results.Count
$passed = @($script:results | Where-Object { $_.Ok }).Count
Write-Host ("passed {0}/{1}" -f $passed, $total)
if ($script:failedSteps -gt 0) {
    Write-Host "R0 GATE: FAIL" -ForegroundColor Red
    exit 1
}
Write-Host "R0 GATE: PASS" -ForegroundColor Green
exit 0
