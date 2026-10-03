<#
  Provenance contract for the Vulkan Zenzai runtime.

  build-llama.ps1 writes a build-only attestation after CMake has built the
  required DLLs, then derives a portable runtime receipt from that evidence.
  The attestation contains local source/build paths and never enters dist.
  The distribution manifest is derived from the portable receipt; it is never
  an assertion supplied by a caller. The -ForTesting seam is intentionally
  explicit and is rejected by all production callers.
#>

function Get-ZenzaiVulkanRuntimeContract {
    [ordered]@{
        Schema          = 2
        ReceiptSchema   = 1
        Configuration   = 'vulkan'
        Tag             = 'b4846'
        Commit          = '10131b23ee0dbd9d03dfc618cec9fe3402919277'
        PatchSha256     = '77B59C26C36CF3E6EA5EB89267E14D1E06C8F85CF83B13522DF111E1E4BC4F82'
        GgmlVulkan      = 'ON'
        GgmlBackendDl   = 'ON'
        ManifestName    = 'zenzai-vulkan-runtime-manifest.json'
        ReceiptName     = 'zenzai-vulkan-runtime-receipt.json'
        AttestationName = 'zenzai-vulkan-build-attestation.json'
        DllNames        = [string[]]@(
            'llama.dll',
            'ggml.dll',
            'ggml-base.dll',
            'ggml-cpu.dll',
            'ggml-vulkan.dll'
        )
        CMakeOptions    = [ordered]@{
            BUILD_SHARED_LIBS   = 'ON'
            LLAMA_BUILD_TESTS   = 'OFF'
            LLAMA_BUILD_EXAMPLES = 'OFF'
            LLAMA_BUILD_SERVER  = 'OFF'
            GGML_NATIVE         = 'OFF'
            GGML_AVX            = 'ON'
            GGML_AVX2           = 'ON'
            GGML_BMI2           = 'ON'
            GGML_VULKAN         = 'ON'
            GGML_BACKEND_DL     = 'ON'
        }
    }
}

function Get-ZenzaiVulkanRuntimeManifestPath {
    param([Parameter(Mandatory)][string]$RuntimeDirectory)
    Join-Path ([IO.Path]::GetFullPath($RuntimeDirectory)) `
        (Get-ZenzaiVulkanRuntimeContract).ManifestName
}

function Get-ZenzaiVulkanRuntimeReceiptPath {
    param([Parameter(Mandatory)][string]$RuntimeDirectory)
    Join-Path ([IO.Path]::GetFullPath($RuntimeDirectory)) `
        (Get-ZenzaiVulkanRuntimeContract).ReceiptName
}

function Get-ZenzaiVulkanBuildAttestationPath {
    param([Parameter(Mandatory)][string]$BuildDirectory)
    Join-Path ([IO.Path]::GetFullPath($BuildDirectory)) `
        (Get-ZenzaiVulkanRuntimeContract).AttestationName
}

function Assert-ZenzaiRegularPath {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Label,
        [switch]$Directory
    )

    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    $expectedContainer = [bool]$Directory
    if ($item.PSIsContainer -ne $expectedContainer -or
        ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        $kind = if ($Directory) { 'directory' } else { 'file' }
        throw "$Label is not a regular $kind`: $Path"
    }
    return $item
}

function Read-ZenzaiJsonFile {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Label)
    Assert-ZenzaiRegularPath -Path $Path -Label $Label | Out-Null
    try {
        Get-Content -LiteralPath $Path -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "$Label is not valid JSON: $Path ($($_.Exception.Message))"
    }
}

function Read-ZenzaiVulkanRuntimeManifest {
    param([Parameter(Mandatory)][string]$ManifestPath)
    Read-ZenzaiJsonFile -Path $ManifestPath -Label 'Zenzai Vulkan runtime manifest'
}

function Read-ZenzaiVulkanRuntimeReceipt {
    param([Parameter(Mandatory)][string]$ReceiptPath)
    Read-ZenzaiJsonFile -Path $ReceiptPath -Label 'Zenzai Vulkan runtime provenance receipt'
}

function ConvertTo-ZenzaiCanonicalJson {
    param([Parameter(Mandatory)]$Value)
    $Value | ConvertTo-Json -Depth 20 -Compress
}

function Get-ZenzaiTextSha256 {
    param([Parameter(Mandatory)][string]$Text)
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
    $hash = [Security.Cryptography.SHA256]::HashData($bytes)
    ([BitConverter]::ToString($hash) -replace '-', '').ToUpperInvariant()
}

function Get-ZenzaiFileSha256 {
    param([Parameter(Mandatory)][string]$Path)
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash.ToUpperInvariant()
}

function Get-ZenzaiRepositoryRoot {
    [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
}

function Assert-ZenzaiCanonicalProductionBuildPaths {
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$PatchPath,
        [Parameter(Mandatory)][string]$CMakeCachePath
    )

    $repoRoot = Get-ZenzaiRepositoryRoot
    $expected = [ordered]@{
        runtime = Join-Path $repoRoot 'engine-host\vendor\llama\vulkan'
        source  = Join-Path $repoRoot '.llama-build\source-b4846-patched'
        patch   = Join-Path $repoRoot 'patches\llama-b4846-nospacekey-vulkan-required.patch'
        cache   = Join-Path $repoRoot '.llama-build\build-vulkan-dl\CMakeCache.txt'
    }
    $actual = [ordered]@{
        runtime = [IO.Path]::GetFullPath($RuntimeDirectory)
        source  = [IO.Path]::GetFullPath($SourceDirectory)
        patch   = [IO.Path]::GetFullPath($PatchPath)
        cache   = [IO.Path]::GetFullPath($CMakeCachePath)
    }
    foreach ($kind in $expected.Keys) {
        if (-not [string]::Equals($actual[$kind], [IO.Path]::GetFullPath($expected[$kind]),
                [StringComparison]::OrdinalIgnoreCase)) {
            throw "production provenance $kind path is not the canonical build path: $($actual[$kind])"
        }
    }
    foreach ($kind in @('runtime', 'source')) {
        Assert-ZenzaiRegularPath -Path $actual[$kind] -Label "canonical $kind evidence" -Directory | Out-Null
    }
    foreach ($kind in @('patch', 'cache')) {
        Assert-ZenzaiRegularPath -Path $actual[$kind] -Label "canonical $kind evidence" | Out-Null
    }
    return [pscustomobject]$actual
}

function Get-ZenzaiCMakeCacheValue {
    param(
        [Parameter(Mandatory)][string]$CachePath,
        [Parameter(Mandatory)][string]$Name
    )
    $escaped = [regex]::Escape($Name)
    $entry = Get-Content -LiteralPath $CachePath -ErrorAction Stop |
        Where-Object { $_ -match "^${escaped}:[^=]+=.*$" } |
        Select-Object -First 1
    if ($null -eq $entry) { throw "CMake cache entry is missing: $Name" }
    $entry.Substring($entry.IndexOf('=') + 1).Trim()
}

function Get-ZenzaiRuntimeArtifactRows {
    param([Parameter(Mandatory)][string]$RuntimeDirectory)
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    Assert-ZenzaiRegularPath -Path $runtimeFull -Label 'Zenzai Vulkan runtime directory' -Directory | Out-Null
    $rows = @()
    foreach ($name in $contract.DllNames) {
        $path = Join-Path $runtimeFull $name
        $item = Assert-ZenzaiRegularPath -Path $path -Label "Zenzai Vulkan runtime DLL '$name'"
        $rows += [ordered]@{
            name   = $name
            size   = [int64]$item.Length
            sha256 = Get-ZenzaiFileSha256 -Path $path
        }
    }
    return $rows
}

function Get-ZenzaiBuildArtifactRows {
    param([Parameter(Mandatory)][string]$BuildDirectory)
    $contract = Get-ZenzaiVulkanRuntimeContract
    $buildFull = [IO.Path]::GetFullPath($BuildDirectory)
    Assert-ZenzaiRegularPath -Path $buildFull -Label 'llama Vulkan build directory' -Directory | Out-Null
    $rows = @()
    foreach ($name in $contract.DllNames) {
        $matches = @(Get-ChildItem -LiteralPath $buildFull -Recurse -File -Filter $name -Force)
        if ($matches.Count -ne 1) {
            throw "llama Vulkan build did not produce one canonical '$name' (found $($matches.Count))"
        }
        $item = $matches[0]
        Assert-ZenzaiRegularPath -Path $item.FullName -Label "llama Vulkan build artifact '$name'" | Out-Null
        $rows += [ordered]@{
            name       = $name
            size       = [int64]$item.Length
            sha256     = Get-ZenzaiFileSha256 -Path $item.FullName
            build_path = [IO.Path]::GetFullPath($item.FullName)
        }
    }
    return $rows
}

function ConvertTo-ZenzaiPortableArtifactRows {
    param([Parameter(Mandatory)]$Rows)
    if ($null -eq $Rows) { return @() }
    @($Rows | ForEach-Object {
        [ordered]@{
            name   = [string]$_.name
            size   = [int64]$_.size
            sha256 = ([string]$_.sha256).ToUpperInvariant()
        }
    })
}

function Get-ZenzaiReceiptPayload {
    param([Parameter(Mandatory)]$Receipt)
    $cmake = [ordered]@{}
    foreach ($name in (Get-ZenzaiVulkanRuntimeContract).CMakeOptions.Keys) {
        $cmake[$name] = [string]$Receipt.cmake.$name
    }
    [ordered]@{
        schema             = [int]$Receipt.schema
        kind               = [string]$Receipt.kind
        configuration      = [string]$Receipt.configuration
        tag                = [string]$Receipt.tag
        source_commit      = [string]$Receipt.source_commit
        patch_sha256       = ([string]$Receipt.patch_sha256).ToUpperInvariant()
        cmake_cache_sha256 = ([string]$Receipt.cmake_cache_sha256).ToUpperInvariant()
        cmake              = $cmake
        artifacts          = @(ConvertTo-ZenzaiPortableArtifactRows -Rows $Receipt.artifacts)
        build_artifacts    = @(ConvertTo-ZenzaiPortableArtifactRows -Rows $Receipt.build_artifacts)
        attestation_sha256 = ([string]$Receipt.attestation_sha256).ToUpperInvariant()
        testing_seam       = [bool]$Receipt.testing_seam
    }
}

function Get-ZenzaiBuildAttestationPayload {
    param([Parameter(Mandatory)]$Attestation)
    $cmake = [ordered]@{}
    foreach ($name in (Get-ZenzaiVulkanRuntimeContract).CMakeOptions.Keys) {
        $cmake[$name] = [string]$Attestation.cmake.$name
    }
    $buildArtifacts = @($Attestation.build_artifacts | ForEach-Object {
        [ordered]@{
            name       = [string]$_.name
            size       = [int64]$_.size
            sha256     = ([string]$_.sha256).ToUpperInvariant()
            build_path = [string]$_.build_path
        }
    })
    [ordered]@{
        schema             = [int]$Attestation.schema
        kind               = [string]$Attestation.kind
        configuration      = [string]$Attestation.configuration
        tag                = [string]$Attestation.tag
        source_commit      = [string]$Attestation.source_commit
        patch_sha256       = ([string]$Attestation.patch_sha256).ToUpperInvariant()
        cmake_cache_sha256 = ([string]$Attestation.cmake_cache_sha256).ToUpperInvariant()
        cmake              = $cmake
        build_artifacts    = $buildArtifacts
        build              = [ordered]@{
            source_directory = [string]$Attestation.build.source_directory
            patch_path       = [string]$Attestation.build.patch_path
            cmake_cache_path = [string]$Attestation.build.cmake_cache_path
        }
        testing_seam       = [bool]$Attestation.testing_seam
    }
}

function Get-ZenzaiManifestPayload {
    param([Parameter(Mandatory)]$Manifest)
    $cmake = [ordered]@{}
    foreach ($name in (Get-ZenzaiVulkanRuntimeContract).CMakeOptions.Keys) {
        $cmake[$name] = [string]$Manifest.provenance.cmake.$name
    }
    $buildArtifacts = @($Manifest.provenance.build_artifacts | ForEach-Object {
        [ordered]@{
            name   = [string]$_.name
            size   = [int64]$_.size
            sha256 = ([string]$_.sha256).ToUpperInvariant()
        }
    })
    $dlls = @($Manifest.dlls | ForEach-Object {
        [ordered]@{
            name   = [string]$_.name
            size   = [int64]$_.size
            sha256 = ([string]$_.sha256).ToUpperInvariant()
        }
    })
    [ordered]@{
        schema          = [int]$Manifest.schema
        configuration   = [string]$Manifest.configuration
        payload_state   = [string]$Manifest.payload_state
        receipt_sha256  = ([string]$Manifest.receipt_sha256).ToUpperInvariant()
        receipt_payload_sha256 = ([string]$Manifest.receipt_payload_sha256).ToUpperInvariant()
        provenance      = [ordered]@{
            tag           = [string]$Manifest.provenance.tag
            source_commit = [string]$Manifest.provenance.source_commit
            patch_sha256  = ([string]$Manifest.provenance.patch_sha256).ToUpperInvariant()
            attestation_sha256 = ([string]$Manifest.provenance.attestation_sha256).ToUpperInvariant()
            cmake         = $cmake
            build_artifacts = $buildArtifacts
            testing_seam  = [bool]$Manifest.provenance.testing_seam
        }
        dlls            = $dlls
    }
}

function Assert-ZenzaiReceiptShape {
    param([Parameter(Mandatory)]$Receipt, [switch]$ForTesting)
    $contract = Get-ZenzaiVulkanRuntimeContract
    $properties = @($Receipt.PSObject.Properties.Name)
    $allowed = @('schema', 'kind', 'configuration', 'tag', 'source_commit',
        'patch_sha256', 'cmake_cache_sha256', 'cmake', 'artifacts',
        'build_artifacts', 'attestation_sha256', 'testing_seam', 'payload_sha256')
    $unknown = @($properties | Where-Object { $allowed -notcontains $_ })
    if ($unknown.Count -gt 0) { throw "provenance receipt has unknown field(s): $($unknown -join ', ')" }
    if ([int]$Receipt.schema -ne $contract.ReceiptSchema) { throw 'provenance receipt schema mismatch' }
    if ([string]$Receipt.kind -cne 'zenzai-vulkan-runtime-receipt') { throw 'provenance receipt kind mismatch' }
    if ([string]$Receipt.configuration -cne $contract.Configuration) { throw 'provenance receipt configuration mismatch' }
    if ([string]$Receipt.tag -cne $contract.Tag) { throw 'provenance receipt tag mismatch' }
    if ([string]$Receipt.source_commit -cne $contract.Commit) { throw 'provenance receipt source commit mismatch' }
    if ([string]$Receipt.patch_sha256 -cne $contract.PatchSha256) { throw 'provenance receipt patch hash mismatch' }
    if ([bool]$Receipt.testing_seam -and -not $ForTesting) {
        throw 'testing provenance receipt is not accepted by the production gate'
    }
    if ([string]$Receipt.payload_sha256 -notmatch '^[0-9A-Fa-f]{64}$') {
        throw 'provenance receipt payload_sha256 is invalid'
    }
    if ([string]$Receipt.attestation_sha256 -notmatch '^[0-9A-Fa-f]{64}$') {
        throw 'provenance receipt attestation_sha256 is invalid'
    }
    if (-not [bool]$Receipt.testing_seam -and
        ([string]$Receipt.attestation_sha256).ToUpperInvariant() -eq ('0' * 64)) {
        throw 'production provenance receipt is missing build attestation binding'
    }
    $payloadHash = Get-ZenzaiTextSha256 -Text (ConvertTo-ZenzaiCanonicalJson (Get-ZenzaiReceiptPayload -Receipt $Receipt))
    if ($payloadHash -cne ([string]$Receipt.payload_sha256).ToUpperInvariant()) {
        throw 'provenance receipt payload hash mismatch (receipt was modified)'
    }
    foreach ($name in $contract.CMakeOptions.Keys) {
        if ([string]$Receipt.cmake.$name -cne [string]$contract.CMakeOptions[$name]) {
            throw "provenance receipt CMake value mismatch: $name"
        }
    }
    $rows = @($Receipt.artifacts)
    if ($rows.Count -ne $contract.DllNames.Count) { throw 'provenance receipt DLL list is incomplete' }
    $names = @($rows | ForEach-Object { [string]$_.name })
    if (@($contract.DllNames | Where-Object { $names -notcontains $_ }).Count -gt 0 -or
        @($names | Where-Object { $contract.DllNames -notcontains $_ }).Count -gt 0 -or
        @($names | Group-Object | Where-Object Count -gt 1).Count -gt 0) {
        throw 'provenance receipt DLL set is not exact'
    }
    foreach ($row in $rows) {
        if ([string]$row.sha256 -notmatch '^[0-9A-Fa-f]{64}$' -or [int64]$row.size -lt 0) {
            throw "provenance receipt artifact row is invalid: $($row.name)"
        }
    }
    $buildRows = @($Receipt.build_artifacts)
    if ($buildRows.Count -ne $contract.DllNames.Count) {
        throw 'provenance receipt build artifact list is incomplete'
    }
    $buildNames = @($buildRows | ForEach-Object { [string]$_.name })
    if (@($contract.DllNames | Where-Object { $buildNames -notcontains $_ }).Count -gt 0 -or
        @($buildNames | Where-Object { $contract.DllNames -notcontains $_ }).Count -gt 0 -or
        @($buildNames | Group-Object | Where-Object Count -gt 1).Count -gt 0) {
        throw 'provenance receipt build artifact set is not exact'
    }
    foreach ($row in $buildRows) {
        if ([string]$row.sha256 -notmatch '^[0-9A-Fa-f]{64}$' -or [int64]$row.size -lt 0) {
            throw "provenance receipt build artifact row is invalid: $($row.name)"
        }
    }
    foreach ($runtimeRow in $rows) {
        $buildRow = @($buildRows | Where-Object { [string]$_.name -ceq [string]$runtimeRow.name })[0]
        if ($null -eq $buildRow -or [int64]$buildRow.size -ne [int64]$runtimeRow.size -or
            [string]$buildRow.sha256 -cne ([string]$runtimeRow.sha256).ToUpperInvariant()) {
            throw "provenance receipt runtime/build artifact binding mismatch: $($runtimeRow.name)"
        }
    }
}

function Assert-ZenzaiBuildAttestationShape {
    param([Parameter(Mandatory)]$Attestation, [switch]$ForTesting)
    $contract = Get-ZenzaiVulkanRuntimeContract
    $allowed = @('schema', 'kind', 'configuration', 'tag', 'source_commit',
        'patch_sha256', 'cmake_cache_sha256', 'cmake', 'build_artifacts',
        'build', 'testing_seam', 'payload_sha256')
    $unknown = @($Attestation.PSObject.Properties.Name | Where-Object { $allowed -notcontains $_ })
    if ($unknown.Count -gt 0) { throw "build attestation has unknown field(s): $($unknown -join ', ')" }
    if ([int]$Attestation.schema -ne $contract.ReceiptSchema) { throw 'build attestation schema mismatch' }
    if ([string]$Attestation.kind -cne 'zenzai-vulkan-build-attestation') { throw 'build attestation kind mismatch' }
    if ([string]$Attestation.configuration -cne $contract.Configuration) { throw 'build attestation configuration mismatch' }
    if ([string]$Attestation.tag -cne $contract.Tag) { throw 'build attestation tag mismatch' }
    if ([string]$Attestation.source_commit -cne $contract.Commit) { throw 'build attestation source commit mismatch' }
    if ([string]$Attestation.patch_sha256 -cne $contract.PatchSha256) { throw 'build attestation patch hash mismatch' }
    if ([bool]$Attestation.testing_seam -and -not $ForTesting) { throw 'testing build attestation is not accepted' }
    if ([string]$Attestation.payload_sha256 -notmatch '^[0-9A-Fa-f]{64}$') { throw 'build attestation payload hash is invalid' }
    $payloadHash = Get-ZenzaiTextSha256 -Text (ConvertTo-ZenzaiCanonicalJson (Get-ZenzaiBuildAttestationPayload -Attestation $Attestation))
    if ($payloadHash -cne ([string]$Attestation.payload_sha256).ToUpperInvariant()) { throw 'build attestation payload hash mismatch' }
    foreach ($name in $contract.CMakeOptions.Keys) {
        if ([string]$Attestation.cmake.$name -cne [string]$contract.CMakeOptions[$name]) {
            throw "build attestation CMake value mismatch: $name"
        }
    }
    $buildRows = @($Attestation.build_artifacts)
    if ($buildRows.Count -ne $contract.DllNames.Count) { throw 'build attestation artifact list is incomplete' }
    $names = @($buildRows | ForEach-Object { [string]$_.name })
    if (@($contract.DllNames | Where-Object { $names -notcontains $_ }).Count -gt 0 -or
        @($names | Where-Object { $contract.DllNames -notcontains $_ }).Count -gt 0 -or
        @($names | Group-Object | Where-Object Count -gt 1).Count -gt 0) {
        throw 'build attestation artifact set is not exact'
    }
    foreach ($row in $buildRows) {
        if ([string]$row.sha256 -notmatch '^[0-9A-Fa-f]{64}$' -or [int64]$row.size -lt 0 -or
            [string]::IsNullOrWhiteSpace([string]$row.build_path)) {
            throw "build attestation artifact row is invalid: $($row.name)"
        }
    }
    foreach ($name in @('source_directory', 'patch_path', 'cmake_cache_path')) {
        if ([string]::IsNullOrWhiteSpace([string]$Attestation.build.$name)) {
            throw "build attestation evidence is missing: $name"
        }
    }
}

function Assert-ZenzaiVulkanBuildAttestation {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$AttestationPath,
        [switch]$ForTesting
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $path = [IO.Path]::GetFullPath($AttestationPath)
    $attestation = Read-ZenzaiJsonFile -Path $path -Label 'Zenzai Vulkan build attestation'
    Assert-ZenzaiBuildAttestationShape -Attestation $attestation -ForTesting:$ForTesting
    if (-not [bool]$attestation.testing_seam) {
        $expectedPath = Get-ZenzaiVulkanBuildAttestationPath -BuildDirectory `
            (Join-Path (Get-ZenzaiRepositoryRoot) '.llama-build\build-vulkan-dl')
        if (-not [string]::Equals($path, [IO.Path]::GetFullPath($expectedPath),
                [StringComparison]::OrdinalIgnoreCase)) {
            throw "production build attestation is not at the canonical build output: $path"
        }
        $source = [IO.Path]::GetFullPath([string]$attestation.build.source_directory)
        $patch = [IO.Path]::GetFullPath([string]$attestation.build.patch_path)
        $cache = [IO.Path]::GetFullPath([string]$attestation.build.cmake_cache_path)
        Assert-ZenzaiCanonicalProductionBuildPaths -RuntimeDirectory (Join-Path (Get-ZenzaiRepositoryRoot) 'engine-host\vendor\llama\vulkan') `
            -SourceDirectory $source -PatchPath $patch -CMakeCachePath $cache | Out-Null
        Assert-ZenzaiRegularPath -Path $source -Label 'llama source evidence' -Directory | Out-Null
        Assert-ZenzaiRegularPath -Path $patch -Label 'llama patch evidence' | Out-Null
        Assert-ZenzaiRegularPath -Path $cache -Label 'CMake cache evidence' | Out-Null
        $head = (& git -C $source rev-parse HEAD 2>$null).Trim()
        if ($LASTEXITCODE -ne 0 -or $head -cne $contract.Commit) { throw "llama source evidence is not pinned: $source" }
        if ((Get-ZenzaiFileSha256 -Path $patch) -cne $contract.PatchSha256) { throw 'llama patch evidence hash mismatch' }
        if ((Get-ZenzaiFileSha256 -Path $cache) -cne ([string]$attestation.cmake_cache_sha256).ToUpperInvariant()) { throw 'CMake cache evidence hash mismatch' }
        foreach ($name in $contract.CMakeOptions.Keys) {
            $actual = Get-ZenzaiCMakeCacheValue -CachePath $cache -Name $name
            if ($actual -cne [string]$contract.CMakeOptions[$name]) { throw "CMake cache evidence mismatch: $name=$actual" }
        }
        $patchMarker = Join-Path $source '.nospacekey-runtime-patch.sha256'
        if (-not (Test-Path -LiteralPath $patchMarker -PathType Leaf) -or
            (Get-Content -LiteralPath $patchMarker -Raw).Trim().ToUpperInvariant() -cne $contract.PatchSha256) {
            throw 'patched llama source is missing its pinned patch marker'
        }
        $buildRoot = [IO.Path]::GetFullPath((Split-Path -Parent $cache))
        $buildPrefix = $buildRoot.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
        foreach ($row in @($attestation.build_artifacts)) {
            $buildPath = [IO.Path]::GetFullPath([string]$row.build_path)
            if (-not $buildPath.StartsWith($buildPrefix, [StringComparison]::OrdinalIgnoreCase)) {
                throw "build artifact is outside the canonical build output: $buildPath"
            }
            $item = Assert-ZenzaiRegularPath -Path $buildPath -Label "build artifact '$($row.name)'"
            if ([int64]$item.Length -ne [int64]$row.size -or
                (Get-ZenzaiFileSha256 -Path $buildPath) -cne ([string]$row.sha256).ToUpperInvariant()) {
                throw "build artifact evidence hash mismatch: $($row.name)"
            }
        }
    }
    [pscustomobject]@{ AttestationPath = $path; Attestation = $attestation }
}

function Assert-ZenzaiVulkanRuntimeReceipt {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [string]$ReceiptPath,
        [string]$BuildAttestationPath,
        [switch]$ForTesting,
        [switch]$AllowArtifactChanges
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    Assert-ZenzaiRegularPath -Path $runtimeFull -Label 'Zenzai Vulkan runtime directory' -Directory | Out-Null
    if ([string]::IsNullOrWhiteSpace($ReceiptPath)) { $ReceiptPath = Join-Path $runtimeFull $contract.ReceiptName }
    else { $ReceiptPath = [IO.Path]::GetFullPath($ReceiptPath) }
    $prefix = $runtimeFull.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
    if (-not $ReceiptPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'provenance receipt must be inside the runtime directory' }
    $receipt = Read-ZenzaiVulkanRuntimeReceipt -ReceiptPath $ReceiptPath
    Assert-ZenzaiReceiptShape -Receipt $receipt -ForTesting:$ForTesting
    if (-not [string]::IsNullOrWhiteSpace($BuildAttestationPath)) {
        $attestationResult = Assert-ZenzaiVulkanBuildAttestation -AttestationPath $BuildAttestationPath -ForTesting:$ForTesting
        if ((Get-ZenzaiFileSha256 -Path $attestationResult.AttestationPath) -cne ([string]$receipt.attestation_sha256).ToUpperInvariant()) {
            throw 'runtime receipt is not bound to the local build attestation'
        }
        $attestation = $attestationResult.Attestation
        foreach ($name in @('source_commit', 'patch_sha256', 'cmake_cache_sha256')) {
            $receiptValue = [string]$receipt.$name
            $attestationValue = [string]$attestation.$name
            if ($name -ne 'source_commit') {
                $receiptValue = $receiptValue.ToUpperInvariant()
                $attestationValue = $attestationValue.ToUpperInvariant()
            }
            if ($receiptValue -cne $attestationValue) { throw "runtime receipt differs from build attestation: $name" }
        }
        foreach ($name in $contract.CMakeOptions.Keys) {
            if ([string]$receipt.cmake.$name -cne [string]$attestation.cmake.$name) { throw "runtime receipt differs from build attestation: $name" }
        }
    }
    if (-not $AllowArtifactChanges) {
        $actualRows = Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull
        foreach ($expected in @($receipt.artifacts)) {
            $actual = @($actualRows | Where-Object { $_.name -ceq [string]$expected.name })[0]
            if ($null -eq $actual -or [int64]$actual.size -ne [int64]$expected.size -or
                $actual.sha256 -cne ([string]$expected.sha256).ToUpperInvariant()) {
                throw "runtime DLL does not match provenance receipt: $($expected.name)"
            }
        }
    }
    [pscustomobject]@{ RuntimeDirectory = $runtimeFull; ReceiptPath = $ReceiptPath; Receipt = $receipt }
}

function Write-ZenzaiVulkanBuildAttestation {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$PatchPath,
        [Parameter(Mandatory)][string]$CMakeCachePath
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    $sourceFull = [IO.Path]::GetFullPath($SourceDirectory)
    $patchFull = [IO.Path]::GetFullPath($PatchPath)
    $cacheFull = [IO.Path]::GetFullPath($CMakeCachePath)
    Assert-ZenzaiCanonicalProductionBuildPaths -RuntimeDirectory $runtimeFull `
        -SourceDirectory $sourceFull -PatchPath $patchFull -CMakeCachePath $cacheFull | Out-Null
    $cmake = [ordered]@{}
    foreach ($name in $contract.CMakeOptions.Keys) { $cmake[$name] = Get-ZenzaiCMakeCacheValue -CachePath $cacheFull -Name $name }
    $gitCommit = (& git -C $sourceFull rev-parse HEAD 2>$null).Trim()
    if ($LASTEXITCODE -ne 0) { throw "cannot resolve llama source commit: $sourceFull" }
    $buildArtifacts = @(Get-ZenzaiBuildArtifactRows -BuildDirectory (Split-Path -Parent $cacheFull))
    $runtimeArtifacts = @(Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull)
    foreach ($runtimeArtifact in $runtimeArtifacts) {
        $buildArtifact = @($buildArtifacts | Where-Object { $_.name -ceq $runtimeArtifact.name })[0]
        if ($null -eq $buildArtifact -or [int64]$buildArtifact.size -ne [int64]$runtimeArtifact.size -or
            $buildArtifact.sha256 -cne $runtimeArtifact.sha256) {
            throw "runtime DLL '$($runtimeArtifact.name)' does not match the canonical CMake build output"
        }
    }
    $attestation = [ordered]@{
        schema             = $contract.ReceiptSchema
        kind               = 'zenzai-vulkan-build-attestation'
        configuration      = $contract.Configuration
        tag                = $contract.Tag
        source_commit      = $gitCommit
        patch_sha256       = Get-ZenzaiFileSha256 -Path $patchFull
        cmake_cache_sha256 = Get-ZenzaiFileSha256 -Path $cacheFull
        cmake              = $cmake
        build_artifacts    = $buildArtifacts
        build              = [ordered]@{
            source_directory = $sourceFull
            patch_path       = $patchFull
            cmake_cache_path = $cacheFull
        }
        testing_seam       = $false
    }
    $attestation.payload_sha256 = Get-ZenzaiTextSha256 -Text (ConvertTo-ZenzaiCanonicalJson $attestation)
    $attestationPath = Get-ZenzaiVulkanBuildAttestationPath -BuildDirectory (Split-Path -Parent $cacheFull)
    [IO.File]::WriteAllText($attestationPath,
        ($attestation | ConvertTo-Json -Depth 20) + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false))
    Assert-ZenzaiVulkanBuildAttestation -AttestationPath $attestationPath | Out-Null
    $attestationPath
}

function Write-ZenzaiVulkanRuntimeReceipt {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [string]$SourceDirectory,
        [string]$PatchPath,
        [string]$CMakeCachePath,
        [string]$BuildAttestationPath,
        [switch]$ForTesting
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    Assert-ZenzaiRegularPath -Path $runtimeFull -Label 'Zenzai Vulkan runtime directory' -Directory | Out-Null
    if ($ForTesting) {
        $runtimeArtifacts = @(Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull)
        $attestationHash = ('0' * 64)
        $sourceCommit = $contract.Commit
        $patchHash = $contract.PatchSha256
        $cacheHash = ('0' * 64)
        $cmake = [ordered]@{}
        foreach ($name in $contract.CMakeOptions.Keys) { $cmake[$name] = [string]$contract.CMakeOptions[$name] }
        $buildArtifacts = @(ConvertTo-ZenzaiPortableArtifactRows -Rows $runtimeArtifacts)
    } else {
        $expectedRuntime = [IO.Path]::GetFullPath((Join-Path (Get-ZenzaiRepositoryRoot) 'engine-host\vendor\llama\vulkan'))
        if (-not [string]::Equals($runtimeFull, $expectedRuntime, [StringComparison]::OrdinalIgnoreCase)) {
            throw "production provenance runtime output is not canonical: $runtimeFull"
        }
        if ([string]::IsNullOrWhiteSpace($BuildAttestationPath)) {
            if ([string]::IsNullOrWhiteSpace($SourceDirectory) -or [string]::IsNullOrWhiteSpace($PatchPath) -or
                [string]::IsNullOrWhiteSpace($CMakeCachePath)) {
                throw 'production provenance receipt requires build attestation or source/cache evidence'
            }
            $BuildAttestationPath = Write-ZenzaiVulkanBuildAttestation -RuntimeDirectory $runtimeFull `
                -SourceDirectory $SourceDirectory -PatchPath $PatchPath -CMakeCachePath $CMakeCachePath
        }
        $attestationResult = Assert-ZenzaiVulkanBuildAttestation -AttestationPath $BuildAttestationPath
        $attestation = $attestationResult.Attestation
        $runtimeArtifacts = @(Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull)
        $attestationHash = Get-ZenzaiFileSha256 -Path $attestationResult.AttestationPath
        $sourceCommit = [string]$attestation.source_commit
        $patchHash = [string]$attestation.patch_sha256
        $cacheHash = [string]$attestation.cmake_cache_sha256
        $cmake = [ordered]@{}
        foreach ($name in $contract.CMakeOptions.Keys) { $cmake[$name] = [string]$attestation.cmake.$name }
        $buildArtifacts = @(ConvertTo-ZenzaiPortableArtifactRows -Rows $attestation.build_artifacts)
        foreach ($runtimeArtifact in $runtimeArtifacts) {
            $buildArtifact = @($buildArtifacts | Where-Object { $_.name -ceq $runtimeArtifact.name })[0]
            if ($null -eq $buildArtifact -or [int64]$buildArtifact.size -ne [int64]$runtimeArtifact.size -or
                $buildArtifact.sha256 -cne $runtimeArtifact.sha256) {
                throw "runtime DLL '$($runtimeArtifact.name)' does not match the build attestation"
            }
        }
    }
    $receipt = [ordered]@{
        schema             = $contract.ReceiptSchema
        kind               = 'zenzai-vulkan-runtime-receipt'
        configuration      = $contract.Configuration
        tag                = $contract.Tag
        source_commit      = $sourceCommit
        patch_sha256       = $patchHash
        cmake_cache_sha256 = $cacheHash
        cmake              = $cmake
        artifacts          = $runtimeArtifacts
        build_artifacts    = $buildArtifacts
        attestation_sha256 = $attestationHash
        testing_seam       = [bool]$ForTesting
    }
    $receipt.payload_sha256 = Get-ZenzaiTextSha256 -Text (ConvertTo-ZenzaiCanonicalJson $receipt)
    $receiptPath = Join-Path $runtimeFull $contract.ReceiptName
    [IO.File]::WriteAllText($receiptPath,
        ($receipt | ConvertTo-Json -Depth 20) + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false))
    Assert-ZenzaiVulkanRuntimeReceipt -RuntimeDirectory $runtimeFull `
        -ReceiptPath $receiptPath -ForTesting:$ForTesting | Out-Null
    $receiptPath
}

function Write-ZenzaiVulkanRuntimeManifest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [string]$ReceiptPath,
        [switch]$RequireExactDllSet,
        [switch]$ForTesting,
        [switch]$Signed
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    Assert-ZenzaiRegularPath -Path $runtimeFull -Label 'Zenzai Vulkan runtime directory' -Directory | Out-Null
    if ([string]::IsNullOrWhiteSpace($ReceiptPath)) { $ReceiptPath = Join-Path $runtimeFull $contract.ReceiptName }
    $receiptResult = Assert-ZenzaiVulkanRuntimeReceipt -RuntimeDirectory $runtimeFull `
        -ReceiptPath $ReceiptPath -ForTesting:$ForTesting -AllowArtifactChanges:$Signed
    if ($RequireExactDllSet) {
        $unexpected = @(Get-ChildItem -LiteralPath $runtimeFull -Filter '*.dll' -File -Force |
            Where-Object { $contract.DllNames -notcontains $_.Name })
        if ($unexpected.Count -gt 0) { throw "Zenzai Vulkan runtime contains unexpected DLL(s): $($unexpected.Name -join ', ')" }
    }
    $receiptHash = Get-ZenzaiFileSha256 -Path $receiptResult.ReceiptPath
    $receiptBuildArtifacts = @($receiptResult.Receipt.build_artifacts)
    if ($receiptBuildArtifacts.Count -eq 0) {
        # The explicit testing seam has no CMake output tree; its fixture rows
        # remain useful for exercising the hash contract.
        $receiptBuildArtifacts = @($receiptResult.Receipt.artifacts)
    }
    $manifest = [ordered]@{
        schema          = $contract.Schema
        configuration   = $contract.Configuration
        payload_state   = if ($Signed) { 'signed' } else { 'unsigned' }
        receipt_sha256  = $receiptHash
        receipt_payload_sha256 = ([string]$receiptResult.Receipt.payload_sha256).ToUpperInvariant()
        provenance      = [ordered]@{
            tag           = [string]$receiptResult.Receipt.tag
            source_commit = [string]$receiptResult.Receipt.source_commit
            patch_sha256  = ([string]$receiptResult.Receipt.patch_sha256).ToUpperInvariant()
            attestation_sha256 = ([string]$receiptResult.Receipt.attestation_sha256).ToUpperInvariant()
            cmake         = [ordered]@{}
            build_artifacts = $receiptBuildArtifacts
            testing_seam  = [bool]$receiptResult.Receipt.testing_seam
        }
        dlls            = @(Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull)
    }
    foreach ($name in $contract.CMakeOptions.Keys) {
        $manifest.provenance.cmake[$name] = [string]$receiptResult.Receipt.cmake.$name
    }
    $manifest.manifest_payload_sha256 = Get-ZenzaiTextSha256 -Text `
        (ConvertTo-ZenzaiCanonicalJson (Get-ZenzaiManifestPayload -Manifest ([pscustomobject]$manifest)))
    $manifestPath = Join-Path $runtimeFull $contract.ManifestName
    [IO.File]::WriteAllText($manifestPath,
        ($manifest | ConvertTo-Json -Depth 20) + [Environment]::NewLine,
        [Text.UTF8Encoding]::new($false))
    return $manifestPath
}

function Assert-ZenzaiVulkanRuntimeBundle {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [string]$ManifestPath,
        [switch]$RequireExactDllSet,
        [switch]$ForTesting
    )
    $contract = Get-ZenzaiVulkanRuntimeContract
    $runtimeFull = [IO.Path]::GetFullPath($RuntimeDirectory)
    Assert-ZenzaiRegularPath -Path $runtimeFull -Label 'Zenzai Vulkan runtime directory' -Directory | Out-Null
    if ([string]::IsNullOrWhiteSpace($ManifestPath)) { $ManifestPath = Join-Path $runtimeFull $contract.ManifestName }
    else { $ManifestPath = [IO.Path]::GetFullPath($ManifestPath) }
    $prefix = $runtimeFull.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
    if (-not $ManifestPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Zenzai Vulkan runtime manifest must be inside the runtime directory'
    }
    $manifest = Read-ZenzaiVulkanRuntimeManifest -ManifestPath $ManifestPath
    if ([int]$manifest.schema -ne $contract.Schema) { throw 'Zenzai Vulkan runtime manifest schema mismatch' }
    if ([string]$manifest.configuration -cne $contract.Configuration) { throw 'Zenzai Vulkan runtime manifest configuration mismatch' }
    if (@('unsigned', 'signed') -notcontains [string]$manifest.payload_state) {
        throw 'Zenzai Vulkan runtime manifest payload state is invalid'
    }
    if (@($manifest.PSObject.Properties.Name) -notcontains 'manifest_payload_sha256') { throw 'manifest payload hash is missing' }
    $manifestPayloadHash = Get-ZenzaiTextSha256 -Text `
        (ConvertTo-ZenzaiCanonicalJson (Get-ZenzaiManifestPayload -Manifest $manifest))
    if ($manifestPayloadHash -cne ([string]$manifest.manifest_payload_sha256).ToUpperInvariant()) {
        throw 'distribution manifest payload hash mismatch (manifest was modified)'
    }
    $receiptPath = Join-Path $runtimeFull $contract.ReceiptName
    $signed = [string]$manifest.payload_state -ceq 'signed'
    $receiptResult = Assert-ZenzaiVulkanRuntimeReceipt -RuntimeDirectory $runtimeFull `
        -ReceiptPath $receiptPath -ForTesting:$ForTesting -AllowArtifactChanges:$signed
    if ((Get-ZenzaiFileSha256 -Path $receiptPath) -cne ([string]$manifest.receipt_sha256).ToUpperInvariant()) {
        throw 'distribution manifest is not bound to its provenance receipt'
    }
    if ([string]$manifest.receipt_payload_sha256 -cne ([string]$receiptResult.Receipt.payload_sha256).ToUpperInvariant()) {
        throw 'distribution manifest receipt payload identity mismatch'
    }
    $prov = $manifest.provenance
    if ([bool]$prov.testing_seam -and -not $ForTesting) { throw 'testing runtime provenance cannot pass the production gate' }
    if ([bool]$prov.testing_seam -ne [bool]$receiptResult.Receipt.testing_seam) {
        throw 'distribution manifest testing seam does not match its receipt'
    }
    if ([string]$prov.tag -cne [string]$receiptResult.Receipt.tag -or
        [string]$prov.source_commit -cne [string]$receiptResult.Receipt.source_commit -or
        [string]$prov.patch_sha256 -cne ([string]$receiptResult.Receipt.patch_sha256).ToUpperInvariant() -or
        [string]$prov.attestation_sha256 -cne ([string]$receiptResult.Receipt.attestation_sha256).ToUpperInvariant()) {
        throw 'distribution manifest provenance does not match its receipt'
    }
    foreach ($name in $contract.CMakeOptions.Keys) {
        if ([string]$prov.cmake.$name -cne [string]$receiptResult.Receipt.cmake.$name) {
            throw "distribution manifest CMake provenance mismatch: $name"
        }
    }
    $expectedBuildArtifacts = @($receiptResult.Receipt.build_artifacts)
    if ($expectedBuildArtifacts.Count -eq 0) {
        $expectedBuildArtifacts = @($receiptResult.Receipt.artifacts)
    }
    $manifestBuildArtifacts = @($prov.build_artifacts)
    if ($manifestBuildArtifacts.Count -ne $expectedBuildArtifacts.Count) {
        throw 'distribution manifest build artifact list is incomplete or contains extras'
    }
    $manifestBuildNames = @($manifestBuildArtifacts | ForEach-Object { [string]$_.name })
    if (@($contract.DllNames | Where-Object { $manifestBuildNames -notcontains $_ }).Count -gt 0 -or
        @($manifestBuildNames | Group-Object | Where-Object Count -gt 1).Count -gt 0) {
        throw 'distribution manifest build artifact set is not exact'
    }
    foreach ($expected in $expectedBuildArtifacts) {
        $actual = @($manifestBuildArtifacts | Where-Object { [string]$_.name -ceq [string]$expected.name })[0]
        if ($null -eq $actual -or [int64]$actual.size -ne [int64]$expected.size -or
            [string]$actual.sha256 -cne ([string]$expected.sha256).ToUpperInvariant()) {
            throw "distribution manifest build artifact provenance mismatch: $($expected.name)"
        }
    }
    if ($RequireExactDllSet) {
        $unexpected = @(Get-ChildItem -LiteralPath $runtimeFull -Filter '*.dll' -File -Force |
            Where-Object { $contract.DllNames -notcontains $_.Name })
        if ($unexpected.Count -gt 0) { throw "Zenzai Vulkan runtime contains unexpected DLL(s): $($unexpected.Name -join ', ')" }
    }
    $manifestDlls = @($manifest.dlls)
    if ($manifestDlls.Count -ne $contract.DllNames.Count) { throw 'manifest DLL list is incomplete' }
    $actualRows = Get-ZenzaiRuntimeArtifactRows -RuntimeDirectory $runtimeFull
    if ($signed) {
        # A caller cannot opt into the post-signing exception merely by
        # editing payload_state. Authenticode is the second binding after the
        # unsigned build receipt, because signing legitimately changes DLL
        # bytes after the receipt was produced.
        foreach ($name in $contract.DllNames) {
            $path = Join-Path $runtimeFull $name
            $signature = Get-AuthenticodeSignature -FilePath $path
            if ($null -eq $signature.SignerCertificate -or
                @('Valid', 'UnknownError') -notcontains [string]$signature.Status) {
                throw "signed runtime DLL has no usable Authenticode signature: $name"
            }
        }
    }
    foreach ($name in $contract.DllNames) {
        $expected = @($manifestDlls | Where-Object { [string]$_.name -ceq $name })[0]
        $actual = @($actualRows | Where-Object { $_.name -ceq $name })[0]
        if ($null -eq $expected -or [int64]$expected.size -ne [int64]$actual.size -or
            [string]$expected.sha256 -cne [string]$actual.sha256) {
            throw "distribution DLL hash mismatch: $name"
        }
        if (-not $signed) {
            $build = @($expectedBuildArtifacts | Where-Object { [string]$_.name -ceq $name })[0]
            if ([string]$build.sha256 -cne [string]$actual.sha256 -or [int64]$build.size -ne [int64]$actual.size) {
                throw "unsigned distribution DLL differs from receipt: $name"
            }
        }
    }
    return [pscustomobject]@{
        RuntimeDirectory = $runtimeFull
        ManifestPath     = $ManifestPath
        ReceiptPath      = $receiptPath
        Manifest         = $manifest
        Receipt          = $receiptResult.Receipt
        Dlls             = @($actualRows | ForEach-Object {
            [pscustomobject]@{ Name = $_.name; Path = Join-Path $runtimeFull $_.name; Sha256 = $_.sha256; Length = $_.size }
        })
    }
}
