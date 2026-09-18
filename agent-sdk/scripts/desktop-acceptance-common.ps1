#requires -Version 5.1
<#
desktop-acceptance-common.ps1 — 桌面真机验收的共享原语（重构方案 §8.1/§8.2/§8.3）。

为什么存在：§8.2（冷启动请求预算）与 §8.3（故障矩阵）都要做同一批底层动作
——私有环境隔离地拉起壳、等 core_ready、抓窗口、按分辨率截图、带 token 读
服务端 ledger、经 DevTools 读 WebView 实际渲染的文本。此前这些能力只长在
verify-desktop-cold-boot.ps1 里；§8.3 若再抄一份就是审计反复点名的
「第二份实现」（方案 §7.2 同类问题）。本文件只放**无场景判断**的原语，
场景与断言留在调用方。

约定：
  * 只 dot-source 使用（` . scripts\desktop-acceptance-common.ps1`）；
    不得设置调用方的 $ErrorActionPreference（历史教训：Stop 泄漏进
    ci-gate 会话把 cargo/npm 的正常 stderr 判成终止错误）。
  * 函数一律 `Owo` 前缀；本文件顶层无副作用（不建目录、不联网、不弹窗）。
  * 取证产物 JSON 一律无 BOM 写出（PowerShell 5.1 的 `-Encoding UTF8` 带
    BOM，会让 node/JSON.parse 读证据文件时炸「Unexpected token ﻿」）。
#>

$script:OwoAcceptanceProbeHeader = 'probe'

function Add-OwoDesktopWin32 {
    <# 幂等加载截图/窗口几何所需程序集与 OwoWin32（可重复调用）。 #>
    Add-Type -AssemblyName System.Drawing
    Add-Type -AssemblyName System.Windows.Forms
    if (-not ('OwoWin32' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class OwoWin32 {
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hWnd, int X, int Y, int W, int H, bool repaint);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out OwoRect rect);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [StructLayout(LayoutKind.Sequential)] public struct OwoRect { public int Left; public int Top; public int Right; public int Bottom; }
}
'@
    }
}

function Get-OwoShellWindow {
    <# 等主窗口句柄出现；超时返回 IntPtr.Zero（不得把"没窗口"当成功）。 #>
    param([int]$ProcessId, [int]$TimeoutSec = 20)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $proc = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
        if ($proc -and $proc.MainWindowHandle -ne [IntPtr]::Zero) { return $proc.MainWindowHandle }
        Start-Sleep -Milliseconds 300
    }
    return [IntPtr]::Zero
}

function Set-OwoWindowGeometry {
    <# 居中缩放到目标尺寸并返回实际尺寸（受虚拟屏限制，可能被夹小）。 #>
    param([IntPtr]$Hwnd, [int]$Width, [int]$Height)
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $targetW = $Width
    $targetH = $Height
    if ($targetW -gt $bounds.Width) { $targetW = $bounds.Width }
    if ($targetH -gt $bounds.Height) { $targetH = $bounds.Height }
    $x = [Math]::Max(0, [int](($bounds.Width - $targetW) / 2))
    $y = [Math]::Max(0, [int](($bounds.Height - $targetH) / 2))
    $null = [OwoWin32]::MoveWindow($Hwnd, $x, $y, $targetW, $targetH, $true)
    return "$targetW x $targetH"
}

function Set-OwoWindowShown {
    param([IntPtr]$Hwnd, [int]$Cmd)   # 0=SW_HIDE 5=SW_SHOW
    return [OwoWin32]::ShowWindow($Hwnd, $Cmd)
}

function Save-OwoWindowShot {
    <# 按窗口矩形截屏到 $Path（PNG），返回 "宽 x 高"。 #>
    param([IntPtr]$Hwnd, [string]$Path)
    $rect = New-Object OwoWin32+OwoRect
    if (-not [OwoWin32]::GetWindowRect($Hwnd, [ref]$rect)) { throw 'GetWindowRect 失败' }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) { throw "窗口尺寸无效：${width}x${height}" }
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size($width, $height)))
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
    return "$width x $height"
}

function Save-OwoEvidenceJson {
    <# 无 BOM 写证据 JSON（禁止改用 Set-Content -Encoding UTF8）。 #>
    param([string]$Dir, [string]$Name, $InputObject, [int]$Depth = 8)
    if (-not (Test-Path -LiteralPath $Dir)) { New-Item -ItemType Directory -Force -Path $Dir | Out-Null }
    $path = Join-Path $Dir $Name
    $text = ($InputObject | ConvertTo-Json -Depth $Depth)
    [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
    return $path
}

function Invoke-OwoCoreApi {
    <# 取证用直连核心：带 x-owo-client: probe 标签（服务端 ledger 会据此把探测流量与真实首屏流量分桶）。 #>
    param(
        [string]$Base,
        [string]$Path,
        [string]$Token,
        [int]$TimeoutSec = 8,
        [string]$Method = 'GET',
        $Body = $null
    )
    $headers = @{ 'x-owo-client' = $script:OwoAcceptanceProbeHeader }
    if ($Token) { $headers['Authorization'] = "Bearer $Token" }
    $params = @{ Uri = ($Base + $Path); Headers = $headers; TimeoutSec = $TimeoutSec; Method = $Method }
    if ($null -ne $Body) {
        $params['ContentType'] = 'application/json'
        $params['Body'] = ($Body | ConvertTo-Json -Depth 6 -Compress)
    }
    return Invoke-RestMethod @params
}

function Get-OwoCount {
    <#
      安全计数：PowerShell 的函数返回会**拆掉单元素数组**（`@(1)` → `1`），
      而 PS 5.1 下标量 PSCustomObject 的 `.Count` 是 $null——于是 `≤5` 这类
      断言会拿着 null 空转成"通过"（实测踩过：重启后只有 1 条业务请求时
      web_business=null）。计数一律走这里。
    #>
    param($Items)
    return @(($Items | Where-Object { $null -ne $_ })).Count
}

function Get-OwoLedgerFacts {
    <#
      读 /diagnostics/requests 并按来源分桶。
      口径：业务 = 非 health / 非 auth/token / 非事件流；web 与 shell 分列，
      避免壳自己的配对引导把 WebView 首屏预算掩盖掉（反之亦然）。
      探测流量（source=probe）整体排除，不进任何断言。
    #>
    param([string]$Base, [string]$Token, [string]$Label)
    $report = Invoke-OwoCoreApi -Base $Base -Path '/diagnostics/requests?limit=512' -Token $Token
    $records = @($report.records)
    function BucketOf([string]$source) { @($records | Where-Object { $_.source -eq $source }) }
    function BusinessOf($set) {
        @($set | Where-Object {
                $_.route_template -ne '/health' -and
                $_.route_template -ne '/auth/token' -and
                $_.route_template -ne '/events/stream'
            })
    }
    # 调用点再包一层 @()：抵消函数返回时的单元素拆包（见 Get-OwoCount 注释）。
    $web = @(BucketOf 'web')
    $shell = @(BucketOf 'shell')
    $other = @(BucketOf 'other')
    $probe = @(BucketOf 'probe')
    $webBusiness = @(BusinessOf $web)
    $webRoutes = @($webBusiness | ForEach-Object { $_.method + ' ' + $_.route_template })
    $webHealthCount = Get-OwoCount @($web | Where-Object { $_.route_template -eq '/health' })
    $webAuthCount = Get-OwoCount @($web | Where-Object { $_.route_template -eq '/auth/token' })
    $webEventsCount = Get-OwoCount @($web | Where-Object { $_.route_template -eq '/events/stream' })
    $webBusinessCount = Get-OwoCount $webBusiness
    $facts = [ordered]@{
        label               = $Label
        captured_at         = (Get-Date).ToUniversalTime().ToString('o')
        server_total        = $report.total
        returned            = $report.returned
        web_total           = Get-OwoCount $web
        web_health          = $webHealthCount
        web_auth            = $webAuthCount
        web_events          = $webEventsCount
        web_business        = $webBusinessCount
        web_business_routes = $webRoutes
        # 同一路由在同一窗口被重复请求即为风暴信号（>1 次需在记录中解释）。
        web_route_repeats   = @(($webRoutes | Group-Object | Where-Object { $_.Count -gt 1 } |
                ForEach-Object { "$($_.Name) x$($_.Count)" }))
        shell_total         = Get-OwoCount $shell
        shell_auth          = Get-OwoCount @($shell | Where-Object { $_.route_template -eq '/auth/token' })
        shell_health        = Get-OwoCount @($shell | Where-Object { $_.route_template -eq '/health' })
        other_total         = Get-OwoCount $other
        probe_total         = Get-OwoCount $probe
        sources             = @($records | ForEach-Object { $_.source } | Sort-Object -Unique)
        records             = @($records | Where-Object { $_.source -ne $script:OwoAcceptanceProbeHeader })
        # 分桶自洽性：web 四桶之和必须等于 web_total。任何 null/漏桶都会立刻在此
        # 暴露（历史上正是"计数被拆成 null"导致 ≤5 断言空转通过）。
        web_buckets_sum     = $webHealthCount + $webAuthCount + $webEventsCount + $webBusinessCount
        web_buckets_ok      = (($webHealthCount + $webAuthCount + $webEventsCount + $webBusinessCount) -eq (Get-OwoCount $web))
    }
    return [pscustomobject]$facts
}

function Invoke-OwoShellIpc {
    <#
      经 CDP 调壳的 IPC 命令（`__TAURI_INTERNALS__.invoke`）并取回结果。
      CDP 的 Runtime.evaluate 不接 awaitPromise，因此把 Promise 结果落到一个
      一次性全局上再轮询读——比"猜它跑完了"可靠。
      返回 $null 即通道不可用：调用方必须判失败，不得当成成功继续。
    #>
    param([int]$Port, [string]$Command, [hashtable]$Arguments = @{}, [int]$TimeoutSec = 10)
    $key = '__owoIpc'
    $payload = ($Arguments | ConvertTo-Json -Compress -Depth 4)
    if (-not $payload) { $payload = '{}' }
    $js = "window.$key='pending';window.__TAURI_INTERNALS__.invoke('$Command',$payload)" +
          ".then(function(r){window.$key=JSON.stringify(r);})" +
          ".catch(function(e){window.$key='ERR:'+e;});'fired'"
    $null = Invoke-OwoCdpEval -Port $Port -Expression $js -TimeoutSec $TimeoutSec
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $raw = Invoke-OwoCdpEval -Port $Port -Expression "String(window.$key)" -TimeoutSec 5
        if ($raw -and $raw -ne 'pending') {
            if ($raw -like 'ERR:*') { $global:OwoCdpLastError = $raw; return $null }
            try { return ($raw | ConvertFrom-Json) } catch { return $raw }
        }
        Start-Sleep -Milliseconds 300
    }
    $global:OwoCdpLastError = "ipc_timeout:$Command"
    return $null
}

function Wait-OwoCoreReady {
    <# 从壳捕获的核心日志里读本次代际的 core_ready 行（ExcludePid 排除上一代）。 #>
    param([string]$LogDir, [int]$TimeoutSec, [int]$ExcludePid = 0)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $logs = @(Get-ChildItem $LogDir -Filter 'desktop-core-*.log' -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTime -Descending)
        foreach ($log in $logs) {
            $lines = @(Get-Content -LiteralPath $log.FullName -ErrorAction SilentlyContinue)
            for ($i = $lines.Count - 1; $i -ge 0; $i--) {
                # core_ready 的键按 serde_json 字典序输出，故只按 "event":"core_ready" 子句判定。
                if ($lines[$i] -notmatch '"event"\s*:\s*"core_ready"') { continue }
                $start = $lines[$i].IndexOf('{')
                $end = $lines[$i].LastIndexOf('}')
                if ($start -lt 0 -or $end -le $start) { continue }
                $ready = $null
                try {
                    $ready = $lines[$i].Substring($start, $end - $start + 1) | ConvertFrom-Json
                } catch { continue }
                if ($ready.event -eq 'core_ready' -and [int]$ready.port -gt 0 -and [int]$ready.pid -ne $ExcludePid) {
                    return $ready
                }
            }
        }
        Start-Sleep -Milliseconds 400
    }
    return $null
}

function Get-OwoCoreLogTail {
    <# 取最近的壳/核心日志尾部文本（错误页断言与失败排障都要用）。 #>
    param([string]$Dir, [string]$Filter = '*.log', [int]$Lines = 60)
    $logs = @(Get-ChildItem -LiteralPath $Dir -Filter $Filter -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 2)
    $text = New-Object System.Collections.Generic.List[string]
    foreach ($log in $logs) {
        foreach ($line in @(Get-Content -LiteralPath $log.FullName -Tail $Lines -ErrorAction SilentlyContinue)) {
            $text.Add($line)
        }
    }
    return ($text -join "`n")
}

function New-OwoShellStartInfo {
    <#
      构造"完全私有环境"的壳启动参数：LOCALAPPDATA/APPDATA/TEMP 全部重定向到
      本次运行目录（壳的数据根、工作区指针、日志都从 LOCALAPPDATA 派生，因此
      这一组变量足以把真实用户目录隔离在外）。
      -CdpPort 非 0 时开启 WebView2 远程调试端口，供 DOM 断言使用。
    #>
    param(
        [string]$ShellExe,
        [string]$LocalAppData,
        [string]$AppData,
        [string]$TempDir,
        [string]$ApiKey,
        [int]$CdpPort = 0,
        [string]$WebViewUserDataDir = '',
        [switch]$NoApiKey
    )
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ShellExe
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = Split-Path -Parent $ShellExe
    $psi.EnvironmentVariables['LOCALAPPDATA'] = $LocalAppData
    $psi.EnvironmentVariables['APPDATA'] = $AppData
    $psi.EnvironmentVariables['TEMP'] = $TempDir
    $psi.EnvironmentVariables['TMP'] = $TempDir
    if ($NoApiKey) {
        $psi.EnvironmentVariables.Remove('OPENAI_API_KEY')
    } elseif ($ApiKey) {
        $psi.EnvironmentVariables['OPENAI_API_KEY'] = $ApiKey
    }
    # 取证需要直连核心读 ledger：不得走只影响 token 引导路径的开发开关。
    $psi.EnvironmentVariables.Remove('OWO_DESKTOP_DEV_AUTH')
    if ($CdpPort -gt 0) {
        $psi.EnvironmentVariables['WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'] = "--remote-debugging-port=$CdpPort"
    }
    if ($WebViewUserDataDir) {
        # WebView2 的用户数据目录由 known-folder API 派生（LOCALAPPDATA 环境变量重定向
        # 管不到它），因此验收必须显式换目录：否则与**已安装版本**共用浏览器进程组，
        # 表现为 DevTools 端口/页面目标不可控（实测踩过）。
        $psi.EnvironmentVariables['WEBVIEW2_USER_DATA_FOLDER'] = $WebViewUserDataDir
    }
    return $psi
}

function Get-OwoWebviewEtbDir {
    <# WebView2 实际写入的目录 = 给定 UDF 下的 EBWebView（实测）。 #>
    param([string]$UserDataDir)
    return (Join-Path $UserDataDir 'EBWebView')
}

function Assert-OwoShellEmbedsCurrentWeb {
    <#
      §7 构建自包含：桌面壳 exe **内嵌** desktop/web 前端资产（tauri-build 在构建期
      打包），web 侧任何改动都必须重建壳才会被本次验收观察到。这里把"证据与源码
      不同批次"变成开场即失败——而不是跑完 8 分钟后拿一份旧界面的数据当结论
      （实测踩过：服务端 SSE 语义已修，壳里仍是旧 events.js）。
    #>
    param([string]$ShellExe, [string]$WebRoot)
    if (-not (Test-Path -LiteralPath $ShellExe)) { throw "缺少桌面壳：$ShellExe（先在 src-tauri 执行 cargo build）" }
    if (-not (Test-Path -LiteralPath $WebRoot)) { throw "找不到前端资产目录：$WebRoot" }
    $built = (Get-Item -LiteralPath $ShellExe).LastWriteTime
    $stale = @(Get-ChildItem -LiteralPath $WebRoot -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object {
            ($_.Extension -in '.js', '.html', '.css') -and
            ($_.FullName -notmatch '\\tests\\') -and
            ($_.LastWriteTime -gt $built)
        } | Select-Object -First 5)
    if ($stale.Count -gt 0) {
        throw ("桌面壳早于前端资产，验收证据会与源码不同批次：{0}（壳构建于 {1}）—— 请先在 src-tauri 执行 cargo build" -f `
                (($stale | ForEach-Object { $_.Name }) -join ', '), $built.ToString('yyyy-MM-dd HH:mm:ss'))
    }
    return $built
}

function Get-OwoFreeTcpPort {
    <# 交给系统分配空闲端口（不猜端口号）。 #>
    $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $port
}

function Get-OwoWebviewDataDir {
    <#
      WebView2 用户数据目录。它由 Windows known-folder API 决定，**不受子进程
      LOCALAPPDATA 环境变量重定向影响**（实测落在真实 %LOCALAPPDATA%\com.owo.agent\
      EBWebView）：桌面验收能隔离核心数据根，却隔离不了 WebView2 的浏览器进程组，
      因此"上一轮的浏览器进程还活着"必须被显式处理。
    #>
    param([string]$Identifier = 'com.owo.agent')
    return (Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) (Join-Path $Identifier 'EBWebView'))
}

function Clear-OwoCdpPortFile {
    <# 删掉 DevToolsActivePort，使"本轮是否真的起了调试端口"成为可判定事实。 #>
    param([string]$DataDir)
    $file = Join-Path $DataDir 'DevToolsActivePort'
    if (Test-Path -LiteralPath $file) {
        Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
        return $true
    }
    return $false
}

function Test-OwoCdpAlive {
    param([int]$Port)
    if ($Port -le 0) { return $false }
    try {
        $null = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/json/version" -TimeoutSec 3
        return $true
    } catch { return $false }
}

function Resolve-OwoCdpPort {
    <#
      返回本轮**实际**可用的 DevTools 端口。
      指定端口被占用时 Chromium 会静默换口，所以真相来源是 UDF 下的
      DevToolsActivePort 第一行；申请端口只作回落候选。都探不通返回 0。
    #>
    param([string]$DataDir, [int]$RequestedPort, [int]$TimeoutSec = 20)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $file = Join-Path $DataDir 'DevToolsActivePort'
        if (Test-Path -LiteralPath $file) {
            $firstLine = ''
            try { $firstLine = (Get-Content -LiteralPath $file -TotalCount 1 | Select-Object -First 1) } catch { }
            $actual = 0
            if ([int]::TryParse(($firstLine -replace '\D', ''), [ref]$actual) -and $actual -gt 0) {
                if (Test-OwoCdpAlive -Port $actual) { return $actual }
            }
        }
        if (Test-OwoCdpAlive -Port $RequestedPort) { return $RequestedPort }
        Start-Sleep -Milliseconds 400
    }
    return 0
}

function Get-OwoWebviewDiagnostics {
    <# 取不到 DOM 事实时的现场取证：谁在用这个 UDF、申请端口有没有人监听。 #>
    param([int]$RequestedPort, [string]$DataDir)
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -and $_.CommandLine -like "*$DataDir*" })
    $listening = @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue |
        Where-Object { $_.LocalPort -eq $RequestedPort } |
        ForEach-Object { "$($_.LocalAddress):$($_.LocalPort)" })
    return [pscustomobject]@{
        requested_port      = $RequestedPort
        requested_listening = $listening
        webview_data_dir    = $DataDir
        port_file_present   = (Test-Path -LiteralPath (Join-Path $DataDir 'DevToolsActivePort'))
        webview_procs       = $procs.Count
        webview_cmdlines    = @($procs | ForEach-Object { $_.CommandLine.Substring(0, [Math]::Min(400, $_.CommandLine.Length)) })
    }
}

function Wait-OwoCdpReady {
    <# 等 DevTools HTTP 端点起来并返回 page 目标的 webSocketDebuggerUrl。 #>
    param([int]$Port, [int]$TimeoutSec = 15)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $pages = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/json/list" -TimeoutSec 3
            $page = @($pages | Where-Object { $_.type -eq 'page' } | Select-Object -First 1)
            if ($page -and $page[0].webSocketDebuggerUrl) { return $page[0].webSocketDebuggerUrl }
        } catch { Start-Sleep -Milliseconds 300 }
    }
    return $null
}

function Invoke-OwoCdpEval {
    <#
      经 CDP Runtime.evaluate 读取 WebView **实际渲染**的结果（returnByValue）。
      这是 §8.3 唯一的"错误页是否真的可见"硬断言通道——截屏只能给人看，
      不能自动判定；而"HTTP 200 + 窗口存在"更证明不了视图内容。

      失败一律返回 $null 并把原因写进 $global:OwoCdpLastError / OwoCdpTrace：
      取不到 DOM 事实时，"为什么取不到"本身就是验收要留档的证据，
      不允许静默降级成"当作没渲染"。
    #>
    param([int]$Port, [string]$Expression, [int]$TimeoutSec = 10)
    $global:OwoCdpLastError = $null
    $trace = New-Object System.Collections.Generic.List[string]
    $wsUrl = Wait-OwoCdpReady -Port $Port -TimeoutSec $TimeoutSec
    $trace.Add("ws_url=$(if ($wsUrl) { $wsUrl } else { '<none>' })")
    if (-not $wsUrl) {
        $global:OwoCdpLastError = "no_page_target (port=$Port)"
        $global:OwoCdpTrace = @($trace)
        return $null
    }
    $ws = New-Object System.Net.WebSockets.ClientWebSocket
    $ct = [System.Threading.CancellationToken]::None
    try {
        $null = $ws.ConnectAsync([Uri]$wsUrl, $ct).GetAwaiter().GetResult()
        $trace.Add("connected state=$($ws.State)")
        $payload = (@{
                id     = 1
                method = 'Runtime.evaluate'
                params = @{ expression = $Expression; returnByValue = $true }
            } | ConvertTo-Json -Compress -Depth 6)
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
        $seg = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $bytes)
        # PS 5.1 不允许换行后接 `.Method()`，整条链必须同一行。
        $null = $ws.SendAsync($seg, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $ct).GetAwaiter().GetResult()
        $trace.Add("sent bytes=$($bytes.Length)")
        $buffer = New-Object 'byte[]' 262144
        $recv = New-Object 'System.ArraySegment[byte]' -ArgumentList @(, $buffer)
        $decoder = [System.Text.Encoding]::UTF8
        $deadline = (Get-Date).AddSeconds($TimeoutSec)
        $frames = 0
        while ((Get-Date) -lt $deadline) {
            $task = $ws.ReceiveAsync($recv, $ct)
            if (-not $task.Wait(4000)) { $trace.Add('receive_timeout_4s'); $global:OwoCdpLastError = 'receive_timeout'; break }
            $result = $task.Result
            if ($result.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) {
                $trace.Add("peer_close state=$($ws.State)"); $global:OwoCdpLastError = 'peer_closed'; break
            }
            $chunk = $decoder.GetString($buffer, 0, $result.Count)
            if (-not $result.EndOfMessage) { $trace.Add('fragmented_message'); $global:OwoCdpLastError = 'fragmented'; break }
            $frames += 1
            $message = $null
            try { $message = $chunk | ConvertFrom-Json } catch { $trace.Add("bad_json len=$($chunk.Length)"); continue }
            if ($null -eq $message) { continue }
            if ($message.id -ne 1) { continue }   # 事件通知帧：继续等响应
            if ($message.error) {
                $global:OwoCdpLastError = "cdp_error $($message.error.code): $($message.error.message)"
                $trace.Add($global:OwoCdpLastError)
                break
            }
            $trace.Add("frames=$frames ok")
            $global:OwoCdpTrace = @($trace)
            return $message.result.result.value
        }
        if (-not $global:OwoCdpLastError) {
            $global:OwoCdpLastError = "no_response frames=$frames budget=${TimeoutSec}s"
        }
        $global:OwoCdpTrace = @($trace)
        return $null
    } catch {
        $global:OwoCdpLastError = "exception: $($_.Exception.Message)"
        $trace.Add($global:OwoCdpLastError)
        $global:OwoCdpTrace = @($trace)
        return $null
    } finally {
        try { $ws.Dispose() } catch { }
    }
}

function Get-OwoVisibleUiText {
    <#
      一次求值得到"首屏到底渲染了什么"的判定素材（选择器全部对齐 desktop/web 真实
      DOM：#health 连接指示、#routeContent 路由视图、#prompt 任务输入框、
      .service-error 统一错误卡、.setup-guide 引导页）。
      不用截屏当断言（只能给人看），也不用 HTTP 200 当断言（证明不了视图内容）。
    #>
    param([int]$Port, [int]$TimeoutSec = 10)
    $expression = @'
(function () {
  function txt(el) { return el ? (el.innerText || "").replace(/\s+/g, " ").trim() : ""; }
  function visible(el) {
    if (!el) return false;
    var r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }
  var actionable = false;
  var fields = document.querySelectorAll("textarea,input,select,button");
  for (var i = 0; i < fields.length; i++) {
    if (visible(fields[i])) { actionable = true; break; }
  }
  return JSON.stringify({
    healthText: txt(document.getElementById("health")),
    routeText: txt(document.getElementById("routeContent")),
    errorCard: txt(document.querySelector(".service-error")),
    setupGuide: txt(document.querySelector(".setup-guide")),
    composerVisible: visible(document.getElementById("prompt")),
    actionable: actionable,
    bodyText: txt(document.body).slice(0, 600)
  });
})()
'@
    $raw = Invoke-OwoCdpEval -Port $Port -Expression $expression -TimeoutSec $TimeoutSec
    if (-not $raw) { return $null }
    try { return ($raw | ConvertFrom-Json) } catch { return $null }
}

function Stop-OwoShellTree {
    <#
      关窗 → 必要时强杀；顺带回收本次私有根下遗留的壳/子进程。
      只按 CommandLine 含 $RunRoot 收敛，不碰用户真实会话里的同名进程。
    #>
    param([int]$ProcessId, [string]$RunRoot)
    $proc = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($proc) {
        try { $null = $proc.CloseMainWindow() } catch { }
        Start-Sleep -Milliseconds 800
        if (-not $proc.HasExited) { Stop-Process -Id $ProcessId -Force -ErrorAction SilentlyContinue }
    }
    if ($RunRoot) {
        $escaped = $RunRoot.Replace('\', '\\')
        Get-CimInstance Win32_Process -Filter "Name='owo-agent-desktop.exe' OR Name='owo-agent.exe'" -ErrorAction SilentlyContinue |
            Where-Object { $_.CommandLine -and $_.CommandLine -like "*$RunRoot*" } |
            ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    }
}
