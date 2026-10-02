# 启动 OwO Agent（Electron 版）。
#
# 与旧 Tauri 壳最大的不同：**前端从磁盘加载**（core 静态托管 desktop/web）。改 `desktop/web/**` 之后
# 在窗口里按 Ctrl+R 就能看到效果，不需要重新编译（这是用户明确要求的）。
#
# 用法：pwsh -NoProfile -File agent-sdk\desktop\electron\start.ps1
param(
    [switch]$DevTools,
    [string]$CoreExe = "",
    # 无 GPU 的环境（CI、容器、远程/沙箱会话）需要禁掉硬件加速，否则
    # GPU 进程会以 exit_code=-1073741819 反复重生，最终
    # FATAL:gpu_data_manager_impl_private.cc "GPU process isn't usable" 直接掀翻壳。
    [switch]$DisableGpu
)
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = Split-Path $PSScriptRoot -Parent      # desktop\
$sdk = Split-Path $root -Parent               # agent-sdk\

# 1) Electron 运行时就绪检查（缺失时给出可执行的一条命令，而不是让用户猜）
$electron = Join-Path $PSScriptRoot "node_modules\electron\dist\electron.exe"
if (-not (Test-Path $electron)) {
    throw @"
缺少 Electron 运行时。请先安装（国内网络用镜像）：
  cd "$PSScriptRoot"
  `$env:ELECTRON_MIRROR = "https://npmmirror.com/mirrors/electron/"
  npm install --no-audit --no-fund
"@
}

# 2) 核心可执行文件：优先显式传入，其次同级目录（打包形态），最后仓库产物（开发形态）
if (-not $CoreExe) {
    $candidates = @(
        (Join-Path $PSScriptRoot "owo-agent.exe"),
        (Join-Path $sdk "target\release\owo-agent.exe"),
        (Join-Path $sdk "dist\OwO-Agent\owo-agent.exe")
    )
    $CoreExe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $CoreExe) { throw "找不到 owo-agent.exe（核心服务）：请先构建或从便携包复制到 desktop\electron\ 下" }

# 3) 让主进程知道用哪个核心（不设则走它自己的候选列表）
$env:OWO_CORE_EXE = $CoreExe

# ELECTRON_RUN_AS_NODE=1 会让 electron.exe 退化成纯 Node 解释器：
# require("electron") 只返回一段路径字符串，主进程解构出的 app/ipcMain 全是
# undefined，于是 L823 抛 "Cannot read properties of undefined (reading 'handle')"。
# 这变量可能被上级环境带进来，进来就在本进程清掉。
if ($env:ELECTRON_RUN_AS_NODE) {
    Write-Host "[start] 检测到 ELECTRON_RUN_AS_NODE=$($env:ELECTRON_RUN_AS_NODE)，已清除（否则壳无法启动）"
    Remove-Item Env:\ELECTRON_RUN_AS_NODE -ErrorAction SilentlyContinue
}

Write-Host "[start] electron : $electron"
Write-Host "[start] core     : $CoreExe"
Write-Host "[start] 数据目录 : $env:LOCALAPPDATA\OwO\Agent"
Write-Host "[start] 改前端后按 Ctrl+R 刷新即可生效（无需重新编译）"
Write-Host "[start] 提示：壳用 --port 0 拉起核心（随机端口），实际地址见 core.log 的 core_ready 行"

$arguments = @(".")
if ($DevTools) { $arguments += "--dev" }
if ($DisableGpu) {
    $arguments += @(
        "--disable-gpu",
        "--disable-gpu-compositing",
        "--disable-software-rasterizer",
        "--in-process-gpu",
        "--no-sandbox"
    )
    Write-Host "[start] 已禁用 GPU 加速（-DisableGpu）"
}
Push-Location $PSScriptRoot
try {
    & $electron @arguments
} finally {
    Pop-Location
}
