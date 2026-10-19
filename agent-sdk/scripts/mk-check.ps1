# 微内核重构专用：基线/回归 `cargo check --workspace --all-targets` 经 §2.4 门禁执行。
#
# 用法（本机只有 Windows PowerShell 5.1，没有 pwsh 7）：
#   & "<repo>\agent-sdk\scripts\mk-check.ps1" -Tag baseline
#   & "<repo>\agent-sdk\scripts\mk-check.ps1" -Tag m1 -WithDevtool   # 连开发工具 workspace 一起验证
#
# 注意（实测踩过的坑）：`Invoke-CiCargo` 内部用 `$global:LASTEXITCODE = <code>` 收口，
# 而 `$LASTEXITCODE` 只在原生命令结束时由 PowerShell 自动写入。因此本脚本必须显式
# 要求 Invoke-CiCargo 把退出码 return 出来，不能依赖 `$LASTEXITCODE`——否则一旦
# 调用方设了 `$ErrorActionPreference='Stop'`，失败会被包成终止性错误、脚本以 1 收口，
# 真实的 101（编译失败）被吞掉，看起来就像“门禁说不出为什么红”。
param(
    [string]$Tag = 'check',
    [ValidateSet('normal', 'strict')][string]$PolicyMode = 'strict',
    # devtools/product-eval 是独立 workspace（自带 Cargo.lock 与 target），不进默认门禁；
    # 改动 core 公共面后必须带此开关跑一次，否则测不到被排除的评测面消费方。
    [switch]$WithDevtool
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $root
. (Join-Path $PSScriptRoot 'resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet | Out-Null
. (Join-Path $PSScriptRoot 'ci-shared.ps1')

$logDir = Join-Path $root 'docs\qa\logs'
if (-not (Test-Path -LiteralPath $logDir)) { New-Item -ItemType Directory -Force -Path $logDir | Out-Null }
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

# 统一使用 --all-targets：把集成测试与 bin 目标一并纳入编译，避免“只 check lib 通过、
# 测试目标已断”的假绿灯。
$code = 1
try {
    $log = Join-Path $logDir ("mk-{0}-{1}.log" -f $Tag, $stamp)
    $code = Invoke-CiCargo -Arguments @('check', '--workspace', '--all-targets', '--locked') `
        -Cwd $root -Label "mk-check-$Tag" -PolicyMode $PolicyMode -LogFile $log -HeartbeatSec 30 -PassThru
    Write-Host "mk-check[$Tag] workspace exit=$code log=$log"
    if ($WithDevtool) {
        $dlog = Join-Path $logDir ("mk-{0}-devtool-{1}.log" -f $Tag, $stamp)
        $dc = Invoke-CiCargo -Arguments @('check', '--manifest-path', 'devtools/product-eval/Cargo.toml', '--all-targets', '--offline') `
            -Cwd $root -Label "mk-check-$Tag-devtool" -PolicyMode normal -LogFile $dlog -HeartbeatSec 30 -PassThru
        Write-Host "mk-check[$Tag] devtool   exit=$dc log=$dlog"
        if ($dc -ne 0 -and $code -eq 0) { $code = $dc }
    }
} finally {
    Write-Host "mk-check[$Tag] final exit=$code"
}
exit $code
