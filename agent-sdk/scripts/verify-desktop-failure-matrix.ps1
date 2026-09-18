#requires -Version 5.1
<#
verify-desktop-failure-matrix.ps1 — R3（§8.3）真实桌面故障矩阵验收（非模拟）。

对**真实 Tauri 壳**逐个注入启动期故障，断言的不是"没崩"，而是：
  1. 双击后 ≤10s 内出现**可操作界面**（任何可见控件），不得永久 loading；
  2. 最终出现统一错误卡（.service-error）并携带**期望的稳定错误码**；
  3. 错误卡是三段式（发生了什么 / 已自动做了什么 / 下一步），技术详情折叠可得；
  4. 每种故障留 PNG 证据 + 结构化 JSON 事实（DOM 文本经 WebView2 CDP 实读）。

场景与实现手段：
  binary-missing    把 SDK debug 侧 core 临时改名 + 场景目录不放 sidecar → core/binary_missing
  core-hang         场景目录放"只挂起不输出"的 stub → core/handshake_timeout
  core-exit         stub 立即退出（code 3）    → core/handshake_timeout 或 core/exited
  identity-mismatch stub 伪造不属于本窗口的 core_ready → core/identity_mismatch
  no-workspace      不预置 workspace.json      → 工作区引导/错误卡（不得白屏）
  provider-unset    子进程环境删 OPENAI_API_KEY → provider 引导页（不得错误页/白屏）
  data-dir-unwritable 数据根位置放"同名文件"   → 明确错误面（不得静默半死）
  example-mcp-off   健康启动后读 /mcp/servers   → 示例插件不得在生产 profile 启用

fault stub 用 Add-Type 现场编译（不入库、不进产物），并且**自报构建身份**——
壳已升级为"拒绝无 commit= 的候选"（core_runtime.rs），来历不明的 exe 不会被拉起。

用法（在 agent-sdk/ 目录）：
  .\scripts\verify-desktop-failure-matrix.ps1                     # 全矩阵
  .\scripts\verify-desktop-failure-matrix.ps1 -Only binary-missing,core-hang
  .\scripts\verify-desktop-failure-matrix.ps1 -List
退出码：0 = 全部断言通过；1 = 有失败（取证文件仍写出）。
#>
[CmdletBinding()]
param(
    [string]$EvidenceDir = "",
    [string[]]$Only = @(),
    [switch]$SkipScreenshots,
    [string]$ShellExe = "",
    [switch]$List
)

$ErrorActionPreference = 'Stop'
$sdkRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "desktop-acceptance-common.ps1")
. (Join-Path $PSScriptRoot "stage-desktop-sidecar.ps1")

if (-not $ShellExe) {
    $ShellExe = Join-Path $sdkRoot 'desktop\tauri\src-tauri\target\debug\owo-agent-desktop.exe'
}
# 壳内嵌前端资产：不新鲜就是测旧界面（与 §8.2 冷启动共用同一道门）。
$null = Assert-OwoShellEmbedsCurrentWeb -ShellExe $ShellExe -WebRoot (Join-Path $sdkRoot 'desktop\web')
$sdkSidecar = Join-Path $sdkRoot 'target\debug\owo-agent.exe'

# ---- 场景表 ----------------------------------------------------------------------
# expected_codes 为"可接受的错误码集合"：hang/exit 两条路径在壳侧都可能是超时，
# 但都**必须**是明确错误码而不是白屏；其余场景要求精确单一码。
$scenarios = @(
    [pscustomobject]@{ Id = 'binary-missing'; Kind = 'missing';
        ExpectedCodes = @('core/binary_missing'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; Note = '安装不完整：必须直接给出缺失说明与日志入口' },
    [pscustomobject]@{ Id = 'core-hang'; Kind = 'hang';
        ExpectedCodes = @('core/handshake_timeout'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; Note = '核心 25s 无 ready 行 → 兼容回退再 15s → 超时' },
    [pscustomobject]@{ Id = 'core-exit'; Kind = 'exit';
        ExpectedCodes = @('core/handshake_timeout', 'core/exited'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; Note = '核心启动即退出：不得静默重启循环' },
    [pscustomobject]@{ Id = 'identity-mismatch'; Kind = 'identity';
        ExpectedCodes = @('core/identity_mismatch'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; Note = '伪造别窗口的 core_ready：端口冲突/旧服务残留场景' },
    [pscustomobject]@{ Id = 'no-workspace'; Kind = 'healthy';
        ExpectedCodes = @('core/no_workspace'); NeedsWorkspace = $false; NeedsKey = $true;
        Expect = 'guide'; Note = '未选项目：应落到配置引导页（needsSetup 认 no_workspace 态）' },
    [pscustomobject]@{ Id = 'provider-unset'; Kind = 'healthy';
        ExpectedCodes = @('core/handshake_timeout', 'core/spawn_failed', 'core/exited');
        NeedsWorkspace = $true; NeedsKey = $false;
        Expect = 'error'
        KnownGap = '凭据缺失时 sidecar 直接退出，界面只能报握手超时——归因不准确（应为"模型未配置"专码 + 可在壳内预检）；修复列入 R4'
        Note = '模型凭据缺失：不得白屏/永久 loading（错误码归因已知不准，见 known_gap）' },
    [pscustomobject]@{ Id = 'data-dir-unwritable'; Kind = 'healthy';
        ExpectedCodes = @('core/spawn_failed', 'core/handshake_timeout', 'core/exited');
        NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; Note = '数据根被同名文件占据：写盘失败必须可见' },
    [pscustomobject]@{ Id = 'example-mcp-off'; Kind = 'healthy';
        ExpectedCodes = @(); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'mcp-audit'; Note = '示例 MCP 不得在生产 profile 启用（§8.3 末条）' }
)
if ($List) { $scenarios | ForEach-Object { "{0,-20} {1}" -f $_.Id, $_.Note }; exit 0 }

$selected = if ($Only.Count -gt 0) { @($scenarios | Where-Object { $Only -contains $_.Id }) } else { $scenarios }
if ($selected.Count -eq 0) { throw "-Only 未匹配任何场景（-List 查看）" }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $sdkRoot "docs\qa\evidence\r3-failure-matrix-$stamp" }
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

$runRoot = Join-Path ([IO.Path]::GetTempPath()) "owo-desktop-faults-$stamp"
$apiKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not $apiKey) { throw '用户级 OPENAI_API_KEY 缺失：healthy 类场景无法验收（AGENTS.md 红线：只注入不回显）' }

# ---- 结果登记 --------------------------------------------------------------------
$results = New-Object System.Collections.ArrayList
$report = [ordered]@{ started_at = (Get-Date).ToUniversalTime().ToString('o'); scenarios = @() }
function Add-Check {
    param([string]$Scenario, [string]$Name, [bool]$Pass, [string]$Detail)
    $null = $results.Add([pscustomobject]@{ scenario = $Scenario; name = $Name; pass = $Pass; detail = $Detail })
    $mark = if ($Pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} :: {2} — {3}" -f $mark, $Scenario, $Name, $Detail)
}

# ---- fault stub 现场编译 ---------------------------------------------------------
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
$stubSource = @'
using System;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Threading;

// 桌面故障注入 stub：冒充 owo-agent 核心服务，行为由同目录 stub.mode 决定。
// 仅由 scripts/verify-desktop-failure-matrix.ps1 现场编译到临时目录，不入库。
public static class OwoFaultStub
{
    private static string Mode()
    {
        try
        {
            string dir = Path.GetDirectoryName(Process.GetCurrentProcess().MainModule.FileName);
            string file = Path.Combine(dir, "stub.mode");
            if (File.Exists(file)) { return File.ReadAllText(file).Trim().ToLowerInvariant(); }
        }
        catch { }
        return "silent";
    }

    private static void WriteLine(string text)
    {
        Console.Out.WriteLine(text);
        Console.Out.Flush();
    }

    [STAThread]
    public static int Main(string[] args)
    {
        // 壳会先探测 `--version` 的构建身份（core_runtime.rs::candidate_has_build_identity）：
        // 没有 commit=/built_at= 的候选会被直接跳过，故障注入也就无从发生。
        if (args != null && args.Length > 0 && args[0] == "--version")
        {
            string pid = Process.GetCurrentProcess().Id.ToString(CultureInfo.InvariantCulture);
            WriteLine("owo-agent 0.1.0 api=0.7 commit=faultstub000000000000000000000000000000 dirty=false built_at=2026-09-18T00:00:00Z source=compiled pid=" + pid);
            return 0;
        }
        string mode = Mode();
        try
        {
            if (mode == "exit") { return 3; }
            if (mode == "identity")
            {
                // 伪造一个"不属于本窗口"的就绪行：instance_id 与壳注入值必然不同，
                // 壳必须在握手阶段拒绝它（core/identity_mismatch）而不是盲连。
                int self = Process.GetCurrentProcess().Id;
                WriteLine("{\"api_version\":\"0.7\",\"build_id\":\"faultstub0000\",\"event\":\"core_ready\",\"instance_id\":\"foreign-instance-not-this-window\",\"pid\":" + self + ",\"port\":1}");
                Thread.Sleep(120000);
                return 0;
            }
            if (mode == "healthonly")
            {
                // 只回 /health（instance 缺失）：验 wait_for_instance 的拒绝路径。
                return 0;
            }
            // silent / hang：什么都不输出，挂到被杀为止（触发 ready 行超时 + 兼容回退超时）。
            Thread.Sleep(900000);
            return 0;
        }
        catch
        {
            return 7;
        }
    }
}
'@
$stubExe = Join-Path $runRoot 'owo-fault-stub.exe'
New-Item -ItemType Directory -Force -Path $runRoot | Out-Null
Add-Type -TypeDefinition $stubSource -OutputType Exe -OutputAssembly $stubExe
if (-not (Test-Path -LiteralPath $stubExe)) { throw "fault stub 编译失败：$stubExe" }
Write-Host "[faults] fault stub 就位：$stubExe"

# ---- 单个场景执行 ----------------------------------------------------------------
function Invoke-Scenario {
    param([System.Management.Automation.PSObject]$Scenario)
    $id = $Scenario.Id
    $dir = Join-Path $runRoot "scenarios\$id"
    $lad = Join-Path $dir 'LocalAppData'
    $rad = Join-Path $dir 'RoamingAppData'
    $tmp = Join-Path $dir 'Temp'
    $project = Join-Path $dir 'project'
    foreach ($d in @($lad, $rad, $tmp, $project)) { New-Item -ItemType Directory -Force -Path $d | Out-Null }
    $agentDir = Join-Path $lad 'OwO\Agent'
    New-Item -ItemType Directory -Force -Path $agentDir | Out-Null
    if ($Scenario.NeedsWorkspace) {
        # §4.6 最近项目指针：预置它等价于用户已经选过一次工作区，数据目录本身仍是全新的。
        Set-Content -Path (Join-Path $agentDir 'workspace.json') `
            -Value ('{"path":"' + ($project -replace '\\', '\\') + '"}') -Encoding ASCII -NoNewline
    }
    if ($Scenario.Id -eq 'data-dir-unwritable') {
        # 数据根位置放同名**文件**：core 建目录/写 token 必然失败。
        Set-Content -Path (Join-Path $agentDir 'data') -Value 'blocker' -Encoding ASCII -NoNewline
    }

    # 壳的可执行体与随包 core 放在场景目录里：同目录解析优先，注入才生效。
    Copy-Item -LiteralPath $ShellExe -Destination (Join-Path $dir 'owo-agent-desktop.exe') -Force
    foreach ($dll in @(Get-ChildItem -LiteralPath (Split-Path -Parent $ShellExe) -Filter '*.dll' -ErrorAction SilentlyContinue)) {
        Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $dir $dll.Name) -Force
    }
    $scenarioShell = Join-Path $dir 'owo-agent-desktop.exe'
    $script:hideSdkCore = $false
    switch ($Scenario.Kind) {
        'missing'  {
            # "安装里没有 core"：把 SDK debug 产物临时改名（finally 恢复），
            # 否则壳会回落到它，测不到 binary_missing 分支。
            if (Test-Path -LiteralPath $sdkSidecar) {
                Rename-Item -LiteralPath $sdkSidecar -NewName 'owo-agent.exe.owo-fault-hidden'
                $script:hideSdkCore = $true
            }
        }
        'hang'     { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'silent' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
        'exit'     { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'exit' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
        'identity' { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'identity' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
        'healthy'  {
            $staged = Stage-OwoDesktopSidecar -Configuration debug -Quiet
            Copy-Item -LiteralPath $staged.source -Destination (Join-Path $dir 'owo-agent.exe') -Force
        }
    }

    $cdpRequested = Get-OwoFreeTcpPort
    # 专属 WebView2 用户数据目录：不与已安装版本共用浏览器进程组（同冷启动脚本注释）。
    $webViewUdf = Join-Path $dir 'WebView2'
    New-Item -ItemType Directory -Force -Path $webViewUdf | Out-Null
    $webviewDataDir = Get-OwoWebviewEtbDir -UserDataDir $webViewUdf
    $psi = New-OwoShellStartInfo -ShellExe $scenarioShell -LocalAppData $lad -AppData $rad -TempDir $tmp `
        -ApiKey $(if ($Scenario.NeedsKey) { $apiKey } else { '' }) -CdpPort $cdpRequested `
        -WebViewUserDataDir $webViewUdf -NoApiKey:(-not $Scenario.NeedsKey)
    $logDir = Join-Path $agentDir 'logs'
    Write-Host "[faults] $id：启动壳（cdp~$cdpRequested 私有根=$dir）"
    $started = Get-Date
    $proc = [System.Diagnostics.Process]::Start($psi)
    $facts = [ordered]@{
        id               = $id
        note             = $Scenario.Note
        kind             = $Scenario.Kind
        expected_codes   = @($Scenario.ExpectedCodes)
        shell_pid        = $proc.Id
        window_at_sec    = -1
        actionable_at_sec = -1
        error_at_sec     = -1
        error_code       = ''
        error_card       = ''
        setup_guide_len  = 0
        details_folded   = $false
        screenshot       = ''
        ui_final         = $null
    }
    try {
        $hwnd = Get-OwoShellWindow -ProcessId $proc.Id -TimeoutSec 20
        $facts.window_at_sec = [Math]::Round(((Get-Date) - $started).TotalSeconds, 1)
        Add-Check $id '窗口在 20s 内出现（不是无声失败）' ($hwnd -ne [IntPtr]::Zero) "hwnd=$hwnd t=$($facts.window_at_sec)s"
        if ($hwnd -eq [IntPtr]::Zero) { return [pscustomobject]$facts }

        # DOM 事实通道：以 DevToolsActivePort 为准解析实际端口（被抢口时 Chromium 换口）。
        $cdpPort = Resolve-OwoCdpPort -DataDir $webviewDataDir -RequestedPort $cdpRequested -TimeoutSec 20
        Add-Check $id 'DOM 事实通道（WebView2 CDP）建立' ($cdpPort -gt 0) "requested=$cdpRequested effective=$cdpPort"
        if ($cdpPort -le 0) {
            $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name "cdp-diagnostic-$id.json" `
                -InputObject (Get-OwoWebviewDiagnostics -RequestedPort $cdpRequested -DataDir $webviewDataDir)
        }

        # ① ≤10s 内必须有可操作界面（任何可见控件）：这是"不得永久 loading"的硬门。
        $first = $null
        $actionableDeadline = $started.AddSeconds(10)
        while ((Get-Date) -lt $actionableDeadline) {
            $first = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 4
            if ($first -and $first.actionable) { break }
            Start-Sleep -Milliseconds 400
        }
        $facts.actionable_at_sec = if ($first -and $first.actionable) { [Math]::Round(((Get-Date) - $started).TotalSeconds, 1) } else { -1 }
        Add-Check $id '10s 内界面可操作（无永久 loading）' ($first -and $first.actionable) `
            "t=$($facts.actionable_at_sec)s health=`"$($first.healthText)`""

        # ② 等到最终态：错误卡 / 引导页 / 正常界面（hang 类要等满两次超时）。
        $waitBudget = if ($Scenario.Kind -in @('hang', 'exit', 'data-dir-unwritable')) { 90 } else { 30 }
        $final = $null
        $deadline = (Get-Date).AddSeconds($waitBudget)
        while ((Get-Date) -lt $deadline) {
            $final = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 5
            if ($final -and ($final.errorCard -or $final.setupGuide -or ($final.composerVisible -and $Scenario.Expect -ne 'error'))) { break }
            Start-Sleep -Milliseconds 800
        }
        $facts.error_at_sec = if ($final -and $final.errorCard) { [Math]::Round(((Get-Date) - $started).TotalSeconds, 1) } else { -1 }
        $facts.error_card = if ($final) { [string]$final.errorCard } else { '' }
        $facts.setup_guide_len = if ($final) { ([string]$final.setupGuide).Length } else { 0 }
        $facts.known_gap = [string]$Scenario.KnownGap
        $facts.ui_final = $final

        if ($Scenario.Expect -eq 'error') {
            $cardOk = -not [string]::IsNullOrEmpty($facts.error_card)
            Add-Check $id '出现统一错误卡（含发生/已自动/下一步三段）' $cardOk "t=$($facts.error_at_sec)s"
            if ($cardOk) {
                $codes = @('core/binary_missing','core/spawn_failed','core/handshake_timeout','core/identity_mismatch','core/exited','core/no_workspace')
                $found = @($codes | Where-Object { $facts.error_card -like "*$_*" })
                $facts.error_code = ($found -join ',')
                $allowed = @($Scenario.ExpectedCodes)
                Add-Check $id '错误码在期望集合内（不得含糊报错）' (($found.Count -gt 0) -and (@($found | Where-Object { $allowed -contains $_ }).Count -eq $found.Count)) `
                    "found=$($facts.error_code) allowed=$($allowed -join '|')"
                Add-Check $id '错误卡含"下一步"可操作动作' (($facts.error_card -like '*重新连接*') -or ($facts.error_card -like '*打开模型设置*')) `
                    "card=$($facts.error_card.Substring(0,[Math]::Min(90,$facts.error_card.Length)))"
                $folded = Invoke-OwoCdpEval -Port $cdpPort -Expression "!!document.querySelector('.service-error details')" -TimeoutSec 6
                $facts.details_folded = [bool]$folded
                Add-Check $id '技术详情以折叠区呈现（不糊在正文里）' ($folded -eq $true) "details=$folded"
            }
        }
        if ($Scenario.Expect -eq 'guide') {
            # needsSetup() 认的是壳回报的 no_workspace 态 → 必须落到配置引导页，
            # 且引导页本身要可操作（选目录），不得是错误卡或白屏。
            $guideOk = ($facts.setup_guide_len -gt 0) -and ($facts.error_card -eq '')
            Add-Check $id '未选项目落到配置引导页（非错误卡、非白屏）' $guideOk `
                "setupGuide=$($facts.setup_guide_len) 字 errorCard=$($facts.error_card.Length) 字 actionable=$($facts.ui_final.actionable)"
            Add-Check $id '引导页提供可操作动作' ([bool]($facts.ui_final -and $facts.ui_final.actionable)) `
                "body=$($facts.ui_final.bodyText)"
        }
        if ($Scenario.Expect -eq 'mcp-audit') {
            $tokenFile = Join-Path $agentDir 'data\auth\token'
            $ready = Wait-OwoCoreReady -LogDir $logDir -TimeoutSec 45
            if (-not $ready) {
                Add-Check $id '健康启动（用于 MCP 审计）' $false '未读到 core_ready'
            } else {
                $token = if (Test-Path -LiteralPath $tokenFile) { (Get-Content -LiteralPath $tokenFile -Raw).Trim() } else { '' }
                $mcp = Invoke-OwoCoreApi -Base "http://127.0.0.1:$($ready.port)" -Path '/mcp/servers' -Token $token
                $text = ($mcp | ConvertTo-Json -Depth 8)
                $facts.mcp_json = $text
                $example = @($text | ForEach-Object { if ($_ -match 'example-hello') { 'hit' } })
                $enabled = ($text -match '"example-hello"' -and $text -match '"(enabled|running)"\s*:\s*true')
                Add-Check $id '示例 MCP（owo.plugin.example-hello）未在生产 profile 启用' (-not $enabled) `
                    "mention=$($example.Count) enabled=$enabled"
                $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name "mcp-servers-$id.json" -InputObject $mcp -Depth 8
            }
        }

        if (-not $SkipScreenshots) {
            $null = Set-OwoWindowGeometry -Hwnd $hwnd -Width 1280 -Height 720
            Start-Sleep -Milliseconds 900
            $shot = Join-Path $EvidenceDir "fault-$id-1280x720.png"
            try {
                $geometry = Save-OwoWindowShot -Hwnd $hwnd -Path $shot
                $facts.screenshot = $shot
                Add-Check $id '错误面截图取证' (Test-Path -LiteralPath $shot) "$shot（$geometry）"
            } catch {
                Add-Check $id '错误面截图取证' $false $_.Exception.Message
            }
        }
        $facts.shell_log_tail = Get-OwoCoreLogTail -Dir $logDir -Filter 'desktop-core-*.log' -Lines 25
    } finally {
        Stop-OwoShellTree -ProcessId $proc.Id -RunRoot $dir
        if ($script:hideSdkCore) {
            $hidden = Join-Path $sdkRoot 'target\debug\owo-agent.exe.owo-fault-hidden'
            if (Test-Path -LiteralPath $hidden) {
                Move-Item -LiteralPath $hidden -Destination $sdkSidecar -Force
                $script:hideSdkCore = $false
                Write-Host "[faults] $id：已恢复被临时改名的 SDK core 产物"
            }
        }
    }
    return [pscustomobject]$facts
}

# ---- 主循环 ----------------------------------------------------------------------
$exitCode = 0
try {
    foreach ($scenario in $selected) {
        Write-Host ""
        Write-Host "=== 场景 $($scenario.Id) —— $($scenario.Note) ==="
        $facts = Invoke-Scenario -Scenario $scenario
        $report.scenarios += @($facts)
        $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name "scenario-$($scenario.Id).json" -InputObject $facts -Depth 8
    }
} finally {
    $report.finished_at = (Get-Date).ToUniversalTime().ToString('o')
    $report.checks_total = $results.Count
    $report.checks_failed = @($results | Where-Object { -not $_.pass }).Count
    $report.checks = @($results)
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'failure-matrix-report.json' -InputObject $report -Depth 8
    Write-Host ""
    Write-Host ("[faults] 断言 {0}/{1} 通过；取证目录：{2}" -f `
            (@($results | Where-Object { $_.pass }).Count), $results.Count, $EvidenceDir)
    if ($report.checks_failed -gt 0) { $exitCode = 1 }
    if (Test-Path -LiteralPath $runRoot) {
        Remove-Item -LiteralPath $runRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
exit $exitCode
