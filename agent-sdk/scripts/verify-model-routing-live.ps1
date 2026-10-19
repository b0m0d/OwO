# verify-model-routing-live.ps1 - M4.1/M4.2 model-routing live acceptance probe.
#
# Drives `owo-agent serve` (ephemeral port, private OWO_AGENT_DATA + temp
# workspace) against the REAL OpenAI-compatible endpoint and asserts, end to
# end, the M4.2 routing semantics:
#   1. session create with explicit model  -> model_override visible + persisted
#   2. session create without model        -> override stays null (auto chain)
#   3. "default" sentinel                  -> normalized to auto, never leaked
#   4. live SSE turn on an overridden session -> 200 + assistant msg persisted
#   5. POST /session/{id}/model clear      -> override null via route
#   6. read-only subagent (auto)           -> success, display = default chain
#   6b. second serve with OWO_MODEL_FAST   -> subagent routed to fast tier;
#       explicit request.model beats the fast tier (priority order)
#   7. audit + traces show live activity   (M1 surfaces keep working live)
#
# Credentials ONLY via env / user-level registry (repo red line - never
# written to disk, never echoed). ASCII header on purpose; prompts are UTF-8
# with BOM (this file is saved with BOM for Windows PowerShell 5.1).
#
# Usage (run AFTER `cargo build -p owo-agent-cli`):
#   pwsh -File agent-sdk\scripts\verify-model-routing-live.ps1 `
#        [[-DefaultModel glm-5.3-flash] [-FastModel glm-4-flash] [-ProfileModel glm-5.3-flash]]
# Exit code: 0 = all checks passed, 1 = at least one check failed, 2 = blocked.
param(
    [string]$DefaultModel = 'glm-5.3-flash',
    [string]$FastModel = 'glm-4-flash',
    [string]$ProfileModel = 'glm-5.3-flash'
)
$ErrorActionPreference = 'Stop'
$sdk = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $sdk 'target\debug\owo-agent.exe'
if (-not (Test-Path $exe)) { Write-Host 'MISSING_EXE (run: cargo build -p owo-agent-cli)'; exit 2 }
$key = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
if ([string]::IsNullOrWhiteSpace($key)) { Write-Host 'NO_KEY (OPENAI_API_KEY user-level env missing)'; exit 2 }

$priv = Join-Path $env:TEMP ('owo-routing-live-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$data = Join-Path $priv 'data'
$ws = Join-Path $priv 'ws'
New-Item -ItemType Directory -Path $data, $ws -Force | Out-Null

$env:OWO_AGENT_DATA = $data
$env:OPENAI_API_KEY = $key
# Pin the main-chain default so the auto (no-override) path differs from the
# per-session override -> asymmetry proves routing.
$env:OPENAI_MODEL = $DefaultModel

function Wait-CoreReady([string]$stdoutFile, $proc, [int]$timeoutSec) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        if (Test-Path $stdoutFile) {
            $line = Select-String -Path $stdoutFile -Pattern 'core_ready' -SimpleMatch | Select-Object -First 1
            if ($line) {
                if ($line.Line -match '"port"\s*:\s*(\d+)') { return [int]$Matches[1] }
                elseif ($line.Line -match '\b(\d{2,5})\b') { return [int]$Matches[1] }
            }
        }
        if ($proc.HasExited) { return $null }
    }
    return $null
}

$fail = 0
function Check($name, $ok, $detail) {
    if ($ok) { Write-Host "PASS $name" } else { $script:fail++; Write-Host "FAIL $name :: $detail" }
}

try {
    # ---- serve #1: no fast tier (auto = OPENAI_MODEL = $DefaultModel) ----
    $stdoutFile = Join-Path $priv 'serve-out.txt'
    $proc = Start-Process -FilePath $exe -ArgumentList @('serve', '--port', '0', '--workspace', $ws) `
        -PassThru -NoNewWindow -RedirectStandardOutput $stdoutFile `
        -RedirectStandardError (Join-Path $priv 'serve-err.txt')
    $port = Wait-CoreReady $stdoutFile $proc 60
    if (-not $port) {
        Write-Host 'SERVE_BOOT_FAIL'
        try { Get-Content (Join-Path $priv 'serve-err.txt') -Tail 20 } catch {}
        throw 'serve1 boot failed'
    }
    Write-Host "SERVE_PORT=$port"
    $base = "http://127.0.0.1:$port"
    $token = (Invoke-RestMethod -Uri "$base/auth/token" -TimeoutSec 10).token
    $H = @{ Authorization = "Bearer $token" }

    # 1) explicit override session
    $s1 = Invoke-RestMethod -Method Post -Uri "$base/session" -Headers $H -ContentType 'application/json' `
        -Body (@{ workspace = $ws; model = $FastModel } | ConvertTo-Json -Compress)
    Check 'session_create_override_visible' ($s1.model -eq $FastModel) ($s1 | ConvertTo-Json -Compress)
    $g1 = Invoke-RestMethod -Uri "$base/session/$($s1.id)" -Headers $H
    Check 'override_persisted' ($g1.model_override -eq $FastModel) ($g1 | ConvertTo-Json -Compress)

    # 2) auto-default session
    $s2 = Invoke-RestMethod -Method Post -Uri "$base/session" -Headers $H -ContentType 'application/json' `
        -Body (@{ workspace = $ws } | ConvertTo-Json -Compress)
    $g2 = Invoke-RestMethod -Uri "$base/session/$($s2.id)" -Headers $H
    Check 'auto_default_null_override' ($null -eq $g2.model_override) ($g2 | ConvertTo-Json -Compress)

    # 3) sentinel create path
    $s3 = Invoke-RestMethod -Method Post -Uri "$base/session" -Headers $H -ContentType 'application/json' `
        -Body (@{ workspace = $ws; model = 'default' } | ConvertTo-Json -Compress)
    $g3 = Invoke-RestMethod -Uri "$base/session/$($s3.id)" -Headers $H
    Check 'sentinel_maps_to_auto' ($null -eq $g3.model_override -and $g3.model -eq $DefaultModel) ($g3 | ConvertTo-Json -Compress)

    # 4) live SSE turn on session 1 (body via @file: no shell-quoting loss)
    $turnBody = @{ prompt = '只回复四个字：路由正常。禁止调用任何工具。' } | ConvertTo-Json -Compress
    $turnBodyFile = Join-Path $priv 'turn1-body.json'
    [IO.File]::WriteAllBytes($turnBodyFile, [Text.Encoding]::UTF8.GetBytes($turnBody))
    $turnRaw = Join-Path $priv 'turn1.txt'
    $curlCode = curl.exe -sS --max-time 150 -H "Authorization: Bearer $token" -H 'Content-Type: application/json' `
        -X POST "$base/session/$($s1.id)/turn" --data-binary "@$turnBodyFile" -o $turnRaw -w '%{http_code}' 2>$null
    Write-Host "turn_http=$curlCode size=$((Get-Item $turnRaw -ErrorAction SilentlyContinue).Length)"
    $turnText = if (Test-Path $turnRaw) { Get-Content $turnRaw -Raw } else { '' }
    Check 'live_turn_completed' ($curlCode -eq '200' -and $turnText -match '"final"|"text"|"delta"') ('http=' + $curlCode + ' len=' + $turnText.Length)
    Start-Sleep -Milliseconds 800
    $g1b = Invoke-RestMethod -Uri "$base/session/$($s1.id)" -Headers $H
    $roles = @($g1b.messages | ForEach-Object { $_.role })
    Check 'live_turn_persisted' ($roles.Count -ge 2) ('roles=' + ($roles -join ','))

    # 5) clear via POST /session/{id}/model (sentinel)
    $m = Invoke-RestMethod -Method Post -Uri "$base/session/$($s1.id)/model" -Headers $H -ContentType 'application/json' `
        -Body '{"model":"default"}'
    Check 'model_clear_via_route' ($null -eq $m.model_override) ($m | ConvertTo-Json -Compress)

    # 6) read-only subagent live (fast tier unset -> auto chain)
    $saBody = @{ prompt = '只回复一个字：完成。禁止调用任何工具。'; read_only = $true } | ConvertTo-Json -Compress
    $sa = Invoke-RestMethod -Method Post -Uri "$base/subagent/run" -Headers $H -ContentType 'application/json' `
        -Body $saBody -TimeoutSec 180
    Check 'subagent_live_ok' ($sa.ok -eq $true -and $sa.text.Length -gt 0) ($sa.model)
    Check 'subagent_auto_display_default' ($sa.model -eq $DefaultModel) ($sa.model)

    # 7/8) audit + trace surfaces keep working live (M1 regression guard)
    $au = Invoke-RestMethod -Uri "$base/audit?limit=50" -Headers $H -TimeoutSec 15
    Check 'audit_has_entries' (($au | ConvertTo-Json -Depth 6 -Compress).Length -gt 20) 'audit empty'
    $tr = Invoke-RestMethod -Uri "$base/traces?limit=5" -Headers $H -TimeoutSec 15
    Check 'trace_recorded' (($tr | ConvertTo-Json -Depth 6 -Compress) -match 'glm') 'no-model-in-traces'

    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue

    # ---- serve #2: OWO_MODEL_FAST set -> fast tier routing + priority ----
    $env:OWO_MODEL_FAST = $FastModel
    $data2 = Join-Path $priv 'data2'
    $ws2 = Join-Path $priv 'ws2'
    New-Item -ItemType Directory -Path $data2, $ws2 -Force | Out-Null
    $env:OWO_AGENT_DATA = $data2
    $stdout2 = Join-Path $priv 'serve2-out.txt'
    $proc2 = Start-Process -FilePath $exe -ArgumentList @('serve', '--port', '0', '--workspace', $ws2) `
        -PassThru -NoNewWindow -RedirectStandardOutput $stdout2 `
        -RedirectStandardError (Join-Path $priv 'serve2-err.txt')
    $port2 = Wait-CoreReady $stdout2 $proc2 60
    if (-not $port2) { throw 'serve2 boot failed' }
    $base2 = "http://127.0.0.1:$port2"
    $token2 = (Invoke-RestMethod -Uri "$base2/auth/token" -TimeoutSec 10).token
    $H2 = @{ Authorization = "Bearer $token2" }
    $sa2Body = @{ prompt = '只回复一个字：好。禁止调用任何工具。'; read_only = $true } | ConvertTo-Json -Compress
    $sa2 = Invoke-RestMethod -Method Post -Uri "$base2/subagent/run" -Headers $H2 -ContentType 'application/json' `
        -Body $sa2Body -TimeoutSec 180
    Check 'fast_tier_live' ($sa2.ok -eq $true -and $sa2.model -eq $FastModel) ($sa2.model)
    # explicit request.model beats the fast tier (priority order). Small cheap
    # models occasionally miss WorkerOutputV1 structure; use a stable model id
    # for the priority leg (routing is what this leg tests, not contract).
    $sa3Body = @{ prompt = '只回复一个字：行。禁止调用任何工具。'; read_only = $true; model = $ProfileModel } | ConvertTo-Json -Compress
    $sa3 = Invoke-RestMethod -Method Post -Uri "$base2/subagent/run" -Headers $H2 -ContentType 'application/json' `
        -Body $sa3Body -TimeoutSec 180
    Check 'explicit_beats_fast_tier_live' ($sa3.ok -eq $true -and $sa3.model -eq $ProfileModel) ($sa3.model)
    Stop-Process -Id $proc2.Id -Force -ErrorAction SilentlyContinue

    # 7/8) audit + traces from serve #1 store live M1 surfaces
    # (re-point data root; audit/traces of serve2 kept separately)
} finally {
    Get-Process owo-agent -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item $priv -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_MODEL_FAST -ErrorAction SilentlyContinue
    Remove-Item Env:OWO_AGENT_DATA -ErrorAction SilentlyContinue
}
if ($fail -gt 0) { Write-Host "ROUTING_LIVE_FAILED count=$fail"; exit 1 }
Write-Host 'ROUTING_LIVE_ALL_PASS'
