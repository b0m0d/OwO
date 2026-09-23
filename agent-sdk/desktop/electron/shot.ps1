# Electron（Chromium）渲染层截图 + DOM 事实：复用 CDP，但目标是标准 Chromium 协议。
param(
    [int]$DebugPort = 9444,
    [string]$OutDir = "",
    [string]$Route = ""
)
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
if (-not $OutDir) { $OutDir = Join-Path $env:TEMP ("owo-el-shots-" + (Get-Date -Format 'HHmmss')) }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

$targets = Invoke-RestMethod -Uri "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 5 -Proxy $null
$page = $targets | Where-Object { $_.type -eq 'page' } | Select-Object -First 1
if (-not $page) { throw "找不到 Electron 页面目标" }
Write-Host "target: $($page.url)"

$socket = New-Object System.Net.WebSockets.ClientWebSocket
$socket.ConnectAsync([Uri]$page.webSocketDebuggerUrl, [System.Threading.CancellationToken]::None).Wait()
$script:id = 0
$script:buf = New-Object byte[] 16777216

function Send-Cdp([string]$method, [hashtable]$arguments) {
    $script:id++
    $rid = $script:id
    $payload = @{ id = $rid; method = $method; params = $arguments } | ConvertTo-Json -Depth 10 -Compress
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
    $socket.SendAsync((New-Object System.ArraySegment[byte] -ArgumentList @(,$bytes)),
        [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [System.Threading.CancellationToken]::None).Wait()
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $stream = New-Object System.IO.MemoryStream
        while ($true) {
            $seg = New-Object System.ArraySegment[byte] -ArgumentList @(,$script:buf)
            $r = $socket.ReceiveAsync($seg, [System.Threading.CancellationToken]::None).Result
            $stream.Write($script:buf, 0, $r.Count)
            if ($r.EndOfMessage) { break }
        }
        $text = [System.Text.Encoding]::UTF8.GetString($stream.ToArray()); $stream.Dispose()
        $p = $text | ConvertFrom-Json
        if ($p.id -eq $rid) { return $text }
    }
    throw "CDP 超时：$method"
}
function Eval([string]$e) {
    $raw = Send-Cdp "Runtime.evaluate" @{ expression = $e; returnByValue = $true; awaitPromise = $true }
    $parsed = $raw | ConvertFrom-Json
    if ($parsed.result.exceptionDetails) { return "JS_ERROR: " + $parsed.result.exceptionDetails.text }
    return $parsed.result.result.value
}

$null = Send-Cdp "Page.enable" @{}

if ($Route) {
    # 用**界面自己的导航按钮**切路由（模拟真实点击，比直接改状态更能发现问题）。
    $clicked = Eval ("(function(){var map={chat:'任务',model:'模型',permissions:'权限',diagnostics:'诊断'};" +
        "var want=map['" + $Route + "'];var btns=document.querySelectorAll('.rail-btn');" +
        "for (var i=0;i<btns.length;i++){ if (btns[i].textContent.indexOf(want)>=0){ btns[i].click(); return 'clicked:'+want; } }" +
        "return 'not-found:'+want;})()")
    Write-Host "nav: $clicked"
    Start-Sleep -Seconds 2
}

Write-Host "== DOM 事实 =="
Write-Host (Eval @'
(function () {
  var s = window.owoStore || {};
  return JSON.stringify({
    route: s.route,
    coreState: (s.coreState && s.coreState.state) || null,
    port: s.connection && s.connection.port,
    health: s.health ? (s.health.healthy + "/" + s.health.api_version) : null,
    workspace: s.workspace,
    configPath: s.configPath,
    provider: s.config && s.config.model ? s.config.model.provider : null,
    baseUrl: s.config && s.config.model ? s.config.model.base_url : null,
    modelName: s.config && s.config.model ? s.config.model.name : null,
    contextWindow: s.config && s.config.model ? s.config.model.context_window : null,
    sessions: (s.sessions || []).length,
    messages: (s.messages || []).length,
    railButtons: Array.prototype.slice.call(document.querySelectorAll(".rail-btn")).map(function (b) { return b.textContent.trim(); }),
    panelCount: document.querySelectorAll(".panel").length,
    bodyTextLength: document.body.innerText.length,
    notice: s.notice || ""
  }, null, 1);
})()
'@)

$raw = Send-Cdp "Page.captureScreenshot" @{ format = "png" }
$data = ($raw | ConvertFrom-Json).result.data
if ($data) {
    $name = if ($Route) { "route-$Route.png" } else { "screen.png" }
    $path = Join-Path $OutDir $name
    [System.IO.File]::WriteAllBytes($path, [Convert]::FromBase64String($data))
    Write-Host "shot: $path"
}
$socket.Dispose()
Write-Host "out: $OutDir"
