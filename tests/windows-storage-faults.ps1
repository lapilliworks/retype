#requires -Version 7.0
#requires -RunAsAdministrator
# Create only a new 64 MiB virtual disk; run the fixture worker with an ordinary interactive token.
[CmdletBinding()]
param(
	[Parameter(Mandatory)][string] $Worker,
	[Parameter(Mandatory)][string] $Report
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Worker = (Resolve-Path -LiteralPath $Worker).Path
$Report = [IO.Path]::GetFullPath($Report)
if (Test-Path -LiteralPath $Report) { throw 'Use a new report directory; existing evidence is never overwritten.' }
if ($Report -match '["\r\n]') { throw 'Report path cannot contain quotes or newlines.' }
[IO.Directory]::CreateDirectory($Report) | Out-Null
$image = Join-Path $Report 'synthetic-64MiB.vhd'
$letter = @('Z','Y','X','W' | Where-Object { -not (Test-Path ($_.ToString()+':\')) } | Select-Object -First 1)
if ($letter.Count -ne 1) { throw 'No unused qualification drive letter is available.' }
$letter = $letter[0]
$task = 'RetypeStorageFault-' + [guid]::NewGuid().ToString('N')
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
function Run-DiskPart([string] $Name, [string[]] $Commands) {
	$script = Join-Path $Report ($Name+'.txt')
	[IO.File]::WriteAllLines($script,$Commands)
	& diskpart.exe /s $script *> (Join-Path $Report ($Name+'.log'))
	if ($LASTEXITCODE -ne 0) { throw "DiskPart failed; inspect $Name.log before any cleanup." }
}
try {
	Run-DiskPart 'create' @("create vdisk file=`"$image`" maximum=64 type=expandable",'exit')
	Run-DiskPart 'attach' @("select vdisk file=`"$image`"",'attach vdisk','create partition primary','format fs=ntfs quick label=RetypeFaults',"assign letter=$letter",'exit')
	$disk = Get-DiskImage -ImagePath $image | Get-Disk
	$volume = $disk | Get-Partition | Get-Volume | Where-Object { $_.DriveLetter -eq $letter }
	if (-not $volume -or $volume.FileSystem -ne 'NTFS' -or $volume.Size -gt 128MB) {
		throw 'The created image is not the selected small NTFS volume.'
	}
	$volumeRoot = $letter + ':\'
	& icacls.exe $volumeRoot /grant ($identity+':(OI)(CI)M') *> (Join-Path $Report 'fixture-acl.log')
	if ($LASTEXITCODE -ne 0) { throw 'Cannot grant the fixture user access to the synthetic volume.' }
	$run = @'
$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Refusing elevated storage test.' }
$request = Get-Content (Join-Path $PSScriptRoot 'request.json') -Raw | ConvertFrom-Json
$env:RETYPE_DURABILITY_ROOT = $request.Root
$env:RETYPE_DURABILITY_ACTION = 'space'
& $request.Worker --exact durability_tests::recovery_worker --nocapture *> (Join-Path $PSScriptRoot 'worker.log')
@{ Exit=$LASTEXITCODE; Elevated=$false; Root=$request.Root } | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'result.json')
'@
	$runPath = Join-Path $Report 'run.ps1'
	[IO.File]::WriteAllText($runPath,$run)
	@{ Worker=$Worker; Root=($volumeRoot+'fixture') } | ConvertTo-Json | Set-Content (Join-Path $Report 'request.json')
	$action = New-ScheduledTaskAction -Execute (Get-Command pwsh -CommandType Application | Select-Object -First 1).Source -Argument ('-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -File "'+$runPath+'"')
	$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
	Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Settings (New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 3)) | Out-Null
	Start-ScheduledTask -TaskName $task
	$deadline = [DateTime]::UtcNow.AddMinutes(3)
	do {
		if ([DateTime]::UtcNow -ge $deadline) { throw 'Storage worker timed out; image and reports retained.' }
		Start-Sleep -Milliseconds 250
	} while ((Get-ScheduledTask -TaskName $task).State -eq 'Running' -or
		-not (Test-Path (Join-Path $Report 'result.json')))
	$result = Get-Content (Join-Path $Report 'result.json') -Raw | ConvertFrom-Json
	if ($result.Elevated -or $result.Exit -ne 0) { throw 'Storage worker failed; retain the image and inspect worker.log.' }
	Write-Output 'Actual NTFS disk exhaustion, protected recovery and exact-byte retry passed.'
} finally {
	if (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue) {
		Stop-ScheduledTask -TaskName $task
		Unregister-ScheduledTask -TaskName $task -Confirm:$false
	}
	$imageInfo = Get-DiskImage -ImagePath $image -ErrorAction SilentlyContinue
	if ($imageInfo -and $imageInfo.Attached) {
		Run-DiskPart 'detach' @("select vdisk file=`"$image`"",'detach vdisk','exit')
		if ((Get-DiskImage -ImagePath $image).Attached) { throw 'Synthetic disk remained attached; inspect detach.log.' }
	}
	if (Test-Path $image) { Get-FileHash $image | ConvertTo-Json | Set-Content (Join-Path $Report 'image-sha256.json') }
}
