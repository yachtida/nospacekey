<#
  swift-toolchain.ps1 -- shared Swift Windows installation discovery helpers.

  Directory names are treated as SemVer 2.0.0. Non-semver directories are
  ignored, and a candidate is usable only when its required leaf exists. This
  keeps all release/build/gate scripts on the same selection contract.

  ASCII-only on purpose: Windows PowerShell 5.1 reads BOM-less UTF-8 as ANSI.
#>

function ConvertTo-SwiftSemVer {
    param([string]$Name)
    if ([string]::IsNullOrWhiteSpace($Name)) { return $null }

    # Swift.org currently uses names such as 6.3.2 and 6.3.2+Asserts. Keep the
    # parser strict so arbitrary folders (for example "nightly") cannot win.
    $pattern = '^(?<major>0|[1-9][0-9]*)\.(?<minor>0|[1-9][0-9]*)\.(?<patch>0|[1-9][0-9]*)(?:-(?<pre>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+(?<build>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$'
    $match = [regex]::Match($Name, $pattern)
    if (-not $match.Success) { return $null }
    try {
        $major = [int]$match.Groups['major'].Value
        $minor = [int]$match.Groups['minor'].Value
        $patch = [int]$match.Groups['patch'].Value
    } catch {
        # A numerically malformed/overflowing folder name is not a usable
        # installation candidate; do not let it abort discovery.
        return $null
    }
    if ($match.Groups['pre'].Success) {
        foreach ($identifier in @($match.Groups['pre'].Value -split '\.')) {
            if ($identifier -match '^[0-9]+$' -and $identifier.Length -gt 1 -and $identifier.StartsWith('0')) {
                return $null
            }
        }
    }

    return [pscustomobject]@{
        Name        = $Name
        Major       = $major
        Minor       = $minor
        Patch       = $patch
        PreRelease  = if ($match.Groups['pre'].Success) { $match.Groups['pre'].Value } else { $null }
        Build       = if ($match.Groups['build'].Success) { $match.Groups['build'].Value } else { $null }
        IsStable    = -not $match.Groups['pre'].Success
    }
}

function Compare-SwiftSemVerIdentifier {
    param([string]$Left, [string]$Right)

    $leftNumeric = $Left -match '^[0-9]+$'
    $rightNumeric = $Right -match '^[0-9]+$'
    if ($leftNumeric -and $rightNumeric) {
        # Compare arbitrarily long numeric identifiers without integer overflow.
        $leftTrimmed = $Left.TrimStart('0')
        $rightTrimmed = $Right.TrimStart('0')
        if ([string]::IsNullOrEmpty($leftTrimmed)) { $leftTrimmed = '0' }
        if ([string]::IsNullOrEmpty($rightTrimmed)) { $rightTrimmed = '0' }
        if ($leftTrimmed.Length -lt $rightTrimmed.Length) { return -1 }
        if ($leftTrimmed.Length -gt $rightTrimmed.Length) { return 1 }
        return [string]::Compare($leftTrimmed, $rightTrimmed, [StringComparison]::Ordinal)
    }
    if ($leftNumeric -and -not $rightNumeric) { return -1 }
    if (-not $leftNumeric -and $rightNumeric) { return 1 }
    return [string]::Compare($Left, $Right, [StringComparison]::Ordinal)
}

function Compare-SwiftVersionedCandidate {
    param([Parameter(Mandatory)][object]$Left, [Parameter(Mandatory)][object]$Right)

    # Return descending order: the newest candidate compares first.
    foreach ($field in @('Major', 'Minor', 'Patch')) {
        if ($Left.$field -lt $Right.$field) { return 1 }
        if ($Left.$field -gt $Right.$field) { return -1 }
    }
    if ($Left.IsStable -and -not $Right.IsStable) { return -1 }
    if (-not $Left.IsStable -and $Right.IsStable) { return 1 }

    if (-not $Left.IsStable -and -not $Right.IsStable) {
        $leftIds = @($Left.PreRelease -split '\.')
        $rightIds = @($Right.PreRelease -split '\.')
        $count = [Math]::Min($leftIds.Count, $rightIds.Count)
        for ($i = 0; $i -lt $count; $i++) {
            # Reverse the identifier arguments because the overall order is
            # descending while SemVer identifiers compare ascending.
            $identifierResult = Compare-SwiftSemVerIdentifier -Left $rightIds[$i] -Right $leftIds[$i]
            if ($identifierResult -ne 0) { return $identifierResult }
        }
        if ($leftIds.Count -lt $rightIds.Count) { return 1 }
        if ($leftIds.Count -gt $rightIds.Count) { return -1 }
    }

    # Build metadata has no precedence; use the directory name only as a
    # deterministic tie-breaker when two complete candidates compare equal.
    return [string]::Compare($Right.Name, $Left.Name, [StringComparison]::Ordinal)
}

function Get-SwiftVersionedCandidates {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][string]$LeafRelativePath,
        [ValidateSet('File', 'Directory')][string]$LeafType = 'File'
    )

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) { return @() }

    $candidates = New-Object 'System.Collections.Generic.List[object]'
    foreach ($directory in @(Get-ChildItem -LiteralPath $Root -Directory -ErrorAction SilentlyContinue)) {
        $version = ConvertTo-SwiftSemVer -Name $directory.Name
        if ($null -eq $version) { continue }

        $leaf = Join-Path $directory.FullName $LeafRelativePath
        $leafExists = if ($LeafType -eq 'Directory') {
            Test-Path -LiteralPath $leaf -PathType Container
        } else {
            Test-Path -LiteralPath $leaf -PathType Leaf
        }
        # A newer directory with a partial/missing install must not hide an
        # older complete install. Skip it and continue searching.
        if (-not $leafExists) { continue }

        [void]$candidates.Add([pscustomobject]@{
            Name       = $directory.Name
            Directory  = $directory.FullName
            LeafPath   = $leaf
            Major      = $version.Major
            Minor      = $version.Minor
            Patch      = $version.Patch
            PreRelease = $version.PreRelease
            Build      = $version.Build
            IsStable   = $version.IsStable
        })
    }

    # Build metadata does not affect SemVer precedence; the comparator uses the
    # directory name only as a stable, deterministic tie-breaker (e.g.
    # 6.3.2+Asserts).
    $sorted = New-Object 'System.Collections.Generic.List[object]'
    foreach ($candidate in $candidates) { [void]$sorted.Add($candidate) }
    $sorted.Sort([System.Comparison[object]]{
        param($left, $right)
        Compare-SwiftVersionedCandidate -Left $left -Right $right
    })
    return @($sorted.ToArray())
}

function Get-SwiftToolchainCandidate {
    param([Parameter(Mandatory)][string]$SwiftRoot)
    $root = Join-Path $SwiftRoot 'Toolchains'
    return @(Get-SwiftVersionedCandidates -Root $root -LeafRelativePath 'usr\bin\swift.exe' -LeafType File) |
        Select-Object -First 1
}

function Get-SwiftRuntimeCandidate {
    param([Parameter(Mandatory)][string]$SwiftRoot)
    $root = Join-Path $SwiftRoot 'Runtimes'
    # swiftCore.dll is the runtime's required load-time leaf. Checking it here
    # prevents an empty/partial newer folder from being staged or put on PATH.
    return @(Get-SwiftVersionedCandidates -Root $root -LeafRelativePath 'usr\bin\swiftCore.dll' -LeafType File) |
        Select-Object -First 1
}

function Get-SwiftPlatformCandidate {
    param([Parameter(Mandatory)][string]$SwiftRoot)
    $root = Join-Path $SwiftRoot 'Platforms'
    return @(Get-SwiftVersionedCandidates -Root $root `
        -LeafRelativePath 'Windows.platform\Developer\SDKs\Windows.sdk' -LeafType Directory) |
        Select-Object -First 1
}

function Get-SwiftSdkPath {
    param(
        [Parameter(Mandatory)][string]$SwiftRoot,
        [object]$RuntimeCandidate
    )

    # Older Swift layouts kept the SDK beside the runtime. Prefer that SDK when
    # present, then use the versioned Platforms layout used by newer installers.
    if ($RuntimeCandidate) {
        $runtimeSdk = Join-Path $RuntimeCandidate.Directory 'Windows.platform\Developer\SDKs\Windows.sdk'
        if (Test-Path -LiteralPath $runtimeSdk -PathType Container) { return $runtimeSdk }
    }
    $platform = Get-SwiftPlatformCandidate -SwiftRoot $SwiftRoot
    if ($platform) { return $platform.LeafPath }
    return $null
}

function Add-SwiftPathEntry {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return }
    $entries = @([string]$env:PATH -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    foreach ($entry in $entries) {
        if ([string]::Equals($entry.TrimEnd('\'), $Path.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
    }
    $env:PATH = "$Path;$env:PATH"
}
