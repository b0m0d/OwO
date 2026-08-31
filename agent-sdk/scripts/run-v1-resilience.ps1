# run-v1-resilience.ps1 - 十期 · 四路：执行安全、取消、幂等与崩溃恢复验收脚本
# (Lane 4: workswarm closeout / cancel-chain / checkpoint-recovery / idempotency).
#
# 验收面（无需任何模型凭据，全内置 echo/sleep/fail worker）：
#   1) ChangeSet 异常闭环 4 场景（workswarm_api::tests 单元层，真实落盘 Worker）：
#      写后失败 / 取消中写入 / 非 Git 内容哈希 / 连续两个写 Worker 租约隔离。
#   2) HTTP 契约恢复面 >=20 预置场景（v1_execution_safety_tests，终态+崩溃恢复+幂等），
#      要求 >=19 通过（允许 1 个环境性抖动，门槛与十期验收一致）。
#   3) 构建位：cargo check --all-targets 通过（并行路线的 WIP 编译错误除外——
#      脚本先自行 check，若 workspace 因子 crate 中段编辑失败会给出提示而非误报测试失败）。
#
# Provider 计费限制（十期 R5 事实声明）：仅离线/脚本化 Provider 可证明「零继续计费」；
# 云端网关取消不停止计费、也不可如此宣称——本脚本不校验任何计费金额，只校验
# 状态终态 / 无孤儿 / 幂等重放等可观测副作用。
#
# Usage:
#   pwsh -File agent-sdk\scripts\run-v1-resilience.ps1 [-Check] [-OnlyCloseout] [-OnlyHttp]
# Exit code: 0 = 全部通过, 1 = 有失败, 2 = 构建位被并行 WIP 阻断（重试即可）。

param(
    [switch]$Check,          # 只做 cargo check --all-targets
    [switch]$OnlyCloseout,   # 只跑 ChangeSet 闭环 4 场景（工作区单元层）
    [switch]$OnlyHttp        # 只跑 HTTP 恢复面（v1_execution_safety_tests）
)

$ErrorActionPreference = "Continue"
$sdkRoot = Split-Path -Parent $PSScriptRoot
Set-Location $sdkRoot

Write-Host "== [resilience] root: $sdkRoot ==" -ForegroundColor Cyan

# ---------------------------------------------------------------------------
# 1) 构建位（并行路线 WIP 可能让 check 临时失败；失败 = 阻断，不是测试失败）
# ---------------------------------------------------------------------------
if (-not $OnlyCloseout -and -not $OnlyHttp) {
    Write-Host "== [resilience] cargo check --all-targets ==" -ForegroundColor Cyan
    cargo check --workspace --all-targets 2>&1 | Tee-Object -Variable checkOut | Out-Null
    $checkLines = $checkOut | Select-String -Pattern "^error"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "BUILD BLOCKED: workspace 编译失败（可能系并行路线的 WIP 编辑）：" -ForegroundColor Red
        $checkLines | ForEach-Object { Write-Host ("    " + $_.Line) -ForegroundColor Yellow }
        exit 2
    }
    Write-Host "cargo check: OK" -ForegroundColor Green
}
if ($Check) { exit 0 }

$failures = @()
$totalOk = 0

# ---------------------------------------------------------------------------
# 2) closeout 单元 4 场景
# ---------------------------------------------------------------------------
if (-not $OnlyHttp) {
    Write-Host "== [resilience] ChangeSet 异常闭环（workswarm_api 单元层）==" -ForegroundColor Cyan
    $unitTests = @(
        "write_then_fail_still_generates_change_set",
        "cancelled_during_write_still_closes_out",
        "non_git_dir_content_hash_detects_new_modify_delete",
        "two_write_workers_sharing_lease_keep_change_sets_isolated"
    )
    foreach ($t in $unitTests) {
        cargo test -p owo-agent-server --lib $t -- --exact --nocapture 2>&1 | Tee-Object -Variable runOut | Out-Null
        $passed = ($runOut | Select-String -Pattern "test result: ok").Count -gt 0
        if ($passed) { $totalOk++ } else { $failures += "closeout/$t" }
        Write-Host ("    {0} : {1}" -f $t, $(if ($passed) { "PASS" } else { "FAIL" })) -ForegroundColor $(if ($passed) { "Green" } else { "Red" })
    }
}

# ---------------------------------------------------------------------------
# 3) HTTP 恢复面 >=20 预置场景，>=19 通过
# ---------------------------------------------------------------------------
if (-not $OnlyCloseout) {
    Write-Host "== [resilience] HTTP 恢复面（v1_execution_safety_tests）==" -ForegroundColor Cyan
    cargo test -p owo-agent-server --test v1_execution_safety_tests -- --test-threads 4 2>&1 |
        Tee-Object -Variable httpOut | Out-Null
    $tests = $httpOut | Select-String -Pattern "^test ([\w_]+) \.\.\. (ok|FAILED)"
    $httpOk = 0; $httpFail = 0
    foreach ($m in $tests) {
        if ($m.Matches[0].Groups[2].Value -eq "ok") { $httpOk++ } else { $httpFail++ }
    }
    # 以测试行解析为准；解析不到则回退到 test result 行。
    $resultLine = $httpOut | Select-String -Pattern "test result: (ok|FAILED)"
    if ($httpOk + $httpFail -eq 0 -and $resultLine.Count -gt 0) {
        $tokens = $resultLine.Line -split "[; ]+"
        foreach ($tok in $tokens) {
            if ($tok -match "^(\d+) passed;") { $httpOk = [int]$Matches[1] }
            if ($tok -match "^(\d+) failed;") { $httpFail = [int]$Matches[1] }
        }
    }
    Write-Host ("    HTTP 恢复面 : {0} passed / {1} failed" -f $httpOk, $httpFail) -ForegroundColor Cyan
    if ($httpFail -gt 1 -or $httpOk -lt 19) {
        $failures += "v1_execution_safety_tests (need >=19/20 passed, got $httpOk/$($httpOk+$httpFail))"
        $httpOut | Select-String -Pattern "FAILED:|panicked at" | ForEach-Object {
            Write-Host ("    " + $_.Line) -ForegroundColor Yellow
        }
    } elseif ($httpFail -eq 1) {
        Write-Host "    WARN: 1 个场景失败（允许 1 个环境性抖动，门槛 >=19/20）" -ForegroundColor Yellow
    }
    $totalOk += $httpOk
}

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "==================== V1 RESILIENCE SUMMARY ====================" -ForegroundColor Cyan
Write-Host ("closeout(4) + http(>=20) passed total : {0}" -f $totalOk)
if ($failures.Count -gt 0) {
    Write-Host "FAILED:" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host ("    - " + $f) -ForegroundColor Red }
    Write-Host "V1 RESILIENCE: FAILED" -ForegroundColor Red
    exit 1
}
Write-Host "V1 RESILIENCE: DONE" -ForegroundColor Green
exit 0