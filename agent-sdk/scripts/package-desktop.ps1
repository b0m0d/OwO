# OwO Agent 便携打包：核心服务 + 桌面壳 + 内置技能包 → dist/OwO-Agent-<版本>-<配置>.zip
# R10 增强：版本号从 Cargo.toml 同步；Authenticode 签名占位（-SignCert）；NSIS 安装包（build-installer.ps1）；SBOM。
# 用法：powershell -ExecutionPolicy Bypass -File scripts\package-desktop.ps1 [-Configuration release|debug] [-SignCert <thumbprint>] [-SkipInstaller] [-SkipSbom]
param(
    [ValidateSet("release", "debug")]
    [string]$Configuration = "release",
    # Authenticode 证书指纹（signtool sign）；缺省打印签名占位提示。
    [string]$SignCert = "",
    [switch]$SkipInstaller,
    [switch]$SkipSbom
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$cargo = if ($env:OWO_CARGO) {
    $env:OWO_CARGO
} elseif (Test-Path (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe")) {
    Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
} else {
    "cargo"
}

$root = Split-Path $PSScriptRoot -Parent

# §2.4：统一资源红线执行层（cargo 步骤全部经 Invoke-CiCargo*；与 ci-gate 同一实现）。
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath

# §6.1.5/§6.2：统一构建门禁——发布打包拒绝 dirty 工作树（覆盖开关：
# OWO_ALLOW_DIRTY_RELEASE=1）；§7.2：ORT 原生依赖经统一解析入口
# resolve-ort.ps1（委托 init-dev-env 单一实现，进程级注入），不再依赖
# 旧终端遗留的环境变量。
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
Assert-OwoCleanTree
try {
    $ortMeta = Resolve-OwoOrtEnv -NoDownload -Quiet
    Write-Host ("[package] ORT resolved: {0} (source={1}, v{2}, {3})" -f $ortMeta.lib_dir, $ortMeta.source, $ortMeta.version, $ortMeta.crt)
} catch {
    throw "ONNX Runtime not found for packaging. Run: pwsh -File scripts\resolve-ort.ps1 -EnsureOrt （$($_.Exception.Message)）"
}

# §6.2：构建封装——LNK4098（CRT 静态/动态混用）按发布失败处理，禁止带入安装包。
# §2.4 资源安全红线：桌面发布链（壳与 sidecar，含 ONNX/Sherpa 原生重链）一律 strict
# 档 -j 1；启动前过内存门与构建空闲门；输出流式落盘后仍全量扫描 linker 噪声。
function Invoke-OwoPackageBuild {
    param([string]$Stage, [string]$WorkingDir, [string[]]$CargoArgs)
    $logPath = Join-Path ([IO.Path]::GetTempPath()) ("owo-package-{0}-{1}.log" -f ($Stage -replace '[^\w]', ''), (Get-Date -Format 'yyyyMMdd-HHmmss'))
    Write-Host "[package] $Stage（§2.4 strict 档 -j 1）..."
    # 调用方只传子命令之后的参数（-p/-–release），此处统一补 build 并过红线归一。
    $log = Invoke-CiCargoCapture -Arguments (@('build') + @($CargoArgs)) -Cwd $WorkingDir -TimeoutSec 7200 -HeartbeatSec 30 `
        -Label ("package-" + ($Stage -replace '[^\w]', '')) -PolicyMode 'strict' -LogFile $logPath -CargoExe $cargo
    if ($global:LASTEXITCODE -ne 0) { throw "$Stage 构建失败（cargo exit=$global:LASTEXITCODE；§2.4 日志：$logPath）" }
    # §7.3 零容忍升级：任意 LNK 码或 linker 告警行按发布失败处理。
    # 唯一豁免：MSVC 工具链信息性 "creating library/exp" 输出
    # （windows-msvc 链接 exe 必打的 informational，非 CRT 问题）。
    $linkerNoise = @($log | Where-Object { $_ -match 'LNK\d{4}|warning: linker' } |
        Where-Object { $_ -notmatch '正在创建库|Creating library' } | Select-Object -First 5)
    if ($linkerNoise.Count -gt 0) {
        throw ("{0}：发布构建检测到 {1} 条 linker 告警/错误——按失败处理（§7.3）：{2}" -f $Stage, $linkerNoise.Count, ($linkerNoise -join ' | '))
    }
}

# R10：版本号从 workspace Cargo.toml 同步（version = "x.y.z"）。
function Get-WorkspaceVersion {
    $cargoToml = Get-Content -LiteralPath (Join-Path $root "Cargo.toml") -Encoding UTF8
    foreach ($line in $cargoToml) {
        if ($line -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    return "0.0.0"
}
$version = Get-WorkspaceVersion
Write-Host "[package] 版本（来自 Cargo.toml）：$version"

$dist = Join-Path $root "dist\OwO-Agent"
$configArgs = @()
if ($Configuration -eq "release") {
    $configArgs = @("--release")
}
if (Test-Path $dist) {
    Remove-Item -LiteralPath $dist -Recurse -Force
}
New-Item -ItemType Directory -Path $dist -Force | Out-Null

Invoke-OwoPackageBuild -Stage "构建核心服务（$Configuration）" -WorkingDir $root `
    -CargoArgs (@("-p", "owo-agent-cli") + $configArgs)

Invoke-OwoPackageBuild -Stage "构建桌面壳（$Configuration）" `
    -WorkingDir (Join-Path $root "desktop\tauri\src-tauri") -CargoArgs $configArgs

$targetDir = Join-Path $root "target\$Configuration"
$desktopTarget = Join-Path $root "desktop\tauri\src-tauri\target\$Configuration"
Copy-Item -LiteralPath (Join-Path $targetDir "owo-agent.exe") -Destination $dist
Copy-Item -LiteralPath (Join-Path $desktopTarget "owo-agent-desktop.exe") -Destination $dist
Copy-Item -LiteralPath (Join-Path $root "skills") -Destination $dist -Recurse
Copy-Item -LiteralPath (Join-Path $root "settings.example.json") -Destination (Join-Path $dist "settings.example.json")
if (Test-Path (Join-Path $root "models\ocr")) {
    Write-Host "[package] 附带本地 ONNX OCR 模型（models/ocr，离线确定性 OCR 通道）..."
    Copy-Item -LiteralPath (Join-Path $root "models") -Destination $dist -Recurse
}

# ONNX Runtime 运行时库：**已经不需要了，别再加回来**。
#
# 这段曾经按"ort 走 load-dynamic、需要 exe 同级 onnxruntime.dll"下载 22MB 的
# ONNX Runtime；但 `crates/owo-agent-perception` 现在的 ort-sys 构建脚本自报
# `cargo:rustc-link-lib=static=onnxruntime`（sherpa-onnx static-MT 资产），
# 两个 exe 的导入表里都没有 onnxruntime.dll——随包不需要它。
# 保留旧逻辑的后果是真实的：网络受限时这一步直接抛错，整条便携打包失败
# （2026-09-22 实测 agent-sdk\dist\ 因此一直缺失，用户以为"打包坏了"）。
# 现在只做一次事实核对：若产物真的动态依赖 onnxruntime.dll，就在日志里明确说出来，
# 而不是去下载一个可能并不需要的文件。
$onnxRuntimeDll = Join-Path $dist "onnxruntime.dll"
foreach ($binary in @("owo-agent.exe", "owo-agent-desktop.exe")) {
    $binaryPath = Join-Path $dist $binary
    if (-not (Test-Path $binaryPath)) { continue }
    $bytes = [System.IO.File]::ReadAllBytes($binaryPath)
    $text = [System.Text.Encoding]::ASCII.GetString($bytes, 0, [Math]::Min($bytes.Length, 80000000))
    if ($text.Contains("onnxruntime.dll")) {
        Write-Host "[package] ⚠ $binary 动态依赖 onnxruntime.dll，但随包未附带该文件——请确认 ORT 链接方式（期望 static）" -ForegroundColor Yellow
    }
}

@"
OwO Agent 便携版（v0.4 P1/P2/P3 + v0.5 M-E）

运行：双击 owo-agent-desktop.exe（自动拉起同目录 owo-agent.exe 核心服务；端口由系统动态分配，实际地址写入日志）。
快捷键：Ctrl+Alt+Shift+O 唤起工作台。
排障：核心拉起失败/身份不符时界面会给出错误页；日志在 %LOCALAPPDATA%\OwO\Agent\logs\，
      可用 `owo-agent.exe --version` 核对随包核心构建身份（commit/dirty/built_at）。

环境变量（可选）：
  OPENAI_API_KEY / OPENAI_BASE_URL / OPENAI_MODEL  模型凭据（缺省内置 BigModel 端点与模型，仅需提供密钥）
  OWO_AGENT_DATA                                    数据目录（会话/审计/技能，默认 %LOCALAPPDATA%\OwO\Agent）
  OWO_SKILLS_DIR                                    内置技能包目录（默认使用随包 skills/）
  OWO_ONNX_OCR_MODEL_DIR                            本地 ONNX OCR 模型目录（默认 models/ocr 或数据目录）

OCR 通道优先级：本地 ONNX（随包/数据目录，无网可用）→ Paddle 云（需 PADDLE_OCR_TOKEN）→ Windows Media.Ocr。

安全：权限默认 deny；写/执行/注入需审批；密码/支付/验证码类锚点熔断不执行。
"@ | Set-Content -LiteralPath (Join-Path $dist "README.txt") -Encoding UTF8

$zip = Join-Path $root "dist\OwO-Agent-$version-$Configuration.zip"
if (Test-Path $zip) {
    Remove-Item -LiteralPath $zip -Force
}
Compress-Archive -Path (Join-Path $dist "*") -DestinationPath $zip
Write-Host "[package] 便携包完成：$zip"

# R10：Authenticode 签名占位——提供 -SignCert 时用 signtool 签名，否则打印提示。
$signtool = if ($env:OWO_SIGNTOOL) {
    $env:OWO_SIGNTOOL
} else {
    (Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1).FullName
}
if ($SignCert) {
    if (-not $signtool) { throw "未找到 signtool.exe（可设置 OWO_SIGNTOOL）" }
    Write-Host "[package] Authenticode 签名（证书 $SignCert）..."
    & $signtool sign /sha1 $SignCert /fd sha256 /td sha256 /tr "http://timestamp.digicert.com" $zip
    if ($LASTEXITCODE -ne 0) { throw "签名失败（exit $LASTEXITCODE）" }
} else {
    Write-Host "[package] 未签名（占位）：release 发布前请以 -SignCert <指纹> 执行 Authenticode 签名"
}

# R10：NSIS 安装包（复用 build-installer.ps1；缺省跳过开关）。
if (-not $SkipInstaller) {
    $installer = Join-Path $PSScriptRoot "build-installer.ps1"
    if (Test-Path $installer) {
        Write-Host "[package] 生成 NSIS 安装包..."
        & $installer -Configuration $Configuration
    } else {
        Write-Host "[package] 跳过 NSIS：build-installer.ps1 不存在"
    }
}

# R10：SBOM（依赖清单 + 模型文件哈希）纳入 release 产物。
if (-not $SkipSbom) {
    $sbom = Join-Path $PSScriptRoot "sbom.ps1"
    if (Test-Path $sbom) {
        Write-Host "[package] 生成 SBOM..."
        & $sbom -DistDir $dist -OutFile (Join-Path $root "dist\sbom.json")
    }
}
# 任务 5（P0）：release 产物清单（构建身份 + SHA-256）纳入 dist——
# 与 SBOM 互补：SBOM 记录依赖/模型来源，本清单把"二进制来自当前源码"绑定成证据。
$manifestScript = Join-Path $PSScriptRoot "release-artifact-manifest.ps1"
if (Test-Path $manifestScript) {
    Write-Host "[package] 生成 release 产物清单（dist\\OwO-Agent，SHA-256 + git 身份）..."
    # 只列真正随包的二进制：onnxruntime.dll 已不再随包（见上），把它留在清单里
    # 会让清单脚本去核一个不存在的文件，整条打包在最后一步失败。
    & $manifestScript -ArtifactsDir "dist\OwO-Agent" `
        -Names @("owo-agent.exe", "owo-agent-desktop.exe") `
        -Out (Join-Path $root "dist\release-manifest.json")
} else {
    Write-Host "[package] 跳过产物清单：release-artifact-manifest.ps1 不存在"
}
Write-Host "[package] 全部完成"
