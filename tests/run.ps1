#requires -Version 7.0
# Run from native Windows; -Check probes prerequisites without running the suite.
[CmdletBinding()]
param([switch]$Check)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
	Write-Error 'Run this script on native Windows.' -ErrorAction Continue
	exit 2
}
$Root = Split-Path -Parent $PSScriptRoot
foreach ($name in @('git', 'cargo', 'rustc')) {
	if (-not (Get-Command $name -CommandType Application -ErrorAction SilentlyContinue)) {
		Write-Error "Required command is missing: $name" -ErrorAction Continue
		exit 2
	}
}

$Limit = 300
$configuredLimit = [Environment]::GetEnvironmentVariable('RETYPE_TEST_TIMEOUT_SECONDS', 'Process')
if ($configuredLimit) {
	$parsed = 0
	if (-not [int]::TryParse($configuredLimit, [ref]$parsed) -or $parsed -lt 1 -or $parsed -gt 3600) {
		Write-Error 'RETYPE_TEST_TIMEOUT_SECONDS must be an integer between 1 and 3600.' -ErrorAction Continue
		exit 2
	}
	$Limit = $parsed
}

$ReportBase = [Environment]::GetEnvironmentVariable('RETYPE_TEST_REPORT_DIR', 'Process')
if (-not $ReportBase) { $ReportBase = Join-Path $Root 'target\test-reports' }
$ScratchBase = [Environment]::GetEnvironmentVariable('RETYPE_TEST_TEMP_DIR', 'Process')
if (-not $ScratchBase) { $ScratchBase = [IO.Path]::GetTempPath() }
if ($ReportBase -notmatch '^([A-Za-z]:[\\/]|\\\\)' -or $ScratchBase -notmatch '^([A-Za-z]:[\\/]|\\\\)') {
	Write-Error 'RETYPE_TEST_REPORT_DIR and RETYPE_TEST_TEMP_DIR must be absolute paths.' -ErrorAction Continue
	exit 2
}
$ReportBase = [IO.Path]::GetFullPath($ReportBase)
$ScratchBase = [IO.Path]::GetFullPath($ScratchBase)
if (-not (Test-Path -LiteralPath $ScratchBase -PathType Container)) {
	Write-Error "Test temporary directory does not exist: $ScratchBase" -ErrorAction Continue
	exit 2
}
[IO.Directory]::CreateDirectory($ReportBase) | Out-Null
$stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
$Report = Join-Path $ReportBase ('Windows-' + $stamp + '-' + [guid]::NewGuid().ToString('N'))
$Scratch = Join-Path $ScratchBase ('retype-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Report | Out-Null
New-Item -ItemType Directory -Path $Scratch | Out-Null
Write-Host "Platform test: Windows`nReports: $Report`nScratch retained: $Scratch"

$Names = @('TEMP', 'TMP', 'LOCALAPPDATA', 'RETYPE_CONFIG', 'RETYPE_STATE_DIR', 'RETYPE_NO_SPAWN',
	'RETYPE_REQUIRE_TEST_TOOLS', 'RETYPE_REQUIRE_NATIVE_WORKFLOW', 'RETYPE_TEST_SESSION', 'RETYPE_DAEMON', 'RETYPE_FIXTURES',
	'RETYPE_FIDELITY_MANIFEST', 'NEXTEST_RETRIES', 'NEXTEST_TEST_THREADS', 'NEXTEST_MESSAGE_FORMAT')
$OriginalEnvironment = @{}
foreach ($name in $Names) {
	$OriginalEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$script:Failed = $false
$script:Completed = $false
$script:ActiveProcess = $null
function Write-Summary([string]$Message) {
	[IO.File]::AppendAllText((Join-Path $Report 'summary.txt'), $Message + [Environment]::NewLine)
	Write-Host $Message
}

function Stop-ProcessTree([System.Diagnostics.Process]$Process) {
	if ($Process.HasExited) { return }
	$taskkill = Join-Path $env:SystemRoot 'System32\taskkill.exe'
	try {
		$killer = Start-Process -FilePath $taskkill -ArgumentList @('/PID', "$($Process.Id)", '/T', '/F') `
			-PassThru -WindowStyle Hidden
		if (-not $killer.WaitForExit(5000)) {
			$killer.Kill()
			$null = $killer.WaitForExit(1000)
			Write-Warning 'taskkill timed out.'
			$script:Failed = $true
		}
	} catch {
		Write-Warning "Could not stop the process tree for $($Process.Id): $_"
		$script:Failed = $true
	}
	if (-not $Process.WaitForExit(5000)) {
		try {
			$Process.Kill()
			if (-not $Process.WaitForExit(1000)) {
				Write-Warning "Process $($Process.Id) did not exit."
				$script:Failed = $true
			}
		} catch {
			if (-not $Process.HasExited) {
				Write-Warning "Could not stop process $($Process.Id): $_"
				$script:Failed = $true
			}
		}
	}
}

function Invoke-Bounded {
	param([string]$Name, [string]$Executable, [string[]]$Arguments, [int]$Seconds)
	$directory = if ($Name -match '^(probe-|git-)') { Join-Path $Report 'metadata' } else { $Report }
	[IO.Directory]::CreateDirectory($directory) | Out-Null
	$stdout = Join-Path $directory "$Name.stdout.log"
	$stderr = Join-Path $directory "$Name.stderr.log"
	$command = Get-Command $Executable -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
	if (-not $command) {
		[IO.File]::WriteAllText($stderr, "Not installed: $Executable`n")
		[IO.File]::WriteAllText($stdout, '')
		return [pscustomobject]@{ Code = 127; Stdout = $stdout; Stderr = $stderr; TimedOut = $false }
	}
	try {
		$process = Start-Process -FilePath $command.Path -ArgumentList $Arguments -WorkingDirectory $Root `
			-RedirectStandardOutput $stdout -RedirectStandardError $stderr -RedirectStandardInput $script:EmptyInput `
			-NoNewWindow -PassThru
		$script:ActiveProcess = $process
		$watch = [Diagnostics.Stopwatch]::StartNew()
		$nextNotice = 15
		while (-not $process.HasExited) {
			if ($watch.Elapsed.TotalSeconds -ge $Seconds) {
				Write-Warning "$Name exceeded ${Seconds}s; stopping its process tree."
				Stop-ProcessTree $process
				$script:ActiveProcess = $null
				return [pscustomobject]@{ Code = 124; Stdout = $stdout; Stderr = $stderr; TimedOut = $true }
			}
			if ($watch.Elapsed.TotalSeconds -ge $nextNotice) {
				Write-Host "$Name still running ($([int]$watch.Elapsed.TotalSeconds)s); logs: $Report"
				$nextNotice += 15
			}
			Start-Sleep -Milliseconds 250
			$process.Refresh()
		}
		$process.WaitForExit()
		$code = $process.ExitCode
		$script:ActiveProcess = $null
		if ($null -eq $code) {
			[IO.File]::AppendAllText($stderr, "Process exited without an available exit status.`n")
			$code = 1
		}
		return [pscustomobject]@{ Code = $code; Stdout = $stdout; Stderr = $stderr; TimedOut = $false }
	} catch {
		if ($script:ActiveProcess) { Stop-ProcessTree $script:ActiveProcess; $script:ActiveProcess = $null }
		[IO.File]::WriteAllText($stderr, "Could not run ${Executable}: $_`n")
		if (-not (Test-Path -LiteralPath $stdout)) { [IO.File]::WriteAllText($stdout, '') }
		return [pscustomobject]@{ Code = 1; Stdout = $stdout; Stderr = $stderr; TimedOut = $false }
	}
}

function Run-Step {
	param([string]$Name, [int]$Seconds, [string]$Executable, [string[]]$Arguments)
	Write-Summary "${Name}: started; limit: ${Seconds}s"
	$watch = [Diagnostics.Stopwatch]::StartNew()
	$result = Invoke-Bounded -Name $Name -Executable $Executable -Arguments $Arguments -Seconds $Seconds
	$elapsed = [Math]::Round($watch.Elapsed.TotalSeconds, 3)
	$message = "${Name}: exit $($result.Code); timed out: $($result.TimedOut); limit: ${Seconds}s; elapsed: ${elapsed}s; "
	$message += "stdout: $($result.Stdout); stderr: $($result.Stderr)"
	Write-Summary $message
	if ($result.Code -ne 0 -or $result.TimedOut) { $script:Failed = $true }
	return $result
}

function Run-SuiteStep {
	param([string]$Name, [string]$Executable, [string[]]$Arguments)
	$remaining = $Limit - [int]($script:SuiteWatch.Elapsed.TotalSeconds)
	if ($remaining -le 0) {
		$stdout = Join-Path $Report "$Name.stdout.log"
		$stderr = Join-Path $Report "$Name.stderr.log"
		[IO.File]::WriteAllText($stdout, '')
		[IO.File]::WriteAllText($stderr, "Not run: the ${Limit}s test-suite budget was exhausted.`n")
		Write-Summary "${Name}: not run; shared ${Limit}s test-suite budget exhausted; stderr: $stderr"
		$script:Failed = $true
		return [pscustomobject]@{ Code = 124; Stdout = $stdout; Stderr = $stderr; TimedOut = $true }
	}
	return (Run-Step -Name $Name -Seconds $remaining -Executable $Executable -Arguments $Arguments)
}

function Test-NextestSummary {
	param([string]$Name, $Result)
	$logs = @($Result.Stdout, $Result.Stderr)
	if (-not (Select-String -Path $logs -Pattern 'Summary .*\b[1-9][0-9]* passed' -Quiet)) {
		Write-Summary "${Name}: no passing nextest summary was recorded."
		$script:Failed = $true
	}
	if (Select-String -Path $logs -Pattern 'Summary .*\b[1-9][0-9]* skipped' -Quiet) {
		Write-Summary "${Name}: nextest reported skipped tests."
		$script:Failed = $true
	}
}

function Probe {
	param([string]$Name, [string]$Executable, [string[]]$Arguments)
	$result = Run-Step -Name "probe-$Name" -Executable $Executable -Arguments $Arguments -Seconds 10
	if ($result.Code -eq 0 -and $Name -eq 'magick-formats' -and
		[string]::IsNullOrWhiteSpace([IO.File]::ReadAllText($result.Stdout))) {
		Write-Summary "probe-magick-formats: empty format listing; stdout: $($result.Stdout)"
		$script:Failed = $true
	}
	return $result
}

function Test-InstallerSyntax {
	$log = Join-Path $Report 'installer-syntax.log'
	try {
		$tokens = $null
		$errors = $null
		$null = [System.Management.Automation.Language.Parser]::ParseFile(
			(Join-Path $Root 'packaging\install.ps1'), [ref]$tokens, [ref]$errors)
		if ($null -ne $errors -and $errors.Count -gt 0) {
			[IO.File]::WriteAllLines($log, [string[]]($errors | ForEach-Object { $_.ToString() }))
			Write-Summary "installer-syntax: failed; log: $log"
			$script:Failed = $true
		} else {
			[IO.File]::WriteAllText($log, "PowerShell installer parsed successfully.`n")
			Write-Summary "installer-syntax: passed; log: $log"
		}
	} catch {
		[IO.File]::WriteAllText($log, "Installer parser failed: $_`n")
		Write-Summary "installer-syntax: failed; log: $log"
		$script:Failed = $true
	}
}

function Stop-ScratchProcesses {
	$job = $null
	try {
		$job = Start-Job -ArgumentList $Scratch, $PID -ScriptBlock {
			param($Path, $RunnerPid)
			$ErrorActionPreference = 'Stop'
			$names = @('retype.exe', 'magick.exe', 'ffmpeg.exe', 'ffprobe.exe', 'pandoc.exe',
				'7z.exe', '7zz.exe', '7za.exe', 'typst.exe', 'xelatex.exe', 'lualatex.exe',
				'pdflatex.exe', 'weasyprint.exe', 'wkhtmltopdf.exe', 'cmd.exe', 'powershell.exe', 'pwsh.exe')
			$leaks = Get-CimInstance Win32_Process -ErrorAction Stop | Where-Object {
				$_.ProcessId -ne $RunnerPid -and $_.ProcessId -ne $PID -and
				$_.Name.ToLowerInvariant() -in $names -and $_.CommandLine -and
				$_.CommandLine.IndexOf($Path + '\', [StringComparison]::OrdinalIgnoreCase) -ge 0
			}
			foreach ($leak in $leaks) {
				# Recheck the command line before terminating a process from the private scratch path.
				$current = Get-CimInstance Win32_Process -Filter "ProcessId = $($leak.ProcessId)" -ErrorAction Stop
				if ($current -and $current.Name -eq $leak.Name -and $current.CommandLine -and
					$current.CommandLine.IndexOf($Path + '\', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
					$process = Get-Process -Id $current.ProcessId -ErrorAction Stop
					$process.Kill()
					if (-not $process.WaitForExit(3000)) { throw "Process $($current.ProcessId) did not exit." }
					[pscustomobject]@{ ProcessId = $current.ProcessId; Name = $current.Name }
				}
			}
		}
		if (-not (Wait-Job -Job $job -Timeout 15)) {
			Stop-Job -Job $job
			Write-Summary 'Timed out checking for leaked test processes.'
			$script:Failed = $true
			return
		}
		$leaks = @(Receive-Job -Job $job -ErrorAction Stop)
		foreach ($leak in $leaks) {
			Write-Summary "Stopped leaked test process $($leak.ProcessId) ($($leak.Name))."
			$script:Failed = $true
		}
	} catch {
		Write-Summary "Could not complete test-process cleanup: $_"
		$script:Failed = $true
	} finally {
		if ($job) { Remove-Job -Job $job -Force -ErrorAction SilentlyContinue }
	}
}

try {
	$Config = Join-Path $Scratch 'retype.toml'
	[IO.File]::WriteAllText(
		$Config, "version = 'retype-config-1'`n[notify]`nenabled = false`n", [Text.UTF8Encoding]::new($false))
	$env:TEMP = $Scratch
	$env:TMP = $Scratch
	$env:LOCALAPPDATA = Join-Path $Scratch 'local'
	$env:RETYPE_CONFIG = $Config
	$env:RETYPE_REQUIRE_TEST_TOOLS = '1'
	$env:RETYPE_REQUIRE_NATIVE_WORKFLOW = '1'
	# Controlled debug session, not evidence of native login/logout behavior.
	$env:RETYPE_TEST_SESSION = 'terminal'
	foreach ($name in @('RETYPE_STATE_DIR', 'RETYPE_NO_SPAWN', 'RETYPE_DAEMON', 'RETYPE_FIXTURES',
		'RETYPE_FIDELITY_MANIFEST', 'NEXTEST_RETRIES', 'NEXTEST_TEST_THREADS', 'NEXTEST_MESSAGE_FORMAT')) {
		# A real null removes the variable; PowerShell can coerce $null to an empty .NET string.
		[Environment]::SetEnvironmentVariable($name, [NullString]::Value, 'Process')
	}
	$script:EmptyInput = Join-Path $Scratch 'stdin'
	[IO.File]::WriteAllText($script:EmptyInput, '')

	$hostInfo = @(
		"Platform: $([Environment]::OSVersion.VersionString)",
		"Architecture: $env:PROCESSOR_ARCHITECTURE",
		"PowerShell: $($PSVersionTable.PSVersion)",
		"Configured timeout seconds: $Limit",
		"Scratch: $Scratch"
	)
	[IO.File]::WriteAllLines((Join-Path $Report 'host.txt'), [string[]]$hostInfo)
	$revision = Invoke-Bounded -Name 'git-revision' -Executable 'git' `
		-Arguments @('rev-parse', '--verify', 'HEAD') -Seconds 10
	if ($revision.Code -ne 0) {
		throw "Cannot determine the Git revision (exit $($revision.Code)); see $($revision.Stdout) and $($revision.Stderr)"
	}
	$null = Run-Step -Name 'git-diff' -Seconds 10 -Executable 'git' `
		-Arguments @('diff', '--binary', '--no-ext-diff', '--no-textconv', 'HEAD')
	$gitStatus = Invoke-Bounded -Name 'git-status' -Executable 'git' -Arguments @('status', '--short') -Seconds 10
	if ($gitStatus.Code -ne 0) {
		Write-Summary "Could not inspect the working tree; see $($gitStatus.Stderr)."
		$script:Failed = $true
	}
	$null = Probe -Name 'rustc' -Executable 'rustc' -Arguments @('-vV')
	$null = Probe -Name 'cargo' -Executable 'cargo' -Arguments @('--version')
	$null = Probe -Name 'nextest' -Executable 'cargo' -Arguments @('nextest', 'show-config', 'version',
		'--config-file', 'tests/nextest.toml', '--user-config-file', 'none')
	$null = Probe -Name 'magick-version' -Executable 'magick' -Arguments @('-version')
	$null = Probe -Name 'magick-formats' -Executable 'magick' -Arguments @('-list', 'format')
	$null = Probe -Name 'heic-decode' -Executable 'magick' -Arguments @('fixtures/native-watch/photo.heic', 'null:')
	$null = Probe -Name 'ffmpeg-version' -Executable 'ffmpeg' -Arguments @('-version')
	$null = Probe -Name 'ffmpeg-encoders' -Executable 'ffmpeg' -Arguments @('-hide_banner', '-encoders')
	$null = Probe -Name 'ffprobe' -Executable 'ffprobe' -Arguments @('-version')
	$null = Probe -Name 'pandoc' -Executable 'pandoc' -Arguments @('--version')
	$engineFound = $false
	foreach ($engine in @('typst', 'xelatex', 'lualatex', 'pdflatex', 'weasyprint', 'wkhtmltopdf')) {
		if (Get-Command $engine -CommandType Application -ErrorAction SilentlyContinue) {
			$null = Probe -Name $engine -Executable $engine -Arguments @('--version')
			$engineFound = $true
		}
	}
	if (-not $engineFound) {
		$engineLog = Join-Path $Report 'probe-pdf-engine.stderr.log'
		[IO.File]::WriteAllText($engineLog, "No PDF engine found.`n")
		Write-Summary "probe-pdf-engine: not found; stderr: $engineLog"
		$script:Failed = $true
	}
	$archiverFound = $false
	foreach ($archiver in @('7zz', '7z', '7za')) {
		if (Get-Command $archiver -CommandType Application -ErrorAction SilentlyContinue) {
			$null = Probe -Name $archiver -Executable $archiver -Arguments @('i')
			$archiverFound = $true
			break
		}
	}
	if (-not $archiverFound) {
		$archiverLog = Join-Path $Report 'probe-7zip.stderr.log'
		[IO.File]::WriteAllText($archiverLog, "No 7-Zip executable found.`n")
		Write-Summary "probe-7zip: not found; stderr: $archiverLog"
		$script:Failed = $true
	}

	if (-not $Check) {
		$null = Run-Step -Name 'format' -Seconds 60 -Executable 'cargo' -Arguments @('fmt', '--all', '--', '--check')
		$null = Run-Step -Name 'clippy' -Seconds $Limit -Executable 'cargo' `
			-Arguments @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
		Test-InstallerSyntax
		$build = Run-Step -Name 'build' -Seconds $Limit -Executable 'cargo' `
			-Arguments @('test', '--workspace', '--locked', '--no-run')
		if ($build.Code -eq 0 -and -not $build.TimedOut) {
			$nextest = @('nextest', 'run', '--workspace', '--locked', '--config-file', 'tests/nextest.toml',
				'--profile', 'platform', '--user-config-file', 'none', '--ignore-default-filter', '--run-ignored', 'all',
				'--status-level', 'pass', '--color', 'never', '--success-output', 'immediate',
				'--failure-output', 'immediate', '--no-tests', 'fail')
			$script:SuiteWatch = [Diagnostics.Stopwatch]::StartNew()
			$workspace = Run-SuiteStep -Name 'workspace' -Executable 'cargo' -Arguments $nextest
			$doctests = Run-SuiteStep -Name 'doctests' -Executable 'cargo' `
				-Arguments @('test', '--workspace', '--doc', '--locked', '--no-fail-fast', '--', '--nocapture')
			$workspaceLogs = @($workspace.Stdout, $workspace.Stderr)
			foreach ($workflow in @('a_real_watched_heic_conversion_and_undo_preserve_the_original',
				'a_real_direct_heic_conversion_and_undo_follow_watched_disposition')) {
				$pattern = 'PASS\s+\[.*\].*' + [regex]::Escape($workflow) + '(\s|$)'
				if (-not (Select-String -Path $workspaceLogs -Pattern $pattern -Quiet)) {
					Write-Summary "The strict native workflow test did not pass: $workflow"
					$script:Failed = $true
				}
			}
			Test-NextestSummary -Name 'workspace' -Result $workspace
			$docLogs = @($doctests.Stdout, $doctests.Stderr)
			if (-not (Select-String -Path $docLogs -SimpleMatch 'Doc-tests' -Quiet) -or
				-not (Select-String -Path $docLogs -SimpleMatch 'test result:' -Quiet)) {
				Write-Summary 'No doctest summaries were recorded.'
				$script:Failed = $true
			}
			$logs = $workspaceLogs + $docLogs
			if (Select-String -Path $logs -Pattern 'SKIPPED:|so this test proves nothing|[1-9][0-9]* ignored' -Quiet) {
				Write-Summary 'Test output contains a skip or ignored test; release evidence is incomplete.'
				$script:Failed = $true
			}
			if (Select-String -Path $logs `
				-Pattern '^\s*(FAIL|TIMEOUT|LEAK)\s+\[|test result: FAILED|^error: test failed' -Quiet) {
				Write-Summary 'Test output contains a failed or timed-out result.'
				$script:Failed = $true
			}
			if ($script:Failed) {
				Select-String -Path $logs -Pattern 'FAILED|TIMEOUT|LEAK|release workflow|SKIPPED:' |
					Select-Object -Last 30 | ForEach-Object { Write-Host $_ }
			}
		} else {
			Write-Summary 'Workspace and doctests not run: build failed.'
		}
	}
	$script:Completed = $true
} catch {
	$script:Failed = $true
	Write-Summary "Platform validation could not finish: $_`n$($_.ScriptStackTrace)"
} finally {
	if (-not $script:Completed) {
		$script:Failed = $true
		Write-Summary 'Platform validation stopped before completion.'
	}
	try {
		if ($script:ActiveProcess) { Stop-ProcessTree $script:ActiveProcess; $script:ActiveProcess = $null }
		Stop-ScratchProcesses
	} catch {
		$script:Failed = $true
		Write-Summary "Test-process cleanup failed: $_`n$($_.ScriptStackTrace)"
	}
	foreach ($name in $Names) {
		$value = $OriginalEnvironment[$name]
		if ($null -eq $value) { $value = [NullString]::Value }
		[Environment]::SetEnvironmentVariable($name, $value, 'Process')
	}
	Write-Summary $(if ($script:Failed) { 'Result: failed' } else { 'Result: passed' })
	Write-Host "`nReports: $Report`nScratch retained: $Scratch"
}
if ($script:Failed) {
	Write-Error "Platform validation failed. See $Report\summary.txt and the step logs." -ErrorAction Continue
	exit 1
}
if ($Check) {
	Write-Host 'Prerequisite probes passed; this is not conversion or undo evidence.'
} else {
	Write-Host 'Native validation passed.'
}
