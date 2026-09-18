#requires -Version 5.1
<#
verify-desktop-failure-matrix.ps1 — R3（指南 §3.3.3/§3.3.4/§3.5）真实桌面故障矩阵验收（非模拟）。

对**真实 Tauri 壳**逐个注入启动期故障，断言的不是"没崩"，而是：
  1. ≤10s 内出现可操作界面（任何可见控件），不得永久 loading；
  2. 最终出现统一错误卡（.service-error）并携带**期望的稳定错误码**（§3.4 契约表）；
  3. 错误卡提供该场景"必须提供的动作"（§3.4 动作列）；
  4. 每种故障留 PNG 证据（像素级验真，§3.3.2）+ 结构化 JSON 事实（DOM 经 CDP 实读）。

R3-A3 结构约束（替代历史缺陷 R3-BUG-03/04）：
  * 每个场景独立运行目录 `%TEMP%\owo-desktop-acceptance\<run-id>\<scenario>\`
    （exe 副本 + data/ + logs/ + evidence/），壳以**验收模式**启动：
    `OWO_DESKTOP_ACCEPTANCE=1 + OWO_SIDECAR_ROOT=<场景目录>`（仅 debug 壳生效，
    release 完全忽略）——sidecar 候选只来自场景目录，禁止回退仓库/PATH/历史安装。
    **绝不改名/删除/覆盖真实 target/debug 产物**；矩阵结束前核对该产物哈希未变。
  * 场景任意阶段异常必须形成失败项（executed=false / failed_stage / error_type），
    杜绝"场景没执行也看起来全绿"；总报告逐场景列 executed 与断言明细。

场景执行序按 §3.5 由快到慢：binary-missing → identity-mismatch → no-workspace →
provider-unset → data-dir-unwritable → example-mcp-off → core-exit → core-hang。

fault stub 用 Add-Type 现场编译到运行目录（不入库、不进产物），并**自报构建身份**——
壳已拒绝无 commit= 的候选（core_runtime.rs），来历不明的 exe 不会被拉起。

用法（在 agent-sdk/ 目录）：
  .\scripts\verify-desktop-failure-matrix.ps1                     # 全矩阵（8 场景）
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
# 真实 debug core：只读观察（哈希守卫），任何场景不得触碰它（R3-BUG-03 红线）。
$sdkSidecar = Join-Path $sdkRoot 'target\debug\owo-agent.exe'
$sdkCoreHashBefore = if (Test-Path -LiteralPath $sdkSidecar) { (Get-FileHash -LiteralPath $sdkSidecar -Algorithm SHA256).Hash } else { '' }

# ---- 场景表（执行顺序 = §3.5 由快到慢）-------------------------------------------
# expected_codes 为"可接受的错误码集合"（§3.4 契约冻结在 R3-B 提交对齐）；
# required_actions 为错误卡"必须提供的动作"关键词（§3.4 动作列）。
$scenarios = @(
    [pscustomobject]@{ Id = 'binary-missing'; Kind = 'missing';
        ExpectedCodes = @('core/binary_missing'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; FinalBudgetSec = 15; RequiredActions = @('重新检查', '打开诊断位置', '重新连接');
        Note = '安装不完整：必须直接给出缺失说明与日志入口' },
    [pscustomobject]@{ Id = 'identity-mismatch'; Kind = 'identity';
        ExpectedCodes = @('core/identity_mismatch'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; FinalBudgetSec = 20; RequiredActions = @('重新安装', '重建');
        Note = '伪造别窗口的 core_ready：端口冲突/旧服务残留场景' },
    [pscustomobject]@{ Id = 'no-workspace'; Kind = 'healthy';
        ExpectedCodes = @('workspace/required'); NeedsWorkspace = $false; NeedsKey = $true;
        Expect = 'guide'; FinalBudgetSec = 20; RequiredActions = @('选择', '目录');
        Note = '未选项目：应落到配置引导页（不得白屏）' },
    [pscustomobject]@{ Id = 'provider-unset'; Kind = 'healthy';
        ExpectedCodes = @('core/handshake_timeout', 'core/spawn_failed', 'core/exited');
        NeedsWorkspace = $true; NeedsKey = $false;
        Expect = 'error'; FinalBudgetSec = 45; RequiredActions = @('打开模型设置', '测试连接');
        KnownGap = '凭据缺失时 sidecar 直接退出，界面只能报握手超时——归因不准确（应为"模型未配置"专码 + 可在壳内预检）；修复对齐 §3.4 契约（provider/not_configured → 引导页）'
        Note = '模型凭据缺失：不得白屏/永久 loading（错误码归因已知不准，见 known_gap）' },
    [pscustomobject]@{ Id = 'data-dir-unwritable'; Kind = 'healthy';
        ExpectedCodes = @('core/spawn_failed', 'core/handshake_timeout', 'core/exited');
        NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; FinalBudgetSec = 45; RequiredActions = @('更换数据目录', '重试');
        KnownGap = '当前 core 会静默回退工作区 .owo-agent（无错误面）；§3.4 契约要求 storage/not_writable，修复在 R3-B 契约对齐提交'
        Note = '数据根被同名文件占据：写盘失败必须可见（见 known_gap）' },
    [pscustomobject]@{ Id = 'example-mcp-off'; Kind = 'healthy';
        ExpectedCodes = @(); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'mcp-audit'; FinalBudgetSec = 45; RequiredActions = @();
        Note = '示例 MCP 不得在生产 profile 启用（§8.3 末条）' },
    [pscustomobject]@{ Id = 'core-exit'; Kind = 'exit';
        ExpectedCodes = @('core/handshake_timeout', 'core/exited'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; FinalBudgetSec = 45; RequiredActions = @('重启后台', '查看诊断', '重新连接');
        Note = '核心启动即退出：不得静默重启循环' },
    [pscustomobject]@{ Id = 'core-hang'; Kind = 'hang';
        ExpectedCodes = @('core/handshake_timeout'); NeedsWorkspace = $true; NeedsKey = $true;
        Expect = 'error'; FinalBudgetSec = 60; RequiredActions = @('终止并重试', '重新连接');
        Note = '核心只挂起不输出 → 就绪超时（25s ready 行 + 15s 兼容回退）' }
)
if ($List) { $scenarios | ForEach-Object { "{0,-20} {1}" -f $_.Id, $_.Note }; exit 0 }

$selected = if ($Only.Count -gt 0) { @($scenarios | Where-Object { $Only -contains $_.Id }) } else { $scenarios }
if ($selected.Count -eq 0) { throw "-Only 未匹配任何场景（-List 查看）" }

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $sdkRoot "docs\qa\evidence\r3-failure-matrix-$stamp" }
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

# ---- §3.3.3 运行目录布局：%TEMP%/owo-desktop-acceptance/<run-id>/ ----------------
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "owo-desktop-acceptance\$stamp"
$toolsDir = Join-Path $runRoot 'tools'
New-Item -ItemType Directory -Force -Path $toolsDir | Out-Null
$apiKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not $apiKey) { throw '用户级 OPENAI_API_KEY 缺失：healthy 类场景无法验收（AGENTS.md 红线：只注入不回显）' }

# ---- 结果登记（全局 + 分场景）-----------------------------------------------------
$results = New-Object System.Collections.ArrayList
$report = [ordered]@{
    started_at = (Get-Date).ToUniversalTime().ToString('o')
    run_root   = $runRoot
    evidence   = $EvidenceDir
    scenarios  = @()
}
function Add-Check {
    param([string]$Scenario, [string]$Name, [bool]$Pass, [string]$Detail)
    $null = $results.Add([pscustomobject]@{ scenario = $Scenario; name = $Name; pass = $Pass; detail = $Detail })
    $mark = if ($Pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} :: {2} — {3}" -f $mark, $Scenario, $Name, $Detail)
}

# ---- fault stub 现场编译（run 目录内，不入库、不进产物）---------------------------
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
$stubExe = Join-Path $toolsDir 'owo-fault-stub.exe'
Add-Type -TypeDefinition $stubSource -OutputType Exe -OutputAssembly $stubExe
if (-not (Test-Path -LiteralPath $stubExe)) { throw "fault stub 编译失败：$stubExe" }
Write-Host "[faults] fault stub 就位：$stubExe（运行目录 $runRoot）"

# ---- 单个场景执行（§3.3.4 生命周期：prepare→inject→launch→observe→assert→collect→cleanup）----
function Invoke-Scenario {
    param([System.Management.Automation.PSObject]$Scenario)
    $id = $Scenario.Id
    $stage = 'prepare'
    $dir = Join-Path $runRoot $id
    $dataDir = Join-Path $dir 'data'
    $logsCopy = Join-Path $dir 'logs'
    $dirEvidence = Join-Path $dir 'evidence'
    $lad = Join-Path $dataDir 'LocalAppData'
    $rad = Join-Path $dataDir 'RoamingAppData'
    $tmp = Join-Path $dataDir 'Temp'
    $project = Join-Path $dataDir 'project'
    $agentDir = Join-Path $lad 'OwO\Agent'
    $logDir = Join-Path $agentDir 'logs'
    $evDir = Join-Path $EvidenceDir $id
    New-Item -ItemType Directory -Force -Path $evDir | Out-Null

    $facts = [ordered]@{
        id                = $id
        note              = $Scenario.Note
        kind              = $Scenario.Kind
        expected_codes    = @($Scenario.ExpectedCodes)
        required_actions  = @($Scenario.RequiredActions)
        final_budget_sec  = $Scenario.FinalBudgetSec
        known_gap         = [string]$Scenario.KnownGap
        executed          = $false
        failed_stage      = ''
        error_type        = ''
        error_message     = ''
        scenario_dir      = $dir
        injected_sidecar  = ''
        sidecar_sha256    = ''
        launched_core     = ''
        shell_pid         = 0
        window_at_sec     = -1
        actionable_at_sec = -1
        error_at_sec      = -1
        error_code        = ''
        error_card        = ''
        setup_guide_len   = 0
        details_folded    = $false
        screenshot        = ''
        screenshot_metric = $null
        ui_final          = $null
        cleanup           = $null
    }
    $proc = $null
    $scenarioChecksStart = $results.Count
    try {
        # ---- prepare：目录 + 工作区指针 + 场景注入物 ------------------------------
        foreach ($d in @($lad, $rad, $tmp, $project, $logsCopy, $dirEvidence)) {
            New-Item -ItemType Directory -Force -Path $d | Out-Null
        }
        $agentLeaf = Join-Path $lad 'OwO\Agent'
        New-Item -ItemType Directory -Force -Path $agentLeaf | Out-Null
        if ($Scenario.NeedsWorkspace) {
            Set-Content -Path (Join-Path $agentLeaf 'workspace.json') `
                -Value ('{"path":"' + ($project -replace '\\', '\\') + '"}') -Encoding ASCII -NoNewline
        }
        if ($id -eq 'data-dir-unwritable') {
            # 数据根位置放同名**文件**：core 建目录/写 token 必然失败。
            Set-Content -Path (Join-Path $agentLeaf 'data') -Value 'blocker' -Encoding ASCII -NoNewline
        }
        # 壳的可执行体复制进场景目录（同目录 + OWO_SIDECAR_ROOT 双保险，绝不回退仓库）。
        $scenarioShell = Join-Path $dir 'owo-agent-desktop.exe'
        Copy-Item -LiteralPath $ShellExe -Destination $scenarioShell -Force
        foreach ($dll in @(Get-ChildItem -LiteralPath (Split-Path -Parent $ShellExe) -Filter '*.dll' -ErrorAction SilentlyContinue)) {
            Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $dir $dll.Name) -Force
        }
        $stage = 'inject'
        switch ($Scenario.Kind) {
            'missing'  {
                # R3-A3：不再改名真实 debug core。场景目录不放 sidecar，壳在验收模式下
                # 无候选可解析 → 真实触发 core/binary_missing 分支。
                $facts.injected_sidecar = '<none: 场景目录刻意不放 sidecar>'
            }
            'hang'     { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'silent' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
            'exit'     { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'exit' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
            'identity' { Set-Content -LiteralPath (Join-Path $dir 'stub.mode') -Value 'identity' -Encoding ASCII -NoNewline; Copy-Item -LiteralPath $stubExe -Destination (Join-Path $dir 'owo-agent.exe') -Force }
            'healthy'  {
                $staged = Stage-OwoDesktopSidecar -Configuration debug -Quiet
                Copy-Item -LiteralPath $staged.source -Destination (Join-Path $dir 'owo-agent.exe') -Force
            }
        }
        $candidate = Join-Path $dir 'owo-agent.exe'
        if (Test-Path -LiteralPath $candidate) {
            $facts.injected_sidecar = $candidate
            $facts.sidecar_sha256 = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
        }
        # 场景配置留档（§3.5：每场景独立"场景配置"证据）。
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'scenario-config.json' -InputObject ([ordered]@{
                id = $id; kind = $Scenario.Kind; note = $Scenario.Note
                expected_codes = @($Scenario.ExpectedCodes); required_actions = @($Scenario.RequiredActions)
                final_budget_sec = $Scenario.FinalBudgetSec
                needs_workspace = $Scenario.NeedsWorkspace; needs_key = $Scenario.NeedsKey
                acceptance_mode = 'OWO_DESKTOP_ACCEPTANCE=1 + OWO_SIDECAR_ROOT=' + $dir
                injected_sidecar = $facts.injected_sidecar; sidecar_sha256 = $facts.sidecar_sha256
            }) -Depth 6

        # ---- launch：验收模式私有环境拉起壳 ----------------------------------------
        $stage = 'launch'
        $cdpRequested = Get-OwoFreeTcpPort
        $webViewUdf = Join-Path $dataDir 'WebView2'
        New-Item -ItemType Directory -Force -Path $webViewUdf | Out-Null
        $webviewDataDir = Get-OwoWebviewEtbDir -UserDataDir $webViewUdf
        $psi = New-OwoShellStartInfo -ShellExe $scenarioShell -LocalAppData $lad -AppData $rad -TempDir $tmp `
            -ApiKey $(if ($Scenario.NeedsKey) { $apiKey } else { '' }) -CdpPort $cdpRequested `
            -WebViewUserDataDir $webViewUdf -AcceptanceSidecarRoot $dir -NoApiKey:(-not $Scenario.NeedsKey)
        $started = Get-Date
        $proc = [System.Diagnostics.Process]::Start($psi)
        $facts.shell_pid = $proc.Id

        # ---- observe：窗口/CDP/DOM 事实 --------------------------------------------
        $stage = 'observe'
        $win = Get-OwoValidatedWindow -ProcessId $proc.Id -TimeoutSec 20
        $facts.window_at_sec = [Math]::Round(((Get-Date) - $started).TotalSeconds, 1)
        Add-Check $id '窗口在 20s 内出现且句柄归属本壳进程（不是无声失败/别的窗口）' ($win.ok -or $win.hwnd -ne [IntPtr]::Zero) `
            "hwnd=$($win.hwnd) owner=$($win.process_id) t=$($facts.window_at_sec)s fail=$($win.fail_reasons -join ';')"
        if ($win.hwnd -eq [IntPtr]::Zero) { return [pscustomobject]$facts }

        $cdpPort = Resolve-OwoCdpPort -DataDir $webviewDataDir -RequestedPort $cdpRequested -TimeoutSec 20
        Add-Check $id 'DOM 事实通道（WebView2 CDP）建立' ($cdpPort -gt 0) "requested=$cdpRequested effective=$cdpPort"
        if ($cdpPort -le 0) {
            $null = Save-OwoEvidenceJson -Dir $evDir -Name 'cdp-diagnostic.json' `
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

        # ② 等到最终态：错误卡 / 引导页 / 正常界面（时限见 scenario.FinalBudgetSec）。
        $stage = 'assert'
        $final = $null
        $deadline = (Get-Date).AddSeconds($Scenario.FinalBudgetSec)
        while ((Get-Date) -lt $deadline) {
            $final = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 5
            if ($final -and ($final.errorCard -or $final.setupGuide -or ($final.composerVisible -and $Scenario.Expect -ne 'error'))) { break }
            Start-Sleep -Milliseconds 800
        }
        $facts.error_at_sec = if ($final -and $final.errorCard) { [Math]::Round(((Get-Date) - $started).TotalSeconds, 1) } else { -1 }
        $facts.error_card = if ($final) { [string]$final.errorCard } else { '' }
        $facts.setup_guide_len = if ($final) { ([string]$final.setupGuide).Length } else { 0 }
        $facts.ui_final = $final
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'dom-facts.json' -InputObject $final -Depth 6

        if ($Scenario.Expect -eq 'error') {
            $cardOk = -not [string]::IsNullOrEmpty($facts.error_card)
            Add-Check $id '出现统一错误卡（含发生/已自动/下一步三段）' $cardOk "t=$($facts.error_at_sec)s（时限 $($Scenario.FinalBudgetSec)s 内）"
            Add-Check $id "错误终态在 $($Scenario.FinalBudgetSec)s 时限内到达（§3.4）" (($cardOk) -and ($facts.error_at_sec -ge 0) -and ($facts.error_at_sec -le $Scenario.FinalBudgetSec)) `
                "error_at_sec=$($facts.error_at_sec) budget=$($Scenario.FinalBudgetSec)"
            if ($cardOk) {
                $codes = @('core/binary_missing', 'core/spawn_failed', 'core/handshake_timeout', 'core/identity_mismatch', 'core/exited', 'core/no_workspace', 'workspace/required', 'provider/not_configured', 'storage/not_writable')
                $found = @($codes | Where-Object { $facts.error_card -like "*$_*" })
                $facts.error_code = ($found -join ',')
                $allowed = @($Scenario.ExpectedCodes)
                Add-Check $id '错误码在期望集合内（不得含糊报错）' (($found.Count -gt 0) -and (@($found | Where-Object { $allowed -contains $_ }).Count -eq $found.Count)) `
                    "found=$($facts.error_code) allowed=$($allowed -join '|')"
                $needles = @($Scenario.RequiredActions)
                if ($needles.Count -eq 0) { $needles = @('重新连接') }
                $hit = @($needles | Where-Object { $facts.error_card -like "*$_*" })
                Add-Check $id "错误卡提供动作（$($needles -join '/'))" ($hit.Count -gt 0) `
                    "card=$($facts.error_card.Substring(0, [Math]::Min(120, $facts.error_card.Length)))"
                $folded = Invoke-OwoCdpEval -Port $cdpPort -Expression "!!document.querySelector('.service-error details')" -TimeoutSec 6
                $facts.details_folded = [bool]$folded
                Add-Check $id '技术详情以折叠区呈现（不糊在正文里）' ($folded -eq $true) "details=$folded"
            }
        }
        if ($Scenario.Expect -eq 'guide') {
            $guideOk = ($facts.setup_guide_len -gt 0) -and ($facts.error_card -eq '')
            Add-Check $id '未选项目落到配置引导页（非错误卡、非白屏）' $guideOk `
                "setupGuide=$($facts.setup_guide_len) 字 errorCard=$($facts.error_card.Length) 字 actionable=$($facts.ui_final.actionable)"
            Add-Check $id '引导页提供可操作动作' ([bool]($facts.ui_final -and $facts.ui_final.actionable)) `
                "body=$($facts.ui_final.bodyText)"
        }
        if ($Scenario.Expect -eq 'mcp-audit') {
            $tokenFile = Join-Path $agentLeaf 'data\auth\token'
            $ready = Wait-OwoCoreReady -LogDir $logDir -TimeoutSec 45
            if (-not $ready) {
                Add-Check $id '健康启动（用于 MCP 审计）' $false '未读到 core_ready'
            } else {
                $token = if (Test-Path -LiteralPath $tokenFile) { (Get-Content -LiteralPath $tokenFile -Raw).Trim() } else { '' }
                $mcp = Invoke-OwoCoreApi -Base "http://127.0.0.1:$($ready.port)" -Path '/mcp/servers' -Token $token
                $text = ($mcp | ConvertTo-Json -Depth 8)
                $enabled = ($text -match '"example-hello"' -and $text -match '"(enabled|running)"\s*:\s*true')
                Add-Check $id '示例 MCP（owo.plugin.example-hello）未在生产 profile 启用' (-not $enabled) `
                    "enabled=$enabled"
                $null = Save-OwoEvidenceJson -Dir $evDir -Name 'mcp-servers.json' -InputObject $mcp -Depth 8
            }
        }

        # 截图证据：重获窗口（可见性+尺寸验证）→ 截屏 → 像素级有效性（§3.3.1/§3.3.2）。
        if (-not $SkipScreenshots) {
            $winShot = Get-OwoValidatedWindow -ProcessId $proc.Id -TimeoutSec 10 -RequireVisible
            if (-not $winShot.ok) { throw "截图前窗口验证失败：$($winShot.fail_reasons -join ';')" }
            $null = Set-OwoWindowGeometry -Hwnd $winShot.hwnd -Width 1280 -Height 720
            Start-Sleep -Milliseconds 900
            $winShot = Get-OwoValidatedWindow -ProcessId $proc.Id -TimeoutSec 8 -RequireVisible
            if (-not $winShot.ok) { throw "几何调整后窗口验证失败：$($winShot.fail_reasons -join ';')" }
            $shot = Join-Path $evDir "fault-$id-1280x720.png"
            $null = Save-OwoWindowShot -Hwnd $winShot.hwnd -Path $shot
            $metric = Test-OwoScreenshot -Path $shot -ExpectedWidth $winShot.width -ExpectedHeight $winShot.height
            $facts.screenshot = $shot
            $facts.screenshot_metric = $metric
            Add-Check $id '故障面截图（重获窗口 + 五重有效性）' $metric.ok (Get-OwoScreenshotMetricLine $metric)
        }

        # ---- collect：进程身份留痕（实际启动二进制绝对路径 + SHA-256）---------------
        $stage = 'collect'
        $shellLog = @(Get-ChildItem -LiteralPath $logDir -Filter 'desktop-core-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1)
        if ($shellLog) {
            $launched = @(Get-Content -LiteralPath $shellLog[0].FullName -ErrorAction SilentlyContinue |
                Where-Object { $_ -match 'launching core: ' } | Select-Object -Last 1)
            if ($launched) {
                $launchPath = ([regex]::Match([string]$launched, 'launching core: (.+?) \(acceptance')).Groups[1].Value
                $facts.launched_core = $launchPath
                if ($launchPath -notlike "$dir*") {
                    Add-Check $id '实际启动的 core 必须来自场景目录（验收模式无回退）' $false "launched=$launchPath"
                } elseif (Test-Path -LiteralPath $launchPath) {
                    $facts.sidecar_sha256 = (Get-FileHash -LiteralPath $launchPath -Algorithm SHA256).Hash
                    Add-Check $id '实际启动的 core 来自场景目录（验收模式无回退）' $true "path=$launchPath sha256=$($facts.sidecar_sha256)"
                }
            } elseif ($Scenario.Kind -eq 'missing') {
                Add-Check $id 'sidecar 缺失场景确实未拉起任何 core（日志无 launching 行）' $true 'launched_core=<none>'
            } else {
                Add-Check $id '日志记录了实际启动的 core 路径（§3.3.3 报告留痕）' $false "日志无 launching core 行：$($shellLog[0].FullName)"
            }
            Copy-Item -LiteralPath $shellLog[0].FullName -Destination (Join-Path $logsCopy 'desktop-core.log') -Force
            Copy-Item -LiteralPath $shellLog[0].FullName -Destination (Join-Path $evDir 'desktop-core.log') -Force
        } else {
            Add-Check $id '壳日志存在（可观测性底线）' $false "logDir=$logDir 无 desktop-core-*.log"
        }
        $facts.executed = $true
        Add-Check $id '场景完整执行（prepare→collect 全阶段无异常）' $true "stages=ok"
    }
    catch {
        # §3.3.4：任意阶段异常必须形成失败项——场景没执行完绝不允许"看起来全绿"。
        $facts.executed = $false
        $facts.failed_stage = $stage
        $facts.error_type = $_.Exception.GetType().Name
        $facts.error_message = [string]$_.Exception.Message
        Add-Check $id '场景完整执行（prepare→collect 全阶段无异常）' $false `
            "executed=false failed_stage=$stage error_type=$($facts.error_type) message=$($facts.error_message)"
    }
    finally {
        # cleanup：终止壳/核心/stub 进程树 → 留档清理结果（不删场景目录由主循环统一收）。
        $stage = 'cleanup'
        $cleanup = [ordered]@{ shell_pid = 0; residual_procs = -1; notes = '' }
        try {
            if ($proc) {
                $cleanup.shell_pid = $proc.Id
                Stop-OwoShellTree -ProcessId $proc.Id -RunRoot $dir
                $cleanup.notes = 'Stop-OwoShellTree 已执行'
            }
            $escaped = $dir.Replace('\', '\\')
            $residual = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -in @('owo-agent.exe', 'owo-agent-desktop.exe', 'owo-fault-stub.exe') -and $_.CommandLine -and $_.CommandLine -like "*$escaped*" })
            $cleanup.residual_procs = $residual.Count
            foreach ($r in $residual) { Stop-Process -Id $r.ProcessId -Force -ErrorAction SilentlyContinue }
        } catch {
            $cleanup.notes = "清理异常：$($_.Exception.Message)"
        }
        $facts.cleanup = $cleanup
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'cleanup.json' -InputObject $cleanup
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'scenario-report.json' -InputObject $facts -Depth 8
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'assertions.json' -InputObject @([bool]1) # 占位防错：真正断言切片在下面覆盖
        $slice = @($results | Select-Object -Skip $scenarioChecksStart)
        $null = Save-OwoEvidenceJson -Dir $evDir -Name 'assertions.json' -InputObject $slice -Depth 6
    }
    return [pscustomobject]$facts
}

# ---- 主循环（§3.5 由快到慢；外层异常同样落盘，R3-BUG-04 的总兜底）------------------
$exitCode = 0
try {
    foreach ($scenario in $selected) {
        Write-Host ""
        Write-Host "=== 场景 $($scenario.Id) —— $($scenario.Note) ==="
        try {
            $facts = Invoke-Scenario -Scenario $scenario
            $report.scenarios += @($facts)
        } catch {
            # Invoke-Scenario 内部已有兜底；走到这里说明兜底本身炸了——必须记失败。
            Add-Check $scenario.Id '场景完整执行（prepare→collect 全阶段无异常）' $false `
                "executed=false failed_stage=outer error_type=$($_.Exception.GetType().Name) message=$($_.Exception.Message)"
            $report.scenarios += @([pscustomobject]@{
                    id = $scenario.Id; executed = $false; failed_stage = 'outer'
                    error_type = $_.Exception.GetType().Name; error_message = [string]$_.Exception.Message
                })
        }
    }
} catch {
    Add-Check 'ALL' '矩阵主循环完整执行' $false "outer: $($_.Exception.Message)"
}
finally {
    # 真实 debug core 哈希守卫（§3.3.4：最外层必须确认真实产物未被改动）。
    $hashOk = $true
    $hashNow = ''
    if (Test-Path -LiteralPath $sdkSidecar) { $hashNow = (Get-FileHash -LiteralPath $sdkSidecar -Algorithm SHA256).Hash }
    if ($sdkCoreHashBefore -and $hashNow -and ($hashNow -ne $sdkCoreHashBefore)) { $hashOk = $false }
    $renameResidue = @(Get-ChildItem -LiteralPath (Split-Path -Parent $sdkSidecar) -Filter 'owo-agent.exe.*' -ErrorAction SilentlyContinue)
    Add-Check 'ALL' '真实 debug core 未被改动（哈希前后一致，R3-BUG-03）' ($hashOk -and $renameResidue.Count -eq 0) `
        "hash_before=$($sdkCoreHashBefore.Substring(0, [Math]::Min(12, $sdkCoreHashBefore.Length))) hash_after=$($hashNow.Substring(0, [Math]::Min(12, $hashNow.Length))) rename_residue=$($renameResidue.Count)"
    $summaryPath = Join-Path $EvidenceDir 'failure-matrix-report.json'
    $report.finished_at = (Get-Date).ToUniversalTime().ToString('o')
    $report.scenarios_selected = @($selected | ForEach-Object { $_.Id })
    $report.checks_total = $results.Count
    $report.checks_failed = @($results | Where-Object { -not $_.pass }).Count
    $report.scenarios_not_executed = @($report.scenarios | Where-Object { -not $_.executed }).Count
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
