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
# §7.2：ORT 经统一解析入口 resolve-ort.ps1（内部委托 init-dev-env 单一实现，
# 进程级注入，缺失即报可操作错误，不留到链接阶段）。
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
Assert-OwoCleanTree
try {
    $ortMeta = Resolve-OwoOrtEnv -NoDownload -Quiet
    Write-Host ("[installer] ORT resolved: {0} (source={1}, v{2}, {3})" -f $ortMeta.lib_dir, $ortMeta.source, $ortMeta.version, $ortMeta.crt)
} catch {
    throw "ONNX Runtime not found for release linking. Run: pwsh -File scripts\resolve-ort.ps1 -EnsureOrt （$($_.Exception.Message)）"
}

$configArgs = @()
if ($Configuration -eq "release") { $configArgs = @("--release") }

Push-Location $root
try {
    Write-Host "[installer] 构建核心服务（$Configuration）..."
    $buildLog = & $cargo build -p owo-agent-cli @configArgs 2>&1 | ForEach-Object { "$_" }
    $buildLog | ForEach-Object { Write-Host $_ }
    if ($LASTEXITCODE -ne 0) { throw "核心服务构建失败" }
    # §7.3 零容忍升级：任意 LNK 码（含 LNK4098 CRT 混用、LNK1120 等）或
    # linker 警告行都按发布失败处理，不允许带入安装包。
    $linkerNoise = @($buildLog | Where-Object { $_ -match 'LNK\d{4}|warning: linker' } |
        Where-Object { $_ -notmatch '正在创建库|Creating library' } | Select-Object -First 5)
    if ($linkerNoise.Count -gt 0) {
        throw ("发布构建检测到 {0} 条 linker 告警/错误——按失败处理（§7.3）：{1}" -f $linkerNoise.Count, ($linkerNoise -join ' | '))
    }
} finally {
    Pop-Location
}

$binDir = Join-Path $tauriDir "binaries"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Copy-Item -LiteralPath (Join-Path $root "target\$Configuration\owo-agent.exe") `
    -Destination (Join-Path $binDir "owo-agent.exe-x86_64-pc-windows-msvc.exe") -Force
# §7.3：复制 sidecar 后立即哈希核对（复制损坏零窗口）。
$sidecarSrc = Get-FileHash -LiteralPath (Join-Path $root "target\$Configuration\owo-agent.exe") -Algorithm SHA256
$sidecarDst = Get-FileHash -LiteralPath (Join-Path $binDir "owo-agent.exe-x86_64-pc-windows-msvc.exe") -Algorithm SHA256
if ($sidecarSrc.Hash -ne $sidecarDst.Hash) {
    throw "sidecar 复制后 SHA-256 不一致（src=$($sidecarSrc.Hash) dst=$($sidecarDst.Hash)）——发布中止（§7.3）"
}
Write-Host "[installer] sidecar 复制哈希核对通过：$($sidecarDst.Hash)"

Push-Location $tauriDir
try {
    Write-Host "[installer] 打包 NSIS（npx @tauri-apps/cli build）..."
    & $npx --yes @tauri-apps/cli@2 build
    if ($LASTEXITCODE -ne 0) { throw "NSIS 打包失败" }
} finally {
    Pop-Location
}

Write-Host "[installer] 完成：desktop\tauri\src-tauri\target\release\bundle\nsis\"
