# R11 UI 端到端验收：走完引导 → 进主界面 → 逐个一级路由，量真实 DOM 事实 + 截图。
#
# 为什么必须真渲染：用户投诉的两条（"左侧工作区一堆参数""radio 格式显示有问题"）
# 都是布局事实；另外"设置页/模型页内容是否显示"这种问题在静态检查里看不出来
# （搬动 DOM 节点 + 旧 CSS 只写了侧栏分支，就会白屏）。
#
# 用法：pwsh -NoProfile -File scripts\verify-r11-ui.ps1 [-KeepOpen]
param(
    [switch]$KeepOpen,
    [int]$DebugPort = 9333
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$root = Split-Path $PSScriptRoot -Parent
$dist = Join-Path $root "dist\OwO-Agent"
if (-not (Test-Path (Join-Path $dist "owo-agent-desktop.exe"))) { throw "缺少桌面壳：$dist" }

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$run = Join-Path $env:TEMP "owo-r11-ui-$stamp"
$workspace = Join-Path $run "workspace"
New-Item -ItemType Directory -Path (Join-Path $run "data"), $workspace -Force | Out-Null

# 只隔离数据根（不动 LOCALAPPDATA：WebView2 的 profile 在那底下）。
$env:OWO_AGENT_DATA = Join-Path $run "data"
$env:OWO_CONFIG_FILE = Join-Path $run "config.json"
@{ path = $workspace } | ConvertTo-Json -Compress |
    Set-Content -LiteralPath (Join-Path $env:OWO_AGENT_DATA "workspace.json") -Encoding UTF8
@{ version = 1; model = @{ provider = "ollama"; base_url = "http://127.0.0.1:11434/v1"; name = "local" } } |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $env:OWO_CONFIG_FILE -Encoding UTF8

Get-Process owo-agent, owo-agent-desktop -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
$shell = Start-Process -FilePath (Join-Path $dist "owo-agent-desktop.exe") -WorkingDirectory $dist -PassThru `
    -RedirectStandardOutput (Join-Path $run "shell.out.txt") -RedirectStandardError (Join-Path $run "shell.err.txt")
Write-Host "shell pid=$($shell.Id) run=$run"

function Get-CdpTarget {
    for ($i = 0; $i -lt 60; $i++) {
        try {
            $targets = Invoke-RestMethod -Uri "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 2 -Proxy $null
            $page = $targets | Where-Object { $_.type -eq 'page' -and $_.url -like "*tauri.localhost*" } | Select-Object -First 1
            if ($page) { return $page }
        } catch { }
        Start-Sleep -Milliseconds 700
    }
    throw "WebView2 CDP 目标不可用（端口 $DebugPort）"
}

$target = Get-CdpTarget
$socket = New-Object System.Net.WebSockets.ClientWebSocket
$socket.ConnectAsync([Uri]$target.webSocketDebuggerUrl, [System.Threading.CancellationToken]::None).Wait()
$script:cdpId = 0
$script:buffer = New-Object byte[] 8388608

function Invoke-CdpEval([string]$expression) {
    $script:cdpId++
    $payload = @{ id = $script:cdpId; method = "Runtime.evaluate"; params = @{
        expression = $expression; returnByValue = $true; awaitPromise = $true } } |
        ConvertTo-Json -Depth 10 -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)),
        [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()
    $stream = New-Object System.IO.MemoryStream
    while ($true) {
        $segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$script:buffer)
        $result = $socket.ReceiveAsync($segment, [System.Threading.CancellationToken]::None).Result
        $stream.Write($script:buffer, 0, $result.Count)
        if ($result.EndOfMessage) { break }
    }
    $text = [System.Text.Encoding]::UTF8.GetString($stream.ToArray())
    $stream.Dispose()
    $parsed = $text | ConvertFrom-Json
    if ($parsed.result.exceptionDetails) {
        return "JS_ERROR: " + ($parsed.result.exceptionDetails.text)
    }
    return $parsed.result.result.value
}

function Save-Shot([string]$name) {
    $script:cdpId++
    $payload = @{ id = $script:cdpId; method = "Page.captureScreenshot"; params = @{ format = "png" } } |
        ConvertTo-Json -Depth 6 -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)),
        [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()
    $stream = New-Object System.IO.MemoryStream
    while ($true) {
        $segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$script:buffer)
        $result = $socket.ReceiveAsync($segment, [System.Threading.CancellationToken]::None).Result
        $stream.Write($script:buffer, 0, $result.Count)
        if ($result.EndOfMessage) { break }
    }
    $text = [System.Text.Encoding]::UTF8.GetString($stream.ToArray())
    $stream.Dispose()
    $parsed = $text | ConvertFrom-Json
    if ($parsed.result.data) {
        $path = Join-Path $run "$name.png"
        [System.IO.File]::WriteAllBytes($path, [Convert]::FromBase64String($parsed.result.data))
        return $path
    }
    return $null
}

# 等首个会话可用（core ready + 前端拿到连接）。
$ready = $false
for ($i = 0; $i -lt 60; $i++) {
    $state = Invoke-CdpEval "document.getElementById('health') ? document.getElementById('health').textContent : ''"
    if ($state -match '已连接') { $ready = $true; break }
    Start-Sleep -Seconds 1
}
Write-Host "[引导] health 文本就绪=$ready（$state）"

# 引导页：走完"选择项目工作区 → 选择模型提供商"。
$guideStep = Invoke-CdpEval @'
(function () {
  var pathInput = document.querySelector('[data-role="path"]');
  var form = document.querySelector('[data-role="workspace-form"]');
  if (pathInput && form && !form.dataset.done) { return "workspace-pending"; }
  var providerForm = document.querySelector('[data-role="provider-form"]');
  if (providerForm && !providerForm.dataset.done) { return "provider-pending"; }
  return document.querySelector(".setup-guide") ? "guide-stuck" : "done";
})()
'@
Write-Host "[引导] 起始状态：$guideStep"

if ($guideStep -eq "workspace-pending") {
    $null = Invoke-CdpEval @'
(function () {
  var input = document.querySelector('[data-role="path"]');
  var form = document.querySelector('[data-role="workspace-form"]');
  input.value = input.placeholder && input.value ? input.value : input.value;
  form.dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
  return "submitted";
})()
'@
    Start-Sleep -Seconds 6
}

# 提供商：选 ollama（本地端点，不需要密钥），保存。
$providerResult = Invoke-CdpEval @'
(function () {
  var form = document.querySelector('[data-role="provider-form"]');
  if (!form || form.dataset.done) { return "already-done"; }
  var radio = form.querySelector('input[name="provider-mode"][value="ollama"]');
  if (radio) { radio.checked = true; radio.dispatchEvent(new Event("change", { bubbles: true })); }
  var base = form.querySelector('[data-role="base-url"]');
  var name = form.querySelector('[data-role="model"]');
  if (base && !base.value) base.value = "http://127.0.0.1:11434/v1";
  if (name && !name.value) name.value = "local";
  form.dispatchEvent(new Event("submit", { cancelable: true, bubbles: true }));
  return "submitted";
})()
'@
Write-Host "[引导] 提供商提交：$providerResult"
Start-Sleep -Seconds 8

# 主界面事实：侧栏常显区块 + 一级路由按钮。
$mainFacts = Invoke-CdpEval @'
(function () {
  function visible(el) { return el && el.offsetParent !== null; }
  var sidebar = document.getElementById("sidebar");
  var sections = sidebar ? Array.prototype.slice.call(sidebar.children).filter(function (n) {
    return n.tagName === "SECTION" && visible(n);
  }) : [];
  return JSON.stringify({
    setupGuidePresent: !!document.querySelector(".setup-guide"),
    health: (document.getElementById("health") || {}).textContent,
    sidebarVisibleSectionCount: sections.length,
    sidebarVisibleSections: sections.map(function (n) {
      var h = n.querySelector("h2"); return h ? h.textContent.trim() : "(no-title)";
    }),
    railButtons: Array.prototype.slice.call(document.querySelectorAll("[data-rail-target]")).map(function (b) { return b.dataset.railTarget; }),
    toolsPanelVisible: visible(document.getElementById("toolsPanel")),
    toolsPanelChildCount: document.getElementById("toolsPanel") ? document.getElementById("toolsPanel").children.length : 0
  });
})()
'@
Write-Host "[主界面] $mainFacts"
$mainShot = Save-Shot "after-setup"

# 逐个一级路由：量内容是否真的渲染（不是空白）。
$routeFacts = @()
foreach ($route in @("model", "settings", "projects", "permissions")) {
    $null = Invoke-CdpEval "window.owoRouter && window.owoRouter.go('$route'); 'ok'"
    Start-Sleep -Seconds 3
    $fact = Invoke-CdpEval @'
(function () {
  function visible(el) { return el && el.offsetParent !== null; }
  var content = document.getElementById("routeContent");
  var settings = document.getElementById("settingsSection");
  function visibleTextOf(el) {
    if (!el) return "";
    var clone = el.cloneNode(true);
    return (clone.innerText || clone.textContent || "").trim();
  }
  return JSON.stringify({
    route: location.search || "(chat)",
    routeContentVisible: visible(content),
    routeContentTextLength: visibleTextOf(content).length,
    routeHeaderText: (document.getElementById("routeHeader") || {}).innerText,
    settingsVisible: visible(settings),
    settingsTextLength: visibleTextOf(settings).length,
    modelCardVisible: visible(document.getElementById("modelCard")),
    sessionModelCardVisible: visible(document.getElementById("sessionModelCard")),
    sidebarVisible: visible(document.getElementById("sidebar")),
    toolCapGridVisible: visible(document.querySelector(".tool-capabilities-grid"))
  });
})()
'@
    $routeFacts += [pscustomobject]@{ route = $route; facts = $fact }
    Write-Host "[路由 $route] $fact"
    $null = Save-Shot "route-$route"
}

# 模型页关键字段可见性（用户投诉"找不到"的那几项）。
$null = Invoke-CdpEval "window.owoRouter && window.owoRouter.go('model'); 'ok'"
Start-Sleep -Seconds 3
$modelFields = Invoke-CdpEval @'
(function () {
  function info(id) {
    var el = document.getElementById(id);
    if (!el) return null;
    var r = el.getBoundingClientRect();
    return { visible: el.offsetParent !== null, w: Math.round(r.width), h: Math.round(r.height) };
  }
  return JSON.stringify({
    provider: info("settingsProvider"),
    baseUrl: info("settingsBaseUrl"),
    modelName: info("settingsModelName"),
    apiKey: info("settingsApiKey"),
    apiKeyEnv: info("settingsApiKeyEnv"),
    applyBtn: info("modelApplyBtn"),
    configPathText: (document.getElementById("modelConfigPath") || {}).textContent,
    hintText: (document.getElementById("modelHint") || {}).textContent
  });
})()
'@
Write-Host "[模型页字段] $modelFields"
$modelShot = Save-Shot "model-page"

$socket.Dispose()
if (-not $KeepOpen) {
    Get-Process owo-agent-desktop -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dist*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process owo-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dist*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue
}
Write-Host ""
Write-Host "产物目录：$run"
Write-Host "截图：$mainShot / $modelShot"
