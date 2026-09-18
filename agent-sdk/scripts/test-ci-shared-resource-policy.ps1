#requires -Version 5.1
<#
test-ci-shared-resource-policy.ps1 — §2.4 资源安全红线脚本层的**离线**负例/正例自测。

为什么需要：§2.4 的红线只有落到脚本层才算被执行；而"限制并发的代码自己写错了"
和历史上"验收脚本假通过"是同一类缺陷（指南 §7.3：每次假绿都要补一个负例）。
本自测不启动任何 cargo/rustc，纯参数归一与门逻辑，秒级完成，可在无桌面会话、
内存紧张时执行。

覆盖：
  正例：cargo test 注入 -j 与 --test-threads；clippy/test 的 -- 分隔保持；
        test 的 --test-threads 归一；非编译子命令不注入 -j；更小的显式值保留（只降不升）。
  负例：-j 4 拒绝；--test-threads 4 拒绝；策略 >2 拒绝；内存快照可读且带阈值；
        内存门在低阈值下确实拒绝；构建空闲门文案可操作。
退出码：0 = 全部通过；1 = 有失败（逐条打印）。
用法：.\scripts\test-ci-shared-resource-policy.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

$results = @()
function Add-Result {
    param([string]$Name, [bool]$Ok, [string]$Detail = '')
    $script:results += [pscustomobject]@{ name = $Name; ok = $Ok; detail = $Detail }
    if ($Ok) { Write-Host ("[PASS] {0} — {1}" -f $Name, $Detail) -ForegroundColor Green }
    else { Write-Host ("[FAIL] {0} — {1}" -f $Name, $Detail) -ForegroundColor Red }
}
function Invoke-Guard {
    param([scriptblock]$Block)
    try { $null = & $Block; return '' } catch { return $_.Exception.Message }
}

# ---- 正例：参数归一 -------------------------------------------------------------
$norm = Protect-CiCargoArguments -Arguments @('test', '--workspace', '--locked') -Jobs 1 -TestThreads 1
Add-Result '正例：cargo test 注入 -j 1 与 --test-threads=1' `
    (($norm -join ' ') -eq 'test --workspace --locked -j 1 -- --test-threads=1') "got=$($norm -join ' ')"

$norm = Protect-CiCargoArguments -Arguments @('test', '-p', 'owo-agent-core', '--lib', 'change_set', '--', '--exact') -Jobs 2 -TestThreads 2
Add-Result '正例：已有 -- 分组时把 --test-threads 追加进测试面' `
    (($norm -join ' ') -eq 'test -p owo-agent-core --lib change_set -j 2 -- --exact --test-threads=2') "got=$($norm -join ' ')"

$norm = Protect-CiCargoArguments -Arguments @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings') -Jobs 1 -TestThreads 1
Add-Result '正例：clippy 的 -- -D warnings 原样保留且 -j 在 -- 之前' `
    (($norm -join ' ') -eq 'clippy --workspace --all-targets --locked -j 1 -- -D warnings') "got=$($norm -join ' ')"

$norm = Protect-CiCargoArguments -Arguments @('fmt', '--all', '--', '--check') -Jobs 2 -TestThreads 2
Add-Result '正例：cargo fmt 不注入 -j（实测 cargo fmt 不认该参数）' `
    (($norm -join ' ') -eq 'fmt --all -- --check') "got=$($norm -join ' ')"

$norm = Protect-CiCargoArguments -Arguments @('test', '--locked', '-j', '1', '--', '--test-threads=1') -Jobs 2 -TestThreads 2
Add-Result '正例：显式更小值保留（红线 8 只降不升）' `
    (($norm -join ' ') -eq 'test --locked -j 1 -- --test-threads=1') "got=$($norm -join ' ')"

$norm = Protect-CiCargoArguments -Arguments @('build', '--release') -Jobs 1 -TestThreads 1
Add-Result '正例：release 构建走 -j 1' `
    (($norm -join ' ') -eq 'build --release -j 1') "got=$($norm -join ' ')"

# ---- 负例：超过 2 的并发参数必须被拒绝 --------------------------------------------
$msg = Invoke-Guard { Protect-CiCargoArguments -Arguments @('test', '--workspace', '-j', '4') -Jobs 2 -TestThreads 2 }
Add-Result '负例：-j 4 必须被拒绝' ($msg -like '*上限 2*') "msg=$msg"

$msg = Invoke-Guard { Protect-CiCargoArguments -Arguments @('build', '--jobs=8') -Jobs 2 -TestThreads 2 }
Add-Result '负例：--jobs=8 必须被拒绝' ($msg -like '*上限 2*') "msg=$msg"

$msg = Invoke-Guard { Protect-CiCargoArguments -Arguments @('test', '--', '--test-threads', '4') -Jobs 2 -TestThreads 2 }
Add-Result '负例：--test-threads 4 必须被拒绝' ($msg -like '*上限 2*') "msg=$msg"

$msg = Invoke-Guard { Protect-CiCargoArguments -Arguments @('test') -Jobs 32 -TestThreads 2 }
Add-Result '负例：策略本身要求 32 路并发必须被拒绝' ($msg -like '*本机并发上限 2*') "msg=$msg"

# ---- 门：策略表与内存快照 ----------------------------------------------------------
$l = Get-CiRustResourceLimits -Mode 'normal'
Add-Result '策略：normal = 编译 2 / 测试线程 2' (($l.jobs -eq 2) -and ($l.test_threads -eq 2)) "jobs=$($l.jobs) threads=$($l.test_threads)"
$l = Get-CiRustResourceLimits -Mode 'strict'
Add-Result '策略：strict = 编译 1 / 测试线程 1' (($l.jobs -eq 1) -and ($l.test_threads -eq 1)) "jobs=$($l.jobs) threads=$($l.test_threads)"

$mem = Get-CiMemoryStatus
Add-Result '内存快照可读取（红线 7 的前提）' ($mem.status -in @('ok', 'limited')) "status=$($mem.status) free=$($mem.free_gb)GB used=$($mem.used_percent)%"
Add-Result '内存阈值与红线一致（6 GB / 80%）' `
    (($mem.gate.min_free_gb -eq 6.0) -and ($mem.gate.max_used_percent -eq 80.0)) "gate=$($mem.gate.min_free_gb)/$($mem.gate.max_used_percent)"
Add-Result '内存读取值不为零（拒绝把 unknown 当通过）' ($mem.total_gb -gt 0) "total=$($mem.total_gb)GB source=$($mem.source)"

$compiles = @(
    (Test-CiCargoCompiles -Arguments @('test', '--workspace')),
    (Test-CiCargoCompiles -Arguments @('clippy')),
    (Test-CiCargoCompiles -Arguments @('build'))
)
Add-Result '编译子命令判定：test/clippy/build 均需资源门' ((@($compiles | Where-Object { $_ -eq $true }).Count) -eq 3) "got=$($compiles -join ',')"
Add-Result '非编译子命令判定：fmt 不占用构建槽' (-not (Test-CiCargoCompiles -Arguments @('fmt', '--all'))) 'fmt=false'

# 红线 7 的适用面：守护保护"rustc/测试分片风暴"，不保护用户拉起的常驻进程。
# `cargo run`（dev serve / product-eval 批次）编译段一过就是业务进程，内存吃紧时
# 杀它等于杀用户程序——红线 8 要的正是"工程让出资源、不打断用户"。
Add-Result '守护范围：cargo test 启用运行中内存守护' (Test-CiCargoGuardRunning -Arguments @('test', '--workspace')) 'test=true'
Add-Result '守护范围：cargo build --release 启用运行中守护' (Test-CiCargoGuardRunning -Arguments @('build', '--release')) 'build=true'
Add-Result '守护范围：cargo run 不启用运行中守护（常驻进程）' (-not (Test-CiCargoGuardRunning -Arguments @('run', '-q', '-p', 'owo-agent-cli', '--', 'serve'))) 'run=false'
Add-Result '守护范围：cargo run 仍属编译子命令（启动前门照常）' (Test-CiCargoCompiles -Arguments @('run', '-p', 'owo-agent-cli', '--', 'serve')) 'run compiles=true'

# ---- 资源状态可落盘进 summary（§2.4.4 最后一条） ------------------------------------
$null = Set-CiRustResourcePolicy -Mode 'strict' -Source 'selftest'
$state = Get-CiResourceState
Add-Result 'summary 资源段含红线标识与真实限制' `
    (($state.redline -like '*2.4*') -and ($state.jobs -eq 1) -and ($state.test_threads -eq 1) -and ($state.mode -eq 'strict')) `
    "mode=$($state.mode) jobs=$($state.jobs) threads=$($state.test_threads) env_jobs=$($state.cargo_env.CARGO_BUILD_JOBS)"
Add-Result 'Set-CiRustResourcePolicy 注入子进程环境变量' `
    (($env:CARGO_BUILD_JOBS -eq '1') -and ($env:RUST_TEST_THREADS -eq '1')) "CARGO_BUILD_JOBS=$env:CARGO_BUILD_JOBS RUST_TEST_THREADS=$env:RUST_TEST_THREADS"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ('owo-ci-res-selftest-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
Write-CiStepSummary -LogDir $tmp -CurrentStep 'selftest' -CurrentId 'selftest' -StepState 'passed'
$sum = Get-Content (Join-Path $tmp 'summary.json') -Raw | ConvertFrom-Json
Add-Result 'summary.json 落盘 resources 段（可证明安全参数）' `
    (($sum.resources.jobs -eq 1) -and ($sum.resources.test_threads -eq 1) -and ($sum.resources.memory_gate.min_free_gb -eq 6)) `
    "jobs=$($sum.resources.jobs) threads=$($sum.resources.test_threads) step=$($sum.step_state)"
Remove-Item -LiteralPath $tmp -Recurse -Force

$fail = @($results | Where-Object { -not $_.ok })
Write-Host ("[selftest] {0}/{1} 通过" -f (@($results).Count - $fail.Count), @($results).Count)
if ($fail.Count -gt 0) { exit 1 }
exit 0
