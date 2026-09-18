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

param(
    [switch]$SkipCargo,
    [switch]$SkipTs,
    [switch]$SkipWeb
)

$ErrorActionPreference = "Continue"
# §2.4 Rust 编译与测试资源安全红线：统一经 ci-shared 执行层（与 ci-gate/dev 同一实现）——
# 所有 cargo 调用走 Invoke-CiCargo：显式注入 -j / --test-threads（normal=2/2、strict=1/1）、
# 启动前内存门（可用 <6GB 或已用 ≥80% 拒绝，红线 7）与构建空闲门（红线 4）、30s 心跳 +
# 逐行回显（红线 5）、真实退出码写 $global:LASTEXITCODE（红线 10）。
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath
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
    # §2.4 红线 10：进入步骤块前显式清空退出码——若本步的 cargo 经统一入口"启动失败"
    # （Invoke-CiLoggedCommand 会自行写非零码），不残留上一步的旧值；仍按原语义
    # "$exit -eq 0 才判通过"，失败不会被改写成通过。
    # 注意：不能依赖 `& $Action` 隐式清空——块内若含 `return`（受控表达失败），
    # PowerShell 5.1 不会重置调用方的 $LASTEXITCODE，会把上一步的旧码误记为本步失败。
    $global:LASTEXITCODE = $null
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
    # §2.4 红线 3：并发参数一律由策略层注入，本脚本不再自带 -j / --test-threads。
    # 各步显式传 -PolicyMode：完整依赖图（多包 check）= strict(-j 1)，定向测试 = normal(-j 2)。
    # Invoke-CiCargo 不 throw（内存门越线时写退出码 137），成败仍由 Invoke-Step
    # 读 $global:LASTEXITCODE 判定——原失败分支语义不变（红线 10）。
    Invoke-Step "cargo fmt --all -- --check" {
        # cargo fmt 非编译子命令（实测不认 -j），normal 档仅走统一入口与日志。
        Invoke-CiCargo -Arguments @('fmt', '--all', '--', '--check') -Cwd $sdkRoot `
            -HeartbeatSec 30 -Label 'r0-fmt' -PolicyMode 'normal'
    }

    Invoke-Step "cargo check core/server/cli" {
        # §2.4 红线 2：同时 check core+server+cli 三个包 = 构建完整 workspace 依赖图，
        # 且 owo-agent-core 链接 ONNX/Sherpa/SQLite 原生依赖 → strict 档（-j 1）。
        Invoke-CiCargo -Arguments @('check', '-p', 'owo-agent-core', '-p', 'owo-agent-server', '-p', 'owo-agent-cli') -Cwd $sdkRoot `
            -HeartbeatSec 30 -Label 'r0-check-core-server-cli' -PolicyMode 'strict'
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
        # §2.4 红线 1：带 --test <名> 过滤器的定向测试（单包多套件）→ normal 档；
        # 参数归一层在尾部注入 --test-threads=2，且按 §2.4.2 逐套件串行、不并发多组。
        Invoke-CiCargo -Arguments (@('test', '-p', 'owo-agent-server') + @($serverSuites)) -Cwd $sdkRoot `
            -HeartbeatSec 30 -Label 'r0-test-server-suites' -PolicyMode 'normal'
    }

    Invoke-Step "cargo test cli worker_child_tests" {
        # §2.4 红线 1：--test worker_child_tests 定向过滤器 → normal 档（-j 2 / 线程 2）。
        Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-cli', '--test', 'worker_child_tests') -Cwd $sdkRoot `
            -HeartbeatSec 30 -Label 'r0-test-cli-worker-child' -PolicyMode 'normal'
    }

    Invoke-Step "cargo test core workswarm_tests" {
        # §2.4 红线 1/2 边界：core 完整无过滤测试才降 strict；本步带 --test 过滤器 → normal。
        Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'workswarm_tests') -Cwd $sdkRoot `
            -HeartbeatSec 30 -Label 'r0-test-core-workswarm' -PolicyMode 'normal'
    }

    if (Test-Path "crates/owo-agent-core/tests/workswarm_recovery_tests.rs") {
        Invoke-Step "cargo test core workswarm_recovery_tests" {
            # §2.4 红线 1：core 单集成测试文件定向跑 → normal 档。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'workswarm_recovery_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-ws-recovery' -PolicyMode 'normal'
        }
    }

    # V1 三日 ProductEval executor suites (Lane 1 single-agent / Lane 2 workswarm adapter).
    if (Test-Path "crates/owo-agent-core/tests/product_eval_single_agent_tests.rs") {
        Invoke-Step "cargo test core product_eval_single_agent_tests" {
            # §2.4 红线 1：带 --test 过滤器的定向测试 → normal 档。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'product_eval_single_agent_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-pe-single' -PolicyMode 'normal'
        }
    }
    if (Test-Path "crates/owo-agent-core/tests/product_eval_workswarm_tests.rs") {
        Invoke-Step "cargo test core product_eval_workswarm_tests" {
            # §2.4 红线 1：带 --test 过滤器的定向测试 → normal 档。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'product_eval_workswarm_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-pe-workswarm' -PolicyMode 'normal'
        }
    }
    # R1 statistics suite (Lane 1): Wilson CI / percentiles / enablement edges.
    if (Test-Path "crates/owo-agent-core/tests/product_eval_statistics_tests.rs") {
        Invoke-Step "cargo test core product_eval_statistics_tests" {
            # §2.4 红线 1：带 --test 过滤器的定向测试 → normal 档。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'product_eval_statistics_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-pe-stats' -PolicyMode 'normal'
        }
    }
    # V1 四期 (Lane 3) artifact review store/core suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-core/tests/artifact_review_tests.rs") {
        Invoke-Step "cargo test core artifact_review_tests" {
            # §2.4 红线 1：带 --test 过滤器的定向测试 → normal 档。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'artifact_review_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-artifact-review' -PolicyMode 'normal'
        }
    }
    # V1 四期 (Lane 2) long-task responsiveness suite is included as soon as it exists.
    if (Test-Path "crates/owo-agent-core/tests/workswarm_responsiveness_tests.rs") {
        Invoke-Step "cargo test core workswarm_responsiveness_tests" {
            # §2.4 红线 1：带 --test 过滤器的定向测试 → normal 档（响应性用例对线程数敏感，
            # 2 线程即满足 run-v1-resilience 同款口径）。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-core', '--test', 'workswarm_responsiveness_tests') -Cwd $sdkRoot `
                -HeartbeatSec 30 -Label 'r0-test-core-ws-resp' -PolicyMode 'normal'
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
