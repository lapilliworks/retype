#requires -Version 7.0
# Run trusted qualification commands with a verified non-administrator token.
[CmdletBinding()]
param(
	[Parameter(Mandatory)][string]$Command,
	[Parameter(Mandatory)][ValidateSet('X64', 'Arm64')][string]$Architecture
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$directory = Join-Path $env:RETYPE_TEST_REPORT_DIR ('user-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $directory | Out-Null
$worker = Join-Path $directory 'worker.ps1'
$workingDirectory = if ($env:GITHUB_WORKSPACE) { $env:GITHUB_WORKSPACE } else { (Get-Location).Path }
$body = @'
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$directory = $PSScriptRoot
Set-Location -LiteralPath '__WORKING_DIRECTORY__'
$PID | Set-Content (Join-Path $directory 'pid')
Start-Transcript -Path (Join-Path $directory 'worker.log') | Out-Null
$code = 1
try {
	$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
	$principal = [Security.Principal.WindowsPrincipal]::new($identity)
	if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
		throw 'Refusing elevated product qualification.'
	}
	$os = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
	$process = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
	if ($os -ne '__ARCHITECTURE__' -or $process -ne $os) { throw 'Expected native OS and PowerShell architecture.' }
	@{ Elevated = $false; OSArchitecture = $os; ProcessArchitecture = $process; User = $identity.Name } |
		ConvertTo-Json | Set-Content (Join-Path $directory 'identity.json')
	& whoami.exe /all | Set-Content (Join-Path $directory 'token.txt')
	$global:LASTEXITCODE = 0
	& {
__COMMAND__
	}
	if ($LASTEXITCODE -ne 0) { throw "Qualification exited with $LASTEXITCODE." }
	$code = 0
} catch {
	Write-Host "$_`n$($_.ScriptStackTrace)"
} finally {
	Stop-Transcript | Out-Null
	@{ ExitCode = $code } | ConvertTo-Json | Set-Content (Join-Path $directory 'result.tmp')
	Move-Item -LiteralPath (Join-Path $directory 'result.tmp') -Destination (Join-Path $directory 'result.json')
}
exit $code
'@
$body.Replace('__ARCHITECTURE__', $Architecture).Replace('__WORKING_DIRECTORY__', $workingDirectory.Replace("'", "''")).Replace('__COMMAND__', $Command) | Set-Content $worker
$pwsh = (Get-Process -Id $PID).Path
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("& '$worker'"))
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
	& runas.exe /trustlevel:0x20000 "`"$pwsh`" -NoLogo -NoProfile -NonInteractive -EncodedCommand $encoded"
	if ($LASTEXITCODE -ne 0) { throw 'Could not launch the restricted worker.' }
} else {
	Start-Process -FilePath $pwsh -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded) -NoNewWindow | Out-Null
}
$startup = [DateTime]::UtcNow.AddSeconds(30)
$deadline = [DateTime]::UtcNow.AddMinutes(45)
$result = Join-Path $directory 'result.json'
while (-not (Test-Path -LiteralPath $result)) {
	if ([DateTime]::UtcNow -gt $startup -and -not (Test-Path -LiteralPath (Join-Path $directory 'pid'))) {
		throw "Restricted worker did not start; reports: $directory"
	}
	if ([DateTime]::UtcNow -gt $deadline) {
		$pidFile = Join-Path $directory 'pid'
		if (Test-Path -LiteralPath $pidFile) {
			$workerPid = [int](Get-Content -LiteralPath $pidFile -Raw)
			& taskkill.exe /PID $workerPid /T /F
		}
		throw "Non-administrator qualification timed out; reports: $directory"
	}
	Start-Sleep -Seconds 1
}
Get-Content -LiteralPath (Join-Path $directory 'worker.log')
$outcome = Get-Content -LiteralPath $result -Raw | ConvertFrom-Json
if ($outcome.ExitCode -ne 0) { throw "Non-administrator qualification failed; reports: $directory" }
