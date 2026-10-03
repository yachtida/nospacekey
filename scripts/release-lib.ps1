<#
  release-lib.ps1 -- pure helper functions for scripts/release.ps1.
  ASCII-only on purpose (WinPS 5.1 reads BOM-less UTF-8 as ANSI).

  Split out of release.ps1 so they are unit-testable: release.ps1 has a Mandatory
  -Version parameter and a side-effectful body, so dot-sourcing IT would prompt
  and execute; dot-sourcing THIS file only defines functions.
  Tests: scripts/verify-release-lib.ps1 (plain assertions, no Pester).
#>

. (Join-Path $PSScriptRoot 'version-lifetime.ps1')

# Extract MyAppVersion from .iss preprocessor text. Since version unification
# (plan#5) the define lives in installer\version.iss (nospacekey.iss #includes it),
# so callers must feed THAT file's text, not nospacekey.iss.
function Get-IssVersion([string]$issText) {
    if ($issText -match '#define\s+MyAppVersion\s+"([^"]+)"') { return $Matches[1] }
    return $null
}

function Get-IssAppName([string]$issText) {
    if ($issText -match '#define\s+MyAppName\s+"([^"]+)"') { return $Matches[1] }
    return $null
}

# Extract the Inno AppId. Two source forms:
#   AppId={{GUID}   Inno escaping: only '{{' -> literal '{'; '}' is NOT doubled.
#                   Runtime value {GUID}; ARP key / winget ProductCode = {GUID}_is1.
#   AppId=nospacekey   plain string (pinned by the persist-engine lifecycle track to
#                   keep the update identity of existing installs); ARP key nospacekey_is1.
# The double-closed mis-escape AppId={{GUID}} must NOT match: it would silently
# produce ARP key {GUID}}_is1 and break winget upgrade detection -- returning
# $null here turns that mistake into a hard preflight failure instead.
function Get-IssAppId([string]$issText) {
    if ($issText -match '(?m)^\s*AppId=\{\{([0-9A-Fa-f\-]+)\}\s*$') { return ('{' + $Matches[1] + '}') }
    if ($issText -match '(?m)^\s*AppId=([A-Za-z0-9_.\-]+)\s*$') { return $Matches[1] }
    return $null
}

# One line of SHA256SUMS.txt in `sha256sum -c` format: lowercase hex, TWO spaces, name.
function New-Sha256SumsLine([string]$hashHex, [string]$fileName) {
    return ('{0}  {1}' -f $hashHex.ToLowerInvariant(), $fileName)
}

# Replace {{TOKEN}} placeholders; throw if any recognizable token survives, so a
# template/token-table drift can never ship a manifest with literal {{...}} in it.
function Expand-ManifestTemplate([string]$text, [hashtable]$tokens) {
    foreach ($k in $tokens.Keys) { $text = $text.Replace('{{' + $k + '}}', [string]$tokens[$k]) }
    if ($text -match '\{\{[A-Z0-9_]+\}\}') { throw "unresolved token in manifest: $($Matches[0])" }
    return $text
}

# winget PackageIdentifier '<Publisher>.<Product...>' -> parts. The Publisher and
# PackageName locale fields are derived from these (single source: the -PackageId
# argument), never typed separately into the templates.
function Get-PackageIdParts([string]$id) {
    $i = $id.IndexOf('.')
    if ($i -lt 1 -or $i -ge ($id.Length - 1)) { throw "PackageId must be <Publisher>.<Product>: $id" }
    return @{ Publisher = $id.Substring(0, $i); Name = $id.Substring($i + 1) }
}

# Resolve a filesystem path without allowing a junction/symlink/reparse point
# anywhere from the drive root through the leaf.  Keep this lexical walk
# separate from the caller's boundary check: a path can be reparse-free and
# still be inside dist.
function Resolve-NoReparsePath {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Label
    )
    try {
        $full = [IO.Path]::GetFullPath($Path)
        $root = [IO.Path]::GetPathRoot($full)
        if ([string]::IsNullOrWhiteSpace($root)) { throw 'path has no filesystem root' }
        $rootItem = Get-Item -LiteralPath $root -Force -ErrorAction Stop
        if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point is not allowed at path root: $root"
        }
        $current = $root
        $relative = $full.Substring($root.Length)
        foreach ($component in @($relative -split '[\\/]+' | Where-Object { $_ })) {
            $current = Join-Path -Path $current -ChildPath $component
            $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "reparse point is not allowed in path component: $current"
            }
        }
        return $full
    } catch {
        throw "$Label path component validation failed: $Path ($($_.Exception.Message))"
    }
}

# The public repository must never receive a dev-signed draft. Keep this exact
# repository identity in one pure helper so the release script and its tests use
# the same case-insensitive rule.
function Test-OfficialReleaseRepo([string]$repo) {
    if ([string]::IsNullOrWhiteSpace($repo)) { return $false }
    return $repo.Trim().Equals('yachtida/nospacekey', [StringComparison]::OrdinalIgnoreCase)
}

function Write-VersionOwnershipManifest([string]$Root, [string]$Version) {
    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    Install-VersionLifetimeSentinel -Directory $rootFull | Out-Null
    $files = @(Get-ChildItem -LiteralPath $rootFull -File -Recurse | Where-Object {
        $_.Name -cne 'version-manifest.json'
    } | ForEach-Object {
        [ordered]@{
            Path = $_.FullName.Substring($rootFull.Length).TrimStart('\').Replace('\', '/')
            Sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        }
    } | Sort-Object Path)
    [ordered]@{ Schema = 1; Version = $Version; Files = $files } |
        ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (Join-Path $rootFull 'version-manifest.json') -Encoding utf8
    return $files.Count
}

# VERSIONINFO may preserve the full SemVer string or expose only its numeric
# Windows version. When the prerelease label is present, require it to match
# exactly instead of accepting a different build from the same numeric line.
function Test-ArtifactFileVersion([string]$fileVersion, [string]$releaseVersion) {
    if ([string]::IsNullOrWhiteSpace($fileVersion) -or [string]::IsNullOrWhiteSpace($releaseVersion)) { return $false }
    if ($fileVersion -ceq $releaseVersion) { return $true }
    $versionCore = ($releaseVersion -split '-', 2)[0]
    $numericPattern = '^' + [regex]::Escape($versionCore) + '(?:\.\d+)?$'
    return $fileVersion -cmatch $numericPattern
}

# Publishable release notes use the same heading shape as docs/release-notes-*.md.
function Test-ReleaseNotesHeading([string]$text, [string]$version) {
    if ([string]::IsNullOrWhiteSpace($text) -or [string]::IsNullOrWhiteSpace($version)) { return $false }
    $expected = '# nospacekey v' + $version
    $lines = [regex]::Split($text, '\r\n|\r|\n')
    $firstNonEmpty = @($lines | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -First 1)
    if ($firstNonEmpty.Count -ne 1 -or ([string]$firstNonEmpty[0]) -cne $expected) { return $false }
    $h1Count = @($lines | Where-Object { $_ -match '^#[ \t]+.*$' }).Count
    return $h1Count -eq 1
}

# Generic scaffolds must not be passed to gh as release notes. Keep the pattern
# deliberately small and explicit: these are the placeholders shipped by the
# reusable template and the common TODO marker used in release drafts.
function Test-ReleaseNotesHasScaffoldPlaceholder([string]$text) {
    if ($null -eq $text) { return $false }
    return [regex]::IsMatch($text, '(?im)\bTODO\b|\bv?X\.Y\.Z\b')
}
