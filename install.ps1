# Installs open-ferry on Windows from a GitHub release.
#
#   irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 | iex
#
# It downloads the release's Windows archive and checks it against the
# release's SHA256SUMS, and, when the GitHub CLI (gh) is installed, against
# the archive's build provenance attestation. It installs open-ferry.exe in
# %LOCALAPPDATA%\Programs\open-ferry. Then it looks for CLIProxyAPI with
# `open-ferry migrate -json`, which changes nothing. When it finds it, it
# asks whether to switch it to open-ferry, which then runs on CLIProxyAPI's
# config and credentials (`open-ferry migrate -yes`; see
# docs/migrating-from-cliproxyapi.md). When it doesn't, and there is no
# config at %APPDATA%\open-ferry\config.yaml, it writes one with
# `open-ferry init`. It changes neither PATH nor any profile.
# With -System, in an administrator terminal, the binary and config go to
# C:\Program Files\open-ferry; init -system uses the machine auth directory.
# It skips migration; account installs use a terminal not opened as administrator.
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. To pass options through
# irm, make the script a script block:
#
#   & ([scriptblock]::Create((irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1))) -Version 0.1.0
#
# Options:
#   -System            install for the whole machine; needs an administrator
#   -Version VERSION   install this version (such as 0.1.0) rather than the
#                      latest release
#   -InstallDir DIR    install in DIR (default:
#                      %LOCALAPPDATA%\Programs\open-ferry;
#                      with -System: C:\Program Files\open-ferry)
#   -ConfigPath PATH   the config to keep, or to write when there is none
#                      (default: %APPDATA%\open-ferry\config.yaml;
#                      with -System: config.yaml beside the machine binary)
#   -NoAttestation     don't check the build provenance attestation, even
#                      when gh is installed; the SHA256SUMS check still runs
#   -NoAutoUpdate      turn automatic updates off (self-update.mode: off in
#                      the config); open-ferry update -mode auto turns them on
#   -Migrate           when CLIProxyAPI is found, switch it to open-ferry
#                      without asking, as for an unattended install; fails
#                      when it can't be switched
#   -NoMigrate         don't look for CLIProxyAPI
#
# It asks before it switches unless PowerShell runs -NonInteractive, where
# it can't; with its input redirected, it reads the answer from it, and the
# end of the input is no answer.
#
# Environment:
#   OPEN_FERRY_INSTALL_SYSTEM    yes: as -System; no: an account install
#   OPEN_FERRY_INSTALL_TEST_PRIVILEGED  0 or 1: fake privilege only with a
#                                loopback base URL and FAKE_OPEN_FERRY_INSTALL_LOG
#   OPEN_FERRY_INSTALL_DIR       the install directory, when -InstallDir
#                                isn't given
#   OPEN_FERRY_INSTALL_CONFIG    the config path, when -ConfigPath isn't
#                                given
#   OPEN_FERRY_INSTALL_MIGRATE   yes: as -Migrate; no: as -NoMigrate, when
#                                neither is given
#   OPEN_FERRY_INSTALL_BASE_URL  where releases are downloaded from, in
#                                place of the GitHub repository's releases
#                                URL (for a mirror, or a test server)
#   OPEN_FERRY_INSTALL_GH        the GitHub CLI command (default: gh)
#   OPEN_FERRY_INSTALL_SELF_UPDATE  off, notify or auto: set self-update.mode
#                                in the config (-NoAutoUpdate is off)
#
# The new open-ferry.exe installs itself (`open-ferry install-self`): it waits
# for any other install or update, refuses to replace a binary a background
# service runs, replaces the old one with a way back, and writes
# install-receipt.json in %LOCALAPPDATA%\open-ferry, which lets open-ferry
# update itself; see docs/updates.md for turning that off.
#
# See https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy#install

param(
    [string]$Version,
    [string]$InstallDir,
    [string]$ConfigPath,
    [switch]$NoAttestation,
    [switch]$NoAutoUpdate,
    [switch]$Migrate,
    [switch]$NoMigrate,
    [switch]$System
)

# Everything runs in this function, which ends by returning or throwing,
# never with exit: under `irm | iex` the script runs in the caller's
# session, which exit would close.
function Install-OpenFerry {
    param(
        [string]$Version,
        [string]$InstallDir,
        [string]$ConfigPath,
        [bool]$NoAttestation,
        [bool]$NoAutoUpdate,
        [bool]$Migrate,
        [bool]$NoMigrate,
        [bool]$System
    )

    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest's progress bar makes Windows PowerShell's downloads
    # many times slower. Set in this function's scope only.
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'Loft-902-Co-LLC/open-ferry-ai-proxy'
    $target = 'x86_64-pc-windows-msvc'

    # --- The platform --------------------------------------------------------

    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) {
        throw "install.ps1 is for Windows. On Linux and macOS, use install.sh: curl -fsSL https://github.com/$repo/releases/latest/download/install.sh | sh"
    }
    # Refuse before downloading or creating any directory.
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    try {
        $principal = New-Object Security.Principal.WindowsPrincipal $identity
        $privileged = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) -or $identity.User.Value -eq 'S-1-5-18'
    } finally {
        $identity.Dispose()
    }
    # Only the fake-release suite's existing loopback and recording seam
    # may pretend to be elevated; a normal run uses the actual token.
    if ($env:OPEN_FERRY_INSTALL_BASE_URL -like 'http://127.0.0.1:*' -and $env:FAKE_OPEN_FERRY_INSTALL_LOG) {
        switch ($env:OPEN_FERRY_INSTALL_TEST_PRIVILEGED) {
            '0' { $privileged = $false }
            '1' { $privileged = $true }
        }
    }
    if ($System -and -not $privileged) {
        throw 'installing open-ferry for the whole machine needs an administrator. Run it in a terminal opened as administrator.'
    }
    if (-not $System -and $privileged) {
        throw 'an account install must not run as administrator. Run it in a terminal not opened as administrator, or install open-ferry for the whole machine with `install.ps1 -System`.'
    }
    # A 32-bit PowerShell on 64-bit Windows sees x86 here and the machine's
    # architecture in PROCESSOR_ARCHITEW6432.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    $note = $null
    switch ($arch) {
        'AMD64' { }
        'ARM64' { $note = "There's no Windows build for ARM64 processors yet: this is the x86_64 build, which Windows 11 runs under emulation." }
        default { throw "There's no open-ferry build for $arch processors. Build it from source: https://github.com/$repo#build-from-source" }
    }

    # --- Options -------------------------------------------------------------

    $selfUpdate = $env:OPEN_FERRY_INSTALL_SELF_UPDATE
    if ($NoAutoUpdate) { $selfUpdate = 'off' }
    if ($selfUpdate -and $selfUpdate -cnotin 'off', 'notify', 'auto') {
        throw "OPEN_FERRY_INSTALL_SELF_UPDATE is '$selfUpdate': use off, notify or auto"
    }
    if (-not $InstallDir) { $InstallDir = $env:OPEN_FERRY_INSTALL_DIR }
    if (-not $InstallDir) {
        if ($System) {
            if (-not $env:ProgramFiles) { throw 'ProgramFiles is not set: pass -InstallDir' }
            $InstallDir = Join-Path $env:ProgramFiles 'open-ferry'
        } else {
            if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set: pass -InstallDir' }
            $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\open-ferry'
        }
    }
    if (-not $ConfigPath) { $ConfigPath = $env:OPEN_FERRY_INSTALL_CONFIG }
    if (-not $ConfigPath) {
        # Where open-ferry init writes by default.
        if ($System) {
            $ConfigPath = Join-Path $InstallDir 'config.yaml'
        } else {
            if (-not $env:APPDATA) { throw 'APPDATA is not set: pass -ConfigPath' }
            $ConfigPath = Join-Path $env:APPDATA 'open-ferry\config.yaml'
        }
    }
    # Absolute, against PowerShell's current location, as they're printed in
    # commands to run from anywhere.
    $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
    $ConfigPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ConfigPath)

    # yes: switch from CLIProxyAPI without asking; no: don't look for it;
    # ask: ask, when someone can be asked.
    if ($Migrate -and $NoMigrate) { throw '-Migrate and -NoMigrate don''t go together' }
    $migrateWhy = '-Migrate'
    if ($Migrate) {
        $migrateMode = 'yes'
    } elseif ($NoMigrate) {
        $migrateMode = 'no'
    } else {
        switch ("$env:OPEN_FERRY_INSTALL_MIGRATE") {
            '' { $migrateMode = 'ask' }
            { $_ -in '1', 'yes', 'true' } {
                $migrateMode = 'yes'
                $migrateWhy = "OPEN_FERRY_INSTALL_MIGRATE=$env:OPEN_FERRY_INSTALL_MIGRATE"
            }
            { $_ -in '0', 'no', 'false' } { $migrateMode = 'no' }
            default { throw "OPEN_FERRY_INSTALL_MIGRATE is yes or no, not $env:OPEN_FERRY_INSTALL_MIGRATE" }
        }
    }

    if ($System) { $migrateMode = 'no' }

    # MAJOR.MINOR.PATCH with an optional pre-release part, as the release
    # workflow requires; a leading v is allowed.
    $versionPattern = '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'
    if ($Version) {
        if ($Version.StartsWith('v')) { $Version = $Version.Substring(1) }
        if ($Version -notmatch $versionPattern) { throw "Not a version: $Version (such as 0.1.0)" }
    }

    $baseUrl = $env:OPEN_FERRY_INSTALL_BASE_URL
    if (-not $baseUrl) { $baseUrl = "https://github.com/$repo/releases" }
    $baseUrl = $baseUrl.TrimEnd('/')

    # Windows PowerShell 5.1 may not offer TLS 1.2 by default; when it leaves
    # the choice to Windows (0, SystemDefault), it does.
    if ($PSVersionTable.PSEdition -ne 'Core') {
        $protocols = [Net.ServicePointManager]::SecurityProtocol
        if ([int]$protocols -ne 0 -and ($protocols -band [Net.SecurityProtocolType]::Tls12) -eq 0) {
            [Net.ServicePointManager]::SecurityProtocol = $protocols -bor [Net.SecurityProtocolType]::Tls12
        }
    }

    # --- Downloads -----------------------------------------------------------

    function Save-Download([string]$Url, [string]$OutFile) {
        try {
            $response = Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -PassThru
        } catch {
            throw "Couldn't download ${Url}: $($_.Exception.Message)"
        }
        # Never take a download that a redirect moved off HTTPS.
        if ($Url.StartsWith('https://')) {
            $final = $null
            $base = $response.BaseResponse
            if ($null -ne $base) {
                if ($base.PSObject.Properties['ResponseUri']) {
                    $final = $base.ResponseUri
                } elseif ($base.PSObject.Properties['RequestMessage'] -and $null -ne $base.RequestMessage) {
                    $final = $base.RequestMessage.RequestUri
                }
            }
            if ($null -ne $final -and $final.Scheme -ne 'https') {
                throw "The download of $Url was redirected off HTTPS, to $final"
            }
        }
    }

    # One argument for a Windows command line, quoted so it reads back as it
    # is: backslashes before a quote are doubled, and so is a trailing run.
    function ConvertTo-CommandLineArgument([string]$Value) {
        $escaped = [Text.RegularExpressions.Regex]::Replace($Value, '(\\*)"', '$1$1\"')
        $escaped = [Text.RegularExpressions.Regex]::Replace($escaped, '(\\+)$', '$1$1')
        return '"' + $escaped + '"'
    }

    # Runs a native command with its output on the console, and returns its
    # exit code. Its standard error isn't taken for a PowerShell error, as
    # Windows PowerShell may when its own output is redirected.
    function Invoke-Native([scriptblock]$Command) {
        $ErrorActionPreference = 'Continue'
        & $Command | Out-Host
        return $LASTEXITCODE
    }

    # SHA256SUMS lines are "<hash>  <name>", or "<hash> *<name>" in binary
    # mode. Returns a name-to-hash table.
    function Read-Sums([string]$Path) {
        $sums = @{}
        foreach ($line in Get-Content -LiteralPath $Path) {
            if ($line -match '^([0-9A-Fa-f]{64}) [ *](.+)$') {
                $sums[$Matches[2]] = $Matches[1].ToLowerInvariant()
            }
        }
        return $sums
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('open-ferry-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $sumsPath = Join-Path $tmp 'SHA256SUMS'
        $suffix = "-$target.zip"
        if (-not $Version) {
            Write-Host 'Finding the latest release...'
            Save-Download "$baseUrl/latest/download/SHA256SUMS" $sumsPath
            $sums = Read-Sums $sumsPath
            # The archive's name carries the version:
            # open-ferry-<version>-<target>.zip.
            foreach ($name in $sums.Keys) {
                if ($name.StartsWith('open-ferry-') -and $name.EndsWith($suffix) -and $name.Length -gt ('open-ferry-'.Length + $suffix.Length)) {
                    $Version = $name.Substring('open-ferry-'.Length, $name.Length - 'open-ferry-'.Length - $suffix.Length)
                    break
                }
            }
            if (-not $Version) { throw "The latest release has no archive for $target" }
            if ($Version -notmatch $versionPattern) { throw "The latest release's archive has an unexpected name: open-ferry-$Version$suffix" }
        } else {
            try {
                Save-Download "$baseUrl/download/v$Version/SHA256SUMS" $sumsPath
            } catch {
                throw "Couldn't download SHA256SUMS for open-ferry ${Version}: is there a release v${Version}? ($($_.Exception.Message))"
            }
            $sums = Read-Sums $sumsPath
        }
        $archive = "open-ferry-$Version$suffix"
        $expected = $sums[$archive]
        if (-not $expected) { throw "open-ferry $Version's SHA256SUMS lists no archive for $target" }

        Write-Host "Downloading open-ferry $Version for $target..."
        if ($note) { Write-Host $note }
        $archivePath = Join-Path $tmp $archive
        Save-Download "$baseUrl/download/v$Version/$archive" $archivePath

        $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "$archive doesn't match SHA256SUMS (expected $expected, got $actual): not installing it"
        }
        Write-Host "Checked $archive against SHA256SUMS."

        $gh = $env:OPEN_FERRY_INSTALL_GH
        if (-not $gh) { $gh = 'gh' }
        if ($NoAttestation) {
            Write-Host 'Not checking the build provenance attestation (-NoAttestation).'
        } elseif (Get-Command $gh -ErrorAction SilentlyContinue) {
            Write-Host 'Checking the build provenance attestation with gh...'
            if ((Invoke-Native { & $gh attestation verify $archivePath --repo $repo }) -ne 0) {
                throw "gh couldn't verify ${archive}'s attestation: not installing it. If gh isn't signed in, run 'gh auth login' and try again, or pass -NoAttestation to rely on the SHA256SUMS check alone."
            }
        } else {
            Write-Host "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed. The SHA256SUMS check passed."
        }

        # --- Install ---------------------------------------------------------

        $unpacked = Join-Path $tmp 'unpacked'
        Expand-Archive -LiteralPath $archivePath -DestinationPath $unpacked
        $source = Join-Path $unpacked "open-ferry-$Version-$target"
        if (-not (Test-Path -LiteralPath (Join-Path $source 'open-ferry.exe') -PathType Leaf)) {
            throw "$archive holds no open-ferry.exe"
        }

        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        $exe = Join-Path $InstallDir 'open-ferry.exe'
        # Staged under a name of its own, so two installs never share it.
        $staged = Join-Path $InstallDir ('.open-ferry.' + $PID + '.' + [Guid]::NewGuid().ToString('N') + '.new')
        # `install-self` is in every release (0.1.0 has it, and no release is
        # older), so a version this script can name has it: no fallback, no probe.
        # The staged file is removed whatever happens, unless the exit was 6,
        # when it may hold the only copy of the new binary.
        $status = $null
        $process = $null
        try {
            # Copied through streams, so the bytes are flushed to disk before the
            # staged binary runs: it is a running image from then on, and
            # nothing can open it for writing to flush it. Both files are closed
            # before it starts.
            $reader = $null
            $writer = $null
            try {
                $reader = New-Object System.IO.FileStream -ArgumentList @((Join-Path $source 'open-ferry.exe'), [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
                $writer = New-Object System.IO.FileStream -ArgumentList @($staged, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
                $reader.CopyTo($writer)
                $writer.Flush($true)
            } finally {
                # The writer's dispose can throw (a failed flush): the reader is
                # still disposed, or the download's cleanup can't remove it.
                try {
                    if ($null -ne $writer) { $writer.Dispose() }
                } finally {
                    if ($null -ne $reader) { $reader.Dispose() }
                }
            }
            # The staged binary moves itself into place: a running open-ferry.exe
            # can be neither replaced nor deleted, but it can be renamed, so it
            # moves the old one aside and deletes it once it has stopped.
            # ProcessStartInfo runs the file as it is named, which & does not for
            # an extension that isn't .exe.
            $info = New-Object System.Diagnostics.ProcessStartInfo
            $info.FileName = $staged
            $info.UseShellExecute = $false
            $installArgs = @('install-self', '-from', $staged, '-to', $exe, '-installer', 'install.ps1')
            if ($System) { $installArgs += '-system' }
            $info.Arguments = ($installArgs | ForEach-Object { ConvertTo-CommandLineArgument $_ }) -join ' '
            $process = [System.Diagnostics.Process]::Start($info)
            $process.WaitForExit()
            $status = $process.ExitCode
        } finally {
            if ($null -ne $process) { $process.Dispose() }
            if ($status -ne 6) {
                # The exited image may still be held briefly. Remove only
                # this install's staged file, with a bounded retry.
                for ($attempt = 0; $attempt -lt 20; $attempt++) {
                    try {
                        Remove-Item -LiteralPath $staged -Force -ErrorAction Stop
                        break
                    } catch {
                        if (-not (Test-Path -LiteralPath $staged)) { break }
                        if ($attempt -lt 19) { Start-Sleep -Milliseconds 100 }
                    }
                }
            }
        }
        if ($status -ne 0) {
            switch ($status) {
                3 { throw 'open-ferry runs as a background service: install-self said how to stop it, above. Nothing was changed.' }
                4 { throw "Couldn't tell whether open-ferry is running from ${exe}: see above. Nothing was changed." }
                5 { throw 'Gave up waiting for another open-ferry install or update to finish. Nothing was changed; run the installer again.' }
                6 { throw "The install didn't finish and open-ferry couldn't put things back: see above for the files to restore by hand." }
                default { throw "Couldn't install open-ferry in ${InstallDir}: see above." }
            }
        }
        foreach ($item in Get-ChildItem -LiteralPath $source) {
            if ($item.Name -ne 'open-ferry.exe') {
                Copy-Item -LiteralPath $item.FullName -Destination $InstallDir -Recurse -Force
            }
        }
        Write-Host "Installed open-ferry $Version as $exe."
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    # How to run open-ferry in the commands printed from here on.
    $trimmed = $InstallDir.TrimEnd('\')
    $onPath = $false
    foreach ($entry in ($env:Path -split ';')) {
        if ($entry -and $entry.TrimEnd('\') -ieq $trimmed) { $onPath = $true }
    }
    $command = 'open-ferry'
    if (-not $onPath) { $command = "& '" + $exe.Replace("'", "''") + "'" }
    $quotedConfig = "'" + $ConfigPath.Replace("'", "''") + "'"

    # --- CLIProxyAPI ---------------------------------------------------------

    # open-ferry's standard output for $Arguments, read as the UTF-8 it
    # writes, whatever the console's code page.
    function Get-NativeOutput([string]$Exe, [string[]]$Arguments) {
        $ErrorActionPreference = 'Continue'
        $saved = $null
        try {
            $saved = [Console]::OutputEncoding
            [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
        } catch { }
        try {
            $lines = & $Exe @Arguments 2>$null
            return (@($lines) -join "`n").Trim()
        } catch {
            return ''
        } finally {
            if ($null -ne $saved) {
                try { [Console]::OutputEncoding = $saved } catch { }
            }
        }
    }

    # Whether someone can be asked: not when PowerShell runs
    # -NonInteractive, where Read-Host fails.
    function Test-CanAsk {
        foreach ($arg in [Environment]::GetCommandLineArgs()) {
            if ($arg -match '^(-|/)+noni') { return $false }
        }
        return $true
    }

    # `open-ferry migrate -json` only looks, and changes nothing. It prints
    # one line of JSON: {"found":false} when there is no CLIProxyAPI.
    $cpa = $null
    $switched = $false
    $switchFailed = $null
    if ($System) {
        Write-Host 'Skipping the migrate offer for a machine install. To migrate CLIProxyAPI, run open-ferry migrate in a terminal opened as administrator.'
    }
    if ($migrateMode -ne 'no') {
        $search = Get-NativeOutput $exe @('migrate', '-json')
        $answer = $null
        if ($search) {
            try { $answer = $search | ConvertFrom-Json } catch { $answer = $null }
        }
        if ($null -ne $answer -and $answer.found -eq $true) {
            $cpa = $answer
        } elseif ($null -ne $answer -and $answer.found -eq $false -and -not $answer.error) {
            if ($migrateMode -eq 'yes') {
                Write-Host "CLIProxyAPI wasn't found, so there is nothing to switch ($migrateWhy)."
            }
        } else {
            $why = ''
            if ($null -ne $answer -and $answer.error) { $why = ": $($answer.error)" }
            Write-Host "open-ferry couldn't look for CLIProxyAPI$why."
            if ($migrateMode -eq 'yes') {
                $switchFailed = "$migrateWhy was given, but open-ferry couldn't look for CLIProxyAPI. To look again: $command migrate -dry-run"
            }
        }
    }
    if ($null -ne $cpa) {
        Write-Host ''
        Write-Host "Found $($cpa.summary)."
        $switchNow = $false
        if ($cpa.can_switch -eq $true) {
            Write-Host "open-ferry can take its place, on its config and credentials. To see how first, run: $command migrate -dry-run"
            if ($migrateMode -eq 'yes') {
                Write-Host "Switching to open-ferry ($migrateWhy)..."
                $switchNow = $true
            } else {
                $reply = $null
                if (Test-CanAsk) {
                    Write-Host -NoNewline 'Switch to open-ferry now? [y/N] '
                    try { $reply = Read-Host } catch { $reply = $null }
                    if ($null -eq $reply) { Write-Host '' }
                }
                if ($null -eq $reply) {
                    Write-Host "There is no one at a terminal to ask, so it isn't switched. To switch to open-ferry, run: $command migrate"
                } elseif ($reply.Trim() -in 'y', 'yes') {
                    $switchNow = $true
                } else {
                    Write-Host 'Not switching.'
                }
            }
        } else {
            Write-Host "open-ferry can't switch it as things are. To see why, and what to do, run: $command migrate -dry-run"
            if ($migrateMode -eq 'yes') {
                $switchFailed = "$migrateWhy was given, but CLIProxyAPI can't be switched as things are. To see why: $command migrate -dry-run"
            }
        }
        if ($switchNow) {
            Write-Host ''
            if ((Invoke-Native { & $exe migrate -yes }) -eq 0) {
                $switched = $true
            } else {
                $switchFailed = "The switch from CLIProxyAPI didn't finish: see what open-ferry migrate said above"
            }
        }
        Write-Host ''
    }

    # --- The config ----------------------------------------------------------

    $wroteConfig = $false
    $skippedInit = $false
    if (Test-Path -LiteralPath $ConfigPath) {
        Write-Host "Keeping your config at $ConfigPath."
    } elseif ($null -ne $cpa) {
        # A switch runs open-ferry on CLIProxyAPI's config: a starting
        # config here would compete with it, on the same port.
        $skippedInit = $true
        if ($switched) {
            Write-Host 'Not writing a starting config: open-ferry runs on CLIProxyAPI''s.'
        } else {
            Write-Host 'Not writing a starting config: switching keeps CLIProxyAPI''s.'
        }
    } else {
        $wroteConfig = $true
        Write-Host 'Writing a starting config with open-ferry init...'
        Write-Host ''
        $initArgs = @('init', '-config', $ConfigPath)
        if ($System) { $initArgs = @('init', '-system', '-config', $ConfigPath) }
        if ((Invoke-Native { & $exe @initArgs }) -ne 0) {
            throw "open-ferry init couldn't write $ConfigPath"
        }
        Write-Host ''
    }

    # The config the update mode goes in: CLIProxyAPI's once open-ferry runs
    # on it, else open-ferry's own, unless there is none yet. The server's
    # first update check comes minutes after it starts, and it follows a
    # change to its config at once.
    $modeConfig = $ConfigPath
    if ($switched) {
        $modeConfig = [string]$cpa.config
    } elseif ($skippedInit) {
        $modeConfig = $null
    }
    if ($selfUpdate) {
        if ($modeConfig) {
            if ((Invoke-Native { & $exe update -mode $selfUpdate -config $modeConfig }) -ne 0) {
                throw "open-ferry update couldn't set self-update.mode to $selfUpdate in $modeConfig"
            }
        } else {
            Write-Host "There's no config yet to set self-update.mode to $selfUpdate in. Once open-ferry runs on one, run: $command update -mode $selfUpdate -config <config>"
        }
    }

    # --- Next steps ----------------------------------------------------------

    Write-Host "open-ferry $Version is installed."
    if (-not $onPath) {
        $quotedDir = "'" + $trimmed.Replace("'", "''") + "'"
        Write-Host ''
        Write-Host "$trimmed isn't on your PATH. To run open-ferry by name, add it to your user PATH with this PowerShell command, then open a new terminal:"
        Write-Host ''
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $quotedDir, 'User')"
    }
    Write-Host ''
    Write-Host 'Next steps:'
    if ($switched) {
        if ($cpa.config) {
            Write-Host "  Check the setup:     $command check -config '$(([string]$cpa.config).Replace("'", "''"))'"
        }
        if ($null -ne $cpa.listen -and $cpa.listen.port) {
            $scheme = 'http'
            if ($cpa.listen.tls -eq $true) { $scheme = 'https' }
            $hostName = [string]$cpa.listen.host
            if ($hostName -in '', '0.0.0.0', '::') {
                $hostName = '127.0.0.1'
            } elseif ($hostName.Contains(':')) {
                $hostName = "[$hostName]"
            }
            Write-Host "  Open the dashboard:  ${scheme}://${hostName}:$($cpa.listen.port)/dashboard/ and sign in with CLIProxyAPI's management key"
        }
        Write-Host "  Switch back:         $command migrate -undo"
    } else {
        if ($null -ne $cpa) {
            Write-Host "  Switch over:         $command migrate"
        }
        if (-not $skippedInit) {
            Write-Host "  Start it:            $command -config $quotedConfig"
            if ($System) {
                Write-Host "  Run it at boot:      $command service install -system -config $quotedConfig"
            } else {
                Write-Host "  Or run it at login:  $command service install -config $quotedConfig"
            }
            if ($wroteConfig) {
                Write-Host '  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above'
            } else {
                Write-Host "  Open the dashboard:  http://127.0.0.1:<port>/dashboard/, with your config's port (8317 by default)"
            }
            Write-Host "  Check the setup:     $command check -config $quotedConfig"
        } else {
            Write-Host "  Or start afresh:     $command init -config $quotedConfig"
        }
    }
    if ($modeConfig) {
        $quotedMode = "'" + $modeConfig.Replace("'", "''") + "'"
        Write-Host ''
        $off = "To turn that off: $command update -mode off -config $quotedMode"
        if ($selfUpdate -eq 'off') {
            Write-Host "Automatic updates are off. To turn them on: $command update -mode auto -config $quotedMode"
        } elseif ($switched -and $cpa.switch -eq 'drop-in') {
            # A drop-in is a copy of open-ferry, and the updater only updates
            # the installed binary.
            Write-Host "The copy of open-ferry in CLIProxyAPI's place says when a release is out, but doesn't install it. To update it, run $command update, then $command migrate -undo, then $command migrate. $off"
        } elseif ($selfUpdate -eq 'notify') {
            Write-Host "open-ferry says when a release is out, but doesn't install it. $off"
        } elseif ($selfUpdate -or $wroteConfig) {
            Write-Host "open-ferry keeps itself up to date. $off"
        } else {
            Write-Host "open-ferry keeps itself up to date, unless your config says otherwise. $off"
        }
    }
    if ($switchFailed) { throw $switchFailed }
}

try {
    if (-not $PSBoundParameters.ContainsKey('System')) {
        switch ("$env:OPEN_FERRY_INSTALL_SYSTEM") {
            { $_ -in '', '0', 'no', 'false' } { $System = $false }
            { $_ -in '1', 'yes', 'true' } { $System = $true }
            default { throw "OPEN_FERRY_INSTALL_SYSTEM is yes or no, not $env:OPEN_FERRY_INSTALL_SYSTEM" }
        }
    }
    Install-OpenFerry -Version $Version -InstallDir $InstallDir -ConfigPath $ConfigPath -NoAttestation $NoAttestation.IsPresent -NoAutoUpdate $NoAutoUpdate.IsPresent -Migrate $Migrate.IsPresent -NoMigrate $NoMigrate.IsPresent -System $System.IsPresent
} catch {
    Write-Host "install.ps1: error: $($_.Exception.Message)" -ForegroundColor Red
    # Run as a file, the script exits with 1. Run from irm, it only returns,
    # as exit would close the caller's PowerShell, or end the caller's
    # script. Only code run from a file has a script name.
    if ((Get-PSCallStack)[0].ScriptName) { exit 1 }
}
