# Run one isolated live paired Single/Team ProductEval batch.
[CmdletBinding()]
param(
    [string]$Suite = (Join-Path (Split-Path -Parent $PSScriptRoot) "evals\v1\suite.json"),
    [string]$OutputRoot = "scratch-eval-runs\team-single-paired",
    [string]$BatchPrefix = "team-single",
    [ValidateRange(2, 100)][int]$Repetitions = 4,
    [string]$Model,
    [string]$CaseFilter,
    [switch]$PlanOnly,
    [ValidateRange(1, 256)][int]$TeamTurns = 8,
    [ValidateRange(0, 10)][int]$TeamRetries = 1
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath
$sdkRoot = Split-Path -Parent $PSScriptRoot
$evalRoot = Join-Path $sdkRoot "devtools\product-eval"
Set-Location $sdkRoot

if (($Repetitions % 2) -ne 0) { throw "Repetitions must be even for counterbalanced AB/BA ordering." }
if ($BatchPrefix -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$') { throw "BatchPrefix contains unsupported characters." }
if ($env:OWO_PRODUCT_EVAL_UNBOUNDED_CALLS -match '^(1|true|yes)$') { throw "Unbounded ProductEval calls are forbidden for paired evidence." }

$suitePath = if ([IO.Path]::IsPathRooted($Suite)) {
    [IO.Path]::GetFullPath($Suite)
} else {
    [IO.Path]::GetFullPath((Join-Path $sdkRoot $Suite))
}
if (-not (Test-Path -LiteralPath $suitePath -PathType Leaf)) { throw "Suite file not found: $suitePath" }
$suiteRoot = [IO.Path]::GetFullPath((Split-Path -Parent $suitePath))
$suiteData = Get-Content -LiteralPath $suitePath -Raw -Encoding UTF8 | ConvertFrom-Json
$selectedCases = New-Object 'System.Collections.Generic.List[object]'
$seenTaskIds = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
$taskIdByContent = New-Object 'System.Collections.Generic.Dictionary[string,string]' ([StringComparer]::Ordinal)
$sha256 = [Security.Cryptography.SHA256]::Create()
try {
    foreach ($taskEntry in @($suiteData.tasks)) {
        if ($taskEntry -isnot [string] -or [IO.Path]::IsPathRooted($taskEntry)) {
            throw "Suite task paths must be relative strings: $taskEntry"
        }
        $taskPath = [IO.Path]::GetFullPath((Join-Path $suiteRoot $taskEntry))
        $suitePrefix = $suiteRoot.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
        if (-not $taskPath.StartsWith($suitePrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Suite task escapes its suite directory: $taskEntry"
        }
        if (-not (Test-Path -LiteralPath $taskPath -PathType Leaf)) {
            throw "Suite task file not found: $taskPath"
        }
        $task = Get-Content -LiteralPath $taskPath -Raw -Encoding UTF8 | ConvertFrom-Json
        $taskId = [string]$task.id
        if ([string]::IsNullOrWhiteSpace($taskId)) { throw "Task file has no ID: $taskPath" }
        if ($CaseFilter -and -not $taskId.Contains($CaseFilter)) { continue }
        if (-not $seenTaskIds.Add($taskId)) { throw "Duplicate task ID in selected suite: $taskId" }

        # Match the Rust gate's semantic fields. Labels, repetition counts and filenames
        # do not create independent observations; only digest equality matters here.
        $canonicalInputs = @(
            foreach ($inputFixture in @($task.inputs)) {
                [ordered]@{
                    path = [string]$inputFixture.path
                    content = [string]$inputFixture.content
                }
            }
        )
        $content = [ordered]@{
            category = [string]$task.category
            instruction = [string]$task.instruction
            inputs = $canonicalInputs
        }
        $canonical = ConvertTo-Json -InputObject $content -Depth 50 -Compress
        $digestBytes = $sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($canonical))
        $fingerprint = [BitConverter]::ToString($digestBytes).Replace('-', '').ToLowerInvariant()
        if ($taskIdByContent.ContainsKey($fingerprint)) {
            throw "Duplicate task content in selected suite: $($taskIdByContent[$fingerprint]) and $taskId"
        }
        $taskIdByContent.Add($fingerprint, $taskId)
        $selectedCases.Add($task)
    }
} finally {
    $sha256.Dispose()
}
$taskCount = $selectedCases.Count
$independentTaskCount = $taskIdByContent.Count
if ($taskCount -lt 3 -or ($taskCount * $Repetitions) -lt 30) {
    throw "Directional paired evidence needs at least 3 independent tasks and 30 planned pairs; selected tasks=$taskCount independent content clusters=$independentTaskCount repetitions=$Repetitions."
}

if ($PlanOnly) {
    $plannedPairs = $taskCount * $Repetitions
    Write-Host ("PLAN ONLY: tasks={0} independent_content_clusters={1} repetitions={2} paired_cells={3}; policy floor=30 independent task-content clusters." -f `
        $taskCount, $independentTaskCount, $Repetitions, $plannedPairs) -ForegroundColor Cyan
    if ($independentTaskCount -lt 30) {
        Write-Host "This suite can produce directional comparisons, but cannot satisfy the independent task-content policy sample floor." -ForegroundColor Yellow
    }
    exit 0
}

. (Join-Path $PSScriptRoot "resolve-ort.ps1")
$null = Resolve-OwoOrtEnv -NoDownload -Quiet
if (-not $env:OPENAI_API_KEY) {
    $userKey = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if ($userKey) { $env:OPENAI_API_KEY = $userKey }
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$batchLabel = "$BatchPrefix-$stamp"
$outputBase = if ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
} else {
    [IO.Path]::GetFullPath((Join-Path $sdkRoot $OutputRoot))
}
$outputPath = [IO.Path]::GetFullPath((Join-Path $outputBase $batchLabel))
if (Test-Path -LiteralPath $outputPath) { throw "Output path already exists; refusing to overwrite: $outputPath" }

$env:OWO_BLOG_BENCHMARK_SUITE = $suitePath
$env:OWO_BLOG_BENCHMARK_OUT = $outputPath
$env:OWO_PAIRED_BATCH_LABEL = $batchLabel
$env:OWO_PAIRED_REPETITIONS = [string]$Repetitions
$env:OWO_PAIRED_TEAM_TURNS = [string]$TeamTurns
$env:OWO_PAIRED_TEAM_RETRIES = [string]$TeamRetries
if ($Model) { $env:OWO_PAIRED_MODEL = $Model } else { Remove-Item Env:OWO_PAIRED_MODEL -ErrorAction SilentlyContinue }
if ($CaseFilter) { $env:OWO_PAIRED_CASE_FILTER = $CaseFilter } else { Remove-Item Env:OWO_PAIRED_CASE_FILTER -ErrorAction SilentlyContinue }

Write-Host "Paired Single/Team benchmark: batch=$batchLabel tasks=$taskCount independent_content_clusters=$independentTaskCount repetitions=$Repetitions output=$outputPath" -ForegroundColor Cyan
Write-Host "Policy evidence counts unique category/instruction/input content; repetitions do not increase that count." -ForegroundColor Yellow
$manifest = Join-Path $evalRoot "Cargo.toml"
$testExit = Invoke-CiCargo -Arguments @(
    'test', '--manifest-path', $manifest, '--test', 'product_eval_fullstack_live',
    '--locked', '-j', '2', '--', '--ignored', 'live_single_vs_team_paired_suite', '--nocapture'
) -Cwd $evalRoot -HeartbeatSec 30 -Label "paired-$BatchPrefix" -PolicyMode normal -PassThru
if ($testExit -ne 0) {
    Write-Host "Paired evaluation failed; preserve and inspect its output directory: $outputPath" -ForegroundColor Red
    exit 1
}

$reportPath = Join-Path $outputPath "paired-report.json"
if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw "Benchmark completed without paired report: $reportPath" }
$report = Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json
$assessment = $report.evaluation_contract.sample_assessment
Write-Host "Paired report: $reportPath" -ForegroundColor Green
Write-Host ("paired_cells={0} planned={1} execution_complete={2} directional_floor={3} independent_cases_single={4} team={5} policy_sample_sufficient={6}" -f `
    $assessment.paired_cells, $assessment.planned_paired_cells, $assessment.paired_execution_complete, `
    $assessment.directional_evidence_floor_met, $assessment.single_independent_case_clusters, `
    $assessment.team_independent_case_clusters, $assessment.policy_sample_sufficient)
exit 0
