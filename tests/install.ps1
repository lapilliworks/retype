# Exercise installer publication failures with a local release and no network or user PATH writes.
param([Parameter(Mandatory = $true)][string]$Binary)
$ErrorActionPreference = 'Stop'

$Root = Split-Path $PSScriptRoot -Parent
$Temp = Join-Path ([IO.Path]::GetTempPath()) ('retype-install-test-' + [guid]::NewGuid().ToString('N'))
$Prefix = Join-Path $Temp 'prefix'
$Tag = 'v0.0.0-test'
$Name = "retype-$Tag-x86_64-pc-windows-msvc"
$Release = Join-Path $Temp $Name
$Archive = Join-Path $Temp "$Name.tar.gz"
$Sums = Join-Path $Temp 'SHA256SUMS'
$FailPublication = $null

function Invoke-RestMethod {
	param($Uri, $Headers)
	if ($Uri -notlike '*/releases?per_page=100') { throw "Unexpected release endpoint: $Uri" }
	[pscustomobject]@{ tag_name = $script:Tag; prerelease = $true; draft = $false }
}

function Invoke-WebRequest {
	param([switch]$UseBasicParsing, $Uri, $OutFile)
	$Source = if ($Uri.EndsWith('/SHA256SUMS')) { $script:Sums } else { $script:Archive }
	Copy-Item -LiteralPath $Source -Destination $OutFile
}

function Get-Process {
	param($Name, $ErrorAction)
	# Other parallel test daemons must not affect this isolated installation.
}

function Move-Item {
	param($LiteralPath, $Destination, [switch]$Force)
	$IsStage = [IO.Path]::GetFileName($LiteralPath).StartsWith('.retype.')
	$Kind = if (Test-Path -LiteralPath $LiteralPath -PathType Container) { 'share' } else { 'binary' }
	if ($IsStage -and $script:FailPublication -eq $Kind) {
		$script:FailPublication = $null
		throw "Simulated $Kind publication failure"
	}
	Microsoft.PowerShell.Management\Move-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force
}

function Pack-Release {
	& tar.exe -czf $Archive -C $Temp $Name
	if ($LASTEXITCODE -ne 0) { throw 'Could not assemble the local release' }
	$Hash = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
	[IO.File]::WriteAllText($Sums, "$Hash  $Name.tar.gz`n")
}

function Assert-PreviousInstallation {
	$Installed = Join-Path $Prefix 'bin\retype.exe'
	if ((Get-FileHash $Installed).Hash -ne (Get-FileHash $Binary).Hash) { throw 'Previous binary was not restored' }
	$Asset = Join-Path $Prefix 'share\retype\previous.txt'
	if ([IO.File]::ReadAllText($Asset) -ne 'previous assets') { throw 'Previous assets were not restored' }
}

try {
	New-Item -ItemType Directory -Path (Join-Path $Release 'bin') | Out-Null
	New-Item -ItemType Directory -Path (Join-Path $Release 'share\retype') | Out-Null
	$SourceBinary = Join-Path $Release 'bin\retype.exe'
	Copy-Item -LiteralPath $Binary -Destination $SourceBinary
	[IO.File]::WriteAllText((Join-Path $Release 'share\retype\asset.txt'), 'new assets')
	Pack-Release
	$env:RETYPE_PREFIX = $Prefix
	$env:RETYPE_TARGET = 'x86_64-pc-windows-msvc'
	$env:RETYPE_VERSION = 'latest'
	$env:RETYPE_RELEASE_BASE = 'https://local-release.invalid'

	$Source = [IO.File]::ReadAllText((Join-Path $Root 'packaging\install.ps1'))
	$PathWrite = "[Environment]::SetEnvironmentVariable('Path', (@(`$binDir) + `$entries) -join ';', 'User')"
	if (-not $Source.Contains($PathWrite)) { throw 'Installer PATH write changed; update the test isolation' }
	$Installer = [scriptblock]::Create($Source.Replace($PathWrite, "Write-Verbose 'User PATH write disabled for test'"))
	& $Installer
	[IO.File]::WriteAllText((Join-Path $Prefix 'share\retype\previous.txt'), 'previous assets')

	foreach ($Kind in @('share', 'binary')) {
		$script:FailPublication = $Kind
		$Failed = $false
		try { & $Installer } catch { $Failed = $true }
		if (-not $Failed) { throw "$Kind publication failure was not reported" }
		Assert-PreviousInstallation
	}

	[IO.File]::WriteAllBytes($SourceBinary, [byte[]](0, 1, 2, 3))
	Pack-Release
	$Failed = $false
	try { & $Installer } catch { $Failed = $true }
	if (-not $Failed) { throw 'An invalid staged binary was accepted' }
	Assert-PreviousInstallation
	$Backups = Get-ChildItem (Join-Path $Prefix 'share') -Directory -Filter 'retype-backup-*'
	if (-not ($Backups | Where-Object { Test-Path (Join-Path $_.FullName 'retype.exe') })) {
		throw 'No persistent previous-binary backup exists'
	}
	Write-Host 'Windows installer tests passed'
} finally {
	if (Test-Path -LiteralPath $Temp) { Remove-Item -LiteralPath $Temp -Recurse -Force }
}
