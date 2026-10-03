<# Shared contract for the pinned inline-prediction Vulkan runtime. #>

function Get-PredictionRuntimeContract {
    [pscustomobject]@{
        Revision = 'c060ca974c773c7c3d17fd1b66dc9d312bc292c0'
        ReceiptName = 'BUILD-RECEIPT.txt'
        RequiredFiles = @(
            'llama-server.exe', 'llama-server-impl.dll', 'llama-common.dll',
            'llama.dll', 'ggml.dll', 'ggml-base.dll', 'ggml-cpu.dll',
            'ggml-vulkan.dll', 'mtmd.dll', 'REVISION', 'BUILD-RECEIPT.txt'
        )
        OptionalFiles = @(
            'vcomp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll'
        )
        BuildFlags = [ordered]@{
            BUILD_SHARED_LIBS = 'ON'
            GGML_BACKEND_DL = 'ON'
            GGML_VULKAN = 'ON'
            GGML_NATIVE = 'OFF'
            GGML_AVX2 = 'ON'
        }
    }
}

function Get-PredictionRuntimeBuildReceipt {
    $contract = Get-PredictionRuntimeContract
    $lines = @(
        'schema=nospacekey-inline-prediction-vulkan-v1',
        "llama_revision=$($contract.Revision)",
        'build_shared_libs=ON',
        'ggml_backend_dl=ON',
        'ggml_vulkan=ON',
        'ggml_native=OFF',
        'ggml_avx2=ON',
        'backend=Vulkan',
        'device=Vulkan0',
        'gpu_layers=all'
    )
    return ($lines -join "`r`n") + "`r`n"
}

function Resolve-PredictionVulkanSdk {
    $candidates = New-Object 'System.Collections.Generic.List[string]'
    if (-not [string]::IsNullOrWhiteSpace($env:VULKAN_SDK)) {
        [void]$candidates.Add($env:VULKAN_SDK)
    }

    $defaultRoot = 'C:\VulkanSDK'
    if (Test-Path -LiteralPath $defaultRoot -PathType Container) {
        # The LunarG installer keeps versioned SDK roots here.  Sort parsed
        # versions so 1.10.x is not treated as older than 1.9.x lexically.
        $versioned = @(Get-ChildItem -LiteralPath $defaultRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object @(
                @{ Expression = {
                    try { [version]$_.Name } catch { [version]'0.0' }
                }; Descending = $true },
                @{ Expression = { $_.Name }; Descending = $true }
            ))
        [void]$candidates.Add($defaultRoot)
        foreach ($item in $versioned) { [void]$candidates.Add($item.FullName) }
    }

    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    $lastReason = 'no SDK candidate was found'
    foreach ($candidate in $candidates) {
        try { $root = [IO.Path]::GetFullPath([string]$candidate) }
        catch {
            $lastReason = "SDK path '$candidate' is invalid"
            continue
        }
        if (-not $seen.Add($root)) { continue }
        if (-not (Test-Path -LiteralPath $root -PathType Container)) {
            $lastReason = "SDK directory '$root' does not exist"
            continue
        }

        $includeDir = Join-Path $root 'Include'
        $includeHeader = Join-Path $includeDir 'vulkan\vulkan.h'
        $library = Join-Path $root 'Lib\vulkan-1.lib'
        $glslc = Join-Path $root 'Bin\glslc.exe'
        if (-not (Test-Path -LiteralPath $includeHeader -PathType Leaf)) {
            $lastReason = "SDK header is missing: $includeHeader"
            continue
        }
        if (-not (Test-Path -LiteralPath $library -PathType Leaf)) {
            $lastReason = "SDK import library is missing: $library"
            continue
        }
        if (-not (Test-Path -LiteralPath $glslc -PathType Leaf)) {
            $lastReason = "SDK glslc is missing: $glslc"
            continue
        }

        return [pscustomobject]@{
            Root = $root
            IncludeDir = $includeDir
            Library = $library
            Glslc = $glslc
        }
    }

    throw "Vulkan SDK preflight failed: $lastReason. Expected Include\vulkan\vulkan.h, Lib\vulkan-1.lib, and Bin\glslc.exe under VULKAN_SDK or C:\VulkanSDK\<version>."
}

function Assert-PredictionCMakeCache {
    param(
        [Parameter(Mandatory)][string]$CachePath
    )
    if (-not (Test-Path -LiteralPath $CachePath -PathType Leaf)) {
        throw "CMake cache is missing: $CachePath"
    }
    $contract = Get-PredictionRuntimeContract
    foreach ($name in $contract.BuildFlags.Keys) {
        $escaped = [regex]::Escape($name)
        $entry = Select-String -LiteralPath $CachePath `
            -Pattern "^${escaped}:(?:BOOL|INTERNAL)=(ON|OFF)$" |
            Select-Object -First 1
        if ($null -eq $entry) {
            throw "CMake cache entry is missing: $name"
        }
        $actual = ([regex]::Match($entry.Line, '=(ON|OFF)$')).Groups[1].Value
        $expected = $contract.BuildFlags[$name]
        if ($actual -ne $expected) {
            throw "CMake cache entry mismatch: $name=$actual (expected $expected)"
        }
    }
}

function Get-PredictionRequiredArtifact {
    param(
        [Parameter(Mandatory)][string]$BuildDirectory,
        [Parameter(Mandatory)][string]$Name
    )
    $matches = @(Get-ChildItem -LiteralPath $BuildDirectory -Recurse -File `
        -Filter $Name -ErrorAction SilentlyContinue)
    if ($matches.Count -ne 1) {
        throw "Prediction artifact '$Name' was not uniquely produced under '$BuildDirectory' (found $($matches.Count))."
    }
    return $matches[0]
}

function Assert-PredictionRuntimeBundle {
    param(
        [Parameter(Mandatory)][string]$RuntimeDirectory,
        [switch]$AllowAdditionalRuntimeFiles
    )
    $contract = Get-PredictionRuntimeContract
    if (-not (Test-Path -LiteralPath $RuntimeDirectory -PathType Container)) {
        throw "prediction runtime directory is missing: $RuntimeDirectory"
    }
    $rootItem = Get-Item -LiteralPath $RuntimeDirectory -Force -ErrorAction Stop
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "prediction runtime directory is a reparse point: $RuntimeDirectory"
    }
    $root = [IO.Path]::GetFullPath($rootItem.FullName)
    foreach ($name in $contract.RequiredFiles) {
        $path = Join-Path $root $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "prediction runtime file is missing: $name"
        }
        $attributes = [IO.File]::GetAttributes($path)
        if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "prediction runtime file is a reparse point: $name"
        }
        if ((Get-Item -LiteralPath $path).Length -eq 0) {
            throw "prediction runtime file is empty: $name"
        }
    }

    $revision = (Get-Content -LiteralPath (Join-Path $root 'REVISION') -Raw).Trim()
    if ($revision -cne $contract.Revision) {
        throw "prediction runtime revision mismatch: '$revision'"
    }

    $receiptPath = Join-Path $root $contract.ReceiptName
    $actualReceipt = (Get-Content -LiteralPath $receiptPath -Raw) -replace "`r`n", "`n"
    $expectedReceipt = (Get-PredictionRuntimeBuildReceipt) -replace "`r`n", "`n"
    if ($actualReceipt.TrimEnd("`n") -cne $expectedReceipt.TrimEnd("`n")) {
        throw "prediction runtime build receipt mismatch: $receiptPath"
    }

    $allowed = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
    foreach ($name in $contract.RequiredFiles) { [void]$allowed.Add($name) }
    if ($AllowAdditionalRuntimeFiles) {
        foreach ($name in $contract.OptionalFiles) { [void]$allowed.Add($name) }
    }
    foreach ($entry in @(Get-ChildItem -LiteralPath $root -Force)) {
        if ($entry.PSIsContainer) {
            throw "unexpected prediction runtime directory: $($entry.Name)"
        }
        $file = $entry
        if ($file.Name -ieq 'vulkan-1.dll') {
            throw 'prediction runtime must not bundle the Vulkan loader vulkan-1.dll'
        }
        if (-not $allowed.Contains($file.Name)) {
            throw "unexpected prediction runtime file: $($file.Name)"
        }
    }

    [pscustomobject]@{
        Root = $root
        Revision = $contract.Revision
        ReceiptPath = $receiptPath
        BackendPath = Join-Path $root 'ggml-vulkan.dll'
    }
}
