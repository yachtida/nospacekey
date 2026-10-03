<#
  stage-dist.ps1 -- assemble the full self-contained 64-bit nospacekey runtime tree into dist\.
  ASCII-only on purpose (no Japanese -> no UTF-8 BOM requirement).

  The installer (installer\nospacekey.iss) ships dist\ verbatim; sign-dist.ps1 signs the PEs in it.
  This closes the gap the verify-*.ps1 scripts leave open: they run from the .build tree and never
  collect the SPM *.resources bundles or the Swift runtime DLLs, which a clean PC does not have.

  Layout produced (dist\):
    nospacekey_tip.dll                         <- target\release
    NospacekeyEngineHost.exe                    <- engine-host\.build\...\release
    NospacekeyConfig.exe                        <- target\release
    NospacekeyUpdateChecker.exe                 <- target\release (scheduled checker)
    *.resources\ (3 dirs: dictionary, EfficientNGram tokenizer, swift-transformers Hub)
    <Swift runtime DLLs>                     <- %LOCALAPPDATA%\Programs\Swift\Runtimes\<newest-semver>\usr\bin
    llama.dll, ggml*.dll + receipt/manifest <- engine-host\vendor\llama\vulkan
    prediction-runtime\llama-server.exe + DLLs <- pinned upstream llama.cpp
    models\README.txt                        <- Zenzai opt-in note (GGUF user-supplied)
    LICENSE, THIRD-PARTY-NOTICES.md          <- repo root

  Usage:
    powershell -File scripts\stage-dist.ps1 -Rebuild     # build release artifacts then stage
    powershell -File scripts\stage-dist.ps1              # stage from existing artifacts
    powershell -File scripts\stage-dist.ps1 -DestinationDir <new-temp-dir>
#>
[CmdletBinding()]
param(
    [switch]$Rebuild,
    # Explicit destinations are intended for isolated verification runs. They
    # must be new paths so a staging audit cannot overwrite a user's dist tree.
    [string]$DestinationDir = ''
)
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'swift-toolchain.ps1')
. (Join-Path $PSScriptRoot 'zenzai-runtime-manifest.ps1')
. (Join-Path $PSScriptRoot 'prediction-runtime-contract.ps1')
. (Join-Path $PSScriptRoot 'release-lib.ps1')

$RepoRoot   = Split-Path -Parent $PSScriptRoot
$WithDevEnv = Join-Path $PSScriptRoot 'with-dev-env.ps1'
$EngineHost = Join-Path $RepoRoot 'engine-host'
$RelDir     = Join-Path $RepoRoot 'target\release'
$EngineRel  = Join-Path $EngineHost '.build\x86_64-unknown-windows-msvc\release'
$VendorLlama= Join-Path $EngineHost 'vendor\llama\vulkan'
$ZenzaiRuntimeManifestName = (Get-ZenzaiVulkanRuntimeContract).ManifestName
$ZenzaiRuntimeReceiptName = (Get-ZenzaiVulkanRuntimeContract).ReceiptName
$ZenzaiBuildAttestationPath = Get-ZenzaiVulkanBuildAttestationPath `
    -BuildDirectory (Join-Path $RepoRoot '.llama-build\build-vulkan-dl')
$PredictionRuntime = Join-Path $EngineHost 'prediction-runtime'
$PredictionRuntimeContract = Get-PredictionRuntimeContract
# newest semver Swift runtime with a complete swiftCore.dll leaf (same pick as
# with-dev-env.ps1 / find-swift.ps1; do NOT pin a version, or staging would
# mismatch the toolchain the build actually used).
$SwiftRoot  = Join-Path $env:LOCALAPPDATA 'Programs\Swift'
$SwiftRtRoot= Join-Path $SwiftRoot 'Runtimes'
$SwiftRtDir = Get-SwiftRuntimeCandidate -SwiftRoot $SwiftRoot
$SwiftRt    = if ($SwiftRtDir) { Split-Path -Parent $SwiftRtDir.LeafPath } else { $null }
$UsingExplicitDestination = -not [string]::IsNullOrWhiteSpace($DestinationDir)
if ($UsingExplicitDestination) {
    $Dist = if ([IO.Path]::IsPathRooted($DestinationDir)) {
        [IO.Path]::GetFullPath($DestinationDir)
    } else {
        [IO.Path]::GetFullPath((Join-Path $RepoRoot $DestinationDir))
    }
} else {
    $Dist = Join-Path $RepoRoot 'dist'
}

$Resources = @(
    'AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.resources',
    'AzooKeyKanaKanjiConverter_EfficientNGram.resources',
    'swift-transformers_Hub.resources'
)

function Step([string]$t) { Write-Host ">> $t" -ForegroundColor Yellow }
function Ok([string]$t)   { Write-Host "   [OK] $t" -ForegroundColor Green }
function Die([string]$t)  { Write-Host "   [FAIL] $t" -ForegroundColor Red; exit 1 }

# Stage one VC++ redistributable DLL app-locally (permitted by the VS redist license).
# These DLLs are NOT guaranteed on a clean PC and may not ship in the Swift runtime folder.
# Idempotent: if section 3's Swift-runtime copy already landed the DLL in $Dist, skip (the
# Swift toolchain's copy is fine). Otherwise find the newest x64 redist copy under the VS
# install roots and copy it; if neither source has it, Die with the caller-supplied reason.
# Uses $Dist from outer scope (resolved at call time, after $Dist is set below).
function Stage-VCRedistDll([string]$name, [string]$reason) {
    if (Test-Path (Join-Path $Dist $name)) { return }
    $candidates = @()
    foreach ($vsRoot in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if (-not $vsRoot) { continue }
        $r = Join-Path $vsRoot 'Microsoft Visual Studio'
        if (Test-Path $r) {
            $candidates += Get-ChildItem -Path $r -Recurse -Filter $name -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\Redist\\.*\\x64\\' }
        }
    }
    $dll = $candidates | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $dll) { Die "$name (VC++ redist, x64) not found; $reason Install the VC++ redist / VS C++ workload." }
    Copy-Item $dll.FullName (Join-Path $Dist $name) -Force
    Ok "staged $name (from $($dll.FullName))"
}

if ($Rebuild) {
    # The release DLL may be loaded (e.g. registered IME, or held by indexer/AV), which makes cargo's
    # relink fail with os error 5 ("access denied") when it tries to replace it. Windows allows renaming
    # a loaded DLL on the same volume, so stash it aside first and let cargo write a fresh one.
    $relDll = Join-Path $RelDir 'nospacekey_tip.dll'
    if (Test-Path $relDll) {
        try { Remove-Item $relDll -Force -ErrorAction Stop }
        catch {
            $stash = "nospacekey_tip.$(Get-Date -Format yyyyMMddHHmmss).old"
            try { Rename-Item -Path $relDll -NewName $stash -ErrorAction Stop; Ok "stashed loaded DLL -> $stash" }
            catch { Die "release nospacekey_tip.dll is locked and cannot be removed or renamed: $relDll (close holders / reboot, or stage without -Rebuild)" }
        }
    }
    # Pass the command as an explicit -Cmd array. The loose-arg form
    # (`& with-dev-env.ps1 cargo build -p ...`) only works via `pwsh -File`, which hands the
    # script a raw arg vector. Called in-process with `&`, PowerShell instead binds `-p` as a
    # PARAMETER of with-dev-env.ps1 -- ambiguous with the common -ProgressAction/-PipelineVariable --
    # and aborts before ValueFromRemainingArguments can capture it.
    Step 'cargo build -p nospacekey_tip --release'
    & $WithDevEnv -Cmd @('cargo', 'build', '-p', 'nospacekey_tip', '--release')
    if ($LASTEXITCODE -ne 0) { Die "cargo build nospacekey_tip exit $LASTEXITCODE" }
    Step 'cargo build -p config --release'
    & (Join-Path $PSScriptRoot 'build-config-ui.ps1')
    if ($LASTEXITCODE -ne 0) { Die "config UI build exit $LASTEXITCODE" }
    & $WithDevEnv -Cmd @('cargo', 'build', '-p', 'config', '--release')
    if ($LASTEXITCODE -ne 0) { Die "cargo build config exit $LASTEXITCODE" }
    Step 'cargo build -p nospacekey-update --release'
    & $WithDevEnv -Cmd @('cargo', 'build', '-p', 'nospacekey-update', '--release')
    if ($LASTEXITCODE -ne 0) { Die "cargo build nospacekey-update exit $LASTEXITCODE" }
    # Never let an old engine executable survive a failed Swift rebuild.  The
    # exact path is removed when possible; if a running process has it locked,
    # rename it within the same directory so the build can still publish a
    # fresh path without deleting anything broader than this artifact.
    $relEngine = Join-Path $EngineRel 'NospacekeyEngineHost.exe'
    if (Test-Path -LiteralPath $relEngine) {
        try { Remove-Item -LiteralPath $relEngine -Force -ErrorAction Stop }
        catch {
            $stash = "NospacekeyEngineHost.$(Get-Date -Format yyyyMMddHHmmssfff).old"
            try {
                Rename-Item -LiteralPath $relEngine -NewName $stash -ErrorAction Stop
                Ok "stashed locked engine executable -> $stash"
            }
            catch {
                Die "release NospacekeyEngineHost.exe is locked and cannot be removed or renamed: $relEngine (close holders / reboot, then retry)"
            }
        }
    }
    Step 'swift build -c release (engine-host)'
    $swiftExit = 0
    Push-Location $EngineHost
    try {
        & $WithDevEnv -Cmd @('swift', 'build', '-c', 'release')
        $swiftExit = $LASTEXITCODE
    } finally { Pop-Location }
    if ($swiftExit -ne 0) { Die "swift build engine-host exit $swiftExit" }
    if (-not (Test-Path -LiteralPath $relEngine -PathType Leaf)) {
        Die "swift build succeeded but did not produce the engine executable: $relEngine"
    }
    Step 'build pinned inline-prediction runtime'
    & (Join-Path $PSScriptRoot 'build-prediction-runtime.ps1') -OutputDir $PredictionRuntime
    if ($LASTEXITCODE -ne 0) { Die "build-prediction-runtime exit $LASTEXITCODE" }
    Step 'build pinned Vulkan Zenzai runtime'
    & (Join-Path $PSScriptRoot 'build-llama.ps1') -Vulkan
    if ($LASTEXITCODE -ne 0) { Die "build-llama -Vulkan exit $LASTEXITCODE" }
}

if (-not (Test-Path -LiteralPath $PredictionRuntime -PathType Container)) {
    $evaluatedRuntime = Join-Path $RepoRoot 'experiments\inline-prediction\.tools\product-runtime'
    if (Test-Path -LiteralPath $evaluatedRuntime -PathType Container) {
        $PredictionRuntime = $evaluatedRuntime
    }
}

# Recreate dist\ clean. An explicit destination is an isolated audit output and
# must not already exist; the default dist\ path keeps its historical rebuild
# behavior for the release pipeline.
if ($UsingExplicitDestination -and (Test-Path -LiteralPath $Dist)) {
    Die "explicit staging destination already exists; choose a new temp directory: $Dist"
}
if (Test-Path -LiteralPath $Dist) { Remove-Item -LiteralPath $Dist -Recurse -Force }
New-Item -ItemType Directory -Force -Path $Dist | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $Dist 'models') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $Dist 'prediction-runtime') | Out-Null

function Copy-Required([string]$src, [string]$destName) {
    if (-not (Test-Path $src)) { Die "missing build artifact: $src (run with -Rebuild)" }
    Copy-Item -Path $src -Destination (Join-Path $Dist $destName) -Force
    Ok "staged $destName"
}

# 1) PEs.
Copy-Required (Join-Path $RelDir 'nospacekey_tip.dll')        'nospacekey_tip.dll'
. (Join-Path $PSScriptRoot 'tip-export-gate.ps1')
try {
    Assert-NospacekeyTipExports -Path (Join-Path $Dist 'nospacekey_tip.dll')
    Ok 'verified exact TIP PE exports'
} catch {
    Die "staged nospacekey_tip.dll export verification failed: $($_.Exception.Message)"
}
Copy-Required (Join-Path $RelDir 'NospacekeyConfig.exe')      'NospacekeyConfig.exe'
Copy-Required (Join-Path $RelDir 'NospacekeyUpdateChecker.exe') 'NospacekeyUpdateChecker.exe'
Copy-Required (Join-Path $EngineRel 'NospacekeyEngineHost.exe') 'NospacekeyEngineHost.exe'

# 2) SPM *.resources bundles (dictionary + tokenizer + hub) -> dist\ (next to the exe).
foreach ($r in $Resources) {
    $src = Join-Path $EngineRel $r
    if (-not (Test-Path $src)) { Die "missing resources bundle: $src (run with -Rebuild)" }
    Copy-Item -Path $src -Destination (Join-Path $Dist $r) -Recurse -Force
    Ok "staged $r\"
}

# 3) Swift runtime DLLs (a clean PC has no Swift toolchain).
if (-not $SwiftRt -or -not (Test-Path -LiteralPath $SwiftRt -PathType Container)) { Die "Swift runtime not found under $SwiftRtRoot\<semver>\usr\bin (a complete swiftCore.dll leaf is required). Install the Swift Windows toolchain (https://www.swift.org/install/windows/)." }
$swiftDlls = @(Get-ChildItem -LiteralPath $SwiftRt -Filter *.dll -File)
if ($swiftDlls.Count -eq 0) { Die "no Swift runtime DLLs in $SwiftRt" }
foreach ($d in $swiftDlls) { Copy-Item $d.FullName (Join-Path $Dist $d.Name) -Force }
Ok "staged $($swiftDlls.Count) Swift runtime DLLs"

# 4) Vulkan llama/ggml runtime DLLs (MIT). This is a required product
# closure: a CPU-only directory must never be mistaken for the Zenzai runtime.
$zenzaiRuntime = $null
try {
    # The local attestation is the build boundary. Verify it and its portable
    # receipt before deriving a manifest so stale metadata cannot decide which
    # bytes are staged.
    Assert-ZenzaiVulkanBuildAttestation -AttestationPath $ZenzaiBuildAttestationPath | Out-Null
    Assert-ZenzaiVulkanRuntimeReceipt -RuntimeDirectory $VendorLlama `
        -BuildAttestationPath $ZenzaiBuildAttestationPath | Out-Null
    # Re-derive the source manifest from the verified build receipt. A stale or
    # hand-authored manifest can never become the distribution identity.
    Write-ZenzaiVulkanRuntimeManifest -RuntimeDirectory $VendorLlama `
        -RequireExactDllSet | Out-Null
    $zenzaiRuntime = Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $VendorLlama `
        -RequireExactDllSet
} catch {
    Die "Vulkan Zenzai runtime is incomplete or invalid at ${VendorLlama}: $($_.Exception.Message)"
}
$llamaDlls = @($zenzaiRuntime.Dlls)
foreach ($d in $llamaDlls) { Copy-Item -LiteralPath $d.Path -Destination (Join-Path $Dist $d.Name) -Force }
$manifestSource = $zenzaiRuntime.ManifestPath
Copy-Item -LiteralPath $manifestSource -Destination (Join-Path $Dist $ZenzaiRuntimeManifestName) -Force
$receiptSource = $zenzaiRuntime.ReceiptPath
Copy-Item -LiteralPath $receiptSource -Destination (Join-Path $Dist $ZenzaiRuntimeReceiptName) -Force
Ok "staged exact Vulkan Zenzai runtime ($($llamaDlls.Count) DLLs + receipt + manifest)"

# 4a) Independent llama-server runtime for inline prediction. Fail closed: unlike the optional
# model, these binaries are part of the application and must exactly match the evaluated revision.
try {
    Assert-PredictionRuntimeBundle -RuntimeDirectory $PredictionRuntime | Out-Null
} catch {
    Die "inline-prediction Vulkan runtime is incomplete or invalid at ${PredictionRuntime}: $($_.Exception.Message)"
}
foreach ($name in $PredictionRuntimeContract.RequiredFiles) {
    $source = Join-Path $PredictionRuntime $name
    Copy-Item -LiteralPath $source -Destination (Join-Path $Dist 'prediction-runtime') -Force
}
Ok "staged pinned inline-prediction Vulkan runtime ($($PredictionRuntimeContract.Revision))"

# 4b) VC++ OpenMP runtime (vcomp140.dll). ggml-cpu.dll STATICALLY imports it and it is NOT in
#     the Swift runtime folder, so a clean PC without the VC++ redist lacks it -> NospacekeyEngineHost.exe
#     fails to start (STATUS_DLL_NOT_FOUND) -> ALL conversion dies (not just Zenzai). App-local
#     deployment of the VC++ redist DLLs is permitted by the VS redistributable license.
Stage-VCRedistDll 'vcomp140.dll' 'ggml-cpu.dll needs it.'

# 4c) VC++ MSVC C runtime (vcruntime140.dll + vcruntime140_1.dll + msvcp140.dll). The Rust TIP
#     DLL (nospacekey_tip.dll) and the engine PEs dynamically import these MSVC CRT DLLs. They MAY be
#     present already if section 3 copied them out of the Swift runtime folder (Copy-Item ... -Force
#     lands any that Swift shipped into $Dist), but that is NOT guaranteed -> a clean PC without the
#     VC++ redist would otherwise lack them and the PEs fail to load (STATUS_DLL_NOT_FOUND). App-local
#     deployment of these redist DLLs is permitted by the VS redistributable license.
#     NOTE: vcruntime140_1.dll (the SEH/__CxxFrameHandler4 helper, present since VS2019 16.x) is a
#     separate DLL from vcruntime140.dll and is listed alongside it in THIRD-PARTY-NOTICES.md; staging
#     only vcruntime140.dll would let a dist pass yet still fail to load on a clean PC.
Stage-VCRedistDll 'vcruntime140.dll'   'the Rust TIP DLL + engine need the MSVC C runtime.'
Stage-VCRedistDll 'vcruntime140_1.dll' 'the Rust TIP DLL + engine need the MSVC C runtime (SEH helper).'
Stage-VCRedistDll 'msvcp140.dll'       'the Rust TIP DLL + engine need the MSVC C++ runtime.'

# llama-server.exe is launched from a subdirectory. Windows resolves app-local dependencies from
# that executable's directory, so clean machines without the VC++ Redistributable need the same
# runtime DLLs beside the server as well as beside the first-party executables in dist root.
foreach ($name in @('vcomp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll')) {
    Copy-Item -LiteralPath (Join-Path $Dist $name) `
        -Destination (Join-Path $Dist 'prediction-runtime') -Force
}
Ok 'staged VC++ runtime beside inline-prediction server'
try {
    Assert-PredictionRuntimeBundle `
        -RuntimeDirectory (Join-Path $Dist 'prediction-runtime') `
        -AllowAdditionalRuntimeFiles | Out-Null
} catch {
    Die "staged inline-prediction Vulkan runtime failed closure verification: $($_.Exception.Message)"
}

# 5) models\ opt-in note (GGUF is user-supplied, CC-BY-SA-4.0, not bundled).
$modelsNote = @'
Place the Zenzai neural model here to enable neural conversion:

  ggml-model-Q5_K_M.gguf

Download it from: https://huggingface.co/Miwa-Keita/zenz-v3.1-small-gguf
License: CC-BY-SA-4.0 (Miwa-Keita). It is NOT bundled with this installer; you
download and place it yourself. Without it, nospacekey uses classic (LOUDS) conversion.
'@
Set-Content -Path (Join-Path $Dist 'models\README.txt') -Value $modelsNote -Encoding ascii
Ok 'staged models\README.txt'

# 6) license docs.
foreach ($lic in @('LICENSE', 'THIRD-PARTY-NOTICES.md')) {
    $src = Join-Path $RepoRoot $lic
    if (Test-Path $src) { Copy-Item $src (Join-Path $Dist $lic) -Force; Ok "staged $lic" }
    else { Die "required distribution notice is missing at repo root: $src" }
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'version-cleanup.ps1') `
    -Destination (Join-Path $Dist 'version-cleanup.ps1') -Force
Ok 'staged version cleanup worker'

# Completeness check (fail-closed on the runtime-critical items).
Step 'completeness check'
$required = @(
    (Join-Path $Dist 'nospacekey_tip.dll'),
    (Join-Path $Dist 'NospacekeyEngineHost.exe'),
    (Join-Path $Dist 'NospacekeyConfig.exe'),
    (Join-Path $Dist 'NospacekeyUpdateChecker.exe'),
    (Join-Path $Dist 'models\README.txt'),
    (Join-Path $Dist $ZenzaiRuntimeReceiptName),
    (Join-Path $Dist $ZenzaiRuntimeManifestName),
    (Join-Path $Dist 'prediction-runtime\llama-server.exe'),
    (Join-Path $Dist 'prediction-runtime\ggml-vulkan.dll'),
    (Join-Path $Dist 'prediction-runtime\REVISION'),
    (Join-Path $Dist 'prediction-runtime\BUILD-RECEIPT.txt'),
    # VC++ runtime DLLs the PEs statically import (NOT guaranteed on a clean PC).
    # vcomp140 = ggml-cpu OpenMP; vcruntime140 + vcruntime140_1 + msvcp140 = MSVC CRT for the Rust DLL + engine.
    (Join-Path $Dist 'vcomp140.dll'),
    (Join-Path $Dist 'vcruntime140.dll'),
    (Join-Path $Dist 'vcruntime140_1.dll'),
    (Join-Path $Dist 'msvcp140.dll')
)
foreach ($r in $Resources) { $required += (Join-Path $Dist $r) }
$required += @($llamaDlls | ForEach-Object { Join-Path $Dist $_.Name })
$missing = $required | Where-Object { -not (Test-Path $_) }
if ($missing) { $missing | ForEach-Object { Write-Host "   missing: $_" -ForegroundColor Red }; Die "dist incomplete" }
if ($swiftDlls.Count -lt 1) { Die "no Swift runtime DLLs staged" }
try {
    Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $Dist `
        -ManifestPath (Join-Path $Dist $ZenzaiRuntimeManifestName) | Out-Null
} catch {
    Die "staged Vulkan Zenzai runtime failed manifest/hash verification: $($_.Exception.Message)"
}

$workspaceVersion = ([regex]::Match(
    (Get-Content -Raw -LiteralPath (Join-Path $RepoRoot 'Cargo.toml')),
    '(?m)^version = "([^"]+)"\r?$')).Groups[1].Value
if ([string]::IsNullOrWhiteSpace($workspaceVersion)) { Die 'workspace version unavailable for ownership manifest' }
$ownershipCount = Write-VersionOwnershipManifest -Root $Dist -Version $workspaceVersion
Ok "staged ownership manifest ($ownershipCount files)"

$fileCount = (Get-ChildItem $Dist -Recurse -File | Measure-Object).Count
$size = [math]::Round(((Get-ChildItem $Dist -Recurse -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Ok "dist complete: $fileCount files, $size MB at $Dist"
exit 0
