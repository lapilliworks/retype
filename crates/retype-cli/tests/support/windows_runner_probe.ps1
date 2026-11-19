param(
	[Parameter(Mandatory = $true)][string]$Runner,
	[Parameter(Mandatory = $true)][string]$Scratch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'These checks require PowerShell 7.' }
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Runner, [ref]$tokens, [ref]$errors)
if ($null -ne $errors -and $errors.Count -ne 0) { throw "Could not parse the runner: $errors" }
foreach ($name in @('Probe', 'Run-Step', 'Run-SuiteStep', 'Test-NextestSummary')) {
	$function = $ast.Find({
		param($node)
		$node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
	}, $true)
	if (-not $function) { throw "The runner has no $name function." }
	. ([ScriptBlock]::Create($function.Extent.Text))
}
$Report = $Scratch

$script:Summary = New-Object 'System.Collections.Generic.List[System.String]'
$script:ExpectedSeconds = 10
$script:ProbeStdout = Join-Path $Scratch 'stdout.log'
$script:ProbeStderr = Join-Path $Scratch 'stderr.log'
[IO.File]::WriteAllText($script:ProbeStderr, '')

function Write-Summary([string]$Message) {
	$script:Summary.Add($Message)
}

function Invoke-Bounded {
	param([string]$Name, [string]$Executable, [string[]]$Arguments, [int]$Seconds)
	if ($Seconds -ne $script:ExpectedSeconds) { throw "Unexpected timeout: $Seconds" }
	return [pscustomobject]@{
		Code = $script:ProbeCode
		TimedOut = $script:ProbeTimedOut
		Stdout = $script:ProbeStdout
		Stderr = $script:ProbeStderr
	}
}

$cases = @(
	@{ Name = 'magick-version'; Code = 0; TimedOut = $false; Output = ''; Failed = $false }
	@{ Name = 'magick-formats'; Code = 0; TimedOut = $false; Output = 'HEIC r--'; Failed = $false }
	@{ Name = 'magick-formats'; Code = 0; TimedOut = $false; Output = ''; Failed = $true }
	@{ Name = 'magick-formats'; Code = 0; TimedOut = $false; Output = " `n`t"; Failed = $true }
	@{ Name = 'magick-formats'; Code = 42; TimedOut = $false; Output = ''; Failed = $true }
	@{ Name = 'magick-formats'; Code = 124; TimedOut = $true; Output = ''; Failed = $true }
	@{ Name = 'magick-version'; Code = 127; TimedOut = $false; Output = ''; Failed = $true }
	@{ Name = 'heic-decode'; Code = 0; TimedOut = $false; Output = ''; Failed = $false }
	@{ Name = 'heic-decode'; Code = 1; TimedOut = $false; Output = ''; Failed = $true }
)
foreach ($case in $cases) {
	$script:Summary.Clear()
	$script:ProbeCode = $case.Code
	$script:ProbeTimedOut = $case.TimedOut
	$script:Failed = $false
	[IO.File]::WriteAllText($script:ProbeStdout, $case.Output)
	$null = Probe -Name $case.Name -Executable 'unused' -Arguments @('-list', 'format')
	if ($script:Failed -ne $case.Failed) { throw "Incorrect failure state for $($case.Name), exit $($case.Code)." }
	$expected = "probe-$($case.Name): exit $($case.Code); timed out: $($case.TimedOut);"
	if (-not ($script:Summary | Where-Object { $_.Contains($expected) })) {
		throw "Missing status '$expected': $($script:Summary -join ', ')"
	}
	if ($case.Name -eq 'magick-formats' -and $case.Code -eq 0 -and [string]::IsNullOrWhiteSpace($case.Output) -and
		-not ($script:Summary | Where-Object { $_.Contains('empty format listing') })) {
		throw 'An empty format listing was not reported.'
	}
}

$script:Failed = $true
$script:ProbeCode = 0
$script:ProbeTimedOut = $false
$null = Probe -Name 'magick-version' -Executable 'unused' -Arguments @('-version')
if (-not $script:Failed) { throw 'A passing probe cleared an earlier failure.' }

$script:ExpectedSeconds = 73
$stepCases = @(
	@{ Code = 0; TimedOut = $false; Failed = $false }
	@{ Code = 42; TimedOut = $false; Failed = $true }
	@{ Code = 124; TimedOut = $true; Failed = $true }
)
foreach ($case in $stepCases) {
	$script:Summary.Clear()
	$script:ProbeCode = $case.Code
	$script:ProbeTimedOut = $case.TimedOut
	$script:Failed = $false
	$result = Run-Step -Name 'workspace' -Seconds 73 -Executable 'unused' -Arguments @()
	if ($result.Code -ne $case.Code -or $script:Failed -ne $case.Failed) {
		throw "Incorrect phase result for exit $($case.Code)."
	}
	$expected = "workspace: exit $($case.Code); timed out: $($case.TimedOut); limit: 73s; elapsed: "
	if (-not ($script:Summary | Where-Object { $_.Contains($expected) })) {
		throw "Missing phase status '$expected': $($script:Summary -join ', ')"
	}
}
$Limit = 1
$script:Failed = $false
$script:SuiteWatch = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Milliseconds 1100
$result = Run-SuiteStep -Name 'exhausted' -Executable 'unused' -Arguments @()
if ($result.Code -ne 124 -or -not $result.TimedOut -or -not $script:Failed) {
	throw 'An exhausted suite budget was not reported as incomplete.'
}
$summaryResult = [pscustomobject]@{ Stdout = $script:ProbeStdout; Stderr = $script:ProbeStderr }
foreach ($case in @(
	@{ Text = 'Summary [1s] 3 tests run: 3 passed'; Failed = $false }
	@{ Text = 'Summary [1s] 0 tests run: 0 passed'; Failed = $true }
	@{ Text = 'Summary [1s] 3 tests run: 2 passed, 1 skipped'; Failed = $true }
)) {
	$script:Failed = $false
	[IO.File]::WriteAllText($script:ProbeStdout, $case.Text)
	Test-NextestSummary -Name 'workspace' -Result $summaryResult
	if ($script:Failed -ne $case.Failed) { throw "Wrong summary result for '$($case.Text)'." }
}
$validation = @($ast.EndBlock.Statements | Where-Object {
	$_ -is [System.Management.Automation.Language.TryStatementAst]
})
if ($validation.Count -ne 1) { throw 'Expected one top-level validation try/catch/finally.' }
$finalize = [ScriptBlock]::Create(($validation[0].Finally.Statements.Extent.Text -join "`n"))
$failure = [ScriptBlock]::Create(($validation[0].CatchClauses[0].Body.Statements.Extent.Text -join "`n"))
$Names = @()
$script:ActiveProcess = $null
function Stop-ScratchProcesses {
	if ($script:CleanupFails) { throw 'synthetic cleanup failure' }
}
foreach ($case in @(
	@{ Completed = $false; Failed = $false; CleanupFails = $false; Expected = 'Result: failed' }
	@{ Completed = $false; Failed = $true; CleanupFails = $false; Expected = 'Result: failed' }
	@{ Completed = $true; Failed = $false; CleanupFails = $false; Expected = 'Result: passed' }
	@{ Completed = $true; Failed = $true; CleanupFails = $false; Expected = 'Result: failed' }
	@{ Completed = $true; Failed = $false; CleanupFails = $true; Expected = 'Result: failed' }
)) {
	$script:Summary.Clear()
	$script:Completed = $case.Completed
	$script:Failed = $case.Failed
	$script:CleanupFails = $case.CleanupFails
	. $finalize
	if ($script:Summary[-1] -ne $case.Expected) { throw "Incorrect final result: $($script:Summary -join ', ')" }
	if (-not $case.Completed -and -not $script:Summary.Contains('Platform validation stopped before completion.')) {
		throw 'An interrupted validation was not reported.'
	}
	if ($case.CleanupFails -and -not ($script:Summary | Where-Object { $_.Contains('synthetic cleanup failure') })) {
		throw 'The cleanup exception was not saved in the report.'
	}
}
$script:Summary.Clear()
$script:Failed = $false
try { throw 'synthetic validation failure' } catch { . $failure }
if (-not $script:Failed -or -not ($script:Summary | Where-Object { $_.Contains('synthetic validation failure') })) {
	throw 'The validation exception was not saved in the report.'
}
$clearEnvironment = @($validation[0].Body.Statements | Where-Object {
	$_ -is [System.Management.Automation.Language.ForEachStatementAst] -and
		$_.Variable.VariablePath.UserPath -eq 'name'
})
if ($clearEnvironment.Count -ne 1) { throw 'Expected one environment-clearing loop.' }
$env:NEXTEST_TEST_THREADS = '7'
$env:NEXTEST_RETRIES = '4'
$env:RETYPE_DAEMON = 'inherited-daemon'
$env:RETYPE_TEST_RECOVERY_ROOT = 'inherited-fixture'
$env:RETYPE_TEST_RECOVERY_TOKEN = 'inherited-token'
$env:RETYPE_TEST_RECOVERY_CHECKPOINT = 'inherited-checkpoint'
. ([ScriptBlock]::Create($clearEnvironment[0].Extent.Text))
foreach ($name in @('NEXTEST_TEST_THREADS', 'NEXTEST_RETRIES', 'RETYPE_DAEMON',
	'RETYPE_TEST_RECOVERY_ROOT', 'RETYPE_TEST_RECOVERY_TOKEN', 'RETYPE_TEST_RECOVERY_CHECKPOINT')) {
	if ($null -ne [Environment]::GetEnvironmentVariable($name, 'Process')) {
		throw "The runner left $name set instead of removing it."
	}
}
$restoreEnvironment = @($validation[0].Finally.Statements | Where-Object {
	$_ -is [System.Management.Automation.Language.ForEachStatementAst]
})
if ($restoreEnvironment.Count -ne 1) { throw 'Expected one environment-restoring loop.' }
$Names = @('NEXTEST_TEST_THREADS', 'NEXTEST_RETRIES', 'RETYPE_DAEMON')
$env:RETYPE_DAEMON = ''
$OriginalEnvironment = @{
	NEXTEST_TEST_THREADS = '7'
	NEXTEST_RETRIES = $null
	RETYPE_DAEMON = [Environment]::GetEnvironmentVariable('RETYPE_DAEMON', 'Process')
}
foreach ($name in $Names) { [Environment]::SetEnvironmentVariable($name, 'modified', 'Process') }
. ([ScriptBlock]::Create($restoreEnvironment[0].Extent.Text))
foreach ($name in $Names) {
	if ([Environment]::GetEnvironmentVariable($name, 'Process') -cne $OriginalEnvironment[$name]) {
		throw "The runner did not restore the original value or absence of $name."
	}
}
Write-Output 'Windows runner reporting checks passed.'
