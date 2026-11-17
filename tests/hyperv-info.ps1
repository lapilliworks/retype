#Requires -Version 5.1
# Run on the Windows Hyper-V host. Collects information only; never changes VM state.
# Requires Windows PowerShell 5.1 or newer and Hyper-V administrative access.
[CmdletBinding()]
param(
	[Parameter(Mandatory, Position = 0)]
	[ValidateNotNullOrEmpty()]
	[string] $VMName,
	[string] $GuestRepository,
	[PSCredential] $Credential,
	[string] $ReportDirectory = (Join-Path $PWD 'hyperv-reports'),
	[ValidateRange(1, 300)]
	[int] $TimeoutSeconds = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Save-Report {
	param([string] $Name, [object] $Data)
	$json = ConvertTo-Json -InputObject $Data -Depth 12
	$path = Join-Path $script:ReportPath "$Name.json"
	[IO.File]::WriteAllText($path, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

function Receive-BoundedJob {
	param([System.Management.Automation.Job] $Job, [int] $Seconds)
	try {
		$completed = Wait-Job -Job $Job -Timeout $Seconds
		if ($null -eq $completed) {
			throw "Query exceeded its ${Seconds}s deadline."
		}
		$result = @(Receive-Job -Job $Job -ErrorAction Stop)
		if ($Job.State -ne 'Completed') {
			throw "Query ended in state $($Job.State)."
		}
		if ($result.Count -ne 1) {
			throw "Expected one query result, received $($result.Count)."
		}
		return $result[0]
	} finally {
		if ($Job.State -notin @('Completed', 'Failed', 'Stopped')) {
			Stop-Job -Job $Job -ErrorAction Stop
		}
		Remove-Job -Job $Job -ErrorAction Stop
	}
}

$hostQuery = {
	param([string] $Name)
	Set-StrictMode -Version Latest
	$ErrorActionPreference = 'Stop'
	Import-Module Hyper-V -ErrorAction Stop
	$vm = @(Get-VM -Name $Name | Where-Object { $_.Name -eq $Name })
	if ($vm.Count -ne 1) { throw 'VM name must identify exactly one local VM.' }
	$vm = $vm[0]
	if ($vm.State.ToString() -ne 'Running') { throw 'The VM must already be running; it will not be started.' }
	$disks = @(Get-VMHardDiskDrive -VM $vm | ForEach-Object {
		$drive = $_
		if (-not $drive.Path) { throw 'Pass-through disks are not supported by this collector.' }
		$vhd = Get-VHD -Path $drive.Path
		[PSCustomObject]@{
			Path = $drive.Path
			ControllerType = $drive.ControllerType.ToString()
			ControllerNumber = $drive.ControllerNumber
			ControllerLocation = $drive.ControllerLocation
			VhdFormat = $vhd.VhdFormat.ToString()
			VhdType = $vhd.VhdType.ToString()
			Size = $vhd.Size
			FileSize = $vhd.FileSize
			LogicalSectorSize = $vhd.LogicalSectorSize
			PhysicalSectorSize = $vhd.PhysicalSectorSize
			ParentPath = $vhd.ParentPath
		}
	})
	[PSCustomObject]@{
		CollectedUtc = [DateTime]::UtcNow.ToString('o')
		OS = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber
		PowerShell = $PSVersionTable.PSVersion.ToString()
		VM = $vm | Select-Object Name, Id, Generation, State, AutomaticCheckpointsEnabled
		VirtualDisks = $disks
		HostVolumes = @(Get-Volume | Select-Object DriveLetter, FileSystem, HealthStatus, Size, SizeRemaining)
	}
}

$guestQuery = {
	Set-StrictMode -Version Latest
	$ErrorActionPreference = 'Stop'
	[PSCustomObject]@{
		CollectedUtc = [DateTime]::UtcNow.ToString('o')
		ComputerName = $env:COMPUTERNAME
		OS = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber
		Architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
		PowerShell = $PSVersionTable.PSVersion.ToString()
		PowerShell7Path = @(Get-Command pwsh -CommandType Application -ErrorAction SilentlyContinue |
			Select-Object -ExpandProperty Source)
		Volumes = @(Get-Volume | Select-Object DriveLetter, FileSystem, HealthStatus, Size, SizeRemaining)
	}
}

$repositoryQuery = {
	param([string] $Repository)
	Set-StrictMode -Version Latest
	$ErrorActionPreference = 'Stop'
	$path = (Resolve-Path -LiteralPath $Repository).ProviderPath
	$git = (Get-Command git -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
	# Do not let Git's status refresh write the guest index.
	$env:GIT_OPTIONAL_LOCKS = '0'
	$revision = @(& $git -C $path rev-parse --verify HEAD 2>&1)
	if ($LASTEXITCODE -ne 0) { throw "Git revision failed: $($revision -join [Environment]::NewLine)" }
	if ($revision.Count -ne 1 -or $revision[0] -notmatch '^[0-9a-f]{40,64}$') {
		throw 'Git returned an invalid revision.'
	}
	$status = @(& $git -C $path -c core.fsmonitor=false status --short --untracked-files=normal 2>&1)
	if ($LASTEXITCODE -ne 0) { throw "Git status failed: $($status -join [Environment]::NewLine)" }
	[PSCustomObject]@{
		CollectedUtc = [DateTime]::UtcNow.ToString('o')
		Repository = $path
		Revision = [string] $revision[0]
		IsClean = $status.Count -eq 0
		Status = @($status | ForEach-Object { $_.ToString() })
	}
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
	throw 'Run this script on the Windows host that owns the Hyper-V VM.'
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
	throw 'Open PowerShell as Administrator on the Hyper-V host.'
}
if (-not (Get-Command Invoke-Command).Parameters.ContainsKey('VMId')) {
	throw 'This PowerShell installation does not expose PowerShell Direct (-VMId).'
}

$base = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ReportDirectory)
[IO.Directory]::CreateDirectory($base) | Out-Null
$name = 'HyperV-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N')
$script:ReportPath = Join-Path $base $name
New-Item -ItemType Directory -Path $script:ReportPath -ErrorAction Stop | Out-Null
Write-Host "Report directory: $script:ReportPath"
$stage = 'host'
try {
	Save-Report 'request' ([PSCustomObject]@{
		VMName = $VMName
		TimeoutSeconds = $TimeoutSeconds
		CollectorSHA256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
		Scope = 'Read-only preflight; not a crash or durability test'
	})
	$job = Start-Job -ScriptBlock $hostQuery -ArgumentList $VMName
	$hostInfo = Receive-BoundedJob -Job $job -Seconds $TimeoutSeconds
	Save-Report 'host' $hostInfo
	$vmId = [Guid] $hostInfo.VM.Id
	if ($null -eq $Credential) { $Credential = Get-Credential -Message "Guest account for VM '$VMName'" }
	if ($null -eq $Credential) { throw 'Guest credentials were not supplied.' }
	if (-not $GuestRepository) {
		$GuestRepository = Read-Host 'Absolute path to the Retype checkout inside the guest'
	}
	if (-not $GuestRepository -or $GuestRepository -notmatch '^(?:[A-Za-z]:\\|\\\\)') {
		throw 'Supply an absolute Windows path to the guest checkout.'
	}
	$stage = 'guest'
	$job = Invoke-Command -VMId $vmId -Credential $Credential -ScriptBlock $guestQuery -AsJob
	Save-Report 'guest' (Receive-BoundedJob -Job $job -Seconds $TimeoutSeconds)
	$stage = 'repository'
	$job = Invoke-Command -VMId $vmId -Credential $Credential -ScriptBlock $repositoryQuery `
		-ArgumentList $GuestRepository -AsJob
	$repository = Receive-BoundedJob -Job $job -Seconds $TimeoutSeconds
	Save-Report 'repository' $repository
	Save-Report 'summary' ([PSCustomObject]@{
		Succeeded = $true
		RepositoryIsClean = $repository.IsClean
		CompletedUtc = [DateTime]::UtcNow.ToString('o')
		Scope = 'Information collected only; no VM or Retype state changes requested'
	})
	if (-not $repository.IsClean) { Write-Warning 'Guest checkout has changes; inspect repository.json before testing.' }
	Write-Host "Collection complete: $script:ReportPath"
} catch {
	Save-Report 'summary' ([PSCustomObject]@{
		Succeeded = $false
		FailedStage = $stage
		Error = $_.Exception.Message
		CompletedUtc = [DateTime]::UtcNow.ToString('o')
	})
	throw
}
