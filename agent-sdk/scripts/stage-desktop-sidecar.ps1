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
        [switch]$Quiet
    )
    $sdkRoot = $script:OwoStageSdkRoot
    $source = Join-Path $sdkRoot "target\$Configuration\owo-agent.exe"
    if (-not (Test-Path -LiteralPath $source)) {
        if ($NoBuild) {
            throw "缺少 $source：请先构建核心服务（cargo build -p owo-agent-cli$(if ($Configuration -eq 'release') { ' --release' })）"
        }
        if (-not $Quiet) { Write-Host "[stage] 构建 owo-agent-cli（$Configuration）..." }
        Push-Location $sdkRoot
        try {
            $configArgs = @('-q', '-p', 'owo-agent-cli')
            if ($Configuration -eq 'release') { $configArgs += '--release' }
            & cargo build @configArgs 2>&1 | ForEach-Object { Write-Host $_ }
            if ($LASTEXITCODE -ne 0) { throw "核心服务构建失败（exit=$LASTEXITCODE）" }
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
        $meta = Stage-OwoDesktopSidecar -Configuration $OwoConfiguration -NoBuild:$OwoNoBuild
        if ($OwoJson) { $meta | ConvertTo-Json -Compress | Write-Output }
        else { Write-Output $meta.destination }
        exit 0
    } catch {
        [Console]::Error.WriteLine("[stage] ERROR: $($_.Exception.Message)")
        exit 2
    }
}
