# 在 Electron 界面里发一条真实消息，验证"能直接对话"。
# 用界面自己的输入框与发送按钮（模拟真实用户操作），并等流式回复。
param(
    [int]$DebugPort = 9444,
    [string]$Prompt = "用一句话回答：1+1 等于几？",
    [int]$TimeoutSec = 120
)
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$targets = Invoke-RestMethod -Uri "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 5 -Proxy $null
$page = $targets | Where-Object { $_.type -eq 'page' } | Select-Object -First 1
if (-not $page) { throw "找不到 Electron 页面" }
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

# 1) 用界面输入框填入并触发 input 事件，再点发送按钮（真实用户路径）
$promptJson = ConvertTo-Json -InputObject $Prompt -Compress
$typed = Eval ("(function(){var ta=document.querySelector('.composer textarea');" +
    "if(!ta) return 'no-textarea';" +
    "var setter=Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value').set;" +
    "setter.call(ta, " + $promptJson + ");" +
    "ta.dispatchEvent(new Event('input', {bubbles:true}));" +
    "var btn=document.querySelector('.composer button[type=submit]');" +
    "if(!btn) return 'no-button';" +
    "btn.click(); return 'sent';})()")
Write-Host "输入与发送：$typed"

# 2) 等消息出现（user + assistant）
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$last = ""
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    $snapshot = Eval @'
(function () {
  var s = window.owoStore || {};
  return JSON.stringify({
    messages: (s.messages || []).length,
    streaming: !!s.streaming,
    streamingLen: (s.streamingText || "").length,
    lastAssistant: (function () {
      var list = (s.messages || []).filter(function (m) { return m.role !== "user"; });
      return list.length ? String(list[list.length - 1].content).slice(0, 300) : "";
    })(),
    notice: s.notice || "",
    domMsgCount: document.querySelectorAll(".msg").length,
    streamingInDom: !!document.querySelector(".msg.streaming")
  });
})()
'@
    if ($snapshot -ne $last) { Write-Host "  $snapshot"; $last = $snapshot }
    if ($snapshot -match '"streaming":false' -and $snapshot -match '"messages":2') { break }
}

Write-Host ""
Write-Host "=== 最终 ==="
Write-Host (Eval @'
(function () {
  var s = window.owoStore || {};
  return JSON.stringify({
    messages: (s.messages || []).map(function (m) { return { role: m.role, content: String(m.content || "").slice(0, 200) }; }),
    notice: s.notice || "",
    sessionId: s.currentSessionId
  }, null, 1);
})()
'@)
$socket.Dispose()
