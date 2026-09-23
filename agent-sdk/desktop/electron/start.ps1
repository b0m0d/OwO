# 启动 OwO Agent（Electron 版）。
#
# 与旧 Tauri 壳最大的不同：**前端从磁盘加载**。改 `src/renderer/**` 之后
# 在窗口里按 Ctrl+R 就能看到效果，不需要重新编译（这是用户明确要求的）。
#
# 用法：pwsh -NoProfile -File agent-sdk\desktop\electron\start.ps1
param(
    [switch]$DevTools,
    [string]$CoreExe = ""
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

Write-Host "[start] electron : $electron"
Write-Host "[start] core     : $CoreExe"
Write-Host "[start] 数据目录 : $env:LOCALAPPDATA\OwO\Agent"
Write-Host "[start] 改前端后按 Ctrl+R 刷新即可生效（无需重新编译）"

$arguments = @(".")
if ($DevTools) { $arguments += "--dev" }
Push-Location $PSScriptRoot
try {
    & $electron @arguments
} finally {
    Pop-Location
}
