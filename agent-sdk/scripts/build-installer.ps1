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
$npx = if ($env:OWO_NPX) {
    $env:OWO_NPX
} else {
    $npxCommand = Get-Command npx.cmd -ErrorAction SilentlyContinue
    if ($npxCommand) { $npxCommand.Source } else { "npx.cmd" }
}

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

# §2.4 资源安全红线：安装包链路属"release/原生依赖重链"，一律 strict 档（-j 1），
# 并在启动前过内存门与构建空闲门；输出流式落盘（红线 5 不隐藏长任务进度），
# 完整日志仍交给下面的 linker 零容忍门扫描。
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath
$buildLogPath = Join-Path ([IO.Path]::GetTempPath()) ("owo-installer-build-{0}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
Write-Host "[installer] 构建核心服务（$Configuration，§2.4 strict 档 -j 1）..."
$buildLog = Invoke-CiCargoCapture -Arguments (@('build', '-p', 'owo-agent-cli') + $configArgs) `
    -Cwd $root -TimeoutSec 7200 -HeartbeatSec 30 -Label 'installer-build' `
    -PolicyMode 'strict' -LogFile $buildLogPath -CargoExe $cargo
if ($global:LASTEXITCODE -ne 0) {
    throw "核心服务构建失败（cargo exit=$global:LASTEXITCODE；§2.4 日志：$buildLogPath）"
}
# §7.3 零容忍升级：任意 LNK 码（含 LNK4098 CRT 混用、LNK1120 等）或
# linker 警告行都按发布失败处理，不允许带入安装包。
$linkerNoise = @($buildLog | Where-Object { $_ -match 'LNK\d{4}|warning: linker' } |
    Where-Object { $_ -notmatch '正在创建库|Creating library' } | Select-Object -First 5)
if ($linkerNoise.Count -gt 0) {
    throw ("发布构建检测到 {0} 条 linker 告警/错误——按失败处理（§7.3）：{1}" -f $linkerNoise.Count, ($linkerNoise -join ' | '))
}

# R3（§7.3）：随包 core 预置统一走 stage-desktop-sidecar.ps1（**唯一实现**，与
# ci-gate 的 desktop-stage 步、真机验收脚本同源）：清残留 → 按 externalBin 正确
# 命名复制 → SHA-256 核对 → 构建身份核对。
# 历史缺陷：这里曾把新 core 复制成 `owo-agent.exe-<triple>.exe`，而 tauri-build
# 只认 `owo-agent-<triple>.exe`——于是安装包持续打包 binaries/ 里另一份来历不明
# 的旧 core（实测 8/31 残留），"哈希核对通过"核对的却是那个没人读的文件。
. (Join-Path $PSScriptRoot "stage-desktop-sidecar.ps1")
try {
    $staged = Stage-OwoDesktopSidecar -Configuration $Configuration -NoBuild
} catch {
    throw "sidecar 预置失败（§7.3）：$($_.Exception.Message)"
}
Write-Host "[installer] 随包 core 就位：$($staged.destination)"
Write-Host "[installer] sidecar 身份：$($staged.identity)"
Write-Host "[installer] sidecar 复制哈希核对通过：$($staged.sha256)"

Push-Location $tauriDir
try {
    Write-Host "[installer] 打包 NSIS（npx @tauri-apps/cli build）..."
    # Tauri 2 的 tauri-build 会为壳注入 static_vcruntime 的 CRT 参数。
    # agent-sdk/.cargo/config.toml 中用于 ORT 核心服务的 /NODEFAULTLIB:LIBCMT
    # 不能传给壳，否则会屏蔽壳所需的 libcmt，触发 mainCRTStartup/__chkstk 等
    # 成批 LNK2001。Cargo 会合并 workspace rustflags，环境变量无法可靠删除
    # 配置数组，因此只在 Tauri 子构建期间临时移开配置文件，并在 finally 原位恢复。
    $cargoConfig = Join-Path $root '.cargo\config.toml'
    $cargoConfigBackup = Join-Path $root '.cargo\config.toml.build-installer-backup'
    $hadCargoConfig = Test-Path -LiteralPath $cargoConfig
    if ($hadCargoConfig) {
        if (Test-Path -LiteralPath $cargoConfigBackup) {
            throw "发现未清理的 Cargo 配置备份：$cargoConfigBackup；拒绝覆盖，先确认上一次发布脚本已恢复"
        }
        Move-Item -LiteralPath $cargoConfig -Destination $cargoConfigBackup -Force
    }
    $oldTargetRustFlags = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS
    $oldEncodedRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
    $oldGenericRustFlags = $env:RUSTFLAGS
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = ''
    $env:RUSTFLAGS = ''
    $env:CARGO_ENCODED_RUSTFLAGS = ''
    # R10（2026-09-22 实测缺陷）：`npx tauri build` 会重新编译桌面壳，而壳的
    # `owo-build-info` 构建脚本在 release 档同样会拒绝 dirty 工作树。本脚本开头
    # 的 `Assert-OwoCleanTree` 只是**本进程**的门禁，覆盖开关 OWO_ALLOW_DIRTY_RELEASE
    # 不会自动传进 npx 子进程——于是"本地显式允许 dirty 打包"仍然在壳编译阶段
    # panic，报错却显示成"NSIS 打包失败"，排查成本极高（实测卡了两轮）。
    # 这里显式传递：父进程允许 → 子构建也允许，语义一致且可追溯。
    $dirtyOverride = $env:OWO_ALLOW_DIRTY_RELEASE
    if ($dirtyOverride -eq "1") {
        Write-Host "[installer] OWO_ALLOW_DIRTY_RELEASE=1：透传给 tauri 子构建（壳的 build.rs 同样门禁）"
    }
    # ⚠ PowerShell 5.1 陷阱（2026-09-22 实测两次踩到）：本脚本顶部设了
    # `$ErrorActionPreference = "Stop"`，而 5.1 会把**外部程序的 stderr 输出**
    # 包装成 ErrorRecord——npx/npm 每次都会往 stderr 打
    # `npm warn Unknown env config "manage-package-manager-versions"`，
    # 于是这一行在 npx 真正跑起来之前就被当成终止性错误抛出，脚本以 exit 1 收场，
    # 现象是"NSIS 打包失败"，而 bundler 其实一次都没执行。
    # 处置：把调用与恢复合并成**一条** try/finally（continue 语义：stderr 仍原样透传，
    # 不吞输出），之后**只认 $LASTEXITCODE**——退出码才是打包成败的唯一权威判据。
    $savedErrorAction = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $npx --yes @tauri-apps/cli@2 build
        $buildExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedErrorAction
        if ($null -eq $oldTargetRustFlags) {
            Remove-Item Env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -ErrorAction SilentlyContinue
        } else {
            $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = $oldTargetRustFlags
        }
        if ($null -eq $oldEncodedRustFlags) {
            Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
        } else {
            $env:CARGO_ENCODED_RUSTFLAGS = $oldEncodedRustFlags
        }
        if ($null -eq $oldGenericRustFlags) {
            Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
        } else {
            $env:RUSTFLAGS = $oldGenericRustFlags
        }
        if ($hadCargoConfig) {
            if (Test-Path -LiteralPath $cargoConfig) {
                throw "Tauri 子构建后 Cargo 配置路径出现意外文件：$cargoConfig；拒绝覆盖"
            }
            Move-Item -LiteralPath $cargoConfigBackup -Destination $cargoConfig -Force
        }
    }
    if ($buildExitCode -ne 0) {
        throw "NSIS 打包失败（npx @tauri-apps/cli build exit=$buildExitCode）"
    }
} finally {
    Pop-Location
}

Write-Host "[installer] 完成：desktop\tauri\src-tauri\target\release\bundle\nsis\"
