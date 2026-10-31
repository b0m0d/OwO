#requires -Version 5.1
$ErrorActionPreference = 'Stop'
$sdkRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $sdkRoot
$gatewayRoot = Join-Path $repoRoot 'gateway'
. (Join-Path $PSScriptRoot 'resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet | Out-Null
. (Join-Path $PSScriptRoot 'ci-shared.ps1')
$ciExit = Invoke-CiCargo -Arguments @('build', '-p', 'owo-intent-gateway', '--release', '--locked', '-j', '1') -Cwd $gatewayRoot -PolicyMode strict -PassThru
if ($ciExit -ne 0) { throw "Gateway build failed: exit=$ciExit" }
$targetRoot = if ($env:CARGO_TARGET_DIR) {
    if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { $env:CARGO_TARGET_DIR } else { Join-Path $gatewayRoot $env:CARGO_TARGET_DIR }
} else { Join-Path $gatewayRoot 'target' }
& python (Join-Path $PSScriptRoot 'stage-ime-gateway.py') --repo $repoRoot --binary (Join-Path $targetRoot 'release\owo-intent-gateway.exe') --output (Join-Path $sdkRoot 'desktop\electron\binaries')
if ($LASTEXITCODE -ne 0) { throw 'Gateway staging failed' }
