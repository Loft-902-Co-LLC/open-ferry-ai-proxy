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
# With -System, in an administrator terminal, the binary goes to
# C:\Program Files\open-ferry and the config to C:\ProgramData\open-ferry\config.yaml.
# init -system uses the protected machine data and auth directory.
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
#                      with -System: C:\ProgramData\open-ferry\config.yaml)
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
#                                loopback base URL and FAKE_OPEN_FERRY_INSTALL_LOG;
#                                ignored in an elevated terminal
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
# Under an elevated -System run, BASE_URL, GH, DIR and CONFIG above are
# ignored. Use -InstallDir and -ConfigPath to choose machine paths; downloads
# always come from GitHub and staging stays inside the install directory.
# Real elevation also ignores every FAKE_* setting and refuses Test* parameters.
# Before downloading it checks ownership, write ACLs and reparse points on
# the install directory and its ancestors, and creates protected staging.
# External tools use full paths in trusted machine directories. Real elevation
# disables module autoloading and imports only shipped PSHOME modules; zip and
# SHA256 use .NET. Start with -NoProfile: an already-run profile cannot be undone.
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
    [switch]$System,
    # The fake-release suite passes these explicitly, never through HKCU's
    # inherited environment. They require its loopback server and recorder.
    [string]$TestBaseUrl,
    [ValidateSet('', '0', '1')][string]$TestPrivilege,
    [switch]$TestElevated,
    [string]$TestProgramFiles,
    [string]$TestProgramData,
    [string]$TestGh
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
        [bool]$System,
        [string]$TestBaseUrl,
        [string]$TestPrivilege,
        [bool]$TestElevated,
        [string]$TestProgramFiles,
        [string]$TestProgramData,
        [string]$TestGh
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
    # Emit the small native wrapper in memory: Add-Type can invoke a compiler
    # using the account's TEMP on PowerShell 5.1, even in an elevated terminal.
    if (-not ('OpenFerryInstallerNativeV2' -as [type])) {
        $name = [Reflection.AssemblyName]::new('OpenFerryInstallerNativeV2')
        if ($PSVersionTable.PSEdition -eq 'Core') {
            $assembly = [Reflection.Emit.AssemblyBuilder]::DefineDynamicAssembly($name, 'Run')
        } else {
            $assembly = [AppDomain]::CurrentDomain.DefineDynamicAssembly($name, 'Run')
        }
        $module = $assembly.DefineDynamicModule($name.Name)
        $type = $module.DefineType($name.Name, 'Public, Abstract, Sealed')
        $methods = @(
            @('GetTokenInformation', 'advapi32.dll', [type[]]@([IntPtr], [uint32], [IntPtr], [uint32], [IntPtr])),
            @('SetTokenInformation', 'advapi32.dll', [type[]]@([IntPtr], [uint32], [IntPtr], [uint32])),
            @('CreateDirectoryW', 'kernel32.dll', [type[]]@([string], [IntPtr])),
            @('OpenProcessToken', 'advapi32.dll', [type[]]@([IntPtr], [uint32], [IntPtr])),
            @('CloseHandle', 'kernel32.dll', [type[]]@([IntPtr]))
        )
        foreach ($method in $methods) {
            $native = $type.DefineMethod($method[0], 'Public, Static, PinvokeImpl', [bool], $method[2])
            $native.SetImplementationFlags('PreserveSig')
            $dllImport = [Runtime.InteropServices.DllImportAttribute]
            $attribute = [Reflection.Emit.CustomAttributeBuilder]::new(
                $dllImport.GetConstructor([type[]]@([string])), [object[]]@($method[1]),
                [Reflection.FieldInfo[]]@($dllImport.GetField('SetLastError'), $dllImport.GetField('CharSet'),
                    $dllImport.GetField('CallingConvention'), $dllImport.GetField('ExactSpelling')),
                [object[]]@($true, [Runtime.InteropServices.CharSet]::Unicode,
                    [Runtime.InteropServices.CallingConvention]::Winapi, $true))
            $native.SetCustomAttribute($attribute)
        }
        $null = $type.CreateType()
    }
    # Same rule as scope::Host: TokenElevation, or the SYSTEM SID. A failure
    # to read the token refuses before downloads or directory creation.
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $testSid = $identity.User.Value
    $elevation = [Runtime.InteropServices.Marshal]::AllocHGlobal(4)
    $size = [Runtime.InteropServices.Marshal]::AllocHGlobal(4)
    $tokenOut = [Runtime.InteropServices.Marshal]::AllocHGlobal([IntPtr]::Size)
    [Runtime.InteropServices.Marshal]::WriteIntPtr($tokenOut, [IntPtr]::Zero)
    try {
        # -1 is GetCurrentProcess's pseudo-handle; TOKEN_QUERY is 8.
        if (-not [OpenFerryInstallerNativeV2]::OpenProcessToken([IntPtr](-1), 8, $tokenOut)) {
            throw 'Could not open this process''s token.'
        }
        $token = [Runtime.InteropServices.Marshal]::ReadIntPtr($tokenOut)
        if (-not [OpenFerryInstallerNativeV2]::GetTokenInformation($token, 20, $elevation, 4, $size)) {
            throw 'Could not read whether this process is elevated.'
        }
        $reallyPrivileged = [Runtime.InteropServices.Marshal]::ReadInt32($elevation) -ne 0 -or $identity.User.Value -eq 'S-1-5-18'
    } finally {
        $token = [Runtime.InteropServices.Marshal]::ReadIntPtr($tokenOut)
        if ($token -ne [IntPtr]::Zero) { $null = [OpenFerryInstallerNativeV2]::CloseHandle($token) }
        [Runtime.InteropServices.Marshal]::FreeHGlobal($tokenOut)
        [Runtime.InteropServices.Marshal]::FreeHGlobal($size)
        [Runtime.InteropServices.Marshal]::FreeHGlobal($elevation)
        $identity.Dispose()
    }
    $useTrustedModules = {
        # PSHOME belongs to the running PowerShell installation. Import only
        # its shipped modules by full path; never search the caller's folders.
        $systemModules = [IO.Path]::Combine([Environment]::SystemDirectory, 'WindowsPowerShell', 'v1.0', 'Modules')
        $env:PSModulePath = [IO.Path]::Combine($PSHOME, 'Modules') + ';' + $systemModules
        $PSModuleAutoLoadingPreference = 'None'
        foreach ($moduleName in @('Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Security')) {
            Import-Module -Name ([IO.Path]::Combine($PSHOME, 'Modules', $moduleName, "$moduleName.psd1")) -Force -ErrorAction Stop
        }
        if ($System) {
            Write-Host 'For a machine install, start powershell or pwsh with -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -System. A profile that already ran cannot be undone.'
        }
    }
    if ($reallyPrivileged) { . $useTrustedModules }
    $privileged = $reallyPrivileged
    $testMode = $false
    if ($TestBaseUrl -or $TestPrivilege -or $TestElevated -or $TestProgramFiles -or $TestProgramData -or $TestGh) {
        if ($reallyPrivileged) {
            throw 'Installer test parameters cannot run in a genuinely elevated terminal. Run the tests unelevated.'
        }
        $testUrl = $null
        if (-not [Uri]::TryCreate($TestBaseUrl, [UriKind]::Absolute, [ref]$testUrl) -or
            $testUrl.Scheme -ne 'http' -or $testUrl.Host -ne '127.0.0.1' -or $testUrl.UserInfo -or
            -not $env:FAKE_OPEN_FERRY_INSTALL_LOG) {
            throw 'Installer test parameters need an http://127.0.0.1 loopback base URL and FAKE_OPEN_FERRY_INSTALL_LOG.'
        }
        $testMode = $true
        if ($TestPrivilege) { $privileged = $TestPrivilege -eq '1' }
    } elseif (-not $reallyPrivileged -and $env:OPEN_FERRY_INSTALL_BASE_URL -like 'http://127.0.0.1:*' -and $env:FAKE_OPEN_FERRY_INSTALL_LOG) {
        $testMode = $true
        $TestBaseUrl = $env:OPEN_FERRY_INSTALL_BASE_URL
        switch ($env:OPEN_FERRY_INSTALL_TEST_PRIVILEGED) {
            '0' { $privileged = $false }
            '1' { $privileged = $true }
        }
    }
    $hardened = $System -and ($reallyPrivileged -or $TestElevated)
    if ($hardened -and -not $reallyPrivileged) { . $useTrustedModules }
    if ($reallyPrivileged) {
        foreach ($name in [Environment]::GetEnvironmentVariables('Process').Keys) {
            if ($name -like 'FAKE_*') {
                Write-Host "Ignoring $name in an elevated run."
                [Environment]::SetEnvironmentVariable($name, $null, 'Process')
            }
        }
    }
    if ($hardened) {
        foreach ($option in @(
            @('OPEN_FERRY_INSTALL_BASE_URL', '-Version to choose an official GitHub release'),
            @('OPEN_FERRY_INSTALL_GH', '-NoAttestation to skip the optional check'),
            @('OPEN_FERRY_INSTALL_DIR', '-InstallDir'),
            @('OPEN_FERRY_INSTALL_CONFIG', '-ConfigPath'),
            @('OPEN_FERRY_INSTALL_TEST_PRIVILEGED', 'explicit installer test parameters')
        )) {
            if ([Environment]::GetEnvironmentVariable($option[0])) {
                Write-Host "Ignoring $($option[0]) in an elevated -System run; use $($option[1]) instead."
            }
        }
    }
    if ($System -and -not $privileged) {
        throw 'installing open-ferry for the whole machine needs an administrator. Run it in a terminal opened as administrator.'
    }
    if (-not $System -and $privileged) {
        throw 'an account install must not run as administrator. Run it in a terminal not opened as administrator, or install open-ferry for the whole machine with `install.ps1 -System`.'
    }
    function Get-OwnerTokenBuffer([IntPtr]$Token, [uint32]$Kind) {
        $size = [Runtime.InteropServices.Marshal]::AllocHGlobal(4)
        $buffer = [IntPtr]::Zero
        try {
            [Runtime.InteropServices.Marshal]::WriteInt32($size, 0)
            $null = [OpenFerryInstallerNativeV2]::GetTokenInformation($Token, $Kind, [IntPtr]::Zero, 0, $size)
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            $length = [Runtime.InteropServices.Marshal]::ReadInt32($size)
            if ($errorCode -ne 122 -or $length -le 0) {
                throw "Could not size owner-setup token information (Win32 error $errorCode); refusing the machine install."
            }
            $buffer = [Runtime.InteropServices.Marshal]::AllocHGlobal($length)
            if (-not [OpenFerryInstallerNativeV2]::GetTokenInformation($Token, $Kind, $buffer, $length, $size)) {
                throw "Could not read owner-setup token information (Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error())); refusing the machine install."
            }
            $result = $buffer
            $buffer = [IntPtr]::Zero
            return $result
        } finally {
            if ($buffer -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($buffer) }
            [Runtime.InteropServices.Marshal]::FreeHGlobal($size)
        }
    }
    function Test-AdminOwnerEligible([IntPtr]$Groups) {
        # TOKEN_GROUPS starts with a DWORD, then pointer-aligned SID_AND_ATTRIBUTES.
        $count = [Runtime.InteropServices.Marshal]::ReadInt32($Groups)
        $stride = 2 * [IntPtr]::Size
        for ($index = 0; $index -lt $count; $index++) {
            $entry = [IntPtr]::Add($Groups, [IntPtr]::Size + $index * $stride)
            $sid = [Security.Principal.SecurityIdentifier]::new([Runtime.InteropServices.Marshal]::ReadIntPtr($entry))
            $attributes = [Runtime.InteropServices.Marshal]::ReadInt32($entry, [IntPtr]::Size)
            # SE_GROUP_ENABLED (4) and SE_GROUP_OWNER (8) must both be present.
            if ($sid.Value -eq 'S-1-5-32-544' -and ($attributes -band 12) -eq 12) { return $true }
        }
        return $false
    }
    function Initialize-AdminOwner {
        # Elevation alone does not imply an owner-eligible Administrators group.
        # Keep the query handle read-only until the group check succeeds.
        $token = [IntPtr]::Zero
        $groups = [IntPtr]::Zero
        $originalOwner = [IntPtr]::Zero
        $sid = [IntPtr]::Zero
        $owner = [IntPtr]::Zero
        $keepState = $false
        $tokenOut = [Runtime.InteropServices.Marshal]::AllocHGlobal([IntPtr]::Size)
        [Runtime.InteropServices.Marshal]::WriteIntPtr($tokenOut, [IntPtr]::Zero)
        try {
            if (-not [OpenFerryInstallerNativeV2]::OpenProcessToken([IntPtr](-1), 8, $tokenOut)) {
                throw "Could not open the owner-setup process token (Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error())); refusing the machine install."
            }
            $token = [Runtime.InteropServices.Marshal]::ReadIntPtr($tokenOut)
            $groups = Get-OwnerTokenBuffer $token 2 # TokenGroups
            if (-not (Test-AdminOwnerEligible $groups)) { return $null }
            $null = [OpenFerryInstallerNativeV2]::CloseHandle($token)
            $token = [IntPtr]::Zero
            # TOKEN_QUERY | TOKEN_ADJUST_DEFAULT; TokenOwner is 4.
            if (-not [OpenFerryInstallerNativeV2]::OpenProcessToken([IntPtr](-1), 0x88, $tokenOut)) {
                throw "Could not open the elevated process token to set its default owner (Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error())); refusing the machine install."
            }
            $token = [Runtime.InteropServices.Marshal]::ReadIntPtr($tokenOut)
            # This buffer owns the original SID too; keep it live until restoration.
            $originalOwner = Get-OwnerTokenBuffer $token 4
            $admin = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
            $bytes = [byte[]]::new($admin.BinaryLength)
            $admin.GetBinaryForm($bytes, 0)
            $sid = [Runtime.InteropServices.Marshal]::AllocHGlobal($bytes.Length)
            $owner = [Runtime.InteropServices.Marshal]::AllocHGlobal([IntPtr]::Size)
            [Runtime.InteropServices.Marshal]::Copy($bytes, 0, $sid, $bytes.Length)
            [Runtime.InteropServices.Marshal]::WriteIntPtr($owner, $sid)
            $state = @{ Token = $token; OwnerBuffer = $originalOwner }
            if (-not [OpenFerryInstallerNativeV2]::SetTokenInformation($token, 4, $owner, [IntPtr]::Size)) {
                throw "Could not set the elevated process default owner to Administrators (Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error())); refusing the machine install."
            }
            $keepState = $true
            return $state
        } finally {
            if (-not $keepState) {
                if ($token -ne [IntPtr]::Zero) { $null = [OpenFerryInstallerNativeV2]::CloseHandle($token) }
                if ($originalOwner -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($originalOwner) }
            }
            [Runtime.InteropServices.Marshal]::FreeHGlobal($tokenOut)
            if ($groups -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($groups) }
            if ($owner -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($owner) }
            if ($sid -ne [IntPtr]::Zero) { [Runtime.InteropServices.Marshal]::FreeHGlobal($sid) }
        }
    }
    function Restore-AdminOwner($State) {
        try {
            if (-not [OpenFerryInstallerNativeV2]::SetTokenInformation($State.Token, 4, $State.OwnerBuffer, [IntPtr]::Size)) {
                throw "Could not restore the process default owner (Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error()))."
            }
        } finally {
            $null = [OpenFerryInstallerNativeV2]::CloseHandle($State.Token)
            [Runtime.InteropServices.Marshal]::FreeHGlobal($State.OwnerBuffer)
        }
    }
    $ownerState = $null
    # Keep the changed owner for all staging and children, even install-self/init.
    try {
    if ($hardened -and $reallyPrivileged -and $testSid -ne 'S-1-5-18') {
        $ownerState = Initialize-AdminOwner
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
    $programFiles = [Environment]::GetFolderPath('ProgramFiles')
    if ([Environment]::Is64BitOperatingSystem -and -not [Environment]::Is64BitProcess) {
        $machine = [Microsoft.Win32.RegistryKey]::OpenBaseKey('LocalMachine', 'Registry64')
        try {
            $currentVersion = $machine.OpenSubKey('SOFTWARE\Microsoft\Windows\CurrentVersion')
            try { $programFiles = [string]$currentVersion.GetValue('ProgramFilesDir') }
            finally { if ($currentVersion) { $currentVersion.Dispose() } }
        } finally { $machine.Dispose() }
    }
    $programData = [Environment]::GetFolderPath('CommonApplicationData')
    if ($testMode -and $TestProgramFiles) { $programFiles = $TestProgramFiles }
    if ($testMode -and $TestProgramData) { $programData = $TestProgramData }
    if (-not $InstallDir -and -not $hardened) { $InstallDir = $env:OPEN_FERRY_INSTALL_DIR }
    if (-not $InstallDir) {
        if ($System) {
            if (-not $programFiles) { throw 'Could not find Program Files: pass -InstallDir' }
            $InstallDir = Join-Path $programFiles 'open-ferry'
        } else {
            if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set: pass -InstallDir' }
            $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\open-ferry'
        }
    }
    if (-not $ConfigPath -and -not $hardened) { $ConfigPath = $env:OPEN_FERRY_INSTALL_CONFIG }
    if (-not $ConfigPath) {
        # Where open-ferry init writes by default.
        if ($System) {
            if (-not $programData) { throw 'Could not find ProgramData: pass -ConfigPath' }
            $ConfigPath = Join-Path $programData 'open-ferry\config.yaml'
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

    $baseUrl = $null
    if (-not $hardened) { $baseUrl = $env:OPEN_FERRY_INSTALL_BASE_URL }
    if ($testMode) { $baseUrl = $TestBaseUrl }
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
    function Invoke-Native([scriptblock]$Command, [string]$Directory = $InstallDir) {
        $ErrorActionPreference = 'Continue'
        Push-Location -LiteralPath $Directory -ErrorAction Stop
        try {
            & $Command | Out-Host
            return $LASTEXITCODE
        } finally { Pop-Location }
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

    # The same owner, ACE kinds and write masks as trust/windows.rs. Ancestors
    # may allow adding unrelated children, but never replacing existing ones.
    $trustedSids = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
    if ($testMode) { $trustedSids += $testSid }
    function Assert-TrustedEntry([string]$Path, [bool]$Full, [bool]$Directory) {
        $remedy = 'Fix its ownership and permissions as an administrator, or choose a protected -InstallDir.'
        $item = Get-Item -LiteralPath $Path -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $item.PSIsContainer -ne $Directory) {
            throw "Refusing ${Path}: it is a link or the wrong file type. $remedy"
        }
        $acl = Get-Acl -LiteralPath $Path
        $descriptor = [Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
        if (-not $descriptor.Owner -or $descriptor.Owner.Value -notin $trustedSids) {
            throw "Refusing ${Path}: its owner is not a trusted account. $remedy"
        }
        if ($null -eq $descriptor.DiscretionaryAcl) {
            throw "Refusing ${Path}: it has no DACL, so everyone can write to it. $remedy"
        }
        $mask = 0x500d0040
        if ($Full) { $mask = 0x500d0156 }
        foreach ($ace in $descriptor.DiscretionaryAcl) {
            $inheritOnly = ([int]$ace.AceFlags -band [int][Security.AccessControl.AceFlags]::InheritOnly) -ne 0
            if ($inheritOnly -and -not $Full) { continue }
            if ([int]$ace.AceType -in 0, 4, 5, 9, 11) {
                if ($ace.AceType -ne [Security.AccessControl.AceType]::AccessAllowed) {
                    throw "Refusing ${Path}: its DACL has an allow entry this check can't read. $remedy"
                }
                $sid = $ace.SecurityIdentifier.Value
                if ($sid -in $trustedSids -or ($inheritOnly -and $sid -eq 'S-1-3-0')) { continue }
                if (($ace.AccessMask -band $mask) -ne 0) {
                    throw "Refusing ${Path}: $sid is allowed to write to it. $remedy"
                }
            }
        }
    }
    function Assert-TrustedDirectory([string]$Path) {
        $chain = @()
        $directory = [IO.DirectoryInfo]::new($Path)
        while ($null -ne $directory) {
            $chain += $directory.FullName
            $directory = $directory.Parent
        }
        [Array]::Reverse($chain)
        for ($index = 0; $index -lt $chain.Count; $index++) {
            Assert-TrustedEntry $chain[$index] ($index -eq $chain.Count - 1) $true
        }
    }
    function New-ProtectedDirectory([string]$Path) {
        $acl = [Security.AccessControl.DirectorySecurity]::new()
        $acl.SetAccessRuleProtection($true, $false)
        $owner = 'S-1-5-32-544'
        if ($testMode) { $owner = $testSid }
        $acl.SetOwner([Security.Principal.SecurityIdentifier]::new($owner))
        foreach ($sid in @('S-1-5-18', 'S-1-5-32-544', $owner) | Select-Object -Unique) {
            $rule = [Security.AccessControl.FileSystemAccessRule]::new(
                [Security.Principal.SecurityIdentifier]::new($sid), 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow')
            $acl.AddAccessRule($rule)
        }
        $bytes = $acl.GetSecurityDescriptorBinaryForm()
        $descriptor = [Runtime.InteropServices.Marshal]::AllocHGlobal($bytes.Length)
        $pointerOffset = [IntPtr]::Size
        $length = 3 * [IntPtr]::Size
        $attributes = [Runtime.InteropServices.Marshal]::AllocHGlobal($length)
        try {
            [Runtime.InteropServices.Marshal]::Copy($bytes, 0, $descriptor, $bytes.Length)
            [Runtime.InteropServices.Marshal]::WriteInt32($attributes, 0, $length)
            [Runtime.InteropServices.Marshal]::WriteIntPtr($attributes, $pointerOffset, $descriptor)
            [Runtime.InteropServices.Marshal]::WriteInt32($attributes, 2 * $pointerOffset, 0)
            if (-not [OpenFerryInstallerNativeV2]::CreateDirectoryW($Path, $attributes)) {
                throw "Couldn't create a new protected directory at $Path"
            }
        } finally {
            [Runtime.InteropServices.Marshal]::FreeHGlobal($attributes)
            [Runtime.InteropServices.Marshal]::FreeHGlobal($descriptor)
        }
    }
    function Initialize-InstallDirectory([string]$Path) {
        if ([IO.File]::Exists($Path) -or [IO.Directory]::Exists($Path)) {
            Assert-TrustedDirectory $Path
            return
        }
        $parent = [IO.DirectoryInfo]::new($Path).Parent
        if ($null -eq $parent) { throw "Couldn't find an existing parent of $Path" }
        Initialize-InstallDirectory $parent.FullName
        New-ProtectedDirectory $Path
        Assert-TrustedDirectory $Path
    }

    if ($privileged) {
        Initialize-InstallDirectory $InstallDir
        $destination = Join-Path $InstallDir 'open-ferry.exe'
        if (Test-Path -LiteralPath $destination) { Assert-TrustedEntry $destination $true $false }
        $stagingRoot = $InstallDir
    } else {
        $stagingRoot = [IO.Path]::GetTempPath()
    }
    $tmp = Join-Path $stagingRoot ('open-ferry-install-' + [Guid]::NewGuid().ToString('N'))
    # Create-new, including if another process races us; never adopt a directory.
    if ($privileged) {
        New-ProtectedDirectory $tmp
    } elseif (-not [OpenFerryInstallerNativeV2]::CreateDirectoryW($tmp, [IntPtr]::Zero)) {
        throw "Couldn't create a new staging directory at $tmp"
    }
    try {
        if ($privileged) { Assert-TrustedDirectory $tmp }
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

        $hash = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($archivePath)
        try { $actual = [BitConverter]::ToString($hash.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
        finally { $stream.Dispose(); $hash.Dispose() }
        if ($actual -ne $expected) {
            throw "$archive doesn't match SHA256SUMS (expected $expected, got $actual): not installing it"
        }
        Write-Host "Checked $archive against SHA256SUMS."

        $gh = $null
        if ($hardened) {
            $gh = Join-Path $programFiles 'GitHub CLI\gh.exe'
        } else {
            $gh = $env:OPEN_FERRY_INSTALL_GH
        }
        if ($testMode -and $TestGh) { $gh = $TestGh }
        if (-not $gh) { $gh = 'gh' }
        if ($NoAttestation) {
            Write-Host 'Not checking the build provenance attestation (-NoAttestation).'
        } elseif (Get-Command $gh -CommandType Application -ErrorAction SilentlyContinue) {
            if ($reallyPrivileged) {
                Assert-TrustedDirectory ([IO.Path]::GetDirectoryName($gh))
                Assert-TrustedEntry $gh $true $false
            }
            Write-Host 'Checking the build provenance attestation with gh...'
            if ((Invoke-Native { & $gh attestation verify $archivePath --repo $repo } $tmp) -ne 0) {
                throw "gh couldn't verify ${archive}'s attestation: not installing it. If gh isn't signed in, run 'gh auth login' and try again, or pass -NoAttestation to rely on the SHA256SUMS check alone."
            }
        } else {
            Write-Host "The build provenance attestation wasn't checked: gh, the GitHub CLI, isn't installed. The SHA256SUMS check passed."
        }

        # --- Install ---------------------------------------------------------

        $unpacked = Join-Path $tmp 'unpacked'
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [IO.Compression.ZipFile]::ExtractToDirectory($archivePath, $unpacked)
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
            $info.WorkingDirectory = $InstallDir
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
                3 { throw 'open-ferry runs as a background service: use ''open-ferry update'' to switch versions without downtime (as administrator for a system service). Nothing was changed.' }
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
            Push-Location -LiteralPath $InstallDir -ErrorAction Stop
            try {
                $lines = & $Exe @Arguments 2>$null
                return (@($lines) -join "`n").Trim()
            } finally { Pop-Location }
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
        if (-not [IO.Path]::IsPathRooted($modeConfig) -or $modeConfig -notmatch '^(?:[A-Za-z]:\\|\\\\)') {
            throw "CLIProxyAPI returned a config path that isn't absolute: $modeConfig"
        }
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
            if ($System) {
                Write-Host '  Run these commands from an elevated terminal:'
                Write-Host "  Start it:            $command -config $quotedConfig"
                Write-Host "  Run it at boot:      $command service install -system -config $quotedConfig"
            } else {
                Write-Host "  Start it:            $command -config $quotedConfig"
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
    } finally {
        if ($null -ne $ownerState) {
            try { Restore-AdminOwner $ownerState } catch {
                # Restoration must not replace the install's own result or exception.
                Write-Warning "Installer owner restoration failed: $($_.Exception.Message)" -WarningAction Continue
            }
        }
    }
}

try {
    if (-not $PSBoundParameters.ContainsKey('System')) {
        switch ("$env:OPEN_FERRY_INSTALL_SYSTEM") {
            { $_ -in '', '0', 'no', 'false' } { $System = $false }
            { $_ -in '1', 'yes', 'true' } { $System = $true }
            default { throw "OPEN_FERRY_INSTALL_SYSTEM is yes or no, not $env:OPEN_FERRY_INSTALL_SYSTEM" }
        }
    }
    Install-OpenFerry -Version $Version -InstallDir $InstallDir -ConfigPath $ConfigPath -NoAttestation $NoAttestation.IsPresent -NoAutoUpdate $NoAutoUpdate.IsPresent -Migrate $Migrate.IsPresent -NoMigrate $NoMigrate.IsPresent -System $System.IsPresent -TestBaseUrl $TestBaseUrl -TestPrivilege $TestPrivilege -TestElevated $TestElevated.IsPresent -TestProgramFiles $TestProgramFiles -TestProgramData $TestProgramData -TestGh $TestGh
} catch {
    # Even a failed privilege probe or rejected test parameter must not
    # autoload error-reporting commands from the caller's module folders.
    & {
        param($Failure)
        $PSModuleAutoLoadingPreference = 'None'
        Import-Module -Name ([IO.Path]::Combine($PSHOME, 'Modules', 'Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Utility.psd1')) -Force -ErrorAction Stop
        Write-Host "install.ps1: error: $($Failure.Exception.Message)" -ForegroundColor Red
        # A file exits with 1; irm/iex returns without closing its caller.
        if ((Get-PSCallStack)[0].ScriptName) { exit 1 }
    } $_
}
