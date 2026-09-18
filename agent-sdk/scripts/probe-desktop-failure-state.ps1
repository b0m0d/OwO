<#
.SYNOPSIS
    R3-B 诊断探针（一次性排障工具，不进门禁、不产出验收结论）。
.DESCRIPTION
    以与故障矩阵同一套私有环境（独立 LOCALAPPDATA/APPDATA/TEMP + 验收模式 sidecar 根）
    拉起桌面壳，然后用 WebView2 CDP 直接询问界面与壳侧事实：get_core_connection /
    get_core_state 的真实返回、Web 侧连接快照、boot 状态、引导页/错误卡可见性、
    诊断快照与日志目录内容。用途是把"界面为什么没进引导页"从推测变成可读证据。

    用法：
      .\scripts\probe-desktop-failure-state.ps1                        # 默认 no-workspace
      .\scripts\probe-desktop-failure-state.ps1 -ScenarioId provider-unset
      .\scripts\probe-desktop-failure-state.ps1 -ScenarioId healthy -KeepRunning
.PARAMETER ScenarioId
    no-workspace（无工作区）| provider-unset（有工作区、无密钥）| healthy（有工作区+有密钥）。
#>
[CmdletBinding()]
param(
    [ValidateSet('no-workspace', 'provider-unset', 'healthy')]
    [string]$ScenarioId = 'no-workspace',
    [int]$WaitSec = 12,
    [switch]$KeepRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'desktop-acceptance-common.ps1')

$repoRoot = Split-Path -Parent $PSScriptRoot
$shellExe = Join-Path $repoRoot 'desktop\tauri\src-tauri\target\debug\owo-agent-desktop.exe'
$sidecarExe = Join-Path $repoRoot 'target\debug\owo-agent.exe'
if (-not (Test-Path -LiteralPath $shellExe)) { throw "缺少桌面壳：$shellExe" }
$apiKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')

$runId = 'probe-' + (Get-Date).ToString('yyyyMMdd-HHmmss')
$root = Join-Path $env:TEMP ('owo-desktop-probe-' + $runId)
$dir = Join-Path $root $ScenarioId
$dataDir = Join-Path $dir 'data'
$lad = Join-Path $dataDir 'LocalAppData'
$rad = Join-Path $dataDir 'RoamingAppData'
$tmp = Join-Path $dataDir 'Temp'
$agentLeaf = Join-Path $lad 'OwO\Agent'
foreach ($d in @($dir, $dataDir, $lad, $rad, $tmp, $agentLeaf)) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

function Get-ProbeFact {
    param([int]$Port, [string]$Label, [string]$Expression)
    $value = Invoke-OwoCdpEval -Port $Port -Expression $Expression -TimeoutSec 8
    if ($null -eq $value) {
        $why = '<none>'
        if (Test-Path variable:global:OwoCdpLastError) { $why = [string]$global:OwoCdpLastError }
        $tr = '<no trace>'
        if (Test-Path variable:global:OwoCdpTrace) { $tr = (($global:OwoCdpTrace | Select-Object -Last 4) -join ' | ') }
        $value = "<null> :: $why :: $tr"
    }
    "{0,-22} {1}" -f $Label, [string]$value
}

$proc = $null
try {
    # ---- 场景装配：只造界面状态，绝不触碰真实用户数据 ----
    if ($ScenarioId -ne 'no-workspace') {
        $project = Join-Path $root 'project'
        New-Item -ItemType Directory -Force -Path $project | Out-Null
        Set-Content -Path (Join-Path $agentLeaf 'workspace.json') `
            -Value ('{"path":"' + ($project -replace '\\', '\\') + '"}') -Encoding ASCII -NoNewline
    }
    if ($ScenarioId -ne 'no-workspace') {
        if (-not (Test-Path -LiteralPath $sidecarExe)) { throw "缺少 sidecar：$sidecarExe" }
        Copy-Item -LiteralPath $sidecarExe -Destination (Join-Path $dir 'owo-agent.exe') -Force
    }
    foreach ($dll in @(Get-ChildItem -LiteralPath (Split-Path -Parent $shellExe) -Filter '*.dll' -ErrorAction SilentlyContinue)) {
        Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $dir $dll.Name) -Force
    }
    Copy-Item -LiteralPath $shellExe -Destination (Join-Path $dir 'owo-agent-desktop.exe') -Force

    $cdpRequested = Get-OwoFreeTcpPort
    $webViewUdf = Join-Path $dataDir 'WebView2'
    New-Item -ItemType Directory -Force -Path $webViewUdf | Out-Null
    $needsKey = ($ScenarioId -eq 'healthy')
    $psi = New-OwoShellStartInfo -ShellExe (Join-Path $dir 'owo-agent-desktop.exe') -LocalAppData $lad -AppData $rad `
        -TempDir $tmp -ApiKey $(if ($needsKey) { $apiKey } else { '' }) -CdpPort $cdpRequested `
        -WebViewUserDataDir $webViewUdf -AcceptanceSidecarRoot $dir -NoApiKey:(-not $needsKey)
    $proc = [System.Diagnostics.Process]::Start($psi)
    "[$ScenarioId] 壳 PID=$($proc.Id) sidecar=$(if (Test-Path -LiteralPath (Join-Path $dir 'owo-agent.exe')) { 'staged' } else { '<none>' }) 工作区=$(if (Test-Path -LiteralPath (Join-Path $agentLeaf 'workspace.json')) { 'yes' } else { 'no' }) 密钥=$(if ($needsKey) { 'yes' } else { 'no' })"

    $win = Get-OwoValidatedWindow -ProcessId $proc.Id -TimeoutSec 20
    "窗口                   hwnd=$($win.hwnd) 度量=$($win.width)x$($win.height) fail=$($win.fail_reasons -join ';')"
    $etb = Get-OwoWebviewEtbDir -UserDataDir $webViewUdf
    $port = Resolve-OwoCdpPort -DataDir $etb -RequestedPort $cdpRequested -TimeoutSec 20
    "CDP 端口                $port"
    if ($port -le 0) { throw "CDP 未就绪：$((Get-OwoWebviewDiagnostics -RequestedPort $cdpRequested -DataDir $etb) | ConvertTo-Json -Compress)" }
    # 时间分辨采样：引导页到底是"没渲染"还是"渲染后被清掉"，以及有没有 JS 异常，
    # 只能靠连续采样判定——单次快照在本场景两次给出不同答案。
    $hook = '(()=>{window.__probeErrs=window.__probeErrs||[];' +
        'if(!window.__probeHooked){window.__probeHooked=1;' +
        'addEventListener("error",e=>window.__probeErrs.push("error:"+(e&&e.message)));' +
        'addEventListener("unhandledrejection",e=>window.__probeErrs.push("rej:"+String((e&&e.reason&&e.reason.message)||e)));}return "hooked"})()'
    Get-ProbeFact -Port $port -Label 'hook' -Expression $hook
    $sampler = '(()=>{const c=document.getElementById("routeContent"),h=document.getElementById("routeHeader"),v=document.getElementById("routeView");' +
        'return JSON.stringify({contentLen:c?c.textContent.length:-1,cards:document.querySelectorAll(".setup-card").length,' +
        'guideNode:document.querySelectorAll(".setup-guide").length,errorCard:document.querySelectorAll("#coreErrorCard,.service-error").length,' +
        'viewHidden:v?v.hidden:null,header:(h?h.textContent:"").slice(0,14),state:(window.__owoCoreDiagnostics||{}).state||null,errs:(window.__probeErrs||[]).slice(-3)})})()'
    $marks = @(1, 3, 6, 10, [Math]::Max(12, $WaitSec))
    $prev = 0
    foreach ($at in $marks) {
        Start-Sleep -Seconds ([Math]::Max(0, $at - $prev)); $prev = $at
        Get-ProbeFact -Port $port -Label ("sample t={0}s" -f $at) -Expression $sampler
    }

    "---- 壳侧真实返回（界面判定的唯一依据） ----"
    Get-ProbeFact -Port $port -Label 'get_core_connection' -Expression '(async()=>{try{return JSON.stringify(await window.__TAURI__.core.invoke("get_core_connection"))}catch(e){return "THROW: "+String((e&&e.message)||e)}})()'
    Get-ProbeFact -Port $port -Label 'get_core_state' -Expression '(async()=>{try{return JSON.stringify(await window.__TAURI__.core.invoke("get_core_state"))}catch(e){return "THROW: "+String((e&&e.message)||e)}})()'
    "---- Web 侧判定 ----"
    Get-ProbeFact -Port $port -Label 'canary:globals' -Expression 'JSON.stringify({shell:typeof window.__owoAgentShell,api:typeof window.OwoApiClient,readiness:typeof window.OwoServiceReadiness,guide:typeof window.renderOwoSetupGuide,errCard:typeof window.renderOwoServiceError})'
    Get-ProbeFact -Port $port -Label 'canary:invokeOwner' -Expression '(()=>{const o=window.OwoApiClient&&window.OwoApiClient.tauriInvokeOwner?window.OwoApiClient.tauriInvokeOwner(window):"no-fn";return JSON.stringify({owner:!!o,keys:o&&typeof o==="object"?Object.keys(o).slice(0,8):typeof o})})()'
    Get-ProbeFact -Port $port -Label 'canary:dom' -Expression 'JSON.stringify({bodyId:document.body?document.body.id:"?",setupGuide:!!document.getElementById("setupGuide"),routeContent:!!document.getElementById("routeContent"),errorCard:!!document.getElementById("coreErrorCard"),cards:Array.from(document.querySelectorAll(".owo-service-error,.service-error,[data-error-code]")).length})'
    Get-ProbeFact -Port $port -Label 'connectionSnapshot' -Expression 'JSON.stringify(window.__owoAgentShell.getConnectionSnapshot())'
    Get-ProbeFact -Port $port -Label 'canary:routeView' -Expression 'JSON.stringify({exists:!!document.getElementById("routeView"),hidden:document.getElementById("routeView").hidden,header:(document.getElementById("routeHeader").textContent||"").slice(0,40),contentLen:(document.getElementById("routeContent").textContent||"").length,bodyClass:document.body.className})'
    Get-ProbeFact -Port $port -Label 'canary:page' -Expression 'JSON.stringify({href:location.href.slice(0,60),title:document.title,route:(document.getElementById("routeContent")||{}).textContent})'
    Get-ProbeFact -Port $port -Label 'directTauriProbe' -Expression '(async()=>{try{const r=await window.__TAURI__.core.invoke("get_core_connection");return JSON.stringify(r)}catch(e){return "THROW "+String((e&&e.message)||e)}})()'
    Get-ProbeFact -Port $port -Label 'diagnostics' -Expression 'JSON.stringify(window.__owoCoreDiagnostics)'
    Get-ProbeFact -Port $port -Label 'bootState' -Expression '(()=>{const d=document.getElementById("bootSplash");return "data-boot-state="+(document.body?document.body.getAttribute("data-boot-state"):"")+" bootSplash="+(d?(d.hidden?"hidden":"visible"):"missing")})()'
    Get-ProbeFact -Port $port -Label 'guide/card' -Expression '(()=>{const g=document.getElementById("setupGuide"),c=document.getElementById("coreErrorCard");return "guideVisible="+(g?!g.classList.contains("hidden"):"?")+" cardVisible="+(c?!c.classList.contains("hidden"):"?")})()'
    Get-ProbeFact -Port $port -Label 'guideActions' -Expression 'Array.from(document.querySelectorAll("#setupGuide button,#setupGuide a")).map(e=>e.textContent.trim()).join("/")'
    Get-ProbeFact -Port $port -Label 'cardActions' -Expression 'Array.from(document.querySelectorAll("#coreErrorCard button,#coreErrorCard a")).map(e=>e.textContent.trim()).join("/")'
    "---- 可观测性 ----"
    $logs = @(Get-ChildItem -LiteralPath (Join-Path $agentLeaf 'logs') -ErrorAction SilentlyContinue)
    "日志目录                $(if ($logs.Count) { ($logs | ForEach-Object { "$($_.Name)($($_.Length)B)" }) -join ', ' } else { '<空或不存在>' })"
    $runtime = Join-Path $agentLeaf 'runtime.json'
    "runtime.json            $(if (Test-Path -LiteralPath $runtime) { (Get-Content -LiteralPath $runtime -Raw) } else { '<无>' })"
    "sidecar 进程             $((@(Get-Process -Name owo-agent -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })) -join ',')"
} finally {
    if (-not $KeepRunning) {
        if ($proc) { Stop-OwoShellTree -ProcessId $proc.Id -RunRoot $root }
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    } else {
        "已保留运行中的壳与目录：$root"
    }
}
exit 0
