# 构建 NSIS 安装程序（含核心服务 sidecar）。
# §6.1.5/§6.2：统一构建门禁——拒绝 dirty 工作树；ORT 原生依赖经 init-dev-env
# 单一实现解析（不再依赖旧终端遗留的 ORT_LIB_PATH）；cargo 构建输出中出现
# LNK4098（CRT 静态/动态混用）按发布失败处理，不允许警告带入安装包。
# 用法：powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1 [-Configuration release|debug]
param(
    [ValidateSet("release", "debug")]
    [string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = Split-Path $PSScriptRoot -Parent
$tauriDir = Join-Path $root "desktop\tauri\src-tauri"
$cargo = if ($env:OWO_CARGO) { $env:OWO_CARGO } else { Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe" }
$npx = if ($env:OWO_NPX) { $env:OWO_NPX } else { "D:\前端框架\npx.cmd" }

# §6.1.5/§6.2：门禁（clean tree + ORT 解析）；dirty 覆盖开关：OWO_ALLOW_DIRTY_RELEASE=1。
. (Join-Path $PSScriptRoot "init-dev-env.ps1")
Assert-OwoCleanTree
$ortLib = Get-OwoOrtLibDir
if (-not $ortLib) {
    throw "ONNX Runtime not found for release linking. Run: pwsh -File scripts\init-dev-env.ps1 -EnsureOrt"
}
$env:SHERPA_ONNX_LIB_DIR = $ortLib
$env:ORT_LIB_PATH = $ortLib
$env:ORT_LIB_LOCATION = $ortLib
Write-Host ("[installer] ORT resolved: {0}" -f $ortLib)

$configArgs = @()
if ($Configuration -eq "release") { $configArgs = @("--release") }

Push-Location $root
try {
    Write-Host "[installer] 构建核心服务（$Configuration）..."
    $buildLog = & $cargo build -p owo-agent-cli @configArgs 2>&1 | ForEach-Object { "$_" }
    $buildLog | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) { throw "核心服务构建失败" }
    if ($buildLog -match "LNK4098") {
        throw "检测到 LNK4098（CRT 静态/动态混用）——发布构建不允许该警告带入安装包（§6.2）"
    }
} finally {
    Pop-Location
}

$binDir = Join-Path $tauriDir "binaries"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item -LiteralPath (Join-Path $root "target\$Configuration\owo-agent.exe") `
    -Destination (Join-Path $binDir "owo-agent.exe-x86_64-pc-windows-msvc.exe") -Force

Push-Location $tauriDir
try {
    Write-Host "[installer] 打包 NSIS（npx @tauri-apps/cli build）..."
    & $npx --yes @tauri-apps/cli@2 build
    if ($LASTEXITCODE -ne 0) { throw "NSIS 打包失败" }
} finally {
    Pop-Location
}

Write-Host "[installer] 完成：desktop\tauri\src-tauri\target\release\bundle\nsis\"
