# R10/R11 桌面端修复实测（2026-09-22）
#
# 验证四件事（都是用户明确投诉过的）：
#   1. 核心子进程不弹控制台窗口（壳侧 CREATE_NO_WINDOW 生效）；
#   2. 核心不依附调用方控制台，持有自己的 pid 文件（"关窗即停服务"的机制面）；
#   3. 核心启动即就绪，/health 可达；
#   4. 上一代核心尚未退干净时重启，不得死锁在 pid 冲突上（用户"连不上"的根因）。
#
# ⚠ 默认**不开窗口**。只有 -FullShell 才冷启动整个桌面壳——那会真弹窗口，并且为了
# 拿单实例锁会先把用户正在用的实例杀掉。2026-09-22 因默认走整壳冷启动、反复弹窗
# 被用户当场叫停，此开关因此默认关闭。
#
# 隔离方式：只重定向 OWO_AGENT_DATA / OWO_CONFIG_FILE，不动 %LOCALAPPDATA%。
# （WebView2 的用户数据目录在 LOCALAPPDATA 下，把它换掉会让壳起不来却没有窗口，
# 造成"壳活着、core 不存在"的假红——实测为此白查了两轮。）
#
# 用法：
#   pwsh -NoProfile -File agent-sdk\scripts\verify-r10-desktop-fixes.ps1
#   pwsh -NoProfile -File agent-sdk\scripts\verify-r10-desktop-fixes.ps1 -FullShell
param(
    [string]$Dist = "",
    [switch]$FullShell
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = Split-Path $PSScriptRoot -Parent
if (-not $Dist) { $Dist = Join-Path $root "dist\OwO-Agent" }
$coreExe = Join-Path $Dist "owo-agent.exe"
$shellExe = Join-Path $Dist "owo-agent-desktop.exe"
if (-not (Test-Path $coreExe)) { throw "缺少核心：$coreExe（先构建或解压便携包）" }
if ($FullShell -and -not (Test-Path $shellExe)) { throw "缺少桌面壳：$shellExe" }

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$run = Join-Path $env:TEMP "owo-r10-verify-$stamp"
New-Item -ItemType Directory -Path (Join-Path $run "data") -Force | Out-Null
$env:OWO_AGENT_DATA = Join-Path $run "data"
$env:OWO_CONFIG_FILE = Join-Path $run "config.json"

$workspace = Join-Path $run "workspace"
New-Item -ItemType Directory -Path $workspace -Force | Out-Null
@{ path = $workspace } | ConvertTo-Json -Compress |
    Set-Content -LiteralPath (Join-Path $env:OWO_AGENT_DATA "workspace.json") -Encoding UTF8
@{ version = 1; model = @{ provider = "ollama"; base_url = "http://127.0.0.1:11434/v1"; name = "local" } } |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $env:OWO_CONFIG_FILE -Encoding UTF8

# 准备：清掉上次运行的残留。桌面壳是单实例（命名互斥体），残留壳会让本次启动直接
# WokeExisting 退出；残留核心会占住 pid 文件让新核心拒绝双开。两者都会造成假红。
$leftover = @(Get-Process owo-agent, owo-agent-desktop -ErrorAction SilentlyContinue)
if ($leftover.Count -gt 0) {
    $names = ($leftover | Group-Object ProcessName | ForEach-Object { "$($_.Name)x$($_.Count)" }) -join ' '
    Write-Host "[准备] 清理残留进程（$names）"
    $leftover | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
}

$results = New-Object System.Collections.Generic.List[object]
function Add-Result([string]$name, [bool]$pass, [string]$detail) {
    $results.Add([pscustomobject]@{ name = $name; pass = $pass; detail = $detail })
    Write-Host ("[{0}] {1} — {2}" -f $(if ($pass) { "PASS" } else { "FAIL" }), $name, $detail)
}

# 直连本机端口读文本：必须显式禁用代理。
# 本机装了系统代理（ProxyServer=127.0.0.1:7897），PS5.1 的 Invoke-WebRequest 会把
# 127.0.0.1 的请求也交给代理（"服务活着却判成连不上"），而 PS5.1 又没有 -NoProxy
# 开关（PS7 才有），直接传会抛 ParameterBindingException。
function Get-LocalText([string]$url, [int]$timeoutSec = 5) {
    $request = [System.Net.WebRequest]::Create($url)
    $request.Proxy = $null
    $request.Timeout = $timeoutSec * 1000
    $response = $request.GetResponse()
    try {
        $reader = New-Object System.IO.StreamReader($response.GetResponseStream())
        try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
    } finally { $response.Dispose() }
}

function Wait-Health([int]$port, [int]$timeoutSec = 45) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        try {
            $body = Get-LocalText "http://127.0.0.1:$port/health" 3
            if ($body -match '"healthy"\s*:\s*true') { return $body }
        } catch { }
        Start-Sleep -Milliseconds 400
    }
    return $null
}

function Get-FreePort {
    $listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    try { return $listener.LocalEndpoint.Port } finally { $listener.Stop() }
}

# 启动核心：**用壳支持的 `--port <n>` 模式**（壳在 core_ready 超时时走的就是这条兼容
# 路径），端口因此已知，不必解析日志；stdout/stderr 直接重定向到文件——既不会因管道
# 缓冲区写满而阻塞核心，也不需要 .NET 异步回调（PowerShell 里跨线程脚本块的变量
# 作用域不可靠，实测会静默失效，白查一轮）。
function Start-CoreProcess([int]$port, [string]$tag) {
    $stdout = Join-Path $run "core-$tag.out.txt"
    $stderr = Join-Path $run "core-$tag.err.txt"
    $env:OWO_DESKTOP_PAIRING_SECRET = "verify-pairing-$tag"
    $env:OWO_DESKTOP_INSTANCE_ID = "verify-instance-$tag"
    $env:OWO_DESKTOP_RELEASE = "1"
    $env:OPENAI_BASE_URL = "http://127.0.0.1:11434/v1"
    $env:OPENAI_MODEL = "local"
    $proc = Start-Process -FilePath $coreExe `
        -ArgumentList @("serve", "--port", "$port", "--workspace", $workspace) `
        -WorkingDirectory $Dist -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    return @{ process = $proc; stdout = $stdout; stderr = $stderr }
}

# 控制台检测：控制台程序若没有控制台，GetConsoleWindow() 返回 0；CREATE_NO_WINDOW
# 创建的是**隐藏**控制台，肉眼看不见但句柄存在——只有 GetConsoleWindow +
# IsWindowVisible 才能区分"没弹窗"和"弹了但被藏起来"。
function Get-CoreConsoleState([int]$targetProcessId) {
    if (-not ("OwoConsoleProbe" -as [type])) {
        Add-Type -Namespace OwoConsoleProbe -Name Probe -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();
[DllImport("kernel32.dll")] public static extern bool AttachConsole(uint dwProcessId);
[DllImport("kernel32.dll")] public static extern bool FreeConsole();
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
'@
    }
    $attached = $false
    try {
        [OwoConsoleProbe.Probe]::FreeConsole() | Out-Null
        # 形参名不能叫 $pid：$PID 是只读自动变量，同名绑定直接抛异常。
        $attached = [OwoConsoleProbe.Probe]::AttachConsole([uint32]$targetProcessId)
        if (-not $attached) { return @{ hasConsole = $false; visible = $false } }
        $hwnd = [OwoConsoleProbe.Probe]::GetConsoleWindow()
        $visible = $false
        if ($hwnd -ne [IntPtr]::Zero) { $visible = [OwoConsoleProbe.Probe]::IsWindowVisible($hwnd) }
        return @{ hasConsole = ($hwnd -ne [IntPtr]::Zero); visible = $visible }
    } finally {
        if ($attached) { [OwoConsoleProbe.Probe]::FreeConsole() | Out-Null }
    }
}

function Stop-DistCores {
    Get-Process owo-agent -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$Dist*" } | Stop-Process -Force -ErrorAction SilentlyContinue
}

function Read-GenText($gen) {
    $text = ""
    foreach ($path in @($gen.stdout, $gen.stderr)) {
        if ($path -and (Test-Path $path)) { $text += (Get-Content $path -Raw -ErrorAction SilentlyContinue) }
    }
    return $text
}

Write-Host "== R10/R11 桌面端实测 =="
Write-Host "dist=$Dist"
Write-Host "run=$run"
Write-Host ("mode=" + $(if ($FullShell) { "FullShell（会弹窗口）" } else { "无窗口（默认，安全）" }))

$shell = $null
try {
    $port1 = Get-FreePort
    $gen1 = Start-CoreProcess $port1 "1"
    Write-Host "[info] 核心已启动（无窗口，端口 $port1，pid=$($gen1.process.Id)）"

    $health1 = Wait-Health $port1 60
    Add-Result "核心启动即就绪且 /health 可达" ($null -ne $health1) `
        "port=$port1 pid=$($gen1.process.Id)"

    $core = Get-Process -Id $gen1.process.Id -ErrorAction SilentlyContinue
    Add-Result "核心进程来自随包 sidecar" ($null -ne $core) "path=$(if ($core) { $core.Path } else { '已退出' })"

    if ($core) {
        $state = Get-CoreConsoleState $core.Id
        Add-Result "核心子进程无可见控制台窗口（不再弹 cmd 黑框）" `
            ((-not $state.hasConsole) -or (-not $state.visible)) `
            "hasConsole=$($state.hasConsole) visible=$($state.visible)"
    }

    $pidFile = Join-Path $env:OWO_AGENT_DATA "server.pid"
    Add-Result "核心持有自己的 pid 文件（独立生命周期标记，不依附调用方）" (Test-Path $pidFile) `
        "path=$pidFile"

    if ($FullShell) {
        # ---- 整壳冷启动（仅 -FullShell）：关窗必须只隐藏、服务继续跑 ----
        $shell = Start-Process -FilePath $shellExe -WorkingDirectory $Dist -PassThru `
            -WindowStyle Hidden `
            -RedirectStandardOutput (Join-Path $run "shell.out.txt") `
            -RedirectStandardError (Join-Path $run "shell.err.txt")
        Start-Sleep -Seconds 15
        $shellCore = Get-Process owo-agent -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -like "$Dist*" } | Select-Object -First 1
        Add-Result "整壳冷启动后核心在跑" ($null -ne $shellCore) `
            "pid=$(if ($shellCore) { $shellCore.Id } else { '无' })"
        $mainWindow = Get-Process -Id $shell.Id | Select-Object -ExpandProperty MainWindowHandle
        if ($mainWindow -and $mainWindow -ne 0) {
            if (-not ("OwoWin32" -as [type])) {
                Add-Type -Namespace OwoWin32 -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
'@
            }
            [OwoWin32.Win]::PostMessage([IntPtr]$mainWindow, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
            Start-Sleep -Seconds 4
            $alive = $null -ne (Get-Process -Id $shell.Id -ErrorAction SilentlyContinue)
            Add-Result "关闭工作台窗口后壳仍存活（隐藏到托盘）" $alive "shellAlive=$alive"
        }
    }

    # ---- pid 冲突：旧核心还没退干净就重启，不得死锁 ----
    # 这正是用户"连不上"的真实时序（换工作区/换提供商：壳先关旧核心，再立刻拉新核心）。
    Stop-DistCores
    Start-Sleep -Milliseconds 150
    $port2 = Get-FreePort
    $gen2 = Start-CoreProcess $port2 "2"
    $health2 = Wait-Health $port2 60
    $gen2Text = Read-GenText $gen2
    $conflict = $gen2Text -match '检测到运行中的服务'
    $waited = $gen2Text -match '上一代核心仍在退出'
    Add-Result "旧核心未退干净时重启，新核心最终就绪（无 pid_conflict 死锁）" ($null -ne $health2) `
        "port=$port2 等待旧实例=$waited 冲突提示=$conflict"
    Add-Result "重启链未把 pid 冲突升级成启动失败" (-not ($conflict -and -not $health2 -and -not $waited)) `
        "waitedForOld=$waited conflict=$conflict healthy=$($null -ne $health2)"
} finally {
    Stop-DistCores
    if ($shell) { Stop-Process -Id $shell.Id -Force -ErrorAction SilentlyContinue }
    Get-Process owo-agent-desktop -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$Dist*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
}

$passed = [int](@($results | Where-Object { $_.pass }).Count)
$failed = [int](@($results | Where-Object { -not $_.pass }).Count)
$report = [pscustomobject]@{
    stamp = $stamp
    dist = $Dist
    run_root = $run
    full_shell = [bool]$FullShell
    passed = $passed
    failed = $failed
    checks = $results
}
$reportPath = Join-Path $run "r10-report.json"
$report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
Write-Host ""
Write-Host "== 结果：$passed 通过 / $failed 失败 =="
Write-Host "报告：$reportPath"
if ($failed -gt 0) { exit 1 }
exit 0
