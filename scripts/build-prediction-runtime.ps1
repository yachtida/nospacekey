<# Build the exact pinned llama.cpp Vulkan runtime used for inline prediction. #>
[CmdletBinding()]
param(
    [string]$SourceDir,
    [string]$OutputDir
)
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'prediction-runtime-contract.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$contract = Get-PredictionRuntimeContract
if (-not $SourceDir) {
    $SourceDir = Join-Path $RepoRoot 'experiments\inline-prediction\.tools\llama.cpp'
}
if (-not $OutputDir) {
    $OutputDir = Join-Path $RepoRoot 'engine-host\prediction-runtime'
}

# Resolve the SDK before cloning or configuring so a missing compiler input
# cannot leave a partial runtime tree behind.
$vulkanSdk = Resolve-PredictionVulkanSdk
$env:VULKAN_SDK = $vulkanSdk.Root
$sdkBin = Join-Path $vulkanSdk.Root 'Bin'
if (-not (($env:Path -split ';') -contains $sdkBin)) {
    $env:Path = "$sdkBin;$env:Path"
}

if (-not (Test-Path -LiteralPath $SourceDir -PathType Container)) {
    git clone --filter=blob:none https://github.com/ggml-org/llama.cpp.git $SourceDir
    if ($LASTEXITCODE -ne 0) { throw 'failed to clone llama.cpp' }
    git -C $SourceDir fetch origin $contract.Revision --depth 1
    if ($LASTEXITCODE -ne 0) { throw 'failed to fetch pinned llama.cpp revision' }
    git -C $SourceDir checkout --detach $contract.Revision
    if ($LASTEXITCODE -ne 0) { throw 'failed to checkout pinned llama.cpp revision' }
}
$head = (git -C $SourceDir rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -ne $contract.Revision) {
    throw "llama.cpp source must be exactly $($contract.Revision) (actual: $head)"
}
$sourceChanges = @(git -C $SourceDir status --porcelain --untracked-files=all)
if ($LASTEXITCODE -ne 0 -or $sourceChanges.Count -ne 0) {
    throw 'llama.cpp source must be clean before issuing a fixed build receipt'
}

$resolvedSource = [IO.Path]::GetFullPath($SourceDir)
$BuildDir = Join-Path $RepoRoot '.llama-build\prediction-vulkan-dl'
$resolvedBuild = [IO.Path]::GetFullPath($BuildDir)
if (Test-Path -LiteralPath $resolvedBuild) {
    Remove-Item -LiteralPath $resolvedBuild -Recurse -Force
}

$configureArgs = @(
    '-S', $resolvedSource,
    '-B', $resolvedBuild,
    '-G', 'Visual Studio 17 2022',
    '-A', 'x64',
    '-DBUILD_SHARED_LIBS=ON',
    '-DLLAMA_BUILD_SERVER=ON',
    '-DLLAMA_BUILD_TESTS=OFF',
    '-DLLAMA_BUILD_EXAMPLES=OFF',
    '-DLLAMA_BUILD_UI=OFF',
    '-DLLAMA_USE_PREBUILT_UI=OFF',
    '-DGGML_NATIVE=OFF',
    '-DGGML_AVX=ON',
    '-DGGML_AVX2=ON',
    '-DGGML_VULKAN=ON',
    '-DGGML_BACKEND_DL=ON',
    "-DVulkan_INCLUDE_DIR=$($vulkanSdk.IncludeDir)",
    "-DVulkan_LIBRARY=$($vulkanSdk.Library)",
    "-DVulkan_GLSLC_EXECUTABLE=$($vulkanSdk.Glslc)"
)
cmake @configureArgs
if ($LASTEXITCODE -ne 0) { throw 'prediction Vulkan runtime configure failed' }

$cachePath = Join-Path $resolvedBuild 'CMakeCache.txt'
Assert-PredictionCMakeCache -CachePath $cachePath

cmake --build $resolvedBuild --config Release --target llama-server
if ($LASTEXITCODE -ne 0) { throw 'prediction Vulkan runtime build failed' }

$resolvedOutput = [IO.Path]::GetFullPath($OutputDir)
if ($resolvedOutput.Equals($resolvedSource, [StringComparison]::OrdinalIgnoreCase) -or
    $resolvedOutput.Equals($resolvedBuild, [StringComparison]::OrdinalIgnoreCase)) {
    throw "prediction runtime output must not replace source or build directory: $resolvedOutput"
}
$outputParent = Split-Path -Parent $resolvedOutput
New-Item -ItemType Directory -Path $outputParent -Force | Out-Null
$outputLeaf = Split-Path -Leaf $resolvedOutput
$stagingOutput = Join-Path $outputParent (".{0}.staging-{1}" -f $outputLeaf, [Guid]::NewGuid().ToString('N'))
$previousOutput = $null
try {
    New-Item -ItemType Directory -Path $stagingOutput -Force | Out-Null
    $artifactNames = @($contract.RequiredFiles | Where-Object {
        $_ -ne 'REVISION' -and $_ -ne $contract.ReceiptName
    })
    foreach ($name in $artifactNames) {
        $artifact = Get-PredictionRequiredArtifact -BuildDirectory $resolvedBuild -Name $name
        Copy-Item -LiteralPath $artifact.FullName -Destination (Join-Path $stagingOutput $name) -Force
    }
    Set-Content -LiteralPath (Join-Path $stagingOutput 'REVISION') `
        -Value $contract.Revision -Encoding ascii -NoNewline
    [IO.File]::WriteAllText(
        (Join-Path $stagingOutput $contract.ReceiptName),
        (Get-PredictionRuntimeBuildReceipt),
        [Text.Encoding]::ASCII)

    Assert-PredictionRuntimeBundle -RuntimeDirectory $stagingOutput | Out-Null

    if (Test-Path -LiteralPath $resolvedOutput) {
        $existingOutput = Get-Item -LiteralPath $resolvedOutput -Force -ErrorAction Stop
        if (($existingOutput.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "prediction runtime output must not be a reparse point: $resolvedOutput"
        }
        $previousOutput = Join-Path $outputParent (".{0}.previous-{1}" -f $outputLeaf, [Guid]::NewGuid().ToString('N'))
        # Rename the old tree before publishing. A loaded DLL may prevent file deletion,
        # but Windows can normally keep the old tree intact while the new tree is published.
        Move-Item -LiteralPath $resolvedOutput -Destination $previousOutput -ErrorAction Stop
    }
    Move-Item -LiteralPath $stagingOutput -Destination $resolvedOutput -ErrorAction Stop
    $stagingOutput = $null

    if ($previousOutput) {
        try { Remove-Item -LiteralPath $previousOutput -Recurse -Force -ErrorAction Stop }
        catch { Write-Warning "previous prediction runtime retained because it is in use: $previousOutput" }
    }
} catch {
    if ($stagingOutput -and (Test-Path -LiteralPath $stagingOutput)) {
        Remove-Item -LiteralPath $stagingOutput -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($previousOutput -and (Test-Path -LiteralPath $previousOutput) -and
        -not (Test-Path -LiteralPath $resolvedOutput)) {
        Move-Item -LiteralPath $previousOutput -Destination $resolvedOutput -ErrorAction SilentlyContinue
    }
    throw
}

Assert-PredictionRuntimeBundle -RuntimeDirectory $resolvedOutput | Out-Null
Write-Host "prediction Vulkan runtime staged at $resolvedOutput"
