# Register a recurring live paired Single/Team ProductEval task for the current user.
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Medium")]
param(
    [Parameter(Mandatory = $true)][ValidateSet("Daily", "Weekly")][string]$Schedule,
    [Parameter(Mandatory = $true)][ValidatePattern('^(?:[01]\d|2[0-3]):[0-5]\d$')][string]$At,
    [ValidateSet("Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday")][string]$DayOfWeek = "Sunday",
    [string]$TaskName = "OwO-Team-Single-Paired-Eval",
    [string]$OutputRoot = "scratch-eval-runs\team-single-paired",
    [string]$BatchPrefix = "scheduled",
    [ValidateRange(2, 4)][int]$Repetitions = 4,
    [string]$Model
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if (($Repetitions % 2) -ne 0) { throw "Repetitions must be even." }
if ($BatchPrefix -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$') { throw "BatchPrefix contains unsupported characters." }
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    throw "Task '$TaskName' already exists; refusing to replace it. Unregister it explicitly before changing the schedule."
}

$sdkRoot = Split-Path -Parent $PSScriptRoot
$runner = Join-Path $PSScriptRoot "run-paired-team-eval.ps1"
$atTime = [DateTime]::ParseExact($At, "HH:mm", [Globalization.CultureInfo]::InvariantCulture)
if ($Schedule -eq "Daily") {
    $trigger = New-ScheduledTaskTrigger -Daily -At $atTime
} else {
    $trigger = New-ScheduledTaskTrigger -Weekly -DaysOfWeek $DayOfWeek -At $atTime
}

function Quote-TaskArgument([string]$Value) {
    $escaped = $Value -replace '(\\*)"', '$1$1\\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

$argValues = @(
    '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $runner,
    '-OutputRoot', $OutputRoot, '-BatchPrefix', $BatchPrefix, '-Repetitions', [string]$Repetitions
)
if ($Model) { $argValues += @('-Model', $Model) }
$arguments = ($argValues | ForEach-Object { Quote-TaskArgument ([string]$_) }) -join ' '
$action = New-ScheduledTaskAction -Execute (Join-Path $PSHOME "powershell.exe") -Argument $arguments -WorkingDirectory $sdkRoot
$principal = New-ScheduledTaskPrincipal -UserId ("{0}\{1}" -f $env:USERDOMAIN, $env:USERNAME) -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -MultipleInstances IgnoreNew -ExecutionTimeLimit ([TimeSpan]::FromHours(12))
$task = New-ScheduledTask -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description "Runs an isolated, counterbalanced live ProductEval comparison of Single and forced Team. Model usage is bounded by the selected suite task budgets."
if ($PSCmdlet.ShouldProcess($TaskName, "Register $Schedule paired Single/Team evaluation at $At")) {
    Register-ScheduledTask -TaskName $TaskName -InputObject $task | Out-Null
    Write-Host "Registered $Schedule paired Single/Team evaluation at $At ($DayOfWeek when weekly). Repetitions=$Repetitions; output root=$OutputRoot" -ForegroundColor Green
    Write-Host "Runs only in the current user's interactive session; each run creates a timestamped output directory." -ForegroundColor Yellow
}
