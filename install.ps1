# Installs open-ferry on Windows from a GitHub release.
#
#   irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1 | iex
#
# It downloads the release's Windows archive and checks it against the
# release's SHA256SUMS, and, when the GitHub CLI (gh) is installed, against
# the archive's build provenance attestation. It installs open-ferry.exe in
# %LOCALAPPDATA%\Programs\open-ferry, and, when there is no config at
# %APPDATA%\open-ferry\config.yaml, writes one with `open-ferry init`. It
# changes neither PATH nor any profile.
#
# Runs in Windows PowerShell 5.1 and PowerShell 7. To pass options through
# irm, make the script a script block:
#
#   & ([scriptblock]::Create((irm https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/releases/latest/download/install.ps1))) -Version 0.1.0
#
# Options:
#   -Version VERSION   install this version (such as 0.1.0) rather than the
#                      latest release
#   -InstallDir DIR    install in DIR (default:
#                      %LOCALAPPDATA%\Programs\open-ferry)
#   -ConfigPath PATH   the config to keep, or to write when there is none
#                      (default: %APPDATA%\open-ferry\config.yaml)
#   -NoAttestation     don't check the build provenance attestation, even
#                      when gh is installed; the SHA256SUMS check still runs
#
# Environment:
#   OPEN_FERRY_INSTALL_DIR       the install directory, when -InstallDir
#                                isn't given
#   OPEN_FERRY_INSTALL_CONFIG    the config path, when -ConfigPath isn't
#                                given
#   OPEN_FERRY_INSTALL_BASE_URL  where releases are downloaded from, in
#                                place of the GitHub repository's releases
#                                URL (for a mirror, or a test server)
#   OPEN_FERRY_INSTALL_GH        the GitHub CLI command (default: gh)
#
# See https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy#install

param(
    [string]$Version,
    [string]$InstallDir,
    [string]$ConfigPath,
    [switch]$NoAttestation
)

# Everything runs in this function, which ends by returning or throwing,
# never with exit: under `irm | iex` the script runs in the caller's
# session, which exit would close.
function Install-OpenFerry {
    param(
        [string]$Version,
        [string]$InstallDir,
        [string]$ConfigPath,
        [bool]$NoAttestation
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

    if (-not $InstallDir) { $InstallDir = $env:OPEN_FERRY_INSTALL_DIR }
    if (-not $InstallDir) {
        if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set: pass -InstallDir' }
        $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\open-ferry'
    }
    if (-not $ConfigPath) { $ConfigPath = $env:OPEN_FERRY_INSTALL_CONFIG }
    if (-not $ConfigPath) {
        # Where open-ferry init writes by default.
        if (-not $env:APPDATA) { throw 'APPDATA is not set: pass -ConfigPath' }
        $ConfigPath = Join-Path $env:APPDATA 'open-ferry\config.yaml'
    }
    # Absolute, against PowerShell's current location, as they're printed in
    # commands to run from anywhere.
    $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
    $ConfigPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ConfigPath)

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
        $staged = Join-Path $InstallDir '.open-ferry.exe.new'
        Copy-Item -LiteralPath (Join-Path $source 'open-ferry.exe') -Destination $staged -Force
        # A running open-ferry.exe can be neither replaced nor deleted, but it
        # can be renamed: the old one moves aside, and is deleted once it has
        # stopped, now or at a later install.
        if (Test-Path -LiteralPath $exe) {
            $aside = Join-Path $InstallDir ('.open-ferry.exe.' + [Guid]::NewGuid().ToString('N') + '.old')
            Move-Item -LiteralPath $exe -Destination $aside
        }
        Move-Item -LiteralPath $staged -Destination $exe
        $stillRunning = $false
        foreach ($old in Get-ChildItem -LiteralPath $InstallDir -Filter '.open-ferry.exe.*.old' -Force) {
            Remove-Item -LiteralPath $old.FullName -Force -ErrorAction SilentlyContinue
            if (Test-Path -LiteralPath $old.FullName) { $stillRunning = $true }
        }
        foreach ($item in Get-ChildItem -LiteralPath $source) {
            if ($item.Name -ne 'open-ferry.exe') {
                Copy-Item -LiteralPath $item.FullName -Destination $InstallDir -Recurse -Force
            }
        }
        Write-Host "Installed open-ferry $Version as $exe."
        if ($stillRunning) {
            Write-Host 'An older open-ferry.exe is still running: restart it to run this one.'
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }

    $wroteConfig = $false
    if (Test-Path -LiteralPath $ConfigPath) {
        Write-Host "Keeping your config at $ConfigPath."
    } else {
        $wroteConfig = $true
        Write-Host 'Writing a starting config with open-ferry init...'
        Write-Host ''
        if ((Invoke-Native { & $exe init -config $ConfigPath }) -ne 0) {
            throw "open-ferry init couldn't write $ConfigPath"
        }
        Write-Host ''
    }

    # --- Next steps ----------------------------------------------------------

    $trimmed = $InstallDir.TrimEnd('\')
    $onPath = $false
    foreach ($entry in ($env:Path -split ';')) {
        if ($entry -and $entry.TrimEnd('\') -ieq $trimmed) { $onPath = $true }
    }
    $command = 'open-ferry'
    if (-not $onPath) { $command = "& '" + $exe.Replace("'", "''") + "'" }
    $quotedConfig = "'" + $ConfigPath.Replace("'", "''") + "'"

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
    Write-Host "  Start it:            $command -config $quotedConfig"
    Write-Host "  Or run it at login:  $command service install -config $quotedConfig"
    if ($wroteConfig) {
        Write-Host '  Open the dashboard:  http://127.0.0.1:8317/dashboard/ and sign in with the management key above'
    } else {
        Write-Host "  Open the dashboard:  http://127.0.0.1:<port>/dashboard/, with your config's port (8317 by default)"
    }
    Write-Host "  Check the setup:     $command check -config $quotedConfig"
}

try {
    Install-OpenFerry -Version $Version -InstallDir $InstallDir -ConfigPath $ConfigPath -NoAttestation $NoAttestation.IsPresent
} catch {
    Write-Host "install.ps1: error: $($_.Exception.Message)" -ForegroundColor Red
    # Run as a file, the script exits with 1. Run from irm, it only returns,
    # as exit would close the caller's PowerShell, or end the caller's
    # script. Only code run from a file has a script name.
    if ((Get-PSCallStack)[0].ScriptName) { exit 1 }
}
