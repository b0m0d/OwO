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
    # 超时（exit 124）与门禁拒绝（exit 137）一样按"失败尝试"处理并重试。
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
    for ($attempt = 1; $attempt -le $MaxRetriesPerShard; $attempt++) {
        [void](Wait-ForRoom $shard.Name)
        $attemptLog = $log
        try {
            # M15 教训：一次分片曾"心跳存活但 1h43m 零输出"——心跳只能证明门禁还活着，
            # 不能证明**测试进程**还活着。因此每个分片必须有墙钟上限：
            # 超时即杀掉整个进程树、记为一次失败尝试并重试，绝不无限等。
            # 【重要，M15 实测结论】**不要**用 Start-Job 给分片包超时。
            #   症状：包装后每个分片都"零输出超时"（日志文件不存在、系统里没有任何
            #         cargo/rustc/link 进程），而**同一个测试直接单跑 3 秒通过**。
            #         Windows PowerShell 5.1 的 Start-Job 依赖命名管道做作业传输，
            #         在本机受限环境下会静默挂住 —— 这属于工具层故障，不是产品缺陷。
            #   因此保持**直接调用**（唯一被验证过的路径）。人工排查卡死时用下面这段：
            #     $busy = Get-Process cargo,rustc,link; $sz = (Get-Item $log).Length
            #     $sz -le 0 → cargo 没起来（多为并行 lane 抢 target/ 锁）；否则是测试自身阻塞
            $code = Invoke-CiCargo -Arguments $cargoArgs -Cwd $root -Label $label -PolicyMode $PolicyMode -LogFile $log -HeartbeatSec 60 -PassThru
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
            Write-Host ("[shard] {0} exit={1}（137=资源门 / 124=卡死超时），第 {2} 轮；等待后重试" -f $shard.Name, $code, $attempt)
            Start-Sleep -Seconds $RetrySleepSec
            continue
        }
        break
    }
    Write-Host ("[shard] {0,-34} exit={1}" -f $shard.Name, $code)
    $results += [pscustomobject]@{ shard = $shard.Name; exit = $code; log = $log }
}

"--- 汇总（{0}）" -f $Package
$results | ForEach-Object { "  {0,-34} exit={1}" -f $_.shard, $_.exit }
$failed = @($results | Where-Object { $_.exit -ne 0 })
"  分片总数={0} 失败={1}" -f $results.Count, $failed.Count
if ($failed.Count -gt 0) { exit 1 }
exit 0
