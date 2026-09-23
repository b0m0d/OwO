#requires -Version 5.1
<#
test-mk-smoke-identity.ps1 — P0 证据链的**离线负例/正例**自测（指南 §8 P0 第 9 条）。

为什么需要：M15 的假通过（M0 旧二进制跑出 18/18）之所以可能，是因为验收脚本没有
把"产物身份 = 当前 HEAD"变成硬门。本测试用可执行的假产物（.cmd 垫片，离线、秒级）
直接断言这个门：

  负例：产物自报 commit != 当前 HEAD  → mk-smoke 必须退出 3，且**不启动进程**；
  正例：产物自报 commit == 当前 HEAD  → 身份门通过（-IdentityOnly 不启动进程）。

同时验证：身份不符时报告 identity.ok=false、started=false（旧二进制不可能混进运行态验收）。

退出码：0 = 全部通过；1 = 有失败。
用法：.\scripts\test-mk-smoke-identity.ps1
#>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

$results = @()
function Add-Result {
    param([string]$Name, [bool]$Ok, [string]$Detail = '')
    $script:results += [pscustomobject]@{ name = $Name; ok = $Ok; detail = $Detail }
    if ($Ok) { Write-Host ("[PASS] {0} — {1}" -f $Name, $Detail) -ForegroundColor Green }
    else { Write-Host ("[FAIL] {0} — {1}" -f $Name, $Detail) -ForegroundColor Red }
}

$root = Get-CiRepoRoot
$src = Get-CiGitIdentity -RepoRoot $root
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('owo-mk-smoke-identity-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

# 假产物：一个可被 `& exe --version` 调用的 .cmd 垫片，回显 owo-build-info 的稳定格式。
# commit 由参数写死，从而精确控制"旧/新"两种身份，不依赖本机是否真的存在旧二进制。
function New-FakeExe([string]$Path, [string]$Commit, [string]$Dirty) {
    $line = "owo-agent 0.1.0 api=0.7 commit=$Commit dirty=$Dirty built_at=2026-01-01T00:00:00Z source=compiled"
    $content = "@echo off`r`necho $line`r`n"
    [System.IO.File]::WriteAllText($Path, $content, (New-Object System.Text.UTF8Encoding($false)))
    return $Path
}

$staleCommit = 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef'
$oldExe = New-FakeExe (Join-Path $tmp 'owo-agent-old.cmd') $staleCommit 'false'
# 正例假产物必须与当前源码的 dirty 主张一致（dirty 也是身份的一部分），否则会按设计失败。
$srcDirtyFlag = if ($src.dirty) { 'true' } else { 'false' }
$newExe = New-FakeExe (Join-Path $tmp 'owo-agent-current.cmd') $src.commit $srcDirtyFlag

function Invoke-Smoke([string]$exe, [string]$tag) {
    & (Join-Path $PSScriptRoot 'mk-smoke.ps1') -Tag $tag -ExePath $exe -IdentityOnly | Out-Host
    $code = $LASTEXITCODE
    $evidence = Join-Path $root ("docs\qa\evidence\mk-smoke-{0}-*" -f $tag)
    $dir = Get-ChildItem -Path (Split-Path $evidence -Parent) -Filter ("mk-smoke-{0}-*" -f $tag) -Directory -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    $report = $null
    if ($dir) {
        $reportFile = Join-Path $dir.FullName 'report.json'
        if (Test-Path -LiteralPath $reportFile) { $report = Get-Content -LiteralPath $reportFile -Raw | ConvertFrom-Json }
    }
    return [pscustomobject]@{ code = $code; report = $report; dir = $dir }
}

# ---- 负例：旧二进制（commit 不符）必须失败于身份门，且不启动进程 --------------------
$neg = Invoke-Smoke $oldExe 'p0-neg'
Add-Result '负例：旧二进制必须退出码 3（identity mismatch）' ($neg.code -eq 3) "exit=$($neg.code)"
Add-Result '负例：身份门不匹配（identity.ok=false）' ($null -ne $neg.report -and -not $neg.report.identity.ok) `
    "identity.ok=$(if ($neg.report) { $neg.report.identity.ok } else { '<无报告>' }) reason=$(if ($neg.report) { $neg.report.identity.reason } else { '' })"
Add-Result '负例：身份不符时绝不启动守护进程' ($null -ne $neg.report -and -not $neg.report.started) `
    "started=$(if ($neg.report) { $neg.report.started } else { '<无报告>' })"
Add-Result '负例：报告记录产物 commit 与源码 HEAD（可事后归因）' `
    ($null -ne $neg.report -and $neg.report.binary.commit -eq $staleCommit -and $neg.report.source.commit -eq $src.commit) `
    "binary=$(if ($neg.report) { $neg.report.binary.commit } else { '' }) head=$($src.commit)"
Add-Result '负例：报告带产物 SHA256（身份链完整）' `
    ($null -ne $neg.report -and $neg.report.binary.sha256 -and $neg.report.binary.sha256.Length -eq 64) `
    "sha256=$(if ($neg.report) { $neg.report.binary.sha256 } else { '' })"

# ---- 正例：与 HEAD 同源的产物通过身份门（IdentityOnly 不启动进程） ------------------
$pos = Invoke-Smoke $newExe 'p0-pos'
Add-Result '正例：身份匹配必须退出码 0' ($pos.code -eq 0) "exit=$($pos.code)"
Add-Result '正例：身份门通过（identity.ok=true）' ($null -ne $pos.report -and $pos.report.identity.ok) `
    "identity.ok=$(if ($pos.report) { $pos.report.identity.ok } else { '<无报告>' })"
Add-Result '正例：-IdentityOnly 不启动守护进程' ($null -ne $pos.report -and -not $pos.report.started) `
    "started=$(if ($pos.report) { $pos.report.started } else { '<无报告>' })"

# ---- 真实旧二进制：用**实际构建的 exe** + OWO_BUILD_INFO 覆写为 M0 提交 -------------
# 这直接编码指南 P0 完成条件："用旧 M0 二进制跑当前 HEAD smoke 必须失败"。
# 用运行时身份覆写（真实 exe + 真实 OWO_BUILD_INFO 链）等价于旧二进制，且不依赖本机
# 是否还留着 M0 产物；比 .cmd 垫片更接近真实场景。
$realExe = Join-Path $root 'target\debug\owo-agent.exe'
if (Test-Path -LiteralPath $realExe) {
    $override = Join-Path $tmp 'm0-build-info.json'
    '{"git_commit":"3f4bb0310d07606dd484556a453c8cdbca056963","git_dirty":true,"built_at":"2026-09-19T06:58:26Z"}' |
        Set-Content -LiteralPath $override -Encoding ascii
    $prevOverride = $env:OWO_BUILD_INFO
    $env:OWO_BUILD_INFO = $override
    try { $m0 = Invoke-Smoke $realExe 'p0-m0-neg' }
    finally {
        if ($prevOverride) { $env:OWO_BUILD_INFO = $prevOverride }
        else { Remove-Item Env:\OWO_BUILD_INFO -ErrorAction SilentlyContinue }
    }
    Add-Result '真实旧二进制（M0 commit=3f4bb03）跑当前 HEAD smoke 必须退出 3' ($m0.code -eq 3) "exit=$($m0.code)"
    Add-Result '真实旧二进制：报告 identity.ok=false 且未启动进程' `
        ($null -ne $m0.report -and -not $m0.report.identity.ok -and -not $m0.report.started) `
        "identity.ok=$(if ($m0.report) { $m0.report.identity.ok } else { '<无报告>' }) started=$(if ($m0.report) { $m0.report.started } else { '<无报告>' })"
} else {
    Add-Result '真实旧二进制负例（需先构建 target\debug\owo-agent.exe）' $false "缺少 $realExe"
}

# ---- 单元级：Test-CiBinaryIdentity 的 dirty 漂移检测 --------------------------------
$s = [pscustomobject]@{ commit = 'abc'; dirty = $false }
$b = [pscustomobject]@{ commit = 'abc'; dirty = 'true'; version_line = 'x' }
$v = Test-CiBinaryIdentity -Source $s -Binary $b
Add-Result '单元：产物 dirty=true 而源码 clean 必须被拒（dirty 漂移）' (-not $v.ok) "reason=$($v.reason)"
$b2 = [pscustomobject]@{ commit = 'abc'; dirty = 'false'; version_line = 'x' }
$v2 = Test-CiBinaryIdentity -Source $s -Binary $b2
Add-Result '单元：clean/clean 身份一致放行' ($v2.ok) "reason=$($v2.reason)"

Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue

$fail = @($results | Where-Object { -not $_.ok })
Write-Host ("[test-mk-smoke-identity] {0}/{1} 通过" -f (@($results).Count - $fail.Count), @($results).Count)
if ($fail.Count -gt 0) { exit 1 }
exit 0
