# 微内核重构专用：基线/回归 `cargo check --workspace --all-targets` 经 §2.4 门禁执行。
#
# 用法（本机只有 Windows PowerShell 5.1，没有 pwsh 7）：
#   & "T:\创新创业\OwO-master\agent-sdk\scripts\mk-check.ps1" -Tag baseline
#
# 注意（实测踩过的坑）：`Invoke-CiCargo` 内部用 `$global:LASTEXITCODE = <code>` 收口，
# 而 `$LASTEXITCODE` 只在原生命令结束时由 PowerShell 自动写入。因此本脚本必须显式
# 要求 Invoke-CiCargo 把退出码 return 出来，不能依赖 `$LASTEXITCODE`——否则一旦
# 调用方设了 `$ErrorActionPreference='Stop'`，失败会被包成终止性错误、脚本以 1 收口，
# 真实的 101（编译失败）被吞掉，看起来就像“门禁说不出为什么红”。
param(
    [string]$Tag = 'check',
    [ValidateSet('normal', 'strict')][string]$PolicyMode = 'strict'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
. (Join-Path $PSScriptRoot 'resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet | Out-Null
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

$logDir = Join-Path $root 'docs\qa\logs'
if (-not (Test-Path -LiteralPath $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
$log = Join-Path $logDir ("mk-{0}-{1}.log" -f $Tag, (Get-Date -Format 'yyyyMMdd-HHmmss'))

# 统一使用 --all-targets：把集成测试与 bin 目标一并纳入编译，避免“只 check lib 通过、
# 测试目标已断”的假绿灯。
$args = @('check', '--workspace', '--all-targets', '--locked')

$code = 1
try {
    $code = Invoke-CiCargo -Arguments $args -Cwd $root -Label "mk-check-$Tag" `
        -PolicyMode $PolicyMode -LogFile $log -HeartbeatSec 30 -PassThru
} finally {
    Write-Host "mk-check[$Tag] exit=$code log=$log"
}
exit $code
