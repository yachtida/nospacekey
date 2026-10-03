param(
  [switch]$Vulkan,
  [string]$Tag     = "b4846",
  [string]$Repo    = "https://github.com/azooKey/llama.cpp",
  [string]$WorkDir = "$PSScriptRoot\..\.llama-build",
  [string]$OutDir  = "$PSScriptRoot\..\engine-host\vendor\llama",
  [string]$PatchPath = "$PSScriptRoot\..\patches\llama-b4846-nospacekey-vulkan-required.patch"
)
# Build the azooKey llama.cpp fork (pinned to the tag the converter v0.11.2 references) into
# shared libs, and collect only explicitly required import/runtime files. CPU
# artifacts keep the historical vendor/llama location; Vulkan is isolated below
# vendor/llama/vulkan so the two configurations cannot be mixed.
# CPU by default; pass -Vulkan to enable the GGML_VULKAN backend (needs the Vulkan SDK + glslc).
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot 'zenzai-runtime-manifest.ps1')

$expectedLlamaCommit = "10131b23ee0dbd9d03dfc618cec9fe3402919277"
$expectedPatchSha256 = "77B59C26C36CF3E6EA5EB89267E14D1E06C8F85CF83B13522DF111E1E4BC4F82"

function Assert-LlamaCommit {
  param([string]$Path)

  $actual = (& git -C $Path rev-parse HEAD 2>$null).Trim()
  if ($LASTEXITCODE -ne 0 -or $actual -ne $expectedLlamaCommit) {
    throw "llama checkout '$Path' is not the pinned $Tag commit (expected $expectedLlamaCommit, got '$actual')."
  }
}

function Resolve-OwnedChildPath {
  param(
    [string]$Root,
    [string]$Child,
    [string]$Label
  )

  $rootFull = [IO.Path]::GetFullPath($Root)
  $childFull = [IO.Path]::GetFullPath($Child)
  $rootPrefix = $rootFull.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
  if (-not $childFull.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "$Label '$childFull' must be inside '$rootFull'."
  }

  $cursor = $rootFull
  $pathsToCheck = @($rootFull)
  $relativeChild = $childFull.Substring($rootPrefix.Length)
  foreach ($component in $relativeChild.Split([char[]]@('\', '/'), [StringSplitOptions]::RemoveEmptyEntries)) {
    $cursor = Join-Path $cursor $component
    $pathsToCheck += $cursor
  }
  foreach ($candidate in $pathsToCheck) {
    if (Test-Path -LiteralPath $candidate) {
      $attributes = [IO.File]::GetAttributes($candidate)
      if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "$Label '$childFull' crosses reparse point '$candidate'."
      }
    }
  }
  return $childFull
}

function Prepare-PatchedSource {
  param(
    [string]$BasePath,
    [string]$Path,
    [string]$PatchFile,
    [string]$PatchSha256
  )

  if (-not (Test-Path -LiteralPath $PatchFile -PathType Leaf)) {
    throw "runtime seam patch is missing: $PatchFile"
  }
  $actualPatchSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $PatchFile).Hash.ToUpperInvariant()
  if ($actualPatchSha256 -ne $PatchSha256) {
    throw "runtime seam patch hash mismatch (expected $PatchSha256, got $actualPatchSha256)."
  }

  $baseFull = [IO.Path]::GetFullPath($BasePath)
  $overlayFull = Resolve-OwnedChildPath -Root $baseFull -Child $Path -Label "patched overlay"
  if (Test-Path -LiteralPath $overlayFull) {
    Remove-Item -LiteralPath $overlayFull -Recurse -Force
  }
  git clone --depth 1 --no-local --branch $Tag $baseFull $overlayFull
  if ($LASTEXITCODE -ne 0) { throw "git clone failed for clean runtime seam overlay" }
  Assert-LlamaCommit -Path $overlayFull

  & git -C $overlayFull apply --check $PatchFile
  if ($LASTEXITCODE -ne 0) { throw "runtime seam patch does not apply cleanly to '$overlayFull'." }
  & git -C $overlayFull apply $PatchFile
  if ($LASTEXITCODE -ne 0) { throw "runtime seam patch application failed for '$overlayFull'." }
  Set-Content -LiteralPath (Join-Path $overlayFull ".nospacekey-runtime-patch.sha256") -Value $PatchSha256 -NoNewline -Encoding ascii
  Assert-LlamaCommit -Path $overlayFull
  return $overlayFull
}

function Resolve-VulkanSdk {
  $sdkCandidates = @()
  if (-not [string]::IsNullOrWhiteSpace($env:VULKAN_SDK)) {
    $sdkCandidates = @($env:VULKAN_SDK)
  } else {
    # The LunarG installer uses this location even when the caller did not
    # start a shell with VULKAN_SDK exported.
    $defaultRoot = "C:\VulkanSDK"
    if (Test-Path -LiteralPath $defaultRoot -PathType Container) {
      $sdkCandidates = @($defaultRoot)
      $sdkCandidates += @(Get-ChildItem -LiteralPath $defaultRoot -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        Select-Object -ExpandProperty FullName)
    }
  }
  if ($sdkCandidates.Count -eq 0) {
    throw "Vulkan preflight failed: VULKAN_SDK is not set and no SDK was found under C:\VulkanSDK. Install the Vulkan SDK or set VULKAN_SDK to its root (expected Include\vulkan\vulkan.h, Lib\vulkan-1.lib, and Bin\glslc.exe)."
  }

  $lastReason = $null
  foreach ($candidate in $sdkCandidates) {
    try {
      $sdkRoot = [IO.Path]::GetFullPath([string]$candidate)
    } catch {
      $lastReason = "VULKAN_SDK '$candidate' is not a valid path."
      continue
    }
    if (-not (Test-Path -LiteralPath $sdkRoot -PathType Container)) {
      $lastReason = "Vulkan SDK directory '$sdkRoot' does not exist."
      continue
    }

    $includeDir = Join-Path $sdkRoot "Include"
    $header = Join-Path $includeDir "vulkan\vulkan.h"
    if (-not (Test-Path -LiteralPath $header -PathType Leaf)) {
      $lastReason = "SDK header '$header' is missing."
      continue
    }

    $library = Join-Path (Join-Path $sdkRoot "Lib") "vulkan-1.lib"
    if (-not (Test-Path -LiteralPath $library -PathType Leaf)) {
      $lastReason = "SDK import library '$library' is missing."
      continue
    }

    $glslc = Join-Path (Join-Path $sdkRoot "Bin") "glslc.exe"
    if (-not (Test-Path -LiteralPath $glslc -PathType Leaf)) {
      $glslcCommand = Get-Command glslc.exe -CommandType Application -ErrorAction SilentlyContinue
      if ($null -eq $glslcCommand) {
        $glslcCommand = Get-Command glslc -CommandType Application -ErrorAction SilentlyContinue
      }
      $glslcPath = if ($null -eq $glslcCommand) { $null } elseif ($glslcCommand.Path) { $glslcCommand.Path } else { $glslcCommand.Source }
      if ([string]::IsNullOrWhiteSpace($glslcPath) -or -not (Test-Path -LiteralPath $glslcPath -PathType Leaf)) {
        $lastReason = "glslc.exe is missing from '$($sdkRoot)\Bin' and PATH."
        continue
      }
      $glslc = [IO.Path]::GetFullPath($glslcPath)
    }

    return [pscustomobject]@{
      Root = $sdkRoot
      IncludeDir = $includeDir
      Library = $library
      Glslc = $glslc
    }
  }

  throw "Vulkan preflight failed: no complete Vulkan SDK was found. $lastReason Install the Vulkan SDK or correct VULKAN_SDK."
}

function Assert-CMakeBool {
  param(
    [string]$CachePath,
    [string]$Name,
    [bool]$Expected
  )

  $escapedName = [regex]::Escape($Name)
  $entry = Select-String -LiteralPath $CachePath -Pattern "^${escapedName}:(?:BOOL|INTERNAL)=(ON|OFF)$" |
    Select-Object -First 1
  if ($null -eq $entry) {
    throw "CMake cache assertion failed: $Name is not present in '$CachePath'."
  }

  $actual = ([regex]::Match($entry.Line, "=(ON|OFF)$")).Groups[1].Value
  $expectedText = if ($Expected) { "ON" } else { "OFF" }
  if ($actual -ne $expectedText) {
    throw "CMake cache assertion failed: $Name=$actual, expected $expectedText in '$CachePath'."
  }
}

function Find-RequiredArtifact {
  param(
    [string]$BuildDir,
    [string]$Name
  )

  $matches = @(Get-ChildItem -LiteralPath $BuildDir -Recurse -File -Filter $Name -ErrorAction SilentlyContinue)
  if ($matches.Count -ne 1) {
    throw "Required artifact '$Name' was not uniquely produced under '$BuildDir' (found $($matches.Count))."
  }
  return $matches[0]
}

$configuration = if ($Vulkan) { "vulkan" } else { "cpu" }
$vulkanSdk = $null
if ($Vulkan) {
  # Keep this before clone/configure so an incomplete SDK cannot leave a partial build behind.
  $vulkanSdk = Resolve-VulkanSdk
  $env:VULKAN_SDK = $vulkanSdk.Root
  $sdkBin = Join-Path $vulkanSdk.Root "Bin"
  if (-not (($env:Path -split ';') -contains $sdkBin)) {
    $env:Path = "$sdkBin;$env:Path"
  }
}

# 1) shallow clone pinned to the tag
$resolvedWorkDir = [IO.Path]::GetFullPath($WorkDir)
$resolvedOutDir = [IO.Path]::GetFullPath($OutDir)
$resolvedPatchPath = [IO.Path]::GetFullPath($PatchPath)
if (-not (Test-Path -LiteralPath $resolvedWorkDir -PathType Container)) {
  git clone --depth 1 --branch $Tag $Repo $resolvedWorkDir
  if ($LASTEXITCODE -ne 0) { throw "git clone failed (tag $Tag from $Repo)" }
} else {
  Write-Host "reusing existing clone: $resolvedWorkDir"
}
Assert-LlamaCommit -Path $resolvedWorkDir

$sourceDir = $resolvedWorkDir
if ($Vulkan) {
  $patchedSource = Join-Path $resolvedWorkDir "source-$Tag-patched"
  $sourceDir = Prepare-PatchedSource -BasePath $resolvedWorkDir -Path $patchedSource -PatchFile $resolvedPatchPath -PatchSha256 $expectedPatchSha256
}

# 2) configure + build shared libs (Vulkan backend optional)
$buildName = if ($Vulkan) { "build-vulkan-dl" } else { "build-cpu" }
$build = Resolve-OwnedChildPath -Root $resolvedWorkDir -Child (Join-Path $resolvedWorkDir $buildName) -Label "build directory"
if (Test-Path -LiteralPath $build) {
  Remove-Item -LiteralPath $build -Recurse -Force
}
# GGML_NATIVE=OFF + AVX2 明示: cmake 既定の GGML_NATIVE=ON はビルド機の CPU 機能
# (この開発機では AVX-512)を検出して焼き込むため、非搭載 CPU(Intel 12世代以降の
# 一般消費者向け・Core Ultra 等)では推論初回に 0xC000001D(不正命令)で即死する。
# v1.0.0 の実配布事故(Core Ultra 機でエンジンがクラッシュループ)の再発防止。
# GGML_CPU_ALL_VARIANTS(実行時ディスパッチ)を採らないのは、GGML_BACKEND_DL 前提で
# Swift 側の動的バックエンドロード検証が必要になり、配布 DLL も増えるため。
# AVX2 ベースラインは llama.cpp 公式バイナリ配布と同じ割り切り(Haswell 2013+)。
$backendDl = if ($Vulkan) { "ON" } else { "OFF" }
$vulkanOption = if ($Vulkan) { "ON" } else { "OFF" }
$flags = @("-S", $sourceDir, "-B", $build, "-DBUILD_SHARED_LIBS=ON",
           "-DLLAMA_BUILD_TESTS=OFF", "-DLLAMA_BUILD_EXAMPLES=OFF", "-DLLAMA_BUILD_SERVER=OFF",
           "-DGGML_NATIVE=OFF", "-DGGML_AVX=ON", "-DGGML_AVX2=ON", "-DGGML_BMI2=ON")
$flags += "-DGGML_VULKAN=$vulkanOption"
$flags += "-DGGML_BACKEND_DL=$backendDl"
if ($Vulkan) {
  $flags += "-DVulkan_INCLUDE_DIR=$($vulkanSdk.IncludeDir)"
  $flags += "-DVulkan_LIBRARY=$($vulkanSdk.Library)"
  $flags += "-DGLSLC_EXECUTABLE=$($vulkanSdk.Glslc)"
  $flags += "-DVulkan_GLSLC_EXECUTABLE=$($vulkanSdk.Glslc)"
}
cmake @flags
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed" }

$cachePath = Join-Path $build "CMakeCache.txt"
if (-not (Test-Path -LiteralPath $cachePath -PathType Leaf)) {
  throw "CMake configure did not produce '$cachePath'."
}
$cacheOptions = [ordered]@{
  BUILD_SHARED_LIBS = $true
  LLAMA_BUILD_TESTS = $false
  LLAMA_BUILD_EXAMPLES = $false
  LLAMA_BUILD_SERVER = $false
  GGML_NATIVE = $false
  GGML_AVX = $true
  GGML_AVX2 = $true
  GGML_BMI2 = $true
  GGML_VULKAN = $Vulkan
  GGML_BACKEND_DL = $Vulkan
}
foreach ($option in $cacheOptions.Keys) {
  Assert-CMakeBool -CachePath $cachePath -Name $option -Expected $cacheOptions[$option]
}

cmake --build $build --config Release
if ($LASTEXITCODE -ne 0) { throw "cmake build failed" }

# 3) collect only the files required by this configuration. The separate output
# directory keeps a CPU build from being mistaken for a Vulkan build.
$requiredDlls = @("llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll")
$requiredLibs = @("llama.lib", "ggml.lib", "ggml-base.lib", "ggml-cpu.lib")
if ($Vulkan) {
  $requiredDlls += "ggml-vulkan.dll"
  $requiredLibs += "ggml-vulkan.lib"
}
$requiredNames = @($requiredDlls + $requiredLibs)
$artifacts = foreach ($name in $requiredNames) {
  $source = Find-RequiredArtifact -BuildDir $build -Name $name
  [pscustomobject]@{ Name = $name; Source = $source.FullName }
}

$artifactDir = if ($Vulkan) { Join-Path $resolvedOutDir "vulkan" } else { $resolvedOutDir }
New-Item -ItemType Directory -Force -Path $artifactDir | Out-Null
Get-ChildItem -LiteralPath $artifactDir -File -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -eq "llama.dll" -or $_.Name -eq "llama.lib" -or $_.Name -like "ggml-*.dll" -or $_.Name -like "ggml-*.lib" -or $_.Name -eq "ggml.dll" -or $_.Name -eq "ggml.lib" } |
  Remove-Item -Force
foreach ($artifact in $artifacts) {
  Copy-Item -LiteralPath $artifact.Source -Destination (Join-Path $artifactDir $artifact.Name) -Force
}
if ($Vulkan) {
  # The receipt is emitted only after the exact bytes copied from this CMake
  # build are present. Staging consumes this evidence and derives its manifest;
  # it must never manufacture build identity from caller-supplied values.
  $attestationPath = Get-ZenzaiVulkanBuildAttestationPath -BuildDirectory $build
  $receiptPath = Get-ZenzaiVulkanRuntimeReceiptPath -RuntimeDirectory $artifactDir
  $manifestPath = Get-ZenzaiVulkanRuntimeManifestPath -RuntimeDirectory $artifactDir
  foreach ($metadataPath in @($attestationPath, $receiptPath, $manifestPath)) {
    if (Test-Path -LiteralPath $metadataPath) { Remove-Item -LiteralPath $metadataPath -Force }
  }
  Write-ZenzaiVulkanRuntimeReceipt -RuntimeDirectory $artifactDir `
    -SourceDirectory $sourceDir -PatchPath $resolvedPatchPath `
    -CMakeCachePath $cachePath | Out-Null
  Write-ZenzaiVulkanRuntimeManifest -RuntimeDirectory $artifactDir `
    -RequireExactDllSet | Out-Null
  Write-Host "Vulkan build attestation -> $attestationPath"
  Write-Host "Vulkan runtime provenance receipt -> $receiptPath"
  Write-Host "Vulkan runtime manifest -> $manifestPath"
}
Write-Host "llama artifacts ($configuration) -> $artifactDir"
Get-ChildItem -LiteralPath $artifactDir -File |
  Where-Object { $requiredNames -contains $_.Name } |
  Select-Object Name, Length |
  Format-Table -AutoSize
