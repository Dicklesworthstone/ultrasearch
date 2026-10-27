<#
.SYNOPSIS
Tests a real UltraSearch MSI installation on an elevated, disposable Windows host.
.DESCRIPTION
Requires PowerShell 7, administrator rights, diskpart, and a fresh host with no
UltraSearch installation, service, or data directory. Pass -DisposableHost to
acknowledge that this script installs a service and creates a temporary local user.
It creates its own NTFS VHD, limits indexing to that fixture, tests installed
clients both elevated and as a standard user, and exercises service recovery.
Filename checks run with extraction blocked by a year-long idle threshold and
an additional selected drive that is unavailable. The healthy volume must still
gain searchable filenames and, after admission opens, searchable body text.
A subsequent config reload enables extraction and must make real body text
searchable. This controls idle policy; it does not emulate battery hardware.
The GUI check proves process liveness only; it does not inspect rendered results.
Finally it uninstalls only the MSI from this attempt, removes only its temporary
account, and detaches its VHD. Fixture files, logs, and result.json are retained.
Parsing this script on another platform does not constitute native verification.
.EXAMPLE
pwsh -File .github/scripts/windows-installed-startup.ps1 -DisposableHost -MsiPath target/wix/UltraSearch.msi -BinaryDir target/release -EvidenceDir C:/ultrasearch-evidence/installed-startup
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$MsiPath,
    [Parameter(Mandatory = $true)][string]$BinaryDir,
    [Parameter(Mandatory = $true)][string]$EvidenceDir,
    [switch]$DisposableHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows -or $PSVersionTable.PSVersion.Major -lt 7) {
    throw 'This regression requires Windows and PowerShell 7 or later.'
}
if (-not $DisposableHost) {
    throw 'Use -DisposableHost only on an elevated, fresh, disposable Windows test host.'
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The installed-service regression requires an elevated administrator session.'
}

$serviceName = 'UltraSearchService'
$installDir = Join-Path $env:ProgramFiles 'UltraSearch'
$dataDir = Join-Path $env:ProgramData 'UltraSearch'
if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    throw "Refusing to touch the existing $serviceName service."
}
$uninstallKeys = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
)
foreach ($key in $uninstallKeys) {
    $existingProducts = @(Get-ItemProperty $key -ErrorAction SilentlyContinue | Where-Object {
        $property = $_.PSObject.Properties['DisplayName']
        $null -ne $property -and $property.Value -like 'UltraSearch*'
    })
    if ($existingProducts.Count -gt 0) {
        throw 'Refusing to modify a host with an existing UltraSearch product registration.'
    }
}
foreach ($path in @($installDir, $dataDir, $EvidenceDir)) {
    if (Test-Path -LiteralPath $path) {
        throw "Refusing to reuse an existing path: $path"
    }
}
$MsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
$BinaryDir = (Resolve-Path -LiteralPath $BinaryDir).Path
$EvidenceDir = [IO.Path]::GetFullPath($EvidenceDir)
# Copying ProgramData into itself during failure collection would recurse.
# Check before creating any directories, including the evidence destination.
foreach ($protectedRoot in @($installDir, $dataDir)) {
    $root = [IO.Path]::GetFullPath($protectedRoot).TrimEnd([char[]]'\/')
    if ($EvidenceDir.Equals($root, [StringComparison]::OrdinalIgnoreCase) -or
        $EvidenceDir.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'EvidenceDir must be outside the UltraSearch install and data directories.'
    }
}
if ([IO.Path]::GetExtension($MsiPath) -ne '.msi') {
    throw 'MsiPath must identify the MSI built from the binaries under test.'
}
$binaryNames = @('service.exe', 'cli.exe', 'ui.exe', 'launcher.exe', 'index-worker.exe')
foreach ($name in $binaryNames) {
    if (-not (Test-Path -LiteralPath (Join-Path $BinaryDir $name) -PathType Leaf)) {
        throw "Missing built binary: $name"
    }
}
New-Item -ItemType Directory -Path $EvidenceDir | Out-Null
$script:commandNumber = 0
$checks = [Collections.Generic.List[string]]::new()
$utf8 = [Text.UTF8Encoding]::new($false)

function Save-Text {
    param([string]$Path, [string]$Text)
    [IO.File]::WriteAllText($Path, $Text, $utf8)
}

function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [int]$TimeoutSeconds = 30,
        [int[]]$SuccessCodes = @(0),
        [switch]$AllowFailure,
        [string]$Label = 'command',
        [pscredential]$Credential,
        [string]$WorkingDirectory = $EvidenceDir
    )
    $script:commandNumber += 1
    $prefix = Join-Path $EvidenceDir ('{0:D3}-{1}' -f $script:commandNumber, $Label)
    $start = [Diagnostics.ProcessStartInfo]::new($FilePath)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.WorkingDirectory = $WorkingDirectory
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    if ($null -ne $Credential) {
        $start.UserName = $Credential.UserName
        $start.Domain = [Environment]::MachineName
        $start.Password = $Credential.Password
        $start.LoadUserProfile = $true
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw "Could not start $Label" }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $exitedInTime = $process.WaitForExit($TimeoutSeconds * 1000)
        if (-not $exitedInTime) {
            $process.Kill($true)
            if (-not $process.WaitForExit(5000)) { throw "$Label did not exit after termination." }
        }
        # A descendant can inherit a redirected handle after its parent exits.
        # Bound stream draining as well as the native process wait.
        $reads = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($stdout, $stderr))
        if (-not $reads.Wait(5000)) {
            throw "$Label output streams did not close within five seconds of exit."
        }
        $result = [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout.GetAwaiter().GetResult()
            Stderr = $stderr.GetAwaiter().GetResult()
        }
        Save-Text "$prefix.stdout.txt" $result.Stdout
        Save-Text "$prefix.stderr.txt" $result.Stderr
        if (-not $exitedInTime) {
            throw "$Label exceeded its $TimeoutSeconds second deadline."
        }
        if (-not $AllowFailure -and $result.ExitCode -notin $SuccessCodes) {
            throw "$Label exited $($result.ExitCode). See $prefix.stderr.txt and $prefix.stdout.txt."
        }
        return $result
    }
    finally {
        $process.Dispose()
    }
}

function Passed {
    param([string]$Name)
    $checks.Add($Name)
    Write-Host "PASS: $Name"
}

function Require-ServiceState {
    param([string]$Expected)
    $actual = (Get-Service -Name $serviceName).Status.ToString()
    if ($actual -ne $Expected) {
        throw "Expected service state $Expected; observed $actual."
    }
}

function Wait-Search {
    param(
        [string]$Query,
        [string]$ExpectedPath,
        [string]$Label,
        [pscredential]$Credential,
        [ValidateSet('name', 'content')][string]$Mode = 'name'
    )
    $deadline = [DateTime]::UtcNow.AddSeconds(180)
    do {
        $parameters = @{
            FilePath = $installedCli
            Arguments = @('search', $Query, '--mode', $Mode, '--json', '--timeout-ms', '5000')
            TimeoutSeconds = 15
            Label = $Label
            WorkingDirectory = $installDir
        }
        if ($null -ne $Credential) { $parameters.Credential = $Credential }
        $result = Invoke-Native @parameters
        $response = $result.Stdout | ConvertFrom-Json
        foreach ($hit in $response.hits) {
            # Native indexing preserves the volume GUID root. Match the full
            # canonical path so an unrelated same-name file cannot satisfy this.
            if ($null -ne $hit.path -and $hit.path.Replace('/', '\').Equals($ExpectedPath, [StringComparison]::OrdinalIgnoreCase)) {
                return
            }
        }
        Start-Sleep -Milliseconds 1000
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Search did not return the exact fixture path within 180 seconds: $ExpectedPath"
}

$failure = $null
$installationAttempted = $false
$ui = $null
$standardUser = $null
$vhdPath = Join-Path $EvidenceDir 'fixture.vhdx'
$configPath = Join-Path $dataDir 'config/config.toml'
$installedService = Join-Path $installDir 'service.exe'
$installedCli = Join-Path $installDir 'cli.exe'
$startedUtc = [DateTime]::UtcNow
$driveLetter = $null
$unavailableMountRoot = $null

try {
    $usedDrives = [IO.Directory]::GetLogicalDrives()
    foreach ($code in (90..69)) {
        $candidate = [string][char]$code
        if (($candidate + ':\') -notin $usedDrives) {
            $driveLetter = $candidate
            break
        }
    }
    if ($null -eq $driveLetter) { throw 'No unused drive letter is available for the fixture.' }
    foreach ($code in (90..69)) {
        $candidate = [string][char]$code
        if ($candidate -ne $driveLetter -and ($candidate + ':\') -notin $usedDrives) {
            $unavailableMountRoot = $candidate + ':\'
            break
        }
    }
    if ($null -eq $unavailableMountRoot) { throw 'No second unused drive letter is available for the unavailable-volume regression.' }
    if ($vhdPath.Contains('"') -or $vhdPath.Contains([char]10) -or $vhdPath.Contains([char]13)) {
        throw 'EvidenceDir cannot contain characters that would alter a diskpart command.'
    }
    $diskpartScript = Join-Path $EvidenceDir 'create-vhd.txt'
    Save-Text $diskpartScript @"
create vdisk file="$vhdPath" maximum=256 type=expandable
select vdisk file="$vhdPath"
attach vdisk
create partition primary
format fs=ntfs quick label=UltraSearchSmoke
assign letter=$driveLetter
exit
"@
    $null = Invoke-Native -FilePath 'diskpart.exe' -Arguments @('/s', $diskpartScript) -TimeoutSeconds 90 -Label 'create-vhd'
    $diskImage = Get-DiskImage -ImagePath $vhdPath
    if (-not $diskImage.Attached) { throw 'The fixture VHD was not attached.' }
    $partitions = @($diskImage | Get-Disk | Get-Partition | Where-Object { $_.DriveLetter -eq $driveLetter })
    if ($partitions.Count -ne 1) { throw 'The selected drive letter does not belong to the fixture VHD.' }
    $fixtureVolume = $partitions[0] | Get-Volume
    if ($fixtureVolume.FileSystem -ne 'NTFS') { throw 'The fixture volume is not NTFS.' }
    $volumeRoot = [string]$fixtureVolume.Path
    if (-not $volumeRoot.StartsWith('\\?\Volume{', [StringComparison]::OrdinalIgnoreCase) -or
        -not $volumeRoot.EndsWith('}\')) {
        throw "The fixture has no canonical volume GUID path: $volumeRoot"
    }
    $mount = $driveLetter + ':'
    $mountRoot = $mount + '\'
    $null = Invoke-Native -FilePath 'fsutil.exe' -Arguments @('usn', 'createjournal', 'm=8388608', 'a=1048576', $mount) -Label 'create-usn-journal'
    $null = Invoke-Native -FilePath 'fsutil.exe' -Arguments @('usn', 'queryjournal', $mount) -Label 'query-usn-journal'
    $token = 'usmoke' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $contentToken = 'ubody' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $fixture = $mount + '\' + $token + '.txt'
    $canonicalFixture = $volumeRoot + $token + '.txt'
    Save-Text $fixture "UltraSearch installed-service filename regression fixture. $contentToken"
    $punctuatedName = 'Surface-' + $token + ' [draft].txt'
    $canonicalPunctuated = $volumeRoot + $punctuatedName
    Save-Text ($mountRoot + $punctuatedName) 'Filename punctuation and partial matching fixture.'
    New-Item -ItemType Directory -Path (Split-Path $configPath) -Force | Out-Null
    # The only mounted selected volume is this VHD. An unavailable second
    # selection must remain diagnostic rather than starving the healthy drive.
    # A fresh runner cannot have one year of idle time; filenames must progress.
    $validConfig = @"
volumes = ['$mountRoot', '$unavailableMountRoot']
content_index_volumes = ['$mountRoot', '$unavailableMountRoot']

[scheduler]
idle_warm_seconds = 31536000
idle_deep_seconds = 31536000
cpu_soft_limit_pct = 100
cpu_hard_limit_pct = 100
disk_busy_bytes_per_s = 1099511627776
content_batch_size = 16
power_save_mode = true
"@
    Save-Text $configPath $validConfig
    $installationAttempted = $true
    $installArguments = @('/i', $MsiPath, '/qn', '/norestart', '/L*v', (Join-Path $EvidenceDir 'install-msi.log'))
    $null = Invoke-Native -FilePath 'msiexec.exe' -Arguments $installArguments -TimeoutSeconds 240 -SuccessCodes @(0, 3010) -Label 'install-msi'
    foreach ($name in $binaryNames) {
        $installed = Join-Path $installDir $name
        if (-not (Test-Path -LiteralPath $installed)) { throw "MSI omitted $name" }
        if ((Get-FileHash -LiteralPath $installed).Hash -ne (Get-FileHash -LiteralPath (Join-Path $BinaryDir $name)).Hash) {
            throw "Installed $name differs from the built binary."
        }
    }
    $registration = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
    Save-Text (Join-Path $EvidenceDir 'service-registration.json') ($registration | Select-Object Name, PathName, StartName, StartMode, State, ProcessId | ConvertTo-Json)
    if ($registration.PathName -notin @($installedService, ('"' + $installedService + '"'))) {
        throw "Unexpected installed service image: $($registration.PathName)"
    }
    if ($registration.StartName -ne 'LocalSystem' -or $registration.StartMode -ne 'Auto') {
        throw 'MSI did not register the service as automatic LocalSystem.'
    }
    Require-ServiceState 'Running'
    Passed 'MSI installs the tested binaries and starts the registered LocalSystem service'
    $null = Invoke-Native -FilePath $installedCli -Arguments @('ping', '--json') -Label 'installed-ping'
    $status = Invoke-Native -FilePath $installedCli -Arguments @('status', '--json') -Label 'installed-status'
    $null = $status.Stdout | ConvertFrom-Json
    Wait-Search -Query $token -ExpectedPath $canonicalFixture -Label 'initial-filename-search'
    $blockedStatus = Invoke-Native -FilePath $installedCli -Arguments @('status', '--json') -Label 'closed-admission-status'
    $schedulerState = ($blockedStatus.Stdout | ConvertFrom-Json).scheduler_state
    if ($schedulerState -notmatch 'content_admission=' -or $schedulerState -match 'content_admission=ready(\s|;|$)') {
        throw "The filename regression did not run with closed extraction admission: $schedulerState"
    }
    # Publishing completed filename visibility and publishing end-of-turn
    # health are separate operations. Wait for the latter without requiring
    # their observations to happen in the same process scheduling slice.
    $healthDeadline = [DateTime]::UtcNow.AddSeconds(10)
    while (-not $schedulerState.Contains("configured volume $unavailableMountRoot is unavailable or not NTFS") -and
        [DateTime]::UtcNow -lt $healthDeadline) {
        Start-Sleep -Milliseconds 200
        $health = Invoke-Native -FilePath $installedCli -Arguments @('status', '--json') -Label 'unavailable-volume-health'
        $schedulerState = ($health.Stdout | ConvertFrom-Json).scheduler_state
    }
    if (-not $schedulerState.Contains("configured volume $unavailableMountRoot is unavailable or not NTFS")) {
        throw 'The selected unavailable volume was not reported in ingestion health.'
    }
    $blockedContent = Invoke-Native -FilePath $installedCli -Arguments @('search', $contentToken, '--mode', 'content', '--json') -Label 'content-before-admission'
    if (($blockedContent.Stdout | ConvertFrom-Json).total -ne 0) {
        throw 'Content extraction bypassed the deliberately closed idle gate.'
    }
    Passed 'Initial NTFS filename is searchable while extraction admission is closed'

    # A real standard token is required to exercise the service pipe ACL.
    # The random credential stays in memory and is never placed in arguments.
    $userName = 'usmoke' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
    if (Get-LocalUser -Name $userName -ErrorAction SilentlyContinue) {
        throw 'Refusing to reuse an existing local account.'
    }
    $password = ConvertTo-SecureString ([Guid]::NewGuid().ToString('N') + [Guid]::NewGuid().ToString('N') + '!aA9') -AsPlainText -Force
    $standardUser = New-LocalUser -Name $userName -Password $password -Description 'UltraSearch disposable integration test' -AccountNeverExpires
    $userMembership = @(Get-LocalGroupMember -SID 'S-1-5-32-545' | Where-Object { $_.SID -eq $standardUser.SID })
    if ($userMembership.Count -eq 0) {
        Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $standardUser
    }
    $credential = [pscredential]::new($userName, $password)
    $groups = Invoke-Native -FilePath 'whoami.exe' -Arguments @('/groups', '/fo', 'csv', '/nh') -Credential $credential -WorkingDirectory $installDir -Label 'standard-user-groups'
    if ($groups.Stdout -notmatch 'S-1-5-32-545' -or $groups.Stdout -match 'S-1-5-32-544') {
        throw 'The test client did not run with the intended standard-user group membership.'
    }
    $standardStatus = Invoke-Native -FilePath $installedCli -Arguments @('status', '--json') -Credential $credential -WorkingDirectory $installDir -Label 'standard-user-status'
    $null = $standardStatus.Stdout | ConvertFrom-Json
    Wait-Search -Query $token -ExpectedPath $canonicalFixture -Label 'standard-user-search' -Credential $credential
    Passed 'A standard local user can read service status and search the installed pipe'
    Wait-Search -Query ('name:' + $token.Substring(6, 6)) -ExpectedPath $canonicalFixture -Label 'partial-filename-search' -Credential $credential
    Wait-Search -Query ('name:Surface-' + $token.Substring(0, 10)) -ExpectedPath $canonicalPunctuated -Label 'punctuated-filename-search' -Credential $credential
    Wait-Search -Query ('path:"' + $canonicalPunctuated + '"') -ExpectedPath $canonicalPunctuated -Label 'quoted-windows-path-search' -Credential $credential
    Wait-Search -Query ('path:"' + $mountRoot + $punctuatedName + '"') -ExpectedPath $canonicalPunctuated -Label 'drive-letter-path-search' -Credential $credential
    Wait-Search -Query ('"' + $mountRoot + $punctuatedName + '"') -ExpectedPath $canonicalPunctuated -Label 'pasted-explorer-path-search' -Credential $credential
    Passed 'Partial filenames, punctuation, and pasted drive-letter paths return the real canonical indexed result'

    $laterToken = 'ulate' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    Save-Text ($mount + '\' + $laterToken + '.txt') 'File created after the service became ready.'
    $canonicalLater = $volumeRoot + $laterToken + '.txt'
    Wait-Search -Query $laterToken -ExpectedPath $canonicalLater -Label 'journal-filename-search'
    Wait-Search -Query $laterToken -ExpectedPath $canonicalLater -Label 'standard-user-journal-search' -Credential $credential
    Passed 'Post-startup NTFS filename becomes searchable by a standard user while extraction stays closed'

    # Reopen admission using the supported config path. Keep this valid version
    # for later restart/configuration-failure recovery checks.
    $validConfig = @"
volumes = ['$mountRoot', '$unavailableMountRoot']
content_index_volumes = ['$mountRoot', '$unavailableMountRoot']

[scheduler]
idle_warm_seconds = 0
idle_deep_seconds = 0
cpu_soft_limit_pct = 100
cpu_hard_limit_pct = 100
disk_busy_bytes_per_s = 1099511627776
content_batch_size = 16
power_save_mode = false
"@
    Save-Text $configPath $validConfig
    $reload = Invoke-Native -FilePath $installedCli -Arguments @('reload-config', '--json') -Label 'enable-content-reload'
    if (-not ($reload.Stdout | ConvertFrom-Json).success) {
        throw 'The service rejected the valid extraction-admission configuration.'
    }
    Wait-Search -Query $contentToken -ExpectedPath $canonicalFixture -Mode content -Label 'content-after-admission' -Credential $credential
    Wait-Search -Query $token -ExpectedPath $canonicalFixture -Label 'filename-after-enrichment' -Credential $credential
    Passed 'Opening idle admission enriches the healthy drive despite another unavailable selection, without losing filename search'
    $uiParameters = @{
        FilePath = (Join-Path $installDir 'ui.exe')
        WorkingDirectory = $installDir
        PassThru = $true
        RedirectStandardOutput = (Join-Path $EvidenceDir 'ui.stdout.txt')
        RedirectStandardError = (Join-Path $EvidenceDir 'ui.stderr.txt')
    }
    $ui = Start-Process @uiParameters
    if ($ui.WaitForExit(5000)) { throw "Installed UI exited early with code $($ui.ExitCode)." }
    Passed 'Installed UI remains alive after launch (no rendered UI assertion)'

    $null = Invoke-Native -FilePath $installedService -Arguments @('stop') -TimeoutSeconds 45 -Label 'stop-service'
    Require-ServiceState 'Stopped'
    $stopped = Invoke-Native -FilePath $installedCli -Arguments @('status', '--json') -TimeoutSeconds 20 -AllowFailure -Label 'stopped-diagnostic'
    if ($stopped.ExitCode -eq 0 -or ($stopped.Stdout + $stopped.Stderr) -notmatch 'service is stopped') {
        throw 'The installed CLI did not identify the stopped Windows service.'
    }
    Passed 'A stopped service produces an actionable client diagnostic'
    $null = Invoke-Native -FilePath $installedService -Arguments @('start') -TimeoutSeconds 170 -Label 'restart-service'
    Require-ServiceState 'Running'
    Wait-Search -Query $token -ExpectedPath $canonicalFixture -Label 'restart-search'
    if ($ui.HasExited) { throw 'The UI exited while the service was stopped and restarted.' }
    Passed 'SCM restart restores real search while the UI process remains alive'

    $null = Invoke-Native -FilePath $installedService -Arguments @('stop') -TimeoutSeconds 45 -Label 'stop-before-bad-config'
    try {
        Save-Text $configPath ("[paths]" + [Environment]::NewLine + 'meta_index = [')
        $badConfig = Invoke-Native -FilePath $installedService -Arguments @('start') -TimeoutSeconds 170 -AllowFailure -Label 'bad-config-start'
        if ($badConfig.ExitCode -eq 0) { throw 'A malformed configuration incorrectly reported a successful start.' }
        Require-ServiceState 'Stopped'
        $startupLog = Get-Content -LiteralPath (Join-Path $dataDir 'log/startup.log') -Raw
        if ($startupLog -notmatch 'configuration loading failed' -or $startupLog -notmatch 'invalid configuration TOML') {
            throw 'Startup diagnostics did not preserve the actual configuration failure.'
        }
        Passed 'Malformed TOML stops startup and records the configuration cause'
    }
    finally {
        Save-Text $configPath $validConfig
    }
    $null = Invoke-Native -FilePath $installedService -Arguments @('start') -TimeoutSeconds 170 -Label 'recovered-config-start'
    Wait-Search -Query $laterToken -ExpectedPath $canonicalLater -Label 'recovered-config-search'
    Passed 'Restoring valid configuration restores installed search'

    $servicePid = (Get-CimInstance Win32_Service -Filter "Name='$serviceName'").ProcessId
    $bare = Invoke-Native -FilePath $installedService -AllowFailure -Label 'bare-service-launch'
    if ($bare.ExitCode -eq 0 -or $bare.Stderr -notmatch '1063' -or $bare.Stderr -notmatch '--console') {
        throw 'A bare interactive launch did not explain expected SCM error 1063.'
    }
    if ((Get-CimInstance Win32_Service -Filter "Name='$serviceName'").ProcessId -ne $servicePid) {
        throw 'The incorrect manual launch disturbed the installed service process.'
    }
    $serviceProcesses = @(Get-CimInstance Win32_Process -Filter "Name='service.exe'" | Where-Object {
        $_.ExecutablePath -eq $installedService
    })
    if ($serviceProcesses.Count -ne 1) { throw 'A manual launch left an extra service process.' }
    $null = Invoke-Native -FilePath $installedCli -Arguments @('ping', '--json') -Label 'ping-after-bare-launch'
    Passed 'Manual error 1063 leaves the installed service and its connection healthy'
}
catch {
    $failure = $_
}
finally {
    if ($null -ne $standardUser) {
        try {
            Remove-LocalUser -SID $standardUser.SID
        }
        catch {
            if ($null -eq $failure) { $failure = $_ }
            Write-Warning "Could not remove the temporary local account: $_"
        }
    }
    if ($null -ne $ui) {
        try {
            if (-not $ui.HasExited) {
                $null = $ui.CloseMainWindow()
                if (-not $ui.WaitForExit(5000)) {
                    $ui.Kill($true)
                    if (-not $ui.WaitForExit(5000)) { throw 'The fixture UI process did not exit.' }
                }
            }
        }
        catch {
            if ($null -eq $failure) { $failure = $_ }
            Write-Warning "Could not stop the fixture UI: $_"
        }
        finally {
            $ui.Dispose()
        }
    }
    if ($installationAttempted -and (Test-Path -LiteralPath $installedService) -and
        (Get-Service -Name $serviceName -ErrorAction SilentlyContinue)) {
        try {
            $null = Invoke-Native -FilePath $installedService -Arguments @('stop') -TimeoutSeconds 45 -Label 'cleanup-stop'
        }
        catch {
            Write-Warning "Could not stop the service before collecting evidence: $_"
        }
    }
    if (Test-Path -LiteralPath $dataDir) {
        try {
            Copy-Item -LiteralPath $dataDir -Destination (Join-Path $EvidenceDir 'programdata') -Recurse
        }
        catch {
            if ($null -eq $failure) { $failure = $_ }
            Write-Warning "Could not collect ProgramData evidence: $_"
        }
    }
    if ($installationAttempted) {
        try {
            $uninstallArguments = @('/x', $MsiPath, '/qn', '/norestart', '/L*v', (Join-Path $EvidenceDir 'uninstall-msi.log'))
            $null = Invoke-Native -FilePath 'msiexec.exe' -Arguments $uninstallArguments -TimeoutSeconds 180 -SuccessCodes @(0, 1605, 3010) -Label 'uninstall-msi'
        }
        catch {
            if ($null -eq $failure) { $failure = $_ }
            Write-Warning "Could not uninstall the test MSI: $_"
        }
    }
    if (Test-Path -LiteralPath $vhdPath) {
        try {
            $detachScript = Join-Path $EvidenceDir 'detach-vhd.txt'
            Save-Text $detachScript @"
select vdisk file="$vhdPath"
detach vdisk
exit
"@
            $null = Invoke-Native -FilePath 'diskpart.exe' -Arguments @('/s', $detachScript) -TimeoutSeconds 60 -Label 'detach-vhd'
            if ((Get-DiskImage -ImagePath $vhdPath).Attached) { throw 'The test VHD is still attached.' }
        }
        catch {
            if ($null -eq $failure) { $failure = $_ }
            Write-Warning "Could not detach the test VHD: $_"
        }
    }
    $result = [ordered]@{
        success = $null -eq $failure
        failure = if ($null -eq $failure) { $null } else { $failure.ToString() }
        started_utc = $startedUtc.ToString('o')
        finished_utc = [DateTime]::UtcNow.ToString('o')
        msi = $MsiPath
        binary_dir = $BinaryDir
        vhd = $vhdPath
        checks_passed = $checks.ToArray()
        qualification = 'Native installed-service/NTFS/IPC checks including standard-user clients, closed/open idle-admission filename/content search, plus GUI process liveness; no rendered UI or battery-hardware assertions.'
    }
    Save-Text (Join-Path $EvidenceDir 'result.json') ($result | ConvertTo-Json -Depth 5)
}

if ($null -ne $failure) { throw $failure }
Write-Host "Installed startup regression passed. Evidence retained at $EvidenceDir"
