<#
verify-desktop-cold-boot.ps1 — R3（§8.2）真实桌面冷启动/重连/隐藏期验收（非模拟）。

本脚本启动**真实 Tauri 桌面壳**（含真实 WebView2 与真实 sidecar 进程），并把
「首次可输入前 ≤5 请求」「core 重启只再引导一次、不形成双事件连接」「窗口隐藏
期间业务请求为 0」三组断言的**唯一事实源**定为核心侧 `/diagnostics/requests`
安全 ledger（§8.1）。浏览器侧计数、日志肉眼判断一律不作为证据。

隔离与安全：
- 子进程环境整体重定向（LOCALAPPDATA/APPDATA/TEMP → 本次运行私有目录）：
  全新数据目录冷启动，不污染用户真实 `%LOCALAPPDATA%\OwO\Agent`；
- 模型凭据仅从用户级环境变量注入子进程（AGENTS.md 红线：绝不回显/落盘）；
- 读取 bearer 只从本次私有数据根的 `auth/token` 文件，只用于本机 loopback 探测；
- 脚本自身发出的探测请求统一带 `x-owo-client: probe`，统计口径按 source 排除，
  不会把取证流量算进产品请求预算。

用法（在 agent-sdk/ 目录）：
  .\scripts\verify-desktop-cold-boot.ps1                        # 全量（含隐藏 5 分钟）
  .\scripts\verify-desktop-cold-boot.ps1 -HiddenMinutes 1       # 缩短隐藏窗口期
  .\scripts\verify-desktop-cold-boot.ps1 -SkipHiddenWindow      # 只跑冷启动 + 重启
  .\scripts\verify-desktop-cold-boot.ps1 -EvidenceDir <目录>     # 指定取证输出目录
退出码：0 = 全部断言通过；1 = 存在失败断言（取证文件仍会写出）。
#>
[CmdletBinding()]
param(
    [int]$HiddenMinutes = 5,
    [switch]$SkipHiddenWindow,
    [switch]$SkipScreenshots,
    [string]$EvidenceDir = "",
    [string]$ShellExe = "",
    [string]$SidecarExe = ""
)

$ErrorActionPreference = 'Stop'
$sdkRoot = Split-Path -Parent $PSScriptRoot
# 共享原语（窗口/截图/ledger/CDP/新鲜度门）唯一实现，与 §8.3 故障矩阵同源。
. (Join-Path $PSScriptRoot "desktop-acceptance-common.ps1")
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $ShellExe) {
    $ShellExe = Join-Path $sdkRoot 'desktop\tauri\src-tauri\target\debug\owo-agent-desktop.exe'
}
if (-not $SidecarExe) {
    $SidecarExe = Join-Path $sdkRoot 'target\debug\owo-agent.exe'
}
if (-not (Test-Path $SidecarExe)) { throw "缺少 sidecar 可执行文件：$SidecarExe（先在 agent-sdk 执行 cargo build -p owo-agent-cli）" }
# §7 构建自包含：壳内嵌前端资产，web 改了不重建壳就是在测上一版界面（共享门：同一
# 实现也被 §8.3 使用）。
$null = Assert-OwoShellEmbedsCurrentWeb -ShellExe $ShellExe -WebRoot (Join-Path $sdkRoot 'desktop\web')
if (-not $EvidenceDir) {
    $EvidenceDir = Join-Path $sdkRoot "docs\qa\evidence\r3-cold-boot-$stamp"
}
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

# ---- 私有运行环境（全新数据目录）-------------------------------------------------
$runRoot = Join-Path ([IO.Path]::GetTempPath()) "owo-desktop-coldboot-$stamp"
$localAppData = Join-Path $runRoot 'LocalAppData'
$appData = Join-Path $runRoot 'RoamingAppData'
$runTemp = Join-Path $runRoot 'Temp'
$project = Join-Path $runRoot 'project'
foreach ($dir in @($localAppData, $appData, $runTemp, $project)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
}
# §4.6：壳的“最近项目”指针预置（等价于用户在引导页选过一次目录）；数据目录本身全新。
$agentStateDir = Join-Path $localAppData 'OwO\Agent'
New-Item -ItemType Directory -Force -Path $agentStateDir | Out-Null
Set-Content -Path (Join-Path $agentStateDir 'workspace.json') -Value ('{"path":"' + ($project -replace '\\', '\\') + '"}') -Encoding ASCII -NoNewline
$dataRoot = Join-Path $agentStateDir 'data'
$logDir = Join-Path $agentStateDir 'logs'
$tokenFile = Join-Path $dataRoot 'auth\token'

# ---- 原语全部来自 desktop-acceptance-common.ps1（唯一实现，与 §8.3 同源）------
# 曾在此文件里另存一份窗口/截图/ledger 实现，两份分叉直接造成过一次假绿
# （私有版 ledger 在单条业务记录时把计数算成 null，`≤5` 断言空转通过）。
if (-not $SkipScreenshots) { Add-OwoDesktopWin32 }

# ---- 结果登记 -------------------------------------------------------------------
$results = New-Object System.Collections.ArrayList
function Add-Check {
    param([string]$Name, [bool]$Pass, [string]$Detail)
    $null = $results.Add([pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail })
    $mark = if ($Pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} — {2}" -f $mark, $Name, $Detail)
}
function Save-Json {
    param($Object, [string]$FileName)
    # 证据 JSON 一律无 BOM 写出（Set-Content -Encoding UTF8 会带 BOM，
    # 下游 node/JSON.parse 报「Unexpected token ﻿」）。
    return Save-OwoEvidenceJson -Dir $EvidenceDir -Name $FileName -InputObject $Object
}

function Get-CoreHttpStatus {
    param([string]$Base, [string]$Path, [string]$Token)
    # 只要状态码，不要异常：401 是这里的**期望结果**，不能让它炸成脚本错误。
    try {
        $null = Invoke-OwoCoreApi -Base $Base -Path $Path -Token $Token
        return 200
    } catch {
        $response = $_.Exception.Response
        if ($response -and $response.StatusCode) { return [int]$response.StatusCode }
        return -1
    }
}

function Assert-LedgerBuckets {
    param($Facts, [string]$Where)
    # 分桶自洽 + 计数必须是数字：null/漏桶会让 `≤5` 这类断言空转通过（实测踩过）。
    $ok = ($null -ne $Facts) -and [bool]$Facts.web_buckets_ok -and
          ($Facts.web_business -is [int] -or $Facts.web_business -is [long])
    Add-Check "ledger 分桶自洽（$Where：health+auth+events+business = web_total）" $ok `
        "buckets_sum=$($Facts.web_buckets_sum) web_total=$($Facts.web_total) business=$($Facts.web_business)"
    return $ok
}
function CommitOf([string]$line) { return ([regex]::Match($line, 'commit=(\S+)')).Groups[1].Value }

. (Join-Path $PSScriptRoot "stage-desktop-sidecar.ps1")
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'   # 子进程正常 stderr 不得被当成终止错误
try {
    $staged = Stage-OwoDesktopSidecar -Configuration debug -Quiet
} finally {
    $ErrorActionPreference = $prevEap
}
$coreIdentity = $staged.identity
Write-Host "[coldboot] 随包 core：$coreIdentity"
$stagedCommit = CommitOf $coreIdentity
$sdkCommit = CommitOf ((& $SidecarExe --version 2>&1 | Out-String) -split '\r?\n' | Where-Object { $_ -match 'commit=' } | Select-Object -First 1)
if ($stagedCommit -ne $sdkCommit) {
    throw "随包 core 与 SDK 构建产物不同世代：staged=$stagedCommit sdk=$sdkCommit（§8.3 sidecar 过旧）"
}
$sibling = Join-Path (Split-Path -Parent $ShellExe) 'owo-agent.exe'
if (Test-Path -LiteralPath $sibling) {
    # tauri-build 只在**构建壳**时把随包 core 复制到壳旁边；单独重建 sidecar 不刷新
    # 那个副本 → "同目录优先"会让壳继续跑上一版 core（实测：核心侧 token 换发已
    # 生效，验收仍观察到旧行为）。按 SHA-256 对齐，等价于安装包内的同批次产物。
    $siblingHash = (Get-FileHash -LiteralPath $sibling -Algorithm SHA256).Hash
    if ($siblingHash -ne $staged.sha256) {
        Copy-Item -LiteralPath $staged.source -Destination $sibling -Force
        Write-Host "[coldboot] 壳同目录 sidecar 副本已按哈希刷新（原为上一版构建产物）"
    }
    $siblingLine = ((& $sibling --version 2>&1 | Out-String) -split '\r?\n' | Where-Object { $_ -match 'commit=' } | Select-Object -First 1)
    if (-not $siblingLine) {
        throw "壳同目录存在缺少构建身份的旧 sidecar：$sibling（壳会跳过它，但取证必须在干净布局上做）"
    }
    if ((CommitOf $siblingLine) -ne $stagedCommit) {
        throw "壳同目录 sidecar 与随包 core 不同世代：sibling=$(CommitOf $siblingLine) staged=$stagedCommit"
    }
    Write-Host "[coldboot] 壳同目录 sidecar 与随包 core 同批次 ✓"
}
Save-Json @{ sidecar_version_line = $coreIdentity; shell_exe = $ShellExe; sidecar_exe = $SidecarExe; staged_sha256 = $staged.sha256 } 'binaries.json' | Out-Null

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ShellExe
$psi.UseShellExecute = $false
$psi.WorkingDirectory = Split-Path -Parent $ShellExe
$psi.EnvironmentVariables['LOCALAPPDATA'] = $localAppData
$psi.EnvironmentVariables['APPDATA'] = $appData
$psi.EnvironmentVariables['TEMP'] = $runTemp
$psi.EnvironmentVariables['TMP'] = $runTemp
$apiKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if (-not $apiKey) { throw '用户级环境变量 OPENAI_API_KEY 缺失：sidecar 无法启动（AGENTS.md 凭据红线：只注入，不回显）' }
$psi.EnvironmentVariables['OPENAI_API_KEY'] = $apiKey
# 取证需要直连核心读 ledger；开发便利开关仅影响 token 引导路径（默认协议不变）。
$psi.EnvironmentVariables.Remove('OWO_DESKTOP_DEV_AUTH')
# §8.2「首次可输入前」需要 DOM 事实：开 WebView2 远程调试端口（只绑 loopback，
# 进程退出即消失），用 CDP 读实际渲染结果替代「睡几秒再猜」的软断言。
# 申请到的端口只是候选：Chromium 被抢口时会静默换口，真实端口以 UDF 下
# DevToolsActivePort 为准（Resolve-OwoCdpPort）。
$cdpRequested = Get-OwoFreeTcpPort
$psi.EnvironmentVariables['WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'] = "--remote-debugging-port=$cdpRequested"
# WebView2 的用户数据目录由 Windows known-folder API 派生，**不受 LOCALAPPDATA
# 环境变量重定向影响**：不设它就会与本机已安装版本共用同一个浏览器进程组，
# 页面目标/调试端口都不受本次验收控制（实测：/json/version 活着但取不到 DOM 事实）。
# 因此给本轮一个专属 UDF，浏览器进程组也随之独占。
$webViewUdf = Join-Path $runRoot 'WebView2'
New-Item -ItemType Directory -Force -Path $webViewUdf | Out-Null
$psi.EnvironmentVariables['WEBVIEW2_USER_DATA_FOLDER'] = $webViewUdf
$webviewDataDir = Get-OwoWebviewEtbDir -UserDataDir $webViewUdf
if (Clear-OwoCdpPortFile -DataDir $webviewDataDir) {
    Write-Host "[coldboot] 已清除上一轮 DevToolsActivePort（本轮端口出现即为事实）"
}

$shellProc = [System.Diagnostics.Process]::Start($psi)
Write-Host "[coldboot] 桌面壳 pid=$($shellProc.Id)（私有环境=$runRoot）"
$failureNotes = @()

try {
    $ready = Wait-OwoCoreReady -LogDir $logDir -TimeoutSec 45 -ExcludePid 0
    if (-not $ready) {
        Add-Check '壳在 45s 内拉起 sidecar 并报 core_ready' $false '未读到 core_ready 行'
        throw 'sidecar 未就绪，终止后续断言'
    }
    Add-Check '壳在 45s 内拉起 sidecar 并报 core_ready' $true "pid=$($ready.pid) port=$($ready.port) build_id=$($ready.build_id)"

    $base = "http://127.0.0.1:$($ready.port)"
    $health = Invoke-OwoCoreApi -Base $base -Path '/health' -Token $null
    Add-Check '/health 报告 healthy 且 api_version 与壳期望一致' ($health.healthy -eq $true) "healthy=$($health.healthy) api=$($health.api_version) stage=$($health.stage) build_id=$($health.build_id)"
    Add-Check 'ready 行 build_id = /health.build_id = sidecar --version commit' `
        (($ready.build_id -eq $health.build_id) -and ($coreIdentity -match [regex]::Escape($ready.build_id.Substring(0, [Math]::Min(12, $ready.build_id.Length))))) `
        "ready=$($ready.build_id) health=$($health.build_id)"
    Add-Check '/health.instance_id 非空（壳注入实例身份，握手闭环）' ([string]::IsNullOrEmpty($health.instance_id) -eq $false) "instance_id=$($health.instance_id)"

    $hwnd = Get-OwoShellWindow -ProcessId $shellProc.Id
    Add-Check '桌面主窗口在 20s 内出现（非空白壳/非永久 loading）' ($hwnd -ne [IntPtr]::Zero) "hwnd=$hwnd"

    # DOM 事实通道：以 DevToolsActivePort 为准解析**实际**端口（被抢口时 Chromium 换口）。
    $cdpPort = Resolve-OwoCdpPort -DataDir $webviewDataDir -RequestedPort $cdpRequested -TimeoutSec 25
    Add-Check 'DOM 事实通道（WebView2 CDP）建立' ($cdpPort -gt 0) "requested=$cdpRequested effective=$cdpPort"
    if ($cdpPort -le 0) {
        # 取不到就把现场留下：谁占着这个 UDF 的浏览器进程、申请端口有没有监听。
        $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'cdp-diagnostic.json' `
            -InputObject (Get-OwoWebviewDiagnostics -RequestedPort $cdpRequested -DataDir $webviewDataDir)
    }

    if (-not (Test-Path $tokenFile)) { throw "未取得核心 token 文件：$tokenFile（无法读取 ledger 取证）" }
    $token = (Get-Content $tokenFile -Raw).Trim()
    Add-Check 'token 文件落在本次私有数据根（未污染真实目录）' ($token.Length -ge 32) "path=$tokenFile"

    # §8.2 口径必须落进 DOM：等"任务输入框真的可见可用"，再冻结首屏 ledger 快照。
    # 有界轮询（≤20s）替代固定 sleep——早到不虚报、晚到不漏计。
    $ui = $null
    $uiDeadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $uiDeadline) {
        $ui = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 5
        if ($ui -and $ui.composerVisible) { break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ui) {
        # 取不到 DOM 事实时，"为什么取不到"必须留在断言里（不允许静默降级）。
        Add-Check 'WebView2 CDP 可达（DOM 事实通道建立）' $false `
            "port=$cdpPort 未取到渲染文本：$global:OwoCdpLastError [$($global:OwoCdpTrace -join ' ; ')]"
        Add-Check '首屏 20s 内出现可输入任务框（无永久 loading）' $false '无 DOM 事实'
    } else {
        Add-Check 'WebView2 CDP 可达（DOM 事实通道建立）' $true "port=$cdpPort"
        $cardBrief = if ($ui.errorCard) { $ui.errorCard.Substring(0, [Math]::Min(90, $ui.errorCard.Length)) } else { '' }
        Add-Check '首屏 20s 内出现可输入任务框（无永久 loading）' ([bool]$ui.composerVisible) `
            "composerVisible=$($ui.composerVisible) health=`"$($ui.healthText)`""
        Add-Check '首屏不得停在统一错误卡上（正常冷启动）' ([string]::IsNullOrEmpty($ui.errorCard)) "errorCard=`"$cardBrief`""
        Add-Check '首屏不得误落 provider 引导页（密钥已注入）' ([string]::IsNullOrEmpty($ui.setupGuide)) "setupGuide=$($ui.setupGuide.Length) 字"
    }
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ui-first-screen.json' -InputObject $ui
    $boot = Get-OwoLedgerFacts -Base $base -Token $token -Label 'cold-boot'
    $null = Assert-LedgerBuckets $boot 'cold-boot'
    Save-Json $boot 'ledger-cold-boot.json' | Out-Null
    Write-Host ("[coldboot] 首屏(web)：health={0} auth={1} events={2} business={3} routes={4}" -f `
            $boot.web_health, $boot.web_auth, $boot.web_events, $boot.web_business, ($boot.web_business_routes -join ','))
    Add-Check 'WebView 首次可输入前业务请求 ≤5（服务端 ledger 权威口径）' (($null -ne $boot.web_business) -and ($boot.web_business -le 5)) `
        "web_business=$($boot.web_business) routes=$($boot.web_business_routes -join ',')"
    Add-Check 'WebView 零 /auth/token（壳注入 token 生效，未回落到引导端点）' ($boot.web_auth -eq 0) `
        "web_auth=$($boot.web_auth) shell_auth=$($boot.shell_auth)"
    Add-Check '壳侧配对引导恰好一次（/auth/token source=shell）' ($boot.shell_auth -eq 1) `
        "shell_auth=$($boot.shell_auth)"
    Add-Check '事件连接唯一（/events/stream ≤1，无双重 SSE）' ($boot.web_events -le 1) "web_events=$($boot.web_events)"
    Add-Check '首屏无同路由重复请求（水合未被跑两遍）' ($boot.web_route_repeats.Count -eq 0) `
        "repeats=$($boot.web_route_repeats -join ',')"
    Add-Check 'ledger source 归类含 web/shell（来源标签链路可用）' `
        (($boot.sources -contains 'web') -and ($boot.sources -contains 'shell')) "sources=$($boot.sources -join ',')"

    if (-not $SkipScreenshots -and $hwnd -ne [IntPtr]::Zero) {
        foreach ($size in @(@(900, 600), @(1280, 720), @(1920, 1080))) {
            $targetW = $size[0]; $targetH = $size[1]
            $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
            if ($targetW -gt $bounds.Width) { $targetW = $bounds.Width }
            if ($targetH -gt $bounds.Height) { $targetH = $bounds.Height }
            $x = [Math]::Max(0, [int](($bounds.Width - $targetW) / 2))
            $y = [Math]::Max(0, [int](($bounds.Height - $targetH) / 2))
            $null = [OwoWin32]::MoveWindow($hwnd, $x, $y, $targetW, $targetH, $true)
            Start-Sleep -Milliseconds 1200
            $shot = Join-Path $EvidenceDir ("desktop-{0}x{1}.png" -f $targetW, $targetH)
            try {
                $geometry = Save-OwoWindowShot -Hwnd $hwnd -Path $shot
                Add-Check "截图 ${targetW}x${targetH}" (Test-Path $shot) "$shot（$geometry）"
            } catch {
                Add-Check "截图 ${targetW}x${targetH}" $false $_.Exception.Message
            }
        }
    }

    # ---- core 重启：旧实例终止 → 壳受控重启 → 只再引导一次、无双事件连接 ----------
    $oldPid = [int]$ready.pid
    $oldInstance = [string]$health.instance_id
    Stop-Process -Id $oldPid -Force -ErrorAction SilentlyContinue
    Write-Host "[coldboot] 已强杀 sidecar pid=$oldPid（模拟 core 意外退出）"
    $ready2 = Wait-OwoCoreReady -LogDir $logDir -TimeoutSec 40 -ExcludePid $oldPid
    if (-not $ready2) {
        Add-Check 'core 意外退出后壳自动受控重启并再次 ready' $false '40s 内未出现新 core_ready'
        throw '重启未发生，无法继续重启断言'
    }
    Add-Check 'core 意外退出后壳自动受控重启并再次 ready' ([int]$ready2.pid -ne $oldPid) "new pid=$($ready2.pid) port=$($ready2.port)"
    $base2 = "http://127.0.0.1:$($ready2.port)"
    $health2 = Invoke-OwoCoreApi -Base $base2 -Path '/health' -Token $null
    # 语义澄清（实测校正本脚本第一版断言）：`instance_id` 是**壳窗口**的身份
    # （每次启动壳生成一次，经 OWO_DESKTOP_INSTANCE_ID 注入 core），受控重启刻意
    # 让它保持不变——壳只认自己启动的子进程。跨代际不可复用由 pid 变化 + **换发的
    # bearer** 保证（§8.2 第 5 条），不是靠 instance 漂移。
    Add-Check '重启后窗口身份保持稳定（instance 属壳，不随 core 代际漂移）' `
        (($health2.instance_id -eq $oldInstance) -and -not [string]::IsNullOrEmpty($health2.instance_id)) `
        "old=$oldInstance new=$($health2.instance_id)"
    Add-Check '重启后 core 进程身份确实变化（pid 不同）' ([int]$ready2.pid -ne $oldPid) `
        "old_pid=$oldPid new_pid=$($ready2.pid)"

    # §8.2 第 5 条（硬断言）：新代际必须换发 bearer，旧 bearer 在新核心上必须 401。
    Start-Sleep -Seconds 2
    $tokenNew = ''
    if (Test-Path -LiteralPath $tokenFile) { $tokenNew = (Get-Content -LiteralPath $tokenFile -Raw).Trim() }
    Add-Check 'core 重启后 bearer 已换发（token 文件随代际更新）' `
        (($tokenNew.Length -ge 32) -and ($tokenNew -ne $token)) "old_len=$($token.Length) new_len=$($tokenNew.Length)"
    $oldTokenStatus = Get-CoreHttpStatus -Base $base2 -Path '/sessions' -Token $token
    $newTokenStatus = Get-CoreHttpStatus -Base $base2 -Path '/sessions' -Token $tokenNew
    Add-Check '上一代 bearer 在新核心上被拒（401）' ($oldTokenStatus -eq 401) "status=$oldTokenStatus"
    Add-Check '新一代 bearer 立即可用（200）' ($newTokenStatus -eq 200) "status=$newTokenStatus"

    # 等 WebView 自己恢复（重查壳连接 → 新 token → 重连事件流），再冻结快照：
    # 只 sleep 固定秒数会把"还没来得及重连"误判成"重连成本为零"。
    $afterRestart = $null
    $reconnectDeadline = (Get-Date).AddSeconds(40)
    while ((Get-Date) -lt $reconnectDeadline) {
        $afterRestart = Get-OwoLedgerFacts -Base $base2 -Token $tokenNew -Label 'after-core-restart'
        if ($afterRestart.web_events -ge 1) { break }
        Start-Sleep -Seconds 2
    }
    # 自洽性检查放在轮询**之外**：放里层会把同一条断言重复登记 N 次（实测过）。
    $null = Assert-LedgerBuckets $afterRestart 'after-core-restart'
    Save-Json $afterRestart 'ledger-after-restart.json' | Out-Null
    if ($ui) {
        $ui2 = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 8
        Add-Check '重启后界面自动恢复可输入（不停留在错误卡/loading）' `
        (($ui2 -and $ui2.composerVisible) -and [string]::IsNullOrEmpty($ui2.errorCard)) `
            "composerVisible=$($ui2.composerVisible) errorCard=$($ui2.errorCard.Length) 字"
        $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ui-after-restart.json' -InputObject $ui2
    }
    Write-Host ("[coldboot] 重启后(web)：health={0} auth={1} events={2} business={3} routes={4}" -f `
            $afterRestart.web_health, $afterRestart.web_auth, $afterRestart.web_events, `
            $afterRestart.web_business, ($afterRestart.web_business_routes -join ','))
    Add-Check '重启后壳只重新引导一次（shell /auth/token ≤1）' ($afterRestart.shell_auth -le 1) `
        "shell_auth=$($afterRestart.shell_auth)"
    Add-Check '重启后 WebView 不回落引导端点（web /auth/token = 0）' ($afterRestart.web_auth -eq 0) `
        "web_auth=$($afterRestart.web_auth)"
    Add-Check '重启后不形成双事件连接（/events/stream ≤1）' ($afterRestart.web_events -le 1) `
        "web_events=$($afterRestart.web_events)"
    Add-Check '重启后重新引导的首屏业务请求仍 ≤5' (($null -ne $afterRestart.web_business) -and ($afterRestart.web_business -le 5)) `
        "business=$($afterRestart.web_business) routes=$($afterRestart.web_business_routes -join ',')"
    Add-Check '重启后无同路由重复请求' ($afterRestart.web_route_repeats.Count -eq 0) `
        "repeats=$($afterRestart.web_route_repeats -join ',')"

    # ---- 窗口隐藏期：业务请求必须为 0，只允许 health ------------------------------
    if (-not $SkipHiddenWindow) {
        if ($hwnd -eq [IntPtr]::Zero) {
            Add-Check '隐藏窗口期零业务请求' $false '无可用窗口句柄'
        } else {
            # 隐藏必须走**产品自己的路径**（壳命令 set_window_visible → window.hide()
            # → wry set_visible(false)）。两条实测事实决定了这个选择：
            #   1) 外部 ShowWindow(SW_HIDE) 只动 Win32，页面 visibilityState 不变；
            #   2) 即便走壳的 hide()（它会调 controller.SetIsVisible(false)），
            #      WebView2 仍不把可见性传导给 document.visibilityState。
            # 所以前端改吃壳注入的后台标记（app.js owoSetBackground），本阶段断言
            # 的是**那个标记**，而不是页面自己声称的可见性。
            # 现场取证：隐藏期若有业务请求，必须能区分是"事件驱动刷新"还是
            # "兜底轮询"（invalidator 内部计数），否则只能靠猜。
            $invStateExpr = '(window.owoInvalidatorState ? JSON.stringify(window.owoInvalidatorState()) : "none")'
            $invBefore = [string](Invoke-OwoCdpEval -Port $cdpPort -Expression $invStateExpr -TimeoutSec 6)
            $hide = Invoke-OwoShellIpc -Port $cdpPort -Command 'set_window_visible' -Arguments @{ visible = $false }
            Add-Check '壳隐藏窗口命令可用且回报已隐藏' ($hide -and ($hide.visible -eq $false)) `
                "result=$($hide | ConvertTo-Json -Compress) last_cdp_err=$global:OwoCdpLastError"
            $uiHiddenState = ''
            $hideDeadline = (Get-Date).AddSeconds(10)
            while ((Get-Date) -lt $hideDeadline) {
                $uiHiddenState = [string](Invoke-OwoCdpEval -Port $cdpPort `
                    -Expression 'String(!!(window.owoUiHidden && window.owoUiHidden()))' -TimeoutSec 5)
                if ($uiHiddenState -eq 'true') { break }
                Start-Sleep -Milliseconds 400
            }
            Add-Check '前端后台态确实接管（uiHidden=true，不依赖 visibilityState）' ($uiHiddenState -eq 'true') `
                "uiHidden=$uiHiddenState visibilityState=$(Invoke-OwoCdpEval -Port $cdpPort -Expression 'document.visibilityState' -TimeoutSec 5)"
            if ($uiHiddenState -ne 'true') {
                # 后台态没接管，"隐藏期 0 业务"测了也无意义：显式判失败，不静默跳过。
                Add-Check ("窗口隐藏 {0} 分钟：不产生任何轮询/业务请求（health 亦为 0）" -f $HiddenMinutes) $false `
                    '前端未进入后台态，本项测量无意义'
            } else {
                Write-Host "[coldboot] 已进入后台态（uiHidden=true），静置 $HiddenMinutes 分钟…"
                # 测量窗口从"页面确认进入后台态"起算，**不回拨**：回拨会把隐藏前
                # 已在飞行中的请求算进隐藏期（实测就是这么误红了一条 /automations）。
                Start-Sleep -Seconds 2
                $since = (Get-Date).ToUniversalTime()
                $beforeTotal = (Invoke-OwoCoreApi -Base $base2 -Path '/diagnostics/requests?limit=1' -Token $tokenNew).total
                Start-Sleep -Seconds ($HiddenMinutes * 60)
                $invAfter = [string](Invoke-OwoCdpEval -Port $cdpPort -Expression $invStateExpr -TimeoutSec 6)
                $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'invalidator-hidden-window.json' `
                    -InputObject @{ before_hide = $invBefore; after_hidden = $invAfter }
                $hidden = Get-OwoLedgerFacts -Base $base2 -Token $tokenNew -Label 'hidden-window'
                $null = Assert-LedgerBuckets $hidden 'hidden-window'
                Save-Json $hidden 'ledger-hidden.json' | Out-Null
                $new = @($hidden.records | Where-Object {
                        ([DateTime]::Parse($_.started_at, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind)) -gt $since
                    })
                $newBusiness = @($new | Where-Object {
                        $_.route_template -ne '/health' -and $_.route_template -ne '/auth/token' -and $_.route_template -ne '/events/stream'
                    })
                $newHealth = @($new | Where-Object { $_.route_template -eq '/health' })
                Add-Check ("窗口隐藏 {0} 分钟：不产生任何轮询/业务请求（health 亦为 0）" -f $HiddenMinutes) `
                    (((Get-OwoCount $newBusiness) -eq 0) -and ((Get-OwoCount $newHealth) -eq 0)) `
                    ("新增 total=$(Get-OwoCount $new) health=$(Get-OwoCount $newHealth) business=$(Get-OwoCount $newBusiness) routes=$((($newBusiness | ForEach-Object { $_.route_template }) | Sort-Object -Unique) -join ',') | 失效器 before=$invBefore after=$invAfter（详见 invalidator-hidden-window.json）")
                Add-Check '隐藏期前后 total 增量与服务端一致' ([int]$hidden.server_total -ge [int]$beforeTotal) `
                    "before=$beforeTotal after=$($hidden.server_total)"
            }
            # 唤回：同一条壳命令（与托盘「打开工作台」/全局快捷键同一实现）。
            # 不再用"第二实例唤回"——本沙箱禁止进程开命名管道，第二实例会走
            # InstanceLock 的致命错误分支（实测弹原生错误框且不退），无法作为验收驱动。
            $show = Invoke-OwoShellIpc -Port $cdpPort -Command 'set_window_visible' -Arguments @{ visible = $true }
            Add-Check '壳唤回命令可用且回报已可见' ($show -and ($show.visible -eq $true)) `
                "result=$($show | ConvertTo-Json -Compress) last_cdp_err=$global:OwoCdpLastError"
            $restoredHidden = 'true'
            $restoreDeadline = (Get-Date).AddSeconds(15)
            while ((Get-Date) -lt $restoreDeadline) {
                $restoredHidden = [string](Invoke-OwoCdpEval -Port $cdpPort `
                    -Expression 'String(!!(window.owoUiHidden && window.owoUiHidden()))' -TimeoutSec 5)
                if ($restoredHidden -eq 'false') { break }
                Start-Sleep -Milliseconds 400
            }
            Add-Check '唤回后前端退出后台态并补刷隐藏期间攒下的域' ($restoredHidden -eq 'false') "uiHidden=$restoredHidden"
            Start-Sleep -Seconds 3
            if (-not $SkipScreenshots) {
                $shot = Join-Path $EvidenceDir 'desktop-restored.png'
                try {
                    $geometry = Save-OwoWindowShot -Hwnd $hwnd -Path $shot
                    Add-Check '恢复显示后截图（无永久 loading）' (Test-Path $shot) "$shot（$geometry）"
                } catch {
                    Add-Check '恢复显示后截图（无永久 loading）' $false $_.Exception.Message
                }
                $uiRestored = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 8
                Add-Check '唤回后界面仍可操作（隐藏期未把 UI 拖成错误态）' `
                (($uiRestored -and $uiRestored.actionable) -and [string]::IsNullOrEmpty($uiRestored.errorCard)) `
                    "actionable=$($uiRestored.actionable) errorCard=$($uiRestored.errorCard.Length) 字"
            }
        }
    }
}
catch {
    $failureNotes += $_.Exception.Message
    # 断言链中断**必须计为失败**：否则"后面的断言根本没跑"会被汇总成"全绿"
    # （实测踩过：隐藏阶段用旧 token → 401 → 32/32 通过但 §8.2 隐藏期从未验证）。
    Add-Check '断言链完整执行（无中途中断）' $false "中断于：$($_.Exception.Message)"
    Write-Host "[coldboot] 断言链中断：$($_.Exception.Message)" -ForegroundColor Yellow
}
finally {
    # 收尾：先把窗口放回可见态（否则断言链中途失败会把一个隐藏窗口留给用户，
    # 后续 CloseMainWindow 也发不出去），再关壳 + 兜底清残留进程。
    if ($hwnd -and $hwnd -ne [IntPtr]::Zero) {
        try { $null = [OwoWin32]::ShowWindow($hwnd, 9) } catch { }
    }
    if ($shellProc -and -not $shellProc.HasExited) {
        try { $null = $shellProc.CloseMainWindow() } catch { }
        Start-Sleep -Milliseconds 800
        if (-not $shellProc.HasExited) { Stop-Process -Id $shellProc.Id -Force -ErrorAction SilentlyContinue }
    }
    # sidecar 的 exe 路径在 SDK target 下（不在 runRoot 内），必须按命令行匹配本次
    # 私有 workspace 才能精准清理——否则会留下孤儿核心进程（实测踩过）。
    Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -in @('owo-agent.exe', 'owo-agent-x64.exe', 'owo-agent-desktop.exe') -and
            $_.CommandLine -and $_.CommandLine.Contains($runRoot)
        } |
        ForEach-Object {
            Write-Host "[coldboot] 清理残留进程 pid=$($_.ProcessId) name=$($_.Name)"
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        }
    $summary = [ordered]@{
        stamp         = $stamp
        run_root      = $runRoot
        evidence_dir  = $EvidenceDir
        hidden_minutes = $HiddenMinutes
        skipped_hidden = [bool]$SkipHiddenWindow
        failure_notes = $failureNotes
        checks        = @($results)
        passed        = @($results | Where-Object { $_.pass }).Count
        failed        = @($results | Where-Object { -not $_.pass }).Count
    }
    Save-Json $summary 'cold-boot-report.json' | Out-Null
    Write-Host ("[coldboot] 取证目录：{0}" -f $EvidenceDir)
}

$failed = @($results | Where-Object { -not $_.pass })
Write-Host ("[coldboot] 断言 {0}/{1} 通过" -f (@($results).Count - $failed.Count), @($results).Count)
if ($failed.Count -gt 0) { exit 1 }
exit 0
