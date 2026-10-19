# 分批（sharded）测试：在内存紧张的机器上完成"全量测试"，不绕过 §2.4 门禁。
#
# 背景（M10 实测）：core / server 的"一次 cargo test -p <pkg>"会同时链接 30–40 个测试
# 二进制，link.exe 峰值把可用内存压到 4.4 GB，被 §2.4 内存门按设计终止（exit 137）。
# 分批只改变**一次链接几个二进制**，不改变门禁阈值：
#   * 每个分片启动前主动检查内存（free >= -MinFreeGB 且 used <= -MaxUsedPct）；
#   * 不满足就等，不在门禁拒绝后绕过（门禁拒绝 = exit 137 → 记下原因，等下一轮重试）；
#   * 逐分片记录真实退出码，最后汇总；任一分片失败则整体非 0（但已跑分片的结果保留）。
#
# 用法：
#   & "<repo>\agent-sdk\scripts\mk-tests-sharded.ps1" -Package owo-agent-core -Tag m10-core
#   & "<repo>\agent-sdk\scripts\mk-tests-sharded.ps1" -Package owo-agent-server -Tag m10-server

param(
    [Parameter(Mandatory = $true)][string]$Package,
    [string]$Tag = 'sharded',
    [double]$MinFreeGB = 6.6,
    [double]$MaxUsedPct = 79,
    # 单个分片在门禁拒绝后的最大重试轮数（每轮等 -RetrySleepSec 秒）
    [int]$MaxRetriesPerShard = 20,
    [int]$RetrySleepSec = 45,
    # 单个分片的墙钟上限（分钟）。M15 实测过一次"心跳存活但 1h43m 零输出"的卡死：
    # 心跳只能证明门禁进程活着，不能证明**测试进程**还活着，所以必须有硬超时。
    # P0 修复：本参数以前是"虚假参数"（声明了却从未传给 cargo 执行器）。现在它真实
    # 落成 Invoke-CiCargo -TimeoutSec：超时由外部监督线程触发 `taskkill /PID <cargo> /T /F`
    # 终止**整棵进程树**（cargo→rustc→link→测试可执行文件）并保留日志，返回 124。
    # 超时（124）与门禁拒绝（137）一样按"失败尝试"处理并重试，绝不无限等。
    [int]$ShardTimeoutMin = 20,
    # 分片用的门禁档位：strict（-j 1，且要求磁盘 ≥20 GB）或 normal（-j 2，磁盘 ≥6 GB）。
    # 两种档位下都显式传 `-j 1`，所以并发上限始终是 1；档位只影响**磁盘门阈值**。
    # 实测：strict 档在构建把卷压到 20 GB 以下时会持续拒绝启动（而这与内存无关）。
    [ValidateSet('normal', 'strict')][string]$PolicyMode = 'strict',
    # 跳过已通过的分片（同一包分批续跑时用），例如 -Skip route_contract_tests,slo_tests
    [string[]]$Skip = @(),
    [switch]$SkipLib
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
. (Join-Path $PSScriptRoot 'resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet | Out-Null
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

function Get-Res { 
    $os = Get-CimInstance Win32_OperatingSystem
    [pscustomobject]@{
        FreeGB  = $os.FreePhysicalMemory / 1MB
        UsedPct = (1 - $os.FreePhysicalMemory / $os.TotalVisibleMemorySize) * 100
    }
}
function Wait-ForRoom([string]$What) {
    while ($true) {
        $s = Get-Res
        if ($s.FreeGB -ge $MinFreeGB -and $s.UsedPct -le $MaxUsedPct) { return $true }
        Write-Host ("[shard] 等待内存窗口（{0}）：free={1:N2} GB used={2:N1}%" -f $What, $s.FreeGB, $s.UsedPct)
        Start-Sleep -Seconds $RetrySleepSec
    }
}

$pkgDir = Join-Path $root ('crates\' + $Package)
if (-not (Test-Path -LiteralPath $pkgDir)) { throw "找不到包目录：$pkgDir" }

$shards = New-Object System.Collections.Generic.List[object]
if (-not $SkipLib) { $shards.Add([pscustomobject]@{ Kind = 'lib'; Name = 'lib' }) }
$testsDir = Join-Path $pkgDir 'tests'
if (Test-Path -LiteralPath $testsDir) {
    Get-ChildItem -LiteralPath $testsDir -File -Filter '*.rs' | Sort-Object Name | ForEach-Object {
        $shards.Add([pscustomobject]@{ Kind = 'test'; Name = [IO.Path]::GetFileNameWithoutExtension($_.Name) })
    }
}
if ($Skip.Count -gt 0) {
    $before = $shards.Count
    $kept = @($shards | Where-Object { $Skip -notcontains $_.Name })
    $shards = New-Object System.Collections.Generic.List[object]
    $kept | ForEach-Object { $shards.Add($_) }
    Write-Host ("[shard] 按 -Skip 跳过 {0} 个分片（{1} → {2}）" -f ($before - $shards.Count), $before, $shards.Count)
}

$logDir = Join-Path $root 'docs\qa\logs'
if (-not (Test-Path -LiteralPath $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

Write-Host ("[shard] {0}：共 {1} 个分片（lib + {2} 个集成测试目标）" -f $Package, $shards.Count, ($shards.Count - 1))

$results = @()
foreach ($shard in $shards) {
    $cargoArgs = @('test', '-p', $Package, '--locked', '-j', '1')
    $cargoArgs += if ($shard.Kind -eq 'lib') { @('--lib') } else { @('--test', $shard.Name) }
    $cargoArgs += @('--', '--test-threads=1')
    $label = "shard-$Tag-$($shard.Name)"
    $log = Join-Path $logDir ("mk-{0}-{1}.log" -f $label, $stamp)

    $code = 1
    $reason = 'failed'
    for ($attempt = 1; $attempt -le $MaxRetriesPerShard; $attempt++) {
        [void](Wait-ForRoom $shard.Name)
        # P0：分片墙钟硬超时（唯一已实现的可靠路径）。
        #   * 为什么不用 Start-Job 包超时：M15 实测 Start-Job 在本机受限环境下会因
        #     命名管道作业传输静默挂住——每个分片"零输出超时"，而同一测试单跑 3 秒通过。
        #     那是工具层故障，不是产品缺陷。
        #   * 因此超时交给 ci-shared 的 Invoke-CiLoggedCommand：它在本进程内 500ms 轮询，
        #     到时 `taskkill /T /F` 杀整棵树（cargo/rustc/link/测试进程），日志保留，
        #     退出码 124；运行中内存越线则 137。心跳只证明门禁进程活着，超时才能证明
        #     测试进程是否已经卡死——两者互补，不可互相替代。
        #   人工排查卡死时看：`Get-Process cargo,rustc,link` 与分片日志大小；
        #     日志为空多为 cargo 没起来（并行 lane 抢 target/ 锁），否则是测试自身阻塞。
        try {
            $code = Invoke-CiCargo -Arguments $cargoArgs -Cwd $root -Label $label -PolicyMode $PolicyMode -LogFile $log -HeartbeatSec 60 -TimeoutSec ($ShardTimeoutMin * 60) -PassThru
        } catch {
            # 门禁在**启动前**拒绝时是抛异常（不是返回 137）；磁盘门与内存门都走这条。
            # 只把门禁拒绝当作"等待后重试"，其它异常原样抛出——否则真实失败会被吞成等待。
            if ($_.Exception.Message -match 'resource_limited|资源红线') {
                Write-Host ("[shard] {0} 启动前被资源门拒绝（第 {1} 轮）" -f $shard.Name, $attempt)
                $code = 137
            } else {
                throw
            }
        }
        if ($code -eq 137 -or $code -eq 124) {
            $reason = if ($code -eq 137) { 'resource_limited' } else { 'timeout' }
            Write-Host ("[shard] {0} exit={1}（137=资源门 / 124=卡死超时），第 {2} 轮；等待后重试" -f $shard.Name, $code, $attempt)
            Start-Sleep -Seconds $RetrySleepSec
            continue
        }
        $reason = if ($code -eq 0) { 'passed' } else { 'failed' }
        break
    }
    Write-Host ("[shard] {0,-34} exit={1} reason={2}" -f $shard.Name, $code, $reason)
    $results += [pscustomobject]@{ shard = $shard.Name; exit = $code; reason = $reason; log = $log }
}

"--- 汇总（{0}）" -f $Package
$results | ForEach-Object { "  {0,-34} exit={1} reason={2}" -f $_.shard, $_.exit, $_.reason }
$failed = @($results | Where-Object { $_.exit -ne 0 })
"  分片总数={0} 失败={1}" -f $results.Count, $failed.Count
# §9.3：报告真实退出码 + 失败原因（资源门/超时/测试失败可区分），不隐藏跳过项。
$summary = [ordered]@{
    schema      = 'owo-sharded-tests/1'
    package     = $Package
    tag         = $Tag
    finished_at = (Get-Date).ToUniversalTime().ToString('o')
    policy_mode = $PolicyMode
    timeout_min = $ShardTimeoutMin
    total       = $results.Count
    failed      = $failed.Count
    ok          = ($failed.Count -eq 0)
    shards      = @($results | ForEach-Object { [ordered]@{ shard = $_.shard; exit = $_.exit; reason = $_.reason; log = $_.log } })
}
$summaryPath = Join-Path $logDir ("sharded-summary-{0}-{1}.json" -f $Package, $stamp)
[System.IO.File]::WriteAllText($summaryPath, (ConvertTo-Json -InputObject $summary -Depth 5) + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
"  报告={0}" -f $summaryPath
if ($failed.Count -gt 0) { exit 1 }
exit 0
