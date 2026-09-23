# 关键回归测试：在 Electron 界面里**真的用键盘输入**，确认输入框可用且内容不丢。
# 用 CDP 的 Input.insertText / Input.dispatchKeyEvent（真实键入，不是直接改 value），
# 这样才能抓到"每敲一字重建视图导致焦点丢失"这类问题。
param(
    [int]$DebugPort = 9444,
    [string]$Text = "你好，请用一句话自我介绍",
    [switch]$ThenSend
)
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$targets = Invoke-RestMethod -Uri "http://127.0.0.1:$DebugPort/json/list" -TimeoutSec 5 -Proxy $null
$page = $targets | Where-Object { $_.type -eq 'page' } | Select-Object -First 1
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
$null = Send-Cdp "Input.enable" @{} | Out-Null

# 1) 点一下输入框（真实点击焦点）
$focused = Eval @'
(function () {
  var ta = document.querySelector(".composer-input");
  if (!ta) return "no-input";
  ta.focus();
  ta.click();
  return document.activeElement === ta ? "focused" : ("active=" + (document.activeElement && document.activeElement.className));
})()
'@
Write-Host "聚焦：$focused"

# 2) 逐个字符真实键入（这里是最能暴露重建丢焦点的路径）
$typed = ""
foreach ($ch in $Text.ToCharArray()) {
    $null = Send-Cdp "Input.insertText" @{ text = [string]$ch }
    Start-Sleep -Milliseconds 60
    $current = Eval 'document.querySelector(".composer-input") ? document.querySelector(".composer-input").value : "<没了>"'
    if ($current -ne $typed + [string]$ch) {
        Write-Host "❌ 第 $($typed.Length + 1) 个字符后输入框内容不符：期望 '$typed$ch'，实际 '$current'"
        break
    }
    $typed += [string]$ch
}
Write-Host "键入完成：'$typed'"

# 3) 焦点与内容最终状态
Write-Host (Eval @'
(function () {
  var ta = document.querySelector(".composer-input");
  return JSON.stringify({
    value: ta ? ta.value : null,
    valueLength: ta ? ta.value.length : -1,
    stillFocused: document.activeElement === ta,
    activeTag: document.activeElement ? document.activeElement.className || document.activeElement.tagName : null
  });
})()
'@)

if ($ThenSend) {
    Write-Host "=== 按 Enter 发送 ==="
    $null = Send-Cdp "Input.dispatchKeyEvent" @{ type = "keyDown"; key = "Enter"; code = "Enter"; windowsVirtualKeyCode = 13 }
    $null = Send-Cdp "Input.dispatchKeyEvent" @{ type = "keyUp"; key = "Enter"; code = "Enter"; windowsVirtualKeyCode = 13 }
    Start-Sleep -Seconds 2
    $deadline = (Get-Date).AddSeconds(150)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        $snap = Eval 'JSON.stringify({m:(window.owoStore.messages||[]).length,s:!!window.owoStore.streaming,n:window.owoStore.notice||""})'
        Write-Host "  $snap"
        if ($snap -match '"s":false' -and $snap -match '"m":2') { break }
    }
    Write-Host "=== 对话结果 ==="
    Write-Host (Eval @'
(function () {
  var s = window.owoStore;
  return JSON.stringify((s.messages || []).map(function (m) { return { role: m.role, content: String(m.content).slice(0, 160) }; }), null, 1);
})()
'@)
}
$socket.Dispose()
