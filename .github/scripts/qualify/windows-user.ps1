# Log on a real temporary standard account: the hosted administrator has no
# limited token. Never weaken the installer's elevation check.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Python,
    [Parameter(Mandatory)][string]$Work,
    [switch]$ProbeOnly,
    [switch]$Child,
    [switch]$ProbeTask
)
$ErrorActionPreference = 'Stop'
$diagnostics = Join-Path $Work 'diagnostics'
$probePath = Join-Path $diagnostics 'standard-user-probe.json'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)

if ($Child) {
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'The temporary standard-account logon unexpectedly has administrator rights.'
    }
    # CreateProcessWithLogonW can inherit its caller's environment. Resolve the
    # logged-on account's known folders after its real profile has been loaded.
    $env:USERPROFILE = [Environment]::GetFolderPath('UserProfile')
    $env:APPDATA = [Environment]::GetFolderPath('ApplicationData')
    $env:LOCALAPPDATA = [Environment]::GetFolderPath('LocalApplicationData')
    if (-not $env:USERPROFILE -or -not $env:APPDATA -or -not $env:LOCALAPPDATA) {
        throw 'The standard-account logon has no loaded profile.'
    }
    $userName = $identity.Name.Split('\')[-1]
    if ((Split-Path -Leaf $env:USERPROFILE) -notlike "$userName*") {
        throw 'The known folders do not belong to the logged-on standard account.'
    }
    if ($ProbeTask) {
        $proof = @{
            account = $identity.Name
            is_admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
            profile = $env:USERPROFILE
            source = 'Task Scheduler InteractiveToken'
        } | ConvertTo-Json
        [IO.File]::WriteAllText($probePath, $proof, [Text.UTF8Encoding]::new($false))
        exit 0
    }
    if ($ProbeOnly) {
        $taskName = 'open-ferry-user-probe'
        $shellPath = (Get-Process -Id $PID).Path
        $arguments = "-NoProfile -NonInteractive -File `"$PSCommandPath`" -Child -ProbeTask -Python `"$Python`" -Work `"$Work`""
        $action = New-ScheduledTaskAction -Execute $shellPath -Argument $arguments -WorkingDirectory (Get-Location).Path
        $taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Limited
        try {
            Register-ScheduledTask -TaskName $taskName -Action $action -Principal $taskPrincipal | Out-Null
            Start-ScheduledTask -TaskName $taskName
            $deadline = [DateTime]::UtcNow.AddSeconds(60)
            while (-not (Test-Path -LiteralPath $probePath)) {
                if ([DateTime]::UtcNow -gt $deadline) {
                    $info = Get-ScheduledTaskInfo -TaskName $taskName
                    throw "Standard-account interactive task did not run: LastTaskResult=$($info.LastTaskResult)."
                }
                Start-Sleep -Seconds 2
            }
            $proof = Get-Content -LiteralPath $probePath -Raw | ConvertFrom-Json
            if ($proof.is_admin) { throw 'The interactive task unexpectedly runs as administrator.' }
            Write-Output "PASS preflight: real standard account $($proof.account), interactive task, profile $($proof.profile)."
        } finally {
            Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        }
        exit 0
    }
    & $Python -I (Join-Path $PSScriptRoot 'run.py') $Work
    exit $LASTEXITCODE
}

$accountName = 'ofq' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
if (Get-LocalUser -Name $accountName -ErrorAction SilentlyContinue) {
    throw 'The disposable qualification account already exists; refusing to reuse it.'
}
New-Item -ItemType Directory -Path $diagnostics -Force | Out-Null
# Never serialize or print this generated password or pass it in arguments.
$qualificationPassword = [Security.SecureString]::new()
foreach ($code in [Security.Cryptography.RandomNumberGenerator]::GetBytes(32)) {
    $qualificationPassword.AppendChar([char](33 + ($code % 90)))
}
foreach ($character in 'Aa9!'.ToCharArray()) { $qualificationPassword.AppendChar($character) }
$qualificationPassword.MakeReadOnly()
$qualificationCredential = [Management.Automation.PSCredential]::new(".\$accountName", $qualificationPassword)
$created = $false
$process = $null
try {
    $account = New-LocalUser -Name $accountName -Password $qualificationPassword -AccountNeverExpires -PasswordNeverExpires
    $created = $true
    $usersGroup = Get-LocalGroup -SID 'S-1-5-32-545'
    Add-LocalGroupMember -Group $usersGroup -Member $account
    # The mirror has no private key. Allow reads and redacted diagnostics;
    # installed records live in the standard account's own profile.
    & icacls.exe $Work /grant "*$($account.SID.Value):(OI)(CI)F" /T /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not grant the standard account access to qualification files.' }
    $shellPath = (Get-Process -Id $PID).Path
    $arguments = "-NoProfile -NonInteractive -File `"$PSCommandPath`" -Child -Python `"$Python`" -Work `"$Work`""
    if ($ProbeOnly) { $arguments += ' -ProbeOnly' }
    $process = Start-Process -FilePath $shellPath -ArgumentList $arguments -Credential $qualificationCredential -LoadUserProfile -WindowStyle Hidden -WorkingDirectory (Get-Location).Path -RedirectStandardOutput (Join-Path $diagnostics 'launcher.stdout') -RedirectStandardError (Join-Path $diagnostics 'launcher.stderr') -PassThru
    if (-not $process.WaitForExit(38 * 60 * 1000)) {
        throw 'Standard-account qualification exceeded its 38-minute watchdog.'
    }
    Get-Content -LiteralPath (Join-Path $diagnostics 'launcher.stdout')
    Get-Content -LiteralPath (Join-Path $diagnostics 'launcher.stderr')
    if ($process.ExitCode -ne 0) { throw "Standard-account qualification exited $($process.ExitCode)." }
    if (-not $ProbeOnly) {
        $result = Get-Content -LiteralPath (Join-Path $diagnostics 'result.json') -Raw | ConvertFrom-Json
        if (-not $result.passed) { throw "User qualification failed: $($result.error)" }
    }
} finally {
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force }
    if ($created) { Remove-LocalUser -Name $accountName }
    $qualificationPassword.Dispose()
}
