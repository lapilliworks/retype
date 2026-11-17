# Synthetic collector checks. Does not require Hyper-V or contact a VM.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$source = Join-Path $PSScriptRoot 'hyperv-info.ps1'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref] $tokens, [ref] $errors)
if ($errors.Count -ne 0) { throw ($errors | Out-String) }
foreach ($name in @('Save-Report', 'Receive-BoundedJob')) {
	$function = $ast.Find({
		param($node)
		$node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
	}, $true)
	if ($null -eq $function) { throw "Missing collector function: $name" }
	Invoke-Expression $function.Extent.Text
}
$assignment = $ast.Find({
	param($node)
	$node -is [Management.Automation.Language.AssignmentStatementAst] -and
		$node.Left.Extent.Text -eq '$repositoryQuery'
}, $true)
if ($null -eq $assignment) { throw 'Missing repository query.' }
Invoke-Expression $assignment.Extent.Text

function Assert-QueryRejected {
	param([scriptblock] $Query, [int] $Seconds = 10)
	$job = Start-Job -ScriptBlock $Query
	$id = $job.Id
	$wasRejected = $false
	try {
		Receive-BoundedJob -Job $job -Seconds $Seconds | Out-Null
	} catch {
		$wasRejected = $true
	}
	if (-not $wasRejected) { throw 'Invalid query was accepted.' }
	if (Get-Job -Id $id -ErrorAction SilentlyContinue) { throw 'Query job was not removed.' }
}

$job = Start-Job -ScriptBlock { [PSCustomObject]@{ Value = 'single result' } }
$id = $job.Id
$result = Receive-BoundedJob -Job $job -Seconds 10
if ($result.Value -ne 'single result') { throw 'Successful result was lost.' }
if (Get-Job -Id $id -ErrorAction SilentlyContinue) { throw 'Successful job was not removed.' }
Assert-QueryRejected { throw 'synthetic query failure' }
Assert-QueryRejected { }
Assert-QueryRejected { 'first'; 'second' }
Assert-QueryRejected { Start-Sleep -Seconds 30 } -Seconds 1

$root = Join-Path ([IO.Path]::GetTempPath()) ('retype-hyperv-info-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$script:ReportPath = $root
try {
	Save-Report 'synthetic' ([PSCustomObject]@{ Succeeded = $false; FailedStage = 'guest' })
	$report = Get-Content -LiteralPath (Join-Path $root 'synthetic.json') -Raw | ConvertFrom-Json
	if ($report.Succeeded -or $report.FailedStage -ne 'guest') { throw 'Failure report changed.' }
	$repo = Join-Path $root 'repository with spaces'
	& git init --quiet -- $repo
	if ($LASTEXITCODE -ne 0) { throw 'Cannot create synthetic repository.' }
	& git -C $repo -c user.name=Synthetic -c user.email=synthetic@example.invalid `
		-c commit.gpgsign=false commit --quiet --allow-empty -m 'Synthetic fixture'
	if ($LASTEXITCODE -ne 0) { throw 'Cannot commit synthetic fixture.' }
	$result = & $repositoryQuery $repo
	if (-not $result.IsClean -or $result.Revision -notmatch '^[0-9a-f]{40,64}$') {
		throw 'Clean checkout was not reported correctly.'
	}
	[IO.File]::WriteAllText((Join-Path $repo 'untracked.txt'), 'synthetic')
	$result = & $repositoryQuery $repo
	if ($result.IsClean -or $result.Status.Count -ne 1) { throw 'Dirty checkout was reported as clean.' }
	$wasRejected = $false
	try { & $repositoryQuery $root | Out-Null } catch { $wasRejected = $true }
	if (-not $wasRejected) { throw 'A non-repository directory was accepted.' }
} finally {
	Remove-Item -LiteralPath $root -Recurse -Force
}
Write-Host 'Synthetic collector tests passed; not native Hyper-V evidence.'
