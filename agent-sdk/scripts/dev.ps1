#requires -Version 5.1
<#
dev.ps1 - Unified dev entry (v1 closeout, Lane 1).

Every build / check / test / clippy / fmt / eval / serve / info action goes
through this entry so that:
  - the ONNX Runtime dependency is located+validated exactly once (init-dev-env.ps1),
  - the workspace version / git commit / dirty state are recorded in build-info.json,
  - a fresh terminal (no leftover env, no leftover target dir) behaves identically.

No action in this file writes to the user/registry environment or commits any
machine path. The credential README rule applies: OPENAI_API_KEY is injected
from the user-level registry only when missing in the current process, and is
never printed.

Usage:
  powershell -ExecutionPolicy Bypass -File scripts\dev.ps1 <Command> [args...]
  Examples:
    .\scripts\dev.ps1 build
    .\scripts\dev.ps1 check -p owo-agent-protocol
    .\scripts\dev.ps1 test -p owo-agent-core --lib change_set
    .\scripts\dev.ps1 check
    .\scripts\dev.ps1 clippy
    .\scripts\dev.ps1 fmt
    .\scripts\dev.ps1 info
    .\scripts\dev.ps1 serve -Port 4101
    .\scripts\dev.ps1 eval -Full

PowerShell 5.1 binding note: a bare `-p <value>` pair is silently dropped by
the binder when only ValueFromRemainingArguments is declared, so -p is a
declared alias (Package) here and is re-emitted to cargo verbatim. Valueless
flags (-q/--lib/...) flow through RemainingArgs untouched.
#>

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('build', 'check', 'test', 'clippy', 'fmt', 'eval', 'serve', 'info')]
    [string]$Command = 'info',

    # Cargo package selector (alias p): special-cased, see the note above.
    [Alias('p')]
    [string]$Package,

    # Extra arguments forwarded to the underlying command (cargo / pnpm / scripts).
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$RemainingArgs,

    # Skip environment initialization (use only when already initialized in this shell).
    [switch]$SkipInit,
    # Allow downloading the ONNX Runtime dependency if missing.
    [switch]$EnsureOrt,
    # Also ensure OCR models are present (downloads when -NoDownload is absent).
    [switch]$EnsureModels,
    # Never download anything.
    [switch]$NoDownload
)

$ErrorActionPreference = 'Stop'
$sdkRoot = Split-Path -Parent $PSScriptRoot

# ---------------------------------------------------------------------------
# 0a) §2.4 Rust 资源安全红线：默认注入受限并发，并经统一 cargo 入口执行。
#     完整 workspace（无 -p）与 release 自动降为 1；定向命令为 2。
# ---------------------------------------------------------------------------
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath

# ---------------------------------------------------------------------------
# 0) Credential passthrough (never printed): fresh shells do not inherit the
#    user-level registry value, so read it into the process scope when missing.
# ---------------------------------------------------------------------------
if (-not $env:OPENAI_API_KEY) {
    $k = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if ($k) { $env:OPENAI_API_KEY = $k }
}

# ---------------------------------------------------------------------------
# 1) Environment initialization (shared logic for every entry point).
# ---------------------------------------------------------------------------
if (-not $SkipInit) {
    . (Join-Path $PSScriptRoot "init-dev-env.ps1")
    $null = Initialize-OwoDevEnv -EnsureOrt:$EnsureOrt -EnsureModels:$EnsureModels -NoDownload:$NoDownload
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
function Get-CargoTargetArgs {
    # -p <pkg>（显式绑定，见文件头说明）+ 其余透传参数。
    $a = @()
    if ($Package) { $a += @('-p', $Package) }
    if ($RemainingArgs) { $a += @($RemainingArgs) }
    return $a
}

# §2.4 红线 1/2 的自动选档：release / --workspace / 完整 core → strict（-j 1 /
# --test-threads 1）；单包或带过滤器的定向跑 → normal（-j 2 / 2）。
# 只有更低档，没有更高档（红线 8：不得为省时间提高并发）。
function Get-DevCargoPolicyMode {
    param([string[]]$CargoArgs)
    $a = @($CargoArgs | ForEach-Object { [string]$_ })
    $joined = ' ' + ($a -join ' ') + ' '
    if ($joined -match ' --release( |$)') { return 'strict' }
    $sub = ''
    foreach ($t in $a) { if ($t -notmatch '^-') { $sub = $t; break } }
    if (@('build', 'check', 'test', 'clippy', 'bench', 'doc', 'fix') -notcontains $sub) { return 'normal' }
    if ($joined -match ' --workspace( |$)') { return 'strict' }
    if ($joined -match ' -p owo-agent-core( |$)') {
        if (($joined -match ' --lib( |$)') -or ($joined -match ' --test [^-]')) { return 'normal' }
        return 'strict'
    }
    return 'normal'
}

function Invoke-CargoStep {
    param([string]$Step, [string[]]$CargoArgs)
    Write-Host ""
    Write-Host "===== dev.ps1: $Step =====" -ForegroundColor Cyan
    # §2.4：一律经统一入口执行（显式 -j、内存门、构建空闲门、30s 心跳、真实退出码）。
    # cargo 进度写 stderr；PowerShell 5.1 会包成 NativeCommandError 记录，
    # ErrorActionPreference=Stop 时会被误判为终止错误——流式执行器逐行回显，
    # 不依赖 ErrorAction，也不再用 --quiet + Select-Object -Last 隐藏长任务进度（红线 5）。
    $mode = Get-DevCargoPolicyMode -CargoArgs $CargoArgs
    $logDir = if ($env:OWO_DEV_LOG_DIR) { $env:OWO_DEV_LOG_DIR } else { Join-Path ([IO.Path]::GetTempPath()) 'owo-dev-logs' }
    $log = Join-Path $logDir ("dev-{0}-{1}.log" -f $Step, (Get-Date -Format 'yyyyMMdd-HHmmss'))
    Invoke-CiCargo -Arguments $CargoArgs -Cwd $sdkRoot -HeartbeatSec 30 -LogFile $log -Label $Step -PolicyMode $mode
    $code = $global:LASTEXITCODE
    if ($code -ne 0) { throw "$Step failed (cargo exit $code; §2.4 mode=$mode; log=$log)" }
}

function Get-OwoTsVersion {
    $pkg = Get-Content (Join-Path $sdkRoot "clients\ts\package.json") -Raw | ConvertFrom-Json
    return $pkg.version
}

function Assert-OwoVersionConsistency {
    $wsVersion = (Get-Content (Join-Path $sdkRoot "Cargo.toml") | Select-String -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
    $tsVersion = Get-OwoTsVersion
    $tauriConfVersion = (Get-Content (Join-Path $sdkRoot "desktop\tauri\src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json).version
    $deskCargoVersion = (Get-Content (Join-Path $sdkRoot "desktop\tauri\src-tauri\Cargo.toml") | Select-String -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
    $mismatch = @()
    if ($wsVersion -ne $tsVersion) { $mismatch += "clients/ts/package.json=$tsVersion" }
    if ($wsVersion -ne $tauriConfVersion) { $mismatch += "desktop/tauri/src-tauri/tauri.conf.json=$tauriConfVersion" }
    if ($wsVersion -ne $deskCargoVersion) { $mismatch += "desktop/tauri/src-tauri/Cargo.toml=$deskCargoVersion" }
    if ($mismatch.Count -gt 0) {
        throw "Version mismatch: workspace=$wsVersion vs $($mismatch -join ', '). Keep them in sync (single source: workspace version)."
    }
    Write-Host "[dev] version consistent: $wsVersion (workspace == TS client == desktop tauri)" -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# 2) Command dispatch
# ---------------------------------------------------------------------------
switch ($Command) {
    'info' {
        Assert-OwoVersionConsistency
        Write-Host ""
        Write-Host "--- build-info (agent-sdk/build-info.json) ---" -ForegroundColor Cyan
        $bi = Get-Content (Join-Path $sdkRoot "build-info.json") -Raw | ConvertFrom-Json
        $bi | Format-List
        Write-Host "--- resolved runtime env (session-scoped) ---" -ForegroundColor Cyan
        Write-Host "SHERPA_ONNX_LIB_DIR = $env:SHERPA_ONNX_LIB_DIR"
        Write-Host "ORT_LIB_PATH        = $env:ORT_LIB_PATH"
        Write-Host "OPENAI_API_KEY      = SET:$([bool]$env:OPENAI_API_KEY) LEN:$($env:OPENAI_API_KEY.Length)"
        # §2.4.4：开发入口自证当前生效的资源限制（不是建议值，是将被注入的值）。
        $rl = Get-CiResourceState
        Write-Host "§2.4 resource policy = default mode=$($rl.mode) jobs<=$($rl.jobs) test_threads<=$($rl.test_threads) (release/workspace/core 自动降为 1)"
        $m = Get-CiMemoryStatus
        Write-Host "§2.4 memory gate     = free=$($m.free_gb)GB used=$($m.used_percent)% (需 free>=$($m.gate.min_free_gb)GB 且 used<$($m.gate.max_used_percent)%)"
        Write-Host "hint: run '.\scripts\dev.ps1 build|check|test|clippy|fmt|serve|eval' to work."
    }
    'build' {
        Assert-OwoVersionConsistency
        Invoke-CargoStep 'build' (@('build') + (Get-CargoTargetArgs))
    }
    'check' {
        Assert-OwoVersionConsistency
        Invoke-CargoStep 'check' (@('check') + (Get-CargoTargetArgs))
    }
    'test' {
        Invoke-CargoStep 'test' (@('test') + (Get-CargoTargetArgs))
    }
    'clippy' {
        if ($Package) {
            Invoke-CargoStep 'clippy' (@('clippy', '-p', $Package, '--all-targets') + @($RemainingArgs))
        } else {
            Invoke-CargoStep 'clippy' (@('clippy', '--workspace', '--all-targets') + @($RemainingArgs))
        }
    }
    'fmt' {
        Invoke-CargoStep 'fmt-check' (@('fmt', '--all', '--', '--check') + @($RemainingArgs))
    }
    'eval' {
        Write-Host ""
        Write-Host "===== dev.ps1: eval (product-eval live runner) =====" -ForegroundColor Cyan
        $evalScript = Join-Path $PSScriptRoot "run-product-eval-live.ps1"
        if (-not (Test-Path $evalScript)) { throw "eval runner not found: $evalScript" }
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & $evalScript @RemainingArgs
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -ne 0) { throw "eval failed (exit $LASTEXITCODE)" }
    }
    'serve' {
        Assert-OwoVersionConsistency
        Write-Host ""
        Write-Host "===== dev.ps1: serve (owo-agent-cli serve) =====" -ForegroundColor Cyan
        $serverArgs = @('run', '-q', '-p', 'owo-agent-cli', '--', 'serve', '--workspace', $sdkRoot)
        $portIndex = [Array]::IndexOf(@($RemainingArgs), '-Port')
        if ($portIndex -ge 0 -and @($RemainingArgs).Count -gt $portIndex + 1) {
            $serverArgs += '--port'
            $serverArgs += [string]$RemainingArgs[$portIndex + 1]
        }
        # §2.4：编译段仍受 -j 上限、内存门与构建空闲门约束；常驻服务进程不启用
        # 运行中内存守护（红线 7 保护的是 rustc/测试分片，不是用户拉起的 serve）。
        $limits = Set-CiRustResourcePolicy -Mode 'normal' -Source 'dev.ps1 serve'
        Invoke-CiCargo -Arguments $serverArgs -Cwd $sdkRoot -HeartbeatSec 60 -Label 'serve' -PolicyMode 'normal'
        Write-Host ("    [§2.4] serve：编译段 -j {0}；服务常驻（无运行中守护）" -f $limits.jobs)
        if ($global:LASTEXITCODE -ne 0) { throw "serve stopped (exit $global:LASTEXITCODE)" }
    }
}
exit 0