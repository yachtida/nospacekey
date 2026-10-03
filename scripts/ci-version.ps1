# Give each downloadable build its own install directory. The installer
# deliberately refuses to overwrite an existing directory for the same version.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargoPath = Join-Path $root 'Cargo.toml'
$cargo = [IO.File]::ReadAllText($cargoPath)
$pattern = '(?m)^version\s*=\s*"([^"]+)"'
$matches = [regex]::Matches($cargo, $pattern)
if ($matches.Count -ne 1) { throw 'Expected one workspace version in Cargo.toml' }
$identityJson = & node (Join-Path $PSScriptRoot 'ci-identity.mjs') allocate
if ($LASTEXITCODE -ne 0) { throw 'CI identity allocation failed; use a fresh workflow attempt' }
$identity = $identityJson | ConvertFrom-Json
$baseVersion = $identity.source_version
$version = $identity.version
if ($matches[0].Groups[1].Value -cne $baseVersion) { throw 'Workspace version changed during identity allocation' }
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
