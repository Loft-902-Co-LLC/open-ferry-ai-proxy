# Hosted Windows steps run elevated. Use Task Scheduler's real limited,
# interactive token for the account installer and the user-service lifecycle.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Python,
    [Parameter(Mandatory)][string]$Work
)
$ErrorActionPreference = 'Stop'
$taskName = 'open-ferry-qualification'
$scriptPath = Join-Path $PSScriptRoot 'run.py'
$resultPath = Join-Path $Work 'diagnostics/result.json'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $Python -Argument "-I `"$scriptPath`" `"$Work`"" -WorkingDirectory (Get-Location).Path
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 40)
try {
    Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings | Out-Null
    Start-ScheduledTask -TaskName $taskName
    $deadline = [DateTime]::UtcNow.AddMinutes(38)
    while (-not (Test-Path -LiteralPath $resultPath)) {
        if ([DateTime]::UtcNow -gt $deadline) {
            throw 'Limited user qualification timed out; inspect task state and diagnostics.'
        }
        $info = Get-ScheduledTaskInfo -TaskName $taskName
        if ($info.LastRunTime.Year -gt 2000 -and
            (Get-ScheduledTask -TaskName $taskName).State -ne 'Running') {
            throw "Limited user qualification exited without a result: $($info.LastTaskResult)."
        }
        Start-Sleep -Seconds 10
    }
    $result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
    Get-Content -LiteralPath (Join-Path $Work 'diagnostics/qualification.log')
    if (-not $result.passed) { throw "User qualification failed: $($result.error)" }
} finally {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
}
