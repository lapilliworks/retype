# Install or update retype from a GitHub release.
#
# Run it through PowerShell:
#   irm https://retype.name/install.ps1 | iex
#
# Environment:
#   RETYPE_VERSION       Release tag, or "latest". Default: latest
#   RETYPE_PREFIX        Install prefix. Default: $env:LOCALAPPDATA\Programs\retype
#   RETYPE_REPO          GitHub repository. Default: LapilliWorks/retype
#   RETYPE_RELEASE_BASE  Release download base, for a mirror
#   RETYPE_TARGET        Override target detection, such as x86_64-pc-windows-msvc
#
# This is not a code signature. The installed tier still needs platform signing.

& {
	$ErrorActionPreference = 'Stop'

	if ([System.Environment]::OSVersion.Platform -ne [System.PlatformID]::Win32NT) {
		throw 'This installer is for Windows. Use install.sh on Linux or macOS.'
	}

	$ProgressPreference = 'SilentlyContinue'
	[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

	$Repo = if ($env:RETYPE_REPO) { $env:RETYPE_REPO } else { 'LapilliWorks/retype' }
	$Version = if ($env:RETYPE_VERSION) { $env:RETYPE_VERSION } else { 'latest' }
	$ReleaseBase = if ($env:RETYPE_RELEASE_BASE) { $env:RETYPE_RELEASE_BASE } else {
		"https://github.com/$Repo/releases/download"
	}
	$Target = $env:RETYPE_TARGET

	if (-not $Target) {
		$machine = $env:PROCESSOR_ARCHITEW6432
		if (-not $machine) { $machine = $env:PROCESSOR_ARCHITECTURE }
		switch ($machine) {
			'AMD64' { $arch = 'x86_64' }
			'ARM64' { $arch = 'aarch64' }
			default { throw "Unsupported architecture: $machine" }
		}
		$Target = "$arch-pc-windows-msvc"
	}

	if ($Version -eq 'latest') {
		$api = "https://api.github.com/repos/$Repo/releases?per_page=100"
		$releases = Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'retype-installer' }
		$release = $releases | Where-Object { -not $_.draft } | Select-Object -First 1
		$Tag = $release.tag_name
		if (-not $Tag) { throw "Could not find a tag in the published release response from $api" }
	} elseif ($Version.StartsWith('v')) {
		$Tag = $Version
	} else {
		$Tag = "v$Version"
	}

	$Prefix = $env:RETYPE_PREFIX
	if (-not $Prefix) {
		if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set; set RETYPE_PREFIX.' }
		$Prefix = Join-Path $env:LOCALAPPDATA 'Programs\retype'
	}
	if (-not $Prefix) { throw 'The install prefix is empty.' }
	$Prefix = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Prefix)

	$ReleaseBase = $ReleaseBase.TrimEnd('/')
	$Base = "$ReleaseBase/$Tag"
	$Name = "retype-$Tag-$Target"
	$Archive = "$Name.tar.gz"

	$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("retype-install-" + [guid]::NewGuid().ToString('N'))
	New-Item -ItemType Directory -Path $temp | Out-Null

	$stagedBin = $null
	$stagedShare = $null
	$backup = $null
	$changedBin = $false
	$changedShare = $false
	$hadShare = $false
	try {
		$sumsPath = Join-Path $temp 'SHA256SUMS'
		Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $sumsPath

		$expected = $null
		foreach ($line in Get-Content -LiteralPath $sumsPath) {
			$fields = $line.Trim() -split '\s+', 2
			if ($fields.Count -eq 2 -and ($fields[1] -eq $Archive -or $fields[1] -eq "*$Archive")) {
				$expected = $fields[0].ToLowerInvariant()
				break
			}
		}
		if (-not $expected) { throw "Release $Tag has no checksum for $Archive." }

		$archivePath = Join-Path $temp $Archive
		Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Archive" -OutFile $archivePath
		$actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
		if ($actual -ne $expected) {
			throw "Checksum mismatch for ${Archive}: expected $expected, got $actual."
		}

		$tar = Get-Command tar.exe -ErrorAction SilentlyContinue
		if (-not $tar) { throw 'tar.exe was not found. Windows 10 1803 or newer includes it.' }
		& $tar.Source -xzf $archivePath -C $temp
		if ($LASTEXITCODE -ne 0) { throw "tar failed with exit status $LASTEXITCODE." }

		$source = Join-Path $temp $Name
		$sourceBinary = Join-Path $source 'bin\retype.exe'
		if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
			throw "$Archive does not contain bin\retype.exe."
		}

		$binDir = Join-Path $Prefix 'bin'
		$shareDir = Join-Path $Prefix 'share\retype'
		$shareParent = Split-Path -Path $shareDir -Parent
		New-Item -ItemType Directory -Force -Path $binDir | Out-Null
		New-Item -ItemType Directory -Force -Path $shareParent | Out-Null

		$targetBinary = Join-Path $binDir 'retype.exe'
		if (Test-Path -LiteralPath $targetBinary) {
			$running = Get-Process -Name 'retype' -ErrorAction SilentlyContinue
			if ($running) { throw 'retype is running. Stop the daemon before updating.' }
		}

		$stageId = [guid]::NewGuid().ToString('N')
		$stagedBin = Join-Path $binDir ".retype.$stageId.exe"
		Copy-Item -LiteralPath $sourceBinary -Destination $stagedBin
		$stagedShare = Join-Path $shareParent ".retype.$stageId"
		New-Item -ItemType Directory -Path $stagedShare | Out-Null
		$sourceShare = Join-Path $source 'share\retype'
		if (Test-Path -LiteralPath $sourceShare -PathType Container) {
			Copy-Item -Path (Join-Path $sourceShare '*') -Destination $stagedShare -Recurse -Force
		}
		$output = & $stagedBin --version 2>&1
		if ($LASTEXITCODE -ne 0) { throw "The staged binary does not run; installation unchanged: $output" }

		# Retain the previous installation on the destination filesystem after success.
		$backup = Join-Path $shareParent "retype-backup-$stageId"
		New-Item -ItemType Directory -Path $backup | Out-Null
		if (Test-Path -LiteralPath $targetBinary) {
			Copy-Item -LiteralPath $targetBinary -Destination (Join-Path $backup 'retype.exe')
		}
		$hadShare = Test-Path -LiteralPath $shareDir
		$changedShare = $true
		if ($hadShare) { Move-Item -LiteralPath $shareDir -Destination (Join-Path $backup 'share') }
		Move-Item -LiteralPath $stagedShare -Destination $shareDir
		$changedBin = $true
		Move-Item -LiteralPath $stagedBin -Destination $targetBinary -Force
		$output = & $targetBinary --version 2>&1
		if ($LASTEXITCODE -ne 0) { throw "The installed binary does not run: $output" }

		$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
		$entries = @()
		if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
		if ($entries -notcontains $binDir) {
			try {
				[Environment]::SetEnvironmentVariable('Path', (@($binDir) + $entries) -join ';', 'User')
			} catch {
				Write-Warning "Could not update the user PATH. Add $binDir to PATH manually."
			}
		}
		if (($env:Path -split ';') -notcontains $binDir) {
			$env:Path = "$binDir;$env:Path"
		}

		Write-Host "retype installed at $targetBinary"
		Write-Host "  $output"
		Write-Host "Previous installation backup: $backup"
		Write-Host ''
		Write-Host 'Next: retype doctor'
	} catch {
		$failure = $_
		if ($backup) {
			try {
				if ($changedBin) {
					$previousBinary = Join-Path $backup 'retype.exe'
					if (Test-Path -LiteralPath $previousBinary) {
						Copy-Item -LiteralPath $previousBinary -Destination $stagedBin -Force
						Move-Item -LiteralPath $stagedBin -Destination $targetBinary -Force
					} elseif (Test-Path -LiteralPath $targetBinary) {
						Remove-Item -LiteralPath $targetBinary -Force
					}
				}
				if ($changedShare) {
					$previousShare = Join-Path $backup 'share'
					if (Test-Path -LiteralPath $previousShare) {
						if (Test-Path -LiteralPath $shareDir) { Remove-Item -LiteralPath $shareDir -Recurse -Force }
						Move-Item -LiteralPath $previousShare -Destination $shareDir
					} elseif (-not $hadShare -and (Test-Path -LiteralPath $shareDir)) {
						Remove-Item -LiteralPath $shareDir -Recurse -Force
					}
				}
			} catch {
				throw "Install failed: $failure. Rollback failed: $_. Recover the previous installation from $backup"
			}
			Write-Warning "Previous installation preserved; backup: $backup"
		}
		throw $failure
	} finally {
		foreach ($path in @($stagedBin, $stagedShare)) {
			if ($path -and (Test-Path -LiteralPath $path)) {
				Remove-Item -Recurse -Force -LiteralPath $path -ErrorAction SilentlyContinue
			}
		}
		Remove-Item -Recurse -Force -LiteralPath $temp -ErrorAction SilentlyContinue
	}
}
