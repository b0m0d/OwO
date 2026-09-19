#requires -Version 5.1
<#
stage-desktop-sidecar.ps1 — 桌面壳随包 core（Tauri externalBin）唯一预置入口（R3/§7.3）。

问题背景（实测缺陷链）：
  tauri.conf.json 声明 `externalBin: ["binaries/owo-agent"]`，`tauri_build::build()`
  要求 `binaries/owo-agent-<target-triple>.exe` **必须存在**，否则壳连编译都过不了。
  这个约束催生了三件事，全都发生在门禁看不见的目录里（binaries/ 被 gitignore）：
    1. 有人手工复制一份旧 core 进去救火（实测：2026-08-31 的产物，无构建身份）；
    2. 之后每次构建壳都会把它复制到 target/<cfg>/，被壳的"同目录优先"解析劫持，
       表现为开发时冷启动 40s 握手超时的"莫名失败"；
    3. scripts/build-installer.ps1 曾把新 core 复制成 `owo-agent.exe-<triple>.exe`
       ——**不符合 externalBin 取包名**，tauri-build 根本不读它，于是安装包持续
       打包第 1 条里那份旧 core（哈希核对"通过"，因为核对的是错名字的新文件）。

本入口的职责（单一实现，禁止再复制）：
  * 先清空 binaries/ 下所有 `owo-agent*` 残留（杜绝混代与错名文件）；
  * 从**当前源码构建产物** `target/<cfg>/owo-agent.exe` 按正确三元组名复制；
  * SHA-256 复制完整性核对 + 复制后 `--version` 自报身份与源一致；
  * 产物缺构建身份（无 `commit=`，即 R2 之前的历史 exe）直接拒绝。

用法：
  dot-source： . scripts\stage-desktop-sidecar.ps1 ; Stage-OwoDesktopSidecar [-Configuration release]
  直接执行：   powershell -File scripts\stage-desktop-sidecar.ps1 [-Configuration debug|release] [-Json]
#>
[CmdletBinding()]
param(
    # dot-source 参数卫生：裸名会污染调用方作用域（见 resolve-ort.ps1 同类教训）。
    [ValidateSet('debug', 'release')]
    [Alias('Configuration')]
    [string]$OwoConfiguration = 'debug',
    [Alias('NoBuild')]
    [switch]$OwoNoBuild,
    # 允许暂存"与 HEAD 不同世代"的 core（仅供排障复现旧产物；正常链一律拒绝）。
    [switch]$OwoAllowStaleIdentity,
    [Alias('Json')]
    [switch]$OwoJson
)

$script:OwoStageSdkRoot = Split-Path -Parent $PSScriptRoot

function Get-OwoRustcHostTriple {
    <# 由 rustc 自报 host 三元组，避免把 x86_64-pc-windows-msvc 硬编码进脚本。 #>
    $line = (& rustc -vV 2>&1 | ForEach-Object { $_.ToString() } | Where-Object { $_ -match '^host:\s*' } | Select-Object -First 1)
    if (-not $line) { return 'x86_64-pc-windows-msvc' }
    return ($line -split '\s+' | Select-Object -Last 1)
}

function Stage-OwoDesktopSidecar {
    <#
      预置随包 core 并返回元数据。任何一步不满足都 throw（消息含可操作动作）。
      .PARAMETER Configuration  debug|release（决定取哪个 target 产物）
      .PARAMETER NoBuild        产物不存在时不代跑 cargo（发布链用：必须显式先构建）
    #>
    [CmdletBinding()]
    param(
        [ValidateSet('debug', 'release')]
        [string]$Configuration = 'debug',
        [switch]$NoBuild,
        [switch]$AllowStaleIdentity,
        [switch]$Quiet
    )
    $sdkRoot = $script:OwoStageSdkRoot
    # §2.4 磁盘红线：这里会在"产物缺失/过期"时真的去构建 core（debug 也要几 GB 余量）。
    # 独立执行时补引同一实现，不复制第二份红线逻辑；调用方已 dot-source 过就直接复用。
    if (-not (Get-Command Assert-CiDiskGate -ErrorAction SilentlyContinue)) {
        . (Join-Path $PSScriptRoot "ci-shared.ps1")
        Initialize-CiPath
    }
    # 必须吞返回值：`Assert-CiDiskGate` 成功时会往管道吐一个磁盘状态对象，而本函数的
    # 契约是"返回单个元数据对象"。混进第二个对象后调用方 `$staged.source` 会取到状态
    # 对象的 `source='psdrive'`，Copy-Item 当场炸——实测把 4 个故障场景 + 冷启动全打成
    # 红，且红的原因长得像产品回归（§2.4 门自己成了假红的源头）。
    $null = Assert-CiDiskGate -Mode normal -Path (Join-Path $sdkRoot 'target') -Context 'stage-sidecar'
    $source = Join-Path $sdkRoot "target\$Configuration\owo-agent.exe"
    if (-not (Test-Path -LiteralPath $source)) {
        if ($NoBuild) {
            throw "缺少 $source：请先构建核心服务（cargo build -p owo-agent-cli$(if ($Configuration -eq 'release') { ' --release' })）"
        }
        if (-not $Quiet) { Write-Host "[stage] 构建 owo-agent-cli（$Configuration）..." }
        # §2.4 资源安全红线：sidecar 构建链接 ONNX/Sherpa 原生依赖 → strict 档 -j 1；
        # 本函数可能被 ci-gate/build-installer（已 dot-source ci-shared）复用，也可能
        # 独立执行，因此按"缺才补引"的方式接入同一实现，不复制第二份红线逻辑。
        if (-not (Get-Command Invoke-CiCargo -ErrorAction SilentlyContinue)) {
            . (Join-Path $PSScriptRoot "ci-shared.ps1")
            Initialize-CiPath
        }
        $policyMode = if ($Configuration -eq 'release') { 'strict' } else { 'normal' }
        Push-Location $sdkRoot
        try {
            $configArgs = @('-q', '-p', 'owo-agent-cli')
            if ($Configuration -eq 'release') { $configArgs += '--release' }
            Invoke-CiCargo -Arguments (@('build') + $configArgs) -Cwd $sdkRoot -TimeoutSec 7200 `
                -HeartbeatSec 30 -Label 'stage-sidecar-build' -PolicyMode $policyMode
            if ($global:LASTEXITCODE -ne 0) { throw "核心服务构建失败（exit=$global:LASTEXITCODE；§2.4 档=$policyMode）" }
        } finally {
            Pop-Location
        }
        if (-not (Test-Path -LiteralPath $source)) { throw "构建后仍找不到 $source" }
    }

    # 源产物必须自报构建身份：R2 之前的 exe 一律不得成为随包 core。
    $sourceIdentityText = ((& $source --version 2>&1 | Out-String) -split '\r?\n' |
        Where-Object { $_ -match 'commit=' } | Select-Object -First 1)
    if (-not $sourceIdentityText) {
        throw "target\$Configuration\owo-agent.exe 无构建身份（--version 不含 commit=）——它是历史残留，请重新构建（§7.3）"
    }
    $sourceIdentity = $sourceIdentityText.Trim()

    # 世代核对（实测坑，2026-09-19）：产物**存在时本函数不重建**，于是"提交之后直接跑
    # stage"会把提交前构建的 core 暂存进 binaries/。壳在 src-tauri 编译期就会警告身份
    # 错代，运行时更直接判 `core/identity_mismatch`——整条桌面验收链（矩阵 82 + 冷启动 43
    # + UI 48，实测 30 分钟）会全部因为错误理由变红，且红的原因和真实回归长得一样。
    # 与其让下游炸，不如在这里拒绝，并把可执行动作写进消息。
    $stagedCommit = if ($sourceIdentity -match 'commit=([0-9a-f]{40})') { $Matches[1] } else { '' }
    $headCommit = ''
    try {
        $headLines = @(& git -C $sdkRoot rev-parse HEAD 2>&1 | ForEach-Object { $_.ToString().Trim() })
        $headCommit = ($headLines | Where-Object { $_ -match '^[0-9a-f]{40}$' } | Select-Object -First 1)
    } catch { }
    if ($stagedCommit -and $headCommit -and ($stagedCommit -ne $headCommit)) {
        if (-not $AllowStaleIdentity) {
            throw "随包 core 是别代的产物（core commit=$($stagedCommit.Substring(0,7)) HEAD=$($headCommit.Substring(0,7))）：先删 target\$Configuration\owo-agent.exe 或跑 cargo build -p owo-agent-cli 重建，再 stage；确要复现旧产物用 -AllowStaleIdentity（§7.3）"
        }
        if (-not $Quiet) {
            Write-Host "[stage] 警告：按请求放行错代 core（core=$($stagedCommit.Substring(0,7)) HEAD=$($headCommit.Substring(0,7))）——真机验收会判 core/identity_mismatch"
        }
    }

    $triple = Get-OwoRustcHostTriple
    $binDir = Join-Path $sdkRoot 'desktop\tauri\src-tauri\binaries'
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    # 清残留：错命名/上一代/无身份的历史文件全部删除（这正是安装包错包的源头）。
    Get-ChildItem -LiteralPath $binDir -Filter 'owo-agent*' -Force -ErrorAction SilentlyContinue |
        ForEach-Object {
            if (-not $Quiet) { Write-Host "[stage] 清理 binaries 残留：$($_.Name)" }
            Remove-Item -LiteralPath $_.FullName -Force
        }
    $destination = Join-Path $binDir "owo-agent-$triple.exe"
    Copy-Item -LiteralPath $source -Destination $destination -Force

    $srcHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $dstHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
    if ($srcHash -ne $dstHash) {
        throw "sidecar 复制后 SHA-256 不一致（src=$srcHash dst=$dstHash）——拒绝继续（§7.3）"
    }
    $stagedIdentityText = ((& $destination --version 2>&1 | Out-String) -split '\r?\n' |
        Where-Object { $_ -match 'commit=' } | Select-Object -First 1)
    if (-not $stagedIdentityText) { throw "staged sidecar 无法自报构建身份：$destination" }
    if ($stagedIdentityText.Trim() -ne $sourceIdentity) {
        throw "staged sidecar 身份与源产物不一致（源=「$sourceIdentity」 目标=「$($stagedIdentityText.Trim())」）"
    }
    if (-not $Quiet) {
        Write-Host "[stage] 随包 core 就位：binaries\owo-agent-$triple.exe（sha256=$dstHash）"
        Write-Host "[stage] 身份：$sourceIdentity"
    }
    return [pscustomobject]@{
        source      = $source
        destination = $destination
        triple      = $triple
        sha256      = $dstHash
        identity    = $sourceIdentity
    }
}

# 直接执行入口（人工排障 / 脚本前置自检）。
if ($MyInvocation.InvocationName -ne '.') {
    try {
        $meta = Stage-OwoDesktopSidecar -Configuration $OwoConfiguration -NoBuild:$OwoNoBuild `
            -AllowStaleIdentity:$OwoAllowStaleIdentity
        if ($OwoJson) { $meta | ConvertTo-Json -Compress | Write-Output }
        else { Write-Output $meta.destination }
        exit 0
    } catch {
        [Console]::Error.WriteLine("[stage] ERROR: $($_.Exception.Message)")
        exit 2
    }
}
