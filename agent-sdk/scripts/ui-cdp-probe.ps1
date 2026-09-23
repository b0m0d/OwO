# UI 取证：连 WebView2 的 CDP，取"设置/模型/引导页"的真实 DOM 事实 + 截图。
#
# 为什么不用肉眼看截图：用户投诉的两条（"左侧一堆参数""radio 排版错乱"）都是
# **布局事实**——flex 行内元素的实际位置/尺寸/换行状态。用 CDP 量出来才是证据，
# 截图只作为旁证。本脚本自己起 WebView2 调试端口（必须由 PowerShell 5.1 跑，
# ClientWebSocket 在 5.1 上可用）。
param(
    [int]$DebugPort = 9333,
    [string]$OutDir = ""
)

$ErrorActionPreference = "Stop"
if (-not $OutDir) { $OutDir = Join-Path $env:TEMP ("owo-ui-" + (Get-Date -Format 'HHmmss')) }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

function Get-PageTarget {
    $targets = Invoke-RestMethod -Uri "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 3 -Proxy $null
    return ($targets | Where-Object { $_.type -eq 'page' } | Select-Object -First 1)
}

# 与 WebView2 的 CDP 通信：一个连接上按 id 顺序收发命令（够用，不搞并发）。
function Invoke-Cdp {
    param(
        [System.Net.WebSockets.ClientWebSocket]$Socket,
        [int]$Id,
        [string]$Method,
        [hashtable]$Params = @{}
    )
    $payload = @{ id = $Id; method = $Method; params = $Params } | ConvertTo-Json -Depth 10 -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $segment = New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)
    $Socket.SendAsync($segment, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()

    $buffer = New-Object byte[] 4194304
    $stream = New-Object System.IO.MemoryStream
    while ($true) {
        $recvSegment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)
        $result = $Socket.ReceiveAsync($recvSegment, [System.Threading.CancellationToken]::None).Result
        $stream.Write($buffer, 0, $result.Count)
        if ($result.EndOfMessage) { break }
    }
    $text = [System.Text.Encoding]::UTF8.GetString($stream.ToArray())
    $stream.Dispose()
    return $text
}

$target = Get-PageTarget
if (-not $target) { throw "找不到页面目标（WebView2 调试端口 $DebugPort 无 page）" }
Write-Host "target: $($target.url)"

$socket = New-Object System.Net.WebSockets.ClientWebSocket
$socket.ConnectAsync([Uri]$target.webSocketDebuggerUrl, [System.Threading.CancellationToken]::None).Wait()

$id = 1
# 探针：量三件事——① 侧栏常显区块数；② 引导页 radio 行是否单行、是否等宽；
# ③ 模型配置字段（base_url / 模型名 / API Key / 配置路径）是否存在且可见。
$probe = @'
(function () {
  function rect(el) {
    if (!el) return null;
    var r = el.getBoundingClientRect();
    return { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) };
  }
  function text(el) { return el ? (el.textContent || "").trim() : null; }
  var sidebar = document.getElementById("sidebar");
  var sections = sidebar ? Array.prototype.slice.call(sidebar.children).filter(function (n) {
    return n.tagName === "SECTION" && n.offsetParent !== null;
  }) : [];
  var radios = Array.prototype.slice.call(document.querySelectorAll('.setup-row label'));
  var radioTexts = radios.map(text);
  var uniqueTops = {};
  radios.forEach(function (el) { uniqueTops[Math.round(el.getBoundingClientRect().top)] = 1; });
  var radioRects = radios.map(rect);
  var widths = radioRects.map(function (r) { return r ? r.w : 0; });
  return JSON.stringify({
    route: location.search || "(chat)",
    bodyClasses: document.body.className,
    sidebarVisibleSections: sections.map(text).map(function (t) { return (t || "").slice(0, 12); }),
    setupGuidePresent: !!document.querySelector(".setup-guide"),
    radios: {
      count: radios.length,
      labels: radioTexts,
      rows: Object.keys(uniqueTops).length,
      rects: radioRects,
      widthSpread: widths.length ? Math.max.apply(null, widths) - Math.min.apply(null, widths) : 0,
      display: radios.length ? getComputedStyle(radios[0]).display : null,
      whiteSpace: radios.length ? getComputedStyle(radios[0]).whiteSpace : null,
      lineHeight: radios.length ? Math.round(radios[0].getBoundingClientRect().height) : 0
    },
    modelFields: {
      provider: !!document.getElementById("settingsProvider"),
      baseUrl: !!document.getElementById("settingsBaseUrl"),
      modelName: !!document.getElementById("settingsModelName"),
      apiKey: !!document.getElementById("settingsApiKey"),
      apiKeyEnv: !!document.getElementById("settingsApiKeyEnv"),
      configPath: text(document.getElementById("modelConfigPath")),
      sessionModel: !!document.getElementById("sessionModelCard")
    },
    railButtons: Array.prototype.slice.call(document.querySelectorAll("[data-rail-target]")).map(function (b) {
      return b.dataset.railTarget;
    }),
    toolCapGridVisible: (function () {
      var el = document.querySelector(".tool-capabilities-grid");
      return el ? el.offsetParent !== null : null;
    })()
  });
})()
'@
$probePayload = @{ id = $id; method = "Runtime.evaluate"; params = @{ expression = $probe; returnByValue = $true } } | ConvertTo-Json -Depth 10 -Compress
$bytes = [System.Text.Encoding]::UTF8.GetBytes($probePayload)
$socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)), [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()
$buffer = New-Object byte[] 4194304
$stream = New-Object System.IO.MemoryStream
while ($true) {
    $recvSegment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)
    $result = $socket.ReceiveAsync($recvSegment, [System.Threading.CancellationToken]::None).Result
    $stream.Write($buffer, 0, $result.Count)
    if ($result.EndOfMessage) { break }
}
$probeResponse = [System.Text.Encoding]::UTF8.GetString($stream.ToArray())
$stream.Dispose()

$parsed = $probeResponse | ConvertFrom-Json
$value = $parsed.result.result.value
Write-Host "=== 引导页/布局事实 ==="
Write-Host $value
$value | Set-Content -LiteralPath (Join-Path $OutDir "dom-facts.json") -Encoding UTF8

# 截图（全页，PNG base64）
$id = 2
$shotPayload = @{ id = $id; method = "Page.captureScreenshot"; params = @{ format = "png" } } | ConvertTo-Json -Depth 6 -Compress
$bytes = [System.Text.Encoding]::UTF8.GetBytes($shotPayload)
$socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)), [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()
$stream = New-Object System.IO.MemoryStream
while ($true) {
    $recvSegment = New-Object System.ArraySegment[byte] -ArgumentList @(,$buffer)
    $result = $socket.ReceiveAsync($recvSegment, [System.Threading.CancellationToken]::None).Result
    $stream.Write($buffer, 0, $result.Count)
    if ($result.EndOfMessage) { break }
}
$shotResponse = [System.Text.Encoding]::UTF8.GetString($stream.ToArray()) | ConvertFrom-Json
if ($shotResponse.result.data) {
    $png = [Convert]::FromBase64String($shotResponse.result.data)
    $shotPath = Join-Path $OutDir "screen.png"
    [System.IO.File]::WriteAllBytes($shotPath, $png)
    Write-Host "screenshot: $shotPath ($([math]::Round($png.Length/1KB,1)) KB)"
} else {
    Write-Host "截图失败：$shotResponse"
}
$socket.Dispose()
Write-Host "out: $OutDir"
