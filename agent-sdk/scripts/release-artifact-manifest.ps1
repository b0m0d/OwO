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
$porcelain = GitOut @("status", "--porcelain")
$dirty = [bool]($porcelain -and $porcelain.Trim().Length -gt 0)
$headTime = GitOut @("show", "-s", "--format=%cI", "HEAD")
if (-not $headTime) { $headTime = "" }

$dir = Join-Path $root $ArtifactsDir
if (-not (Test-Path $dir)) { throw "产物目录不存在：$dir" }
if (-not $Out) { $Out = Join-Path $dir "release-manifest.json" }

$artifacts = @()
foreach ($name in $Names) {
    $path = Join-Path $dir $name
    if (-not (Test-Path $path)) { throw "产物缺失：$path（先完成 release 构建）" }
    $bytes = (Get-Item $path).Length
    $sha = (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLowerInvariant()
    $artifacts += [ordered]@{ name = $name; bytes = $bytes; sha256 = $sha }
}

$manifest = [ordered]@{
    schema      = "owo-release-manifest/1"
    generated_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    git         = [ordered]@{ commit = $commit; dirty = $dirty; head_committed_at = $headTime }
    artifacts   = $artifacts
}

$json = $manifest | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($Out, $json + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
Write-Host "清单已写入：$Out"
$json
