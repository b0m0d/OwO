# 任务 5（P0）：release 产物清单——构建身份与产物 hash 绑定（§6.1/§18 完成证据）。
# 用法：powershell -ExecutionPolicy Bypass -File scripts\release-artifact-manifest.ps1 [-Out manifest.json]
# 产出：JSON 清单（git commit/dirty + 每个产物的大小与 SHA-256）。
# 与 /health.build 同源（git rev-parse + porcelain），与 generate-update-manifest.ps1 的
# 签名清单互补：本清单证明"二进制来自当前源码"，签名清单负责分发完整性。

param(
    # 产物目录（缺省 target\release）；相对仓库根。
    [string]$ArtifactsDir = "target\release",
    # 清单输出路径（缺省 <ArtifactsDir>\release-manifest.json）。
    [string]$Out = "",
    # 参与清单的产物名（缺省核心 CLI 与桌面壳产物）。
    [string[]]$Names = @("owo-agent.exe")
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function GitOut([string[]]$GitArgs) {
    $output = & git @GitArgs 2>$null
    if ($LASTEXITCODE -ne 0) { return $null }
    return ($output -join "`n")
}

$commit = GitOut @("rev-parse", "HEAD")
if (-not $commit) { $commit = "unknown" }
# §7.3 统一口径（与 owo-build-info/build.rs、Assert-OwoCleanTree 一致）：
# dirty 作用域 = agent-sdk/ 构建相关树（Set-Location $root 后即 "."）。
$porcelain = GitOut @("status", "--porcelain", "-uall", "--", ".")
$dirty = [bool]($porcelain -and $porcelain.Trim().Length -gt 0)
$headTime = GitOut @("show", "-s", "--format=%cI", "HEAD")
if (-not $headTime) { $headTime = "" }

# §7.3：发布清单同样拒绝 dirty 树（build.rs release 门禁之外的第二道闸，
# 面向"清单宣告的是哪个树"；唯一豁免 OWO_ALLOW_DIRTY_RELEASE=1，如实记录）。
$allowDirty = ($env:OWO_ALLOW_DIRTY_RELEASE -eq "1")
if ($dirty -and -not $allowDirty) {
    throw "release manifest 拒绝 dirty 工作树（产物必须唯一对应 clean commit，§7.3）；本地调试可设 OWO_ALLOW_DIRTY_RELEASE=1"
}

$dir = Join-Path $root $ArtifactsDir
if (-not (Test-Path $dir)) { throw "产物目录不存在：$dir" }
if (-not $Out) { $Out = Join-Path $dir "release-manifest.json" }

# §7.1：身份段与 /health.build、--version、doctor 同构——直接从已构建
# 产物的 --version 输出读取（证明清单记录的是二进制自身主张，而非脚本
# 重新 git 查询的巧合一致），再与当前树交叉核对，错配即失败。
# 版本号打头（oneline 渲染格式："<version> api=…"）。禁止用 `-split " "`
# 取段——PS 管道里单元素 unary split 会把下一行 `-Depth 4` 当输入字符串
# 再切（ConvertTo-Json 参数绑定的经典陷阱），这里用零歧义的 Substring。
function Get-ExeVersion([string]$Line) {
    if (-not $Line) { return "" }
    $i = $Line.IndexOf(" ")
    if ($i -lt 1) { return $Line }
    return $Line.Substring(0, $i)
}

function Get-ExeIdentity([string]$ExePath) {
    $line = (& $ExePath --version 2>&1 | Out-String).Trim()
    function Grab([string]$Text, [string]$Key) {
        $m = [regex]::Match($Text, "$key=(\S+)")
        if ($m.Success) { return $m.Groups[1].Value }
        return ""
    }
    [pscustomobject]@{
        line     = $line
        commit   = Grab $line "commit"
        dirty    = Grab $line "dirty"
        built_at = Grab $line "built_at"
        api      = Grab $line "api"
    }
}

$artifacts = @()
$identity = $null
foreach ($name in $Names) {
    $path = Join-Path $dir $name
    if (-not (Test-Path $path)) { throw "产物缺失：$path（先完成 release 构建）" }
    $bytes = (Get-Item $path).Length
    $sha = (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLowerInvariant()
    $artifacts += [ordered]@{ name = $name; bytes = $bytes; sha256 = $sha }
    if ($name -ieq "owo-agent.exe" -and $null -eq $identity) {
        $identity = Get-ExeIdentity $path
        if (-not $identity.commit) { throw "无法从 $name --version 解析构建身份：$($identity.line)" }
        if ($identity.commit -ne $commit) {
            throw ("产物身份与仓库 HEAD 错配：exe commit={0} tree={1}——产物可能过期或树已移动（§7.1/§7.3）" -f $identity.commit, $commit)
        }
    }
}

# 依赖版本记录（§7.3 manifest 含依赖版本）：ORT 经统一解析入口报告，
# rust 工具链直接查询；本机路径只留稳定布局（不写死个人目录）。
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
$ortMeta = Resolve-OwoOrtEnv -NoDownload -Quiet
$deps = [ordered]@{
    rustc        = ((& rustc --version 2>$null | Out-String).Trim())
    cargo        = ((& cargo --version 2>$null | Out-String).Trim())
    rust_target  = $ortMeta.target
    onnx_runtime = [ordered]@{
        version        = $ortMeta.version
        asset          = $ortMeta.asset
        crt            = $ortMeta.crt
        archive_sha256 = $ortMeta.archive_sha256
        resolved_from  = $ortMeta.source
    }
}

$manifest = [ordered]@{
    schema       = "owo-release-manifest/2"
    generated_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    identity     = [ordered]@{
        version    = if ($identity) { Get-ExeVersion $identity.line } else { "" }
        api_version = if ($identity) { $identity.api } else { "" }
        commit     = if ($identity) { $identity.commit } else { $commit }
        dirty      = if ($identity) { $identity.dirty -eq "true" } else { $dirty }
        built_at   = if ($identity) { $identity.built_at } else { "" }
        source     = "artifact --version"
    }
    git          = [ordered]@{ commit = $commit; dirty = $dirty; head_committed_at = $headTime }
    dirty_override = $allowDirty
    dependencies = $deps
    artifacts    = $artifacts
}

$json = $manifest | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($Out, $json + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
Write-Host "清单已写入：$Out"
$json
