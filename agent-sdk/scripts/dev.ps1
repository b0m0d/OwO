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
    .\scripts\dev.ps1 test -p owo-agent-core --lib change_set
    .\scripts\dev.ps1 check
    .\scripts\dev.ps1 clippy
    .\scripts\dev.ps1 fmt
    .\scripts\dev.ps1 info
    .\scripts\dev.ps1 serve -Port 4101
    .\scripts\dev.ps1 eval -Full
#>

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('build', 'check', 'test', 'clippy', 'fmt', 'eval', 'serve', 'info')]
    [string]$Command = 'info',

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
function Invoke-CargoStep {
    param([string]$Step, [string[]]$CargoArgs)
    Write-Host ""
    Write-Host "===== dev.ps1: $Step =====" -ForegroundColor Cyan
    # cargo 进度写 stderr；PowerShell 5.1 会包成 NativeCommandError 记录，
    # ErrorActionPreference=Stop 时会被误判为终止错误。临时降为 Continue，
    # 以 cargo 真实退出码（$LASTEXITCODE）判定成败。
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & cargo @CargoArgs
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($LASTEXITCODE -ne 0) { throw "$Step failed (cargo exit $LASTEXITCODE)" }
}

function Get-OwoTsVersion {
    $pkg = Get-Content (Join-Path $sdkRoot "clients\ts\package.json") -Raw | ConvertFrom-Json
    return $pkg.version
}

function Assert-OwoVersionConsistency {
    $wsVersion = (Get-Content (Join-Path $sdkRoot "Cargo.toml") | Select-String -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
    $tsVersion = Get-OwoTsVersion
    if ($wsVersion -ne $tsVersion) {
        throw "Version mismatch: workspace=$wsVersion but clients/ts/package.json=$tsVersion. Keep them in sync (single source: workspace)."
    }
    Write-Host "[dev] version consistent: $wsVersion (workspace == TS client)" -ForegroundColor Green
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
        Write-Host "hint: run '.\scripts\dev.ps1 build|check|test|clippy|fmt|serve|eval' to work."
    }
    'build' {
        Assert-OwoVersionConsistency
        Invoke-CargoStep 'build' (@('build') + $RemainingArgs)
    }
    'check' {
        Assert-OwoVersionConsistency
        Invoke-CargoStep 'check' (@('check') + $RemainingArgs)
    }
    'test' {
        Invoke-CargoStep 'test' (@('test') + $RemainingArgs)
    }
    'clippy' {
        Invoke-CargoStep 'clippy' (@('clippy', '--workspace', '--all-targets') + $RemainingArgs)
    }
    'fmt' {
        Invoke-CargoStep 'fmt-check' (@('fmt', '--all', '--', '--check') + $RemainingArgs)
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
        $portIndex = [Array]::IndexOf($RemainingArgs, '-Port')
        if ($portIndex -ge 0 -and $RemainingArgs.Count -gt $portIndex + 1) {
            $serverArgs += '--port'
            $serverArgs += [string]$RemainingArgs[$portIndex + 1]
        }
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & cargo @serverArgs
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -ne 0) { throw "serve stopped (exit $LASTEXITCODE)" }
    }
}
exit 0