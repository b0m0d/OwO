# 机械化的"搬迁依赖扫描"：找出被搬文件里引用的外部 crate，并标出目标 manifest 里**缺少**的那些。
#
# 为什么需要它（实测踩了四次同样的坑）：
#   前几步用"`use` 行 + 人工维护的内联路径词表"来核依赖清单，结果 M5 漏了 chrono/uuid、
#   M10 漏了 tracing/uuid、M13 漏了 thiserror、M14 漏了 tokio/tracing/windows-future。
#   词表是人工维护的，**必然漏**。本脚本改为机械方法：
#     1) 扫文件里所有 `ident::` 形式的根标识符（含 `use` 行与内联全限定路径）；
#     2) 与 **Cargo.lock 里的真实包名**取交集（这样 `std::`/`crate::`/局部模块名会被自动排除）；
#     3) 再与目标 crate 的 manifest 依赖（含 dev-dependencies 与 workspace = true 的引用）比对，
#        打印"引用了但没声明"的清单。
#   它不能替代编译器（feature、trait 来源这类隐式依赖仍要靠 check 兜底），但能把
#   "词表漏项"这一整类错误提前到动工之前。
#
# 用法：
#   & "<repo>\agent-sdk\scripts\mk-deps-scan.ps1" -Files crates/owo-agent-perception/src/*.rs `
#         -Manifest crates/owo-agent-perception/Cargo.toml
#   & "<repo>\agent-sdk\scripts\mk-deps-scan.ps1" -Path crates/owo-agent-perception/src -Manifest crates/owo-agent-perception/Cargo.toml

param(
    [string[]]$Files = @(),
    [string]$Path,
    [Parameter(Mandatory = $true)][string]$Manifest,
    [string]$Root = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $Root

# 1) Cargo.lock 里的真实包名（下划线形式，用于与 `ident::` 比较）
$lock = Join-Path $Root 'Cargo.lock'
if (-not (Test-Path -LiteralPath $lock)) { throw "找不到 Cargo.lock：$lock" }
$pkgNames = [System.Collections.Generic.HashSet[string]]::new()
Select-String -LiteralPath $lock -Pattern '^name = "(.+)"$' | ForEach-Object {
    $n = $_.Matches[0].Groups[1].Value -replace '-', '_'
    [void]$pkgNames.Add($n)
}

# 2) 目标 manifest 已声明的依赖名
$manifestPath = Join-Path $Root $Manifest
$declared = [System.Collections.Generic.HashSet[string]]::new()
$section = ''
foreach ($line in Get-Content -LiteralPath $manifestPath) {
    if ($line -match '^\s*\[(.+)\]\s*$') { $section = $Matches[1]; continue }
    # 依赖段： [dependencies] / [dev-dependencies] / [build-dependencies] /
    # [target.'cfg(...)'.dependencies] —— 统一用"段名以 dependencies 结尾"判定
    if ($section -notmatch 'dependencies\s*$') { continue }
    # 依赖行有两种写法：`name = ...`（内联版本）与 `name.workspace = true`（继承 workspace）
    if ($line -match '^\s*([A-Za-z0-9_\-]+)(\.workspace)?\s*=') {
        [void]$declared.Add(($Matches[1] -replace '-', '_'))
    }
}

# 3) 收集待查文件
$targets = @()
if ($Path) { $targets += Get-ChildItem -LiteralPath (Join-Path $Root $Path) -Recurse -File -Filter *.rs }
foreach ($f in $Files) { $targets += (Resolve-Path -LiteralPath (Join-Path $Root $f)).Path | ForEach-Object { Get-Item -LiteralPath $_ } }
$targets = $targets | Where-Object { $_ } | Sort-Object FullName -Unique

if (-not $targets) { throw "没有待查文件（用 -Path 或 -Files 指定）" }

# 4) 扫描 `ident::` 根（先剥注释，避免把文档里提到的 crate 名算成依赖）
$used = @{}
foreach ($f in $targets) {
    $txt = Get-Content -LiteralPath $f.FullName -Raw
    $txt = [regex]::Replace($txt, '(?s)/\*.*?\*/', '')
    $txt = [regex]::Replace($txt, '//[^\r\n]*', '')
    foreach ($m in [regex]::Matches($txt, '(?<![\w:])([a-z][a-z0-9_]{2,})::')) {
        $name = $m.Groups[1].Value
        if (-not $pkgNames.Contains($name)) { continue }
        if (-not $used.ContainsKey($name)) { $used[$name] = [System.Collections.Generic.HashSet[string]]::new() }
        [void]$used[$name].Add($f.Name)
    }
}

$missing = @()
$present = @()
foreach ($name in ($used.Keys | Sort-Object)) {
    if ($declared.Contains($name)) { $present += $name } else { $missing += $name }
}

"=== mk-deps-scan"
"  扫描文件：{0} 个；Cargo.lock 包名：{1} 个；manifest 已声明：{2} 个" -f $targets.Count, $pkgNames.Count, $declared.Count
"  引用且已声明（{0}）：{1}" -f $present.Count, ($present -join ', ')
if ($missing.Count -gt 0) {
    "  引用但**未声明**（{0}）：" -f $missing.Count
    foreach ($name in $missing) { "    - {0,-18} 出现在：{1}" -f $name, (($used[$name] | Sort-Object) -join ', ') }
    exit 1
}
"  引用但未声明：0 —— 依赖清单完整（仍建议 check 兜底 feature 与 trait 来源）"
exit 0
