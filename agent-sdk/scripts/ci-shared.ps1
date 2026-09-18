# ci-shared.ps1 — CI 脚本公共助手（供 ci-gate.ps1 / ci-nightly.ps1 / ci-weekly.ps1 点源引用）
# 本文件只提供函数与状态，不直接执行外部命令。
# 注意：保持 Windows PowerShell 5.1 兼容语法（仓库规则），同时兼容 PowerShell 7。
# 用法：. (Join-Path $PSScriptRoot "ci-shared.ps1")
#
# 约定：
#   - $script:ciFailures = List[PSCustomObject{name, detail}]；$script:ciSteps = List[string]
#   - $script:ciStepFilter：-Step 过滤词（子串匹配，空=全部执行）
#   - Invoke-CiStep 块内 `return` 表示"受控跳过"（绕过失败标记并在日志说明 SKIP 原因）

function Get-CiRepoRoot {
    # ci-shared.ps1 与 ci-*.ps1 均位于 agent-sdk/scripts/，根目录即上一级
    return Split-Path -Parent $PSScriptRoot
}

function Initialize-CiPath {
    # 保证 cargo 与 npm 可解析；找不到时回退到用户标准安装目录（不写死单机路径）
    $CARGO_HOME = $env:CARGO_HOME
    if (-not $CARGO_HOME) { $CARGO_HOME = Join-Path $env:USERPROFILE ".cargo" }
    $cargoBin = Join-Path $CARGO_HOME "bin"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue) -and (Test-Path (Join-Path $cargoBin "cargo.exe"))) {
        $env:PATH = "$cargoBin;" + $env:PATH
    }
    if (-not (Get-Command npm -ErrorAction SilentlyContinue) -and (Test-Path (Join-Path $env:ProgramFiles "nodejs\npm.cmd"))) {
        $env:PATH = "$env:ProgramFiles\nodejs;" + $env:PATH
    }
}

function New-CiFailureState {
    if ($null -eq $script:ciFailures) { $script:ciFailures = @() }
    if ($null -eq $script:ciSteps) { $script:ciSteps = @() }
}

function Add-CiFailure {
    param([Parameter(Mandatory = $true)][string]$Name, [string]$Detail)
    New-CiFailureState
    $script:ciFailures += [pscustomobject]@{ name = $Name; detail = $Detail }
}

# ---------------------------------------------------------------------------
# §2.4 Rust 编译与测试资源安全红线（唯一实现）
# 本机 32 逻辑处理器 / 约 32 GB 内存；Cargo 默认并发会同时起大量 rustc、link.exe
# 与测试线程，导致物理内存耗尽、系统分页、整机卡死。本节把红线变成脚本可执行门：
#   红线 1/2：normal=编译 2 / 测试线程 2；strict（完整 workspace、完整 core、release、
#             原生依赖重链）=1 / 1；
#   红线 3：禁止依赖 Cargo 默认并发——所有 cargo 调用经 Invoke-CiCargo 显式注入 -j；
#   红线 4：禁止两组 Cargo 并行——启动前检测 cargo/rustc/link，繁忙则等待后拒绝；
#   红线 7：可用物理内存 < 6 GB 或已用率 ≥ 80% 时拒绝启动，运行中触发则中止并
#             以 resource_limited / 非零退出码收口（红线 10：保存真实退出码）；
#   §2.4.4：实际生效的限制写入 summary.json，可证明"测试是在安全参数下跑的"。
# 红线 8/9：本层只降不升——没有任何开关可以提高并发来缩短门禁时间。
# ---------------------------------------------------------------------------

function Get-CiRustResourceLimits {
    param([ValidateSet('normal', 'strict')][string]$Mode = 'normal')
    if ($Mode -eq 'strict') {
        return [pscustomobject]@{ mode = 'strict'; jobs = 1; test_threads = 1; reason = '完整 workspace/完整 core/release/原生依赖重链（§2.4 红线 2）' }
    }
    return [pscustomobject]@{ mode = 'normal'; jobs = 2; test_threads = 2; reason = '普通开发与定向测试（§2.4 红线 1）' }
}

# 设置本轮资源策略：注入 CARGO_BUILD_JOBS/RUST_TEST_THREADS 并记录进 summary。
function Set-CiRustResourcePolicy {
    param(
        [ValidateSet('normal', 'strict')][string]$Mode = 'normal',
        [string]$Source = 'script'
    )
    $limits = Get-CiRustResourceLimits -Mode $Mode
    if ($limits.jobs -gt 2 -or $limits.test_threads -gt 2) {
        throw "§2.4 资源红线：本机并发上限 2（收到 jobs=$($limits.jobs) test_threads=$($limits.test_threads)）"
    }
    $env:CARGO_BUILD_JOBS = [string]$limits.jobs
    $env:RUST_TEST_THREADS = [string]$limits.test_threads
    $script:ciRustPolicyMode = $limits.mode
    $script:ciRustPolicySource = $Source
    return $limits
}

function Get-CiRustPolicyMode {
    if ($script:ciRustPolicyMode) { return $script:ciRustPolicyMode }
    return 'normal'
}

# 内存快照：Get-CimInstance 主路径，WMI 兜底；两者都不可用时如实返回 unknown
# （不猜测、不假装通过；调用方按红线 7 拒绝启动新构建）。
function Get-CiMemoryStatus {
    $param = [pscustomobject]@{ min_free_gb = 6.0; max_used_percent = 80.0 }
    $os = $null
    try { $os = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop } catch {
        try { $os = Get-WmiObject -Class Win32_OperatingSystem -ErrorAction Stop } catch { $os = $null }
    }
    if (-not $os) {
        return [pscustomobject]@{ status = 'unknown'; total_gb = 0.0; free_gb = 0.0; used_percent = 0.0; ok = $false; source = 'none'; gate = $param }
    }
    $totalGb = [math]::Round([double]$os.TotalVisibleMemorySize / 1MB, 2)
    $freeGb = [math]::Round([double]$os.FreePhysicalMemory / 1MB, 2)
    if ($totalGb -le 0) {
        return [pscustomobject]@{ status = 'unknown'; total_gb = 0.0; free_gb = $freeGb; used_percent = 0.0; ok = $false; source = 'cim'; gate = $param }
    }
    $usedPct = [math]::Round((1 - ($freeGb / $totalGb)) * 100, 1)
    $limited = ($freeGb -lt $param.min_free_gb) -or ($usedPct -ge $param.max_used_percent)
    return [pscustomobject]@{
        status       = $(if ($limited) { 'limited' } else { 'ok' })
        total_gb     = $totalGb
        free_gb      = $freeGb
        used_percent = $usedPct
        ok           = (-not $limited)
        source       = 'cim'
        gate         = $param
    }
}

# 红线 7：低于阈值直接拒绝启动新 Rust 构建（给出可操作说明，不静默降级）。
function Assert-CiMemoryGate {
    param([string]$Context = 'cargo')
    $mem = Get-CiMemoryStatus
    if ($mem.status -eq 'unknown') {
        Write-Host ("    [§2.4] 内存状态不可读取（{0}）——本轮仍按 -j≤2 / --test-threads≤2 受限并发执行" -f $Context) -ForegroundColor Yellow
    } elseif (-not $mem.ok) {
        throw ("§2.4 红线 7（resource_limited）：{0} 拒绝启动 Rust 构建——可用物理内存 {1} GB / 已用 {2}%，低于阈值（要求可用 ≥ {3} GB 且已用 < {4}%）。请关闭占用内存的程序后重试，或等待当前构建结束；禁止用提高超时来绕过内存门。" -f `
                $Context, $mem.free_gb, $mem.used_percent, $mem.gate.min_free_gb, $mem.gate.max_used_percent)
    }
    return $mem
}

# 红线 4：不得同时运行两组 Cargo——已有 cargo/rustc/link 时等待，等待超时后拒绝。
function Assert-CiBuildIdle {
    param([int]$WaitSec = 900, [int]$PollSec = 10, [string]$Context = 'cargo')
    $busy = @(Get-Process -Name cargo, rustc, link -ErrorAction SilentlyContinue)
    if ($busy.Count -eq 0) { return $true }
    $deadline = (Get-Date).AddSeconds($WaitSec)
    $groups = (@($busy | Group-Object ProcessName | ForEach-Object { "$($_.Name)x$($_.Count)" }) -join ' ')
    Write-Host ("    [§2.4] 检测到 {0} 个既有构建进程（{1}）——按红线 4 等待其结束，不启动第二组" -f $busy.Count, $groups) -ForegroundColor Yellow
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds $PollSec
        $busy = @(Get-Process -Name cargo, rustc, link -ErrorAction SilentlyContinue)
        if ($busy.Count -eq 0) {
            Write-Host "    [§2.4] 既有构建已结束，继续本轮编译" -ForegroundColor Green
            return $true
        }
    }
    throw ("§2.4 红线 4（build_busy）：{0} 等待 {1}s 后仍有 {2} 个 cargo/rustc/link 进程在跑，拒绝并发启动第二组构建。请等待或显式终止本轮无关的构建进程。" -f `
            $Context, $WaitSec, $busy.Count)
}

# 红线 3 + §2.4.4：显式 -j / --test-threads，且拒绝任何 >2 的本机并发参数。
function Protect-CiCargoArguments {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [int]$Jobs = 2,
        [int]$TestThreads = 2
    )
    if ($Jobs -gt 2 -or $TestThreads -gt 2) {
        throw "§2.4 资源红线：本机并发上限 2（收到 jobs=$Jobs test_threads=$TestThreads）"
    }
    $tokens = @($Arguments | ForEach-Object { [string]$_ })
    # 以第一个 `--` 切 head/tail：head 属 cargo 自身选项，tail 属测试可执行文件。
    $sep = [Array]::IndexOf($tokens, '--')
    if ($sep -ge 0) {
        $head = @($tokens | Select-Object -First $sep)
        $tail = @($tokens | Select-Object -Skip ($sep + 1))
    } else {
        $head = @($tokens)
        $tail = @()
    }
    # 1) head：剥掉调用方自带的 -j/--jobs（>2 直接拒绝，≤2 由本轮策略统一改写）。
    $headClean = New-Object System.Collections.Generic.List[string]
    $skipNext = $false
    $foundJobs = 0
    for ($i = 0; $i -lt $head.Count; $i++) {
        $tok = [string]$head[$i]
        if ($skipNext) { $skipNext = $false; continue }
        if (($tok -eq '-j') -or ($tok -eq '--jobs')) {
            if ($i + 1 -lt $head.Count) {
                if (-not [int]::TryParse([string]$head[$i + 1], [ref]$foundJobs)) {
                    throw "§2.4 资源红线：$tok 之后不是数字（cargo 参数归一失败，拒绝猜测）"
                }
                $skipNext = $true
            }
            continue
        }
        if ($tok -match '^-j(\d+)$') { $foundJobs = [int]$Matches[1]; continue }
        if ($tok -match '^--jobs=(\d+)$') { $foundJobs = [int]$Matches[1]; continue }
        $headClean.Add($tok) | Out-Null
    }
    if ($foundJobs -gt 2) {
        throw "§2.4 红线 3/§2.4.4：检测到本机并发参数 -j $foundJobs（上限 2）。请改用 -j 1（完整测试/release）或 -j 2（定向测试）。"
    }
    # 子命令 = head 中第一个不以 '-' 开头的 token（cargo test/clippy/build/fmt/run/…）。
    $sub = ''
    foreach ($tok in $headClean) { if ($tok -notmatch '^-') { $sub = $tok; break } }
    # 只有会真正编译/rustc 派生的子命令接受 -j（cargo fmt 不认：实测
    # `error: unexpected argument '-j' found`）。非编译子命令跳过注入，不猜测。
    $buildish = @('b', 'build', 'c', 'check', 'clippy', 't', 'test', 'r', 'run', 'bench', 'install', 'package', 'fix', 'doc', 'rustc')
    $injectJobs = ($buildish -contains $sub)
    # 2) tail：仅 cargo test 归一 --test-threads（>2 拒绝，缺失则注入）。
    $effectiveThreads = $TestThreads
    if ($sub -eq 'test') {
        $tailClean = New-Object System.Collections.Generic.List[string]
        $foundThreads = 0
        $skipNext2 = $false
        for ($i = 0; $i -lt $tail.Count; $i++) {
            $tok = [string]$tail[$i]
            if ($skipNext2) { $skipNext2 = $false; continue }
            if ($tok -eq '--test-threads') {
                if (-not [int]::TryParse([string]$tail[$i + 1], [ref]$foundThreads)) {
                    throw "§2.4 资源红线：--test-threads 之后不是数字"
                }
                $skipNext2 = $true
                continue
            }
            if ($tok -match '^--test-threads=(\d+)$') { $foundThreads = [int]$Matches[1]; continue }
            $tailClean.Add($tok) | Out-Null
        }
        if ($foundThreads -gt 2) {
            throw "§2.4 红线 3/§2.4.4：检测到测试并发参数 --test-threads $foundThreads（上限 2）。请改用 1 或 2。"
        }
        # 调用方显式更小时取更小值（红线 8：只降不升）。
        if ($foundThreads -gt 0) { $effectiveThreads = [Math]::Min($foundThreads, $TestThreads) }
        $tail = @($tailClean) + @("--test-threads=$effectiveThreads")
    }
    # 调用方显式更小的 -j 同样取更小值（只降不升）。
    $effectiveJobs = $Jobs
    if ($foundJobs -gt 0) { $effectiveJobs = [Math]::Min($foundJobs, $Jobs) }

    # 3) 重新注入策略值：-j 位于 trailing `--` 之前（cargo 全局选项位置）。
    $result = New-Object System.Collections.Generic.List[string]
    foreach ($tok in $headClean) { $result.Add($tok) | Out-Null }
    if ($injectJobs) {
        $result.Add('-j') | Out-Null
        $result.Add([string]$effectiveJobs) | Out-Null
    }
    if ($tail.Count -gt 0) {
        $result.Add('--') | Out-Null
        foreach ($tok in $tail) { $result.Add($tok) | Out-Null }
    }
    return , @($result)
}

# 子命令是否会派生 rustc/link（决定是否需要构建空闲门与 -j 注入）。
# 注意：不含 tree/fmt/audit/deny——它们不启动编译，也不接受 -j。
function Test-CiCargoCompiles {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    $buildish = @('b', 'build', 'c', 'check', 'clippy', 't', 'test', 'r', 'run', 'bench', 'install', 'package', 'fix', 'doc', 'rustc')
    foreach ($tok in @($Arguments | ForEach-Object { [string]$_ })) {
        if ($tok -match '^-') { continue }
        return ($buildish -contains $tok)
    }
    return $false
}

# 红线 7 的"运行中内存守护"保护对象是**构建与测试风暴**（rustc 群 + 测试分片），
# 不是用户主动拉起的常驻进程：`cargo run` 编译段一过就是 dev serve / product-eval
# 批次；内存吃紧时杀掉它等于杀用户程序（红线 8 要的正是"工程让出资源、不打断用户"）。
# 因此 run 只过启动前的内存门与构建空闲门，不启用运行中守护。
function Test-CiCargoGuardRunning {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    if (-not (Test-CiCargoCompiles -Arguments $Arguments)) { return $false }
    foreach ($tok in @($Arguments | ForEach-Object { [string]$_ })) {
        if ($tok -match '^-') { continue }
        return (@('run', 'r') -notcontains $tok)
    }
    return $false
}

# §2.4 统一 cargo 执行入口：策略注入 + 内存门 + 构建空闲门 + 参数归一 + 心跳/超时
# + 运行中内存守护（红线 7 的"必要时中止"）+ 真实退出码（红线 10）。
function Invoke-CiCargo {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [string]$Cwd = "",
        [int]$TimeoutSec = 0,
        [int]$HeartbeatSec = 30,
        [string]$LogFile = "",
        [string]$Label = 'cargo',
        [ValidateSet('auto', 'normal', 'strict')][string]$PolicyMode = 'auto',
        # 允许显式指定 cargo 可执行文件（发布脚本用 $env:OWO_CARGO 覆盖；缺省走 PATH）。
        [string]$CargoExe = "",
        [switch]$SkipIdleGate
    )
    $mode = if ($PolicyMode -eq 'auto') { Get-CiRustPolicyMode } else { $PolicyMode }
    $limits = Set-CiRustResourcePolicy -Mode $mode -Source $Label
    $exe = if ($CargoExe) { $CargoExe } elseif ($env:OWO_CARGO) { $env:OWO_CARGO } else { 'cargo' }
    $compiles = Test-CiCargoCompiles -Arguments $Arguments
    if ($compiles) {
        $null = Assert-CiMemoryGate -Context $Label
        if (-not $SkipIdleGate) { Assert-CiBuildIdle -Context $Label }
    }
    $safe = Protect-CiCargoArguments -Arguments $Arguments -Jobs $limits.jobs -TestThreads $limits.test_threads
    if ($compiles) {
        Write-Host ("    [§2.4] {0}：-j {1} / --test-threads {2}（{3}）" -f $Label, $limits.jobs, $limits.test_threads, $limits.reason)
        New-CiFailureState
        $script:ciResourceApplied = @($script:ciResourceApplied) + @(
            "{0}={1}(-j {2}/--test-threads {3})" -f $Label, $limits.mode, $limits.jobs, $limits.test_threads
        )
    } else {
        Write-Host ("    [§2.4] {0}：非编译子命令（不注入 -j；红线 1/2 不适用）" -f $Label)
    }
    $script:ciLastCommandResourceLimited = $false
    # 红线 7 的适用范围：编译/测试才开运行中守护；`cargo run`（常驻服务、eval 批次）
    # 只过启动前门，避免把用户程序当"越线构建"杀掉。
    $guardRunning = Test-CiCargoGuardRunning -Arguments $Arguments
    Invoke-CiLoggedCommand -Exe $exe -Arguments $safe -Cwd $Cwd -TimeoutSec $TimeoutSec `
        -HeartbeatSec $HeartbeatSec -LogFile $LogFile -Label $Label -MemoryGuard:$guardRunning
    if ($script:ciLastCommandResourceLimited) {
        # 资源保护触发：非零退出（红线 10），不允许记成通过。
        $global:LASTEXITCODE = 137
    }
}

# §2.4 变体：既要实时进度（红线 5）又要把日志读回来扫描（发布脚本的 LNK 零容忍门）。
# 实现：流式写 LogFile + 实时回显 → 结束后把日志读回作为返回数组；退出码留在
# $global:LASTEXITCODE（含 124 超时 / 137 资源守护，红线 10）。
function Invoke-CiCargoCapture {
    param(
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [string]$Cwd = "",
        [int]$TimeoutSec = 0,
        [int]$HeartbeatSec = 30,
        [string]$Label = 'cargo',
        [Parameter(Mandatory = $true)][string]$LogFile,
        [ValidateSet('auto', 'normal', 'strict')][string]$PolicyMode = 'auto',
        [string]$CargoExe = "cargo"
    )
    $dir = Split-Path -Parent $LogFile
    if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    Invoke-CiCargo -Arguments $Arguments -Cwd $Cwd -TimeoutSec $TimeoutSec -HeartbeatSec $HeartbeatSec `
        -LogFile $LogFile -Label $Label -PolicyMode $PolicyMode -CargoExe $CargoExe
    $lines = @()
    if (Test-Path -LiteralPath $LogFile) { $lines = @(Get-Content -LiteralPath $LogFile -Encoding UTF8) }
    return ,$lines
}

function Get-CiResourceState {
    New-CiFailureState
    $mem = Get-CiMemoryStatus
    $mode = Get-CiRustPolicyMode
    $limits = Get-CiRustResourceLimits -Mode $mode
    return [ordered]@{
        redline         = '§2.4 Rust 编译与测试资源安全红线'
        mode            = $limits.mode
        jobs            = $limits.jobs
        test_threads    = $limits.test_threads
        policy_source   = if ($script:ciRustPolicySource) { $script:ciRustPolicySource } else { 'default' }
        cargo_env       = [ordered]@{ CARGO_BUILD_JOBS = $env:CARGO_BUILD_JOBS; RUST_TEST_THREADS = $env:RUST_TEST_THREADS }
        cpu_logical     = [int]$env:NUMBER_OF_PROCESSORS
        memory_gate     = [ordered]@{ min_free_gb = 6.0; max_used_percent = 80.0 }
        memory_at_write  = [ordered]@{ status = $mem.status; total_gb = $mem.total_gb; free_gb = $mem.free_gb; used_percent = $mem.used_percent; source = $mem.source }
        applied         = @($script:ciResourceApplied)
        guard_events    = @($script:ciResourceEvents)
    }
}

# 断言工具可用；不可用时 -Required 抛错（CI 中安全类门禁必须真实执行），否则返回 $false 供调用方显式 SKIP。
function Assert-CiTool {
    param(
        [Parameter(Mandatory = $true)][string]$Tool,
        [string]$InstallHint = "CI 应通过 taiki-e/install-action 安装",
        [switch]$Required
    )
    if (Get-Command $Tool -ErrorAction SilentlyContinue) { return $true }
    if ($Required) {
        throw "$Tool 不可用（$InstallHint）；安全类门禁不可跳过"
    }
    return $false
}

# 执行单个 CI 步骤：输出回显 + 可选落盘 $LogDir\<id>.log；支持 -Step 过滤。
# R3-D（指南 §3.6）：summary.json 每步增量落盘；Ctrl+C 中断写 interrupted=true，
# 绝不留下"没有结论的空报告"。长任务的实时心跳/超时由块内 Invoke-CiLoggedCommand 负责。
function Invoke-CiStep {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Id,
        [Parameter(Mandatory = $true)][scriptblock]$Block,
        [string]$Cwd = "",
        [string]$LogDir = ""
    )
    New-CiFailureState
    if ($script:ciStepFilter -and -not $Id.Contains($script:ciStepFilter)) {
        return
    }
    $script:ciSteps += $Name
    Write-Host ("==> {0}" -f $Name) -ForegroundColor Cyan
    $logFile = ""
    if ($LogDir) {
        New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
        $logFile = Join-Path $LogDir ($Id + ".log")
        Write-CiStepSummary -LogDir $LogDir -CurrentStep $Name -CurrentId $Id -StepState "running"
    }
    $outText = ""
    if ($Cwd) { Push-Location $Cwd }
    try {
        $global:LASTEXITCODE = $null
        $outText = (& $Block 2>&1 | Out-String)
        $exit = $global:LASTEXITCODE
        if ($null -eq $exit) { $exit = 0 }
        if ($outText) {
            $trimmed = $outText.TrimEnd("`r", "`n")
            if ($trimmed) { Write-Host $trimmed }
        }
        if ($logFile) { $outText | Out-File -LiteralPath $logFile -Encoding UTF8 }
        if ($exit -ne 0) {
            Add-CiFailure $Name ("exit = {0}" -f $exit)
            Write-Host ("    [FAIL] {0} (exit={1})" -f $Name, $exit) -ForegroundColor Red
            if ($logFile) { Write-CiStepSummary -LogDir $LogDir -CurrentStep $Name -CurrentId $Id -StepState "failed" }
        } else {
            Write-Host ("    [OK]   {0}" -f $Name) -ForegroundColor Green
            if ($logFile) { Write-CiStepSummary -LogDir $LogDir -CurrentStep $Name -CurrentId $Id -StepState "passed" }
        }
    } catch [Management.Automation.PipelineStoppedException] {
        # Ctrl+C：中断必须成为可判定的终态（R3-D），不允许"跑没跑完看不出来"。
        $script:ciInterrupted = $true
        Write-CiStepSummary -LogDir $LogDir -CurrentStep $Name -CurrentId $Id -StepState "interrupted"
        Write-Host "    [INTERRUPTED] $Name —— 用户中止，summary.json 已标记 interrupted=true" -ForegroundColor Yellow
        if ($LogDir) {
            exit 130
        }
        throw
    } catch {
        $err = "EXCEPTION: $($_.Exception.Message)`n$($_.ScriptStackTrace)"
        if ($outText) { $err = $outText + $err }
        Write-Host $err -ForegroundColor Red
        if ($logFile) { $err | Out-File -LiteralPath $logFile -Encoding UTF8 }
        Add-CiFailure $Name $_.Exception.Message
        Write-Host ("    [FAIL] {0} : {1}" -f $Name, $_.Exception.Message) -ForegroundColor Red
        if ($logFile) { Write-CiStepSummary -LogDir $LogDir -CurrentStep $Name -CurrentId $Id -StepState "failed" }
    } finally {
        if ($Cwd) { Pop-Location }
    }
}

# R3-D（§3.6）：增量 summary——每步开始前/结束后都落盘一次，任意时刻被杀都有结论。
# §2.4.4：同时落盘 resources 段（模式/jobs/test_threads/内存快照/守护事件），
# 使报告能自证"本轮测试是在安全并发参数下跑的"。
function Write-CiStepSummary {
    param(
        [Parameter(Mandatory = $true)][string]$LogDir,
        [string]$CurrentStep = "",
        [string]$CurrentId = "",
        [string]$StepState = "running"
    )
    if (-not $LogDir) { return }
    New-CiFailureState
    $summary = [ordered]@{
        updated_at   = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        ok           = (@($script:ciFailures).Count -eq 0) -and ($StepState -ne "failed") -and ($StepState -ne "interrupted")
        completed    = $false
        interrupted  = [bool]$script:ciInterrupted
        current_step = $CurrentStep
        current_id   = $CurrentId
        step_state   = $StepState
        resources    = Get-CiResourceState
        steps        = @($script:ciSteps)
        failures     = @($script:ciFailures | ForEach-Object { "{0}: {1}" -f $_.name, $_.detail })
    }
    $json = $summary | ConvertTo-Json -Depth 5
    $tmp = Join-Path $LogDir "summary.json.tmp"
    [System.IO.File]::WriteAllText($tmp, $json, (New-Object System.Text.UTF8Encoding($false)))
    Move-Item -LiteralPath $tmp -Destination (Join-Path $LogDir "summary.json") -Force
}

# R3-D（§3.6）：外部命令流式执行器——30s 心跳、独立超时（超时错误码 124=TIMEOUT）、
# 输出逐行 tee 到文件与终端（替代"整步缓冲到结束才见输出"）。
# 用法：块内 `Invoke-CiLoggedCommand -Exe cargo -Arguments @('test','--workspace') -TimeoutSec 5400 -LogFile ...`
# 结束把退出码写入 $global:LASTEXITCODE（超时=124，§2.4 内存守护=137）；
# 本函数自身不 throw，保持门禁统计语义。Rust 构建一律走 Invoke-CiCargo（含红线门）。
function Invoke-CiLoggedCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string[]]$Arguments = @(),
        [string]$Cwd = "",
        [int]$TimeoutSec = 0,
        [int]$HeartbeatSec = 30,
        [string]$LogFile = "",
        [string]$Label = "",
        # §2.4 红线 7：运行中内存守护——每次心跳复查，越线即终止进程树并以
        # resource_limited 收口（不允许"跑着跑着把机器拖死"）。
        [switch]$MemoryGuard
    )
    $tag = if ($Label) { $Label } else { $Exe }
    $script:ciLastCommandResourceLimited = $false
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    # PS5.1/.NET Framework 无 ArgumentList：逐 token 安全引用拼 Arguments。
    $psi.Arguments = ($Arguments | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    if ($Cwd) { $psi.WorkingDirectory = $Cwd }
    $outQ = New-Object System.Collections.Concurrent.ConcurrentQueue[string]
    $errQ = New-Object System.Collections.Concurrent.ConcurrentQueue[string]
    $proc = $null
    try {
        $proc = [System.Diagnostics.Process]::Start($psi)
    } catch {
        Write-Host ("    [{0}] 启动失败：{1}" -f $tag, $_.Exception.Message) -ForegroundColor Red
        $global:LASTEXITCODE = 1
        return
    }
    $outEvt = Register-ObjectEvent -InputObject $proc -EventName OutputDataReceived -Action {
        if ($null -ne $EventArgs.Data) { $Event.MessageData.Enqueue([string]$EventArgs.Data) }
    } -MessageData $outQ
    $errEvt = Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived -Action {
        if ($null -ne $EventArgs.Data) { $Event.MessageData.Enqueue([string]$EventArgs.Data) }
    } -MessageData $errQ
    $proc.BeginOutputReadLine()
    $proc.BeginErrorReadLine()
    $writer = $null
    if ($LogFile) {
        $logDir = Split-Path -Parent $LogFile
        if ($logDir -and -not (Test-Path -LiteralPath $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
        $writer = [System.IO.StreamWriter]::new($LogFile, $false, (New-Object System.Text.UTF8Encoding($false)))
    }
    function Drain-Queues {
        $line = $null
        while ($outQ.TryDequeue([ref]$line)) {
            if ($null -ne $line) { Write-Host $line; if ($writer) { $writer.WriteLine($line) } }
        }
        while ($errQ.TryDequeue([ref]$line)) {
            if ($null -ne $line) { Write-Host $line; if ($writer) { $writer.WriteLine($line) } }
        }
    }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $lastBeat = [DateTime]::Now
    $timedOut = $false
    $resourceLimited = $false
    try {
        while (-not $proc.HasExited) {
            Drain-Queues
            if (($sw.Elapsed.TotalSeconds -ge $TimeoutSec) -and ($TimeoutSec -gt 0)) {
                $timedOut = $true
                Write-Host ("    [{0}] TIMEOUT：已运行 {1}s ≥ 上限 {2}s，终止进程树（错误码 124）" -f `
                        $tag, [int]$sw.Elapsed.TotalSeconds, $TimeoutSec) -ForegroundColor Red
                $null = & taskkill /PID $proc.Id /T /F 2>&1
                break
            }
            if (([DateTime]::Now - $lastBeat).TotalSeconds -ge $HeartbeatSec) {
                $lastBeat = [DateTime]::Now
                Write-Host ("    [{0}] beat：已运行 {1}s（timeout={2}s）" -f $tag, [int]$sw.Elapsed.TotalSeconds, $(if ($TimeoutSec -gt 0) { $TimeoutSec } else { 'none' }))
                if ($MemoryGuard) {
                    # §2.4 红线 7：心跳同时是内存复查点；越线即停本轮构建（错误码 137），
                    # 不允许把"整机卡顿"当作可接受的测试代价（红线 8）。
                    $mem = Get-CiMemoryStatus
                    if ($mem.status -eq 'limited') {
                        $resourceLimited = $true
                        Write-Host ("    [{0}] RESOURCE_LIMITED：可用内存 {1} GB / 已用 {2}%（阈值 ≥{3} GB 且 <{4}%）——终止进程树，退出码 137" -f `
                                $tag, $mem.free_gb, $mem.used_percent, $mem.gate.min_free_gb, $mem.gate.max_used_percent) -ForegroundColor Red
                        $null = & taskkill /PID $proc.Id /T /F 2>&1
                        break
                    }
                }
            }
            Start-Sleep -Milliseconds 500
        }
        try { $proc.WaitForExit() } catch { }
        Start-Sleep -Milliseconds 200
        Drain-Queues
        $code = if ($resourceLimited) {
            137
        } elseif ($timedOut) {
            124
        } else {
            try { $proc.ExitCode } catch { 124 }
        }
    } finally {
        if ($resourceLimited) {
            New-CiFailureState
            $script:ciResourceEvents = @($script:ciResourceEvents) + @((Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ") + " " + $tag + " resource_limited（§2.4 红线 7：运行中内存越线，已终止）")
            $script:ciLastCommandResourceLimited = $true
        }
        if ($writer) { $writer.Flush(); $writer.Dispose() }
        Unregister-Event -SubscriptionId $outEvt.Id -ErrorAction SilentlyContinue
        Unregister-Event -SubscriptionId $errEvt.Id -ErrorAction SilentlyContinue
        Remove-Event -SourceIdentifier $outEvt.Name -ErrorAction SilentlyContinue
        Remove-Event -SourceIdentifier $errEvt.Name -ErrorAction SilentlyContinue
        if ($proc) { $proc.Dispose() }
    }
    Write-Host ("    [{0}] 完成：exit={1} 用时 {2}s" -f $tag, $code, [int]$sw.Elapsed.TotalSeconds)
    $global:LASTEXITCODE = $code
}

# 汇总并退出：0 = 全过（含受控 SKIP）；1 = 存在失败（含必装工具缺失）。
function Write-CiSummary {
    param([string]$LogDir = "")
    New-CiFailureState
    $fail = @($script:ciFailures)
    $steps = @($script:ciSteps)
    Write-Host ""
    Write-Host ("==== CI 汇总（{0} 步，{1} 失败）====" -f $steps.Count, $fail.Count) -ForegroundColor Cyan
    foreach ($s in $steps) {
        $matches = @($fail | Where-Object { $_.name -eq $s })
        $mark = if ($matches.Count -gt 0) { "X" } else { "v" }
        Write-Host ("  [{0}] {1}" -f $mark, $s)
    }
    foreach ($f in $fail) {
        Write-Host ("  - FAIL {0} : {1}" -f $f.name, $f.detail) -ForegroundColor Red
    }
    $summary = [ordered]@{
        timestamp   = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        ok          = ($fail.Count -eq 0)
        completed   = $true
        interrupted = [bool]$script:ciInterrupted
        resources   = Get-CiResourceState
        steps       = $steps
        failures    = @($fail | ForEach-Object { "{0}: {1}" -f $_.name, $_.detail })
    }
    if ($LogDir) {
        New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
        $summaryJson = $summary | ConvertTo-Json -Depth 5
        $summaryJson | Set-Content -LiteralPath (Join-Path $LogDir "summary.json") -Encoding UTF8
    }
    if ($fail.Count -gt 0) {
        Write-Host "存在失败步骤，退出码 1" -ForegroundColor Red
        exit 1
    }
    Write-Host "全部通过" -ForegroundColor Green
    exit 0
}