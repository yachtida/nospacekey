# Give each downloadable build its own install directory. The installer
# deliberately refuses to overwrite an existing directory for the same version.
[CmdletBinding()]
param(
    [string]$RunNumber = $env:GITHUB_RUN_NUMBER,
    [string]$RunAttempt = $env:GITHUB_RUN_ATTEMPT
)
$ErrorActionPreference = 'Stop'
if ($RunNumber -notmatch '^[1-9][0-9]*$' -or $RunAttempt -notmatch '^[1-9][0-9]*$') {
    throw 'CI version requires positive run and attempt numbers'
}
$root = Split-Path -Parent $PSScriptRoot
$cargoPath = Join-Path $root 'Cargo.toml'
$cargo = [IO.File]::ReadAllText($cargoPath)
$pattern = '(?m)^version\s*=\s*"([^"]+)"'
$matches = [regex]::Matches($cargo, $pattern)
if ($matches.Count -ne 1) { throw 'Expected one workspace version in Cargo.toml' }
$baseVersion = $matches[0].Groups[1].Value
$sha = (& git -C $root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sha -notmatch '^[0-9a-f]{40}$') { throw 'Cannot determine source commit' }
$separator = if ($baseVersion.Contains('-')) { '.' } else { '-' }
$version = "$baseVersion${separator}ci.$RunNumber.$RunAttempt.$($sha.Substring(0, 8))"
if ($baseVersion.Contains('+') -or $baseVersion -match '(?:-|\.)ci\.') { throw 'Source version must not already contain CI/build metadata' }
$utf8 = [Text.UTF8Encoding]::new($false)
$cargo = [regex]::Replace($cargo, $pattern, "version = `"$version`"")
[IO.File]::WriteAllText($cargoPath, $cargo, $utf8)

# Update only source-less workspace packages in the existing lockfile. Registry
# dependencies and their checksums stay pinned, so --locked remains meaningful.
$lockPath = Join-Path $root 'Cargo.lock'
$lock = [IO.File]::ReadAllText($lockPath)
$oldVersion = [regex]::Escape($baseVersion)
$lock = [regex]::Replace($lock, '(?ms)^\[\[package\]\]\r?\n.*?(?=^\[\[package\]\]|\z)', {
    param($block)
    if ($block.Value -match '(?m)^source\s*=') { return $block.Value }
    return [regex]::Replace($block.Value, "(?m)^version = `"$oldVersion`"", "version = `"$version`"")
})
[IO.File]::WriteAllText($lockPath, $lock, $utf8)
& (Join-Path $PSScriptRoot 'sync-version.ps1')
if ($LASTEXITCODE -ne 0) { throw 'CI version synchronization failed' }
& (Join-Path $PSScriptRoot 'sync-version.ps1') -Check
if ($LASTEXITCODE -ne 0) { throw 'CI version consistency check failed' }
if ($env:GITHUB_ENV) {
    "NOSPACEKEY_CI_BASE_VERSION=$baseVersion" >> $env:GITHUB_ENV
    "NOSPACEKEY_CI_VERSION=$version" >> $env:GITHUB_ENV
}
Write-Host "CI install version: $version (source version: $baseVersion)"
