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
$probe = $ast.Find({
	param($node)
	$node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Probe'
}, $true)
if (-not $probe) { throw 'The runner has no Probe function.' }
. ([ScriptBlock]::Create($probe.Extent.Text))
$runStep = $ast.Find({
	param($node)
	$node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Run-Step'
}, $true)
if (-not $runStep) { throw 'The runner has no Run-Step function.' }
. ([ScriptBlock]::Create($runStep.Extent.Text))

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
	@{ Name = 'magick-formats'; Code = 42; TimedOut = $false; Output = ''; Failed = $true }
	@{ Name = 'magick-formats'; Code = 124; TimedOut = $true; Output = ''; Failed = $true }
	@{ Name = 'magick-version'; Code = 127; TimedOut = $false; Output = ''; Failed = $true }
)
foreach ($case in $cases) {
	$script:Summary.Clear()
	$script:ProbeCode = $case.Code
	$script:ProbeTimedOut = $case.TimedOut
	$script:Failed = $false
	[IO.File]::WriteAllText($script:ProbeStdout, $case.Output)
	Probe -Name $case.Name -Executable 'unused' -Arguments @('-list', 'format')
	if ($script:Failed -ne $case.Failed) { throw "Incorrect failure state for $($case.Name), exit $($case.Code)." }
	$expected = "probe-$($case.Name): exit $($case.Code); timed out: $($case.TimedOut);"
	if (-not ($script:Summary | Where-Object { $_.Contains($expected) })) {
		throw "Missing status '$expected': $($script:Summary -join ', ')"
	}
	if ($case.Name -eq 'magick-formats' -and $case.Code -eq 0 -and -not $case.Output -and
		-not ($script:Summary | Where-Object { $_.Contains('empty format listing') })) {
		throw 'An empty format listing was not reported.'
	}
}

$script:Failed = $true
$script:ProbeCode = 0
$script:ProbeTimedOut = $false
Probe -Name 'magick-version' -Executable 'unused' -Arguments @('-version')
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
	$result = Run-Step -Name 'workspace-cli' -Seconds 73 -Executable 'unused' -Arguments @()
	if ($result.Code -ne $case.Code -or $script:Failed -ne $case.Failed) {
		throw "Incorrect phase result for exit $($case.Code)."
	}
	$expected = "workspace-cli: exit $($case.Code); timed out: $($case.TimedOut); limit: 73s; elapsed: "
	if (-not ($script:Summary | Where-Object { $_.Contains($expected) })) {
		throw "Missing phase status '$expected': $($script:Summary -join ', ')"
	}
}
Write-Output 'Windows runner reporting checks passed.'
