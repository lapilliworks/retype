#requires -Version 7.0
# Provision real tools on disposable GitHub-hosted Windows runners.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($env:GITHUB_ACTIONS -ne 'true') { throw 'This setup is intended for GitHub-hosted CI.' }
if (-not $env:GITHUB_PATH) { throw 'GitHub Actions path file is required.' }

$packages = @('imagemagick', '7zip', 'ffmpeg', 'pandoc', 'typst')
& choco install @packages --yes --no-progress
if ($LASTEXITCODE -ne 0) { throw "Chocolatey installation failed with exit $LASTEXITCODE." }

$paths = @(
	(Join-Path $env:ProgramData 'chocolatey\bin'),
	(Join-Path $env:ProgramFiles '7-Zip'),
	(Join-Path $env:ProgramFiles 'Pandoc'),
	(Join-Path $env:LOCALAPPDATA 'Pandoc'),
	(Join-Path $env:ProgramData 'chocolatey\lib\ffmpeg\tools\ffmpeg\bin')
)
$magickDirectories = Get-ChildItem -LiteralPath $env:ProgramFiles -Filter 'ImageMagick-*' -Directory
$paths += @($magickDirectories | ForEach-Object { $_.FullName })
foreach ($path in $paths) {
	if (Test-Path -LiteralPath $path -PathType Container) {
		$env:PATH = "$path;$env:PATH"
		Add-Content -LiteralPath $env:GITHUB_PATH -Value $path
	}
}

foreach ($tool in @('magick', 'ffmpeg', 'ffprobe', 'pandoc', 'typst')) {
	if (-not (Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue)) {
		throw "Required command is missing after setup: $tool"
	}
}
if (-not (Get-Command '7zz', '7z', '7za' -CommandType Application -ErrorAction SilentlyContinue)) {
	throw '7-Zip is missing after setup.'
}
