<#
  with-dev-env.ps1 - run a build/test command inside the project's toolchain env.

  Imports MSVC (vcvars64, found via vswhere), prepends engine-host\vendor\llama\vulkan
  to $env:LIB (so the patched llama.lib links for the Zenzai-trait Swift build), and prepends the
  newest complete semver Swift toolchain/runtime bins to PATH. Then runs the remaining
  arguments as a command
  in the CURRENT directory and propagates its exit code.

  ASCII-only on purpose: no Japanese -> no UTF-8 BOM requirement.

  Usage (run from the directory the command expects):
    pwsh -File scripts\with-dev-env.ps1 cargo test -p ipc protocol            # from repo root
    pwsh -File scripts\with-dev-env.ps1 swift test --filter LLMConfigTests     # from engine-host
    pwsh -File scripts\with-dev-env.ps1 cargo build -p nospacekey_tip --release   # from repo root
#>
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Cmd)
$ErrorActionPreference = 'Stop'
if (-not $Cmd -or $Cmd.Count -eq 0) { Write-Error 'no command given'; exit 2 }

. (Join-Path $PSScriptRoot 'swift-toolchain.ps1')

$RepoRoot    = Split-Path -Parent $PSScriptRoot
$VsWhere     = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$VendorLlama = Join-Path $RepoRoot 'engine-host\vendor\llama'
$VulkanLlama = Join-Path $VendorLlama 'vulkan'
$VulkanRuntimeDllNames = @('llama.dll', 'ggml.dll', 'ggml-base.dll', 'ggml-cpu.dll', 'ggml-vulkan.dll')

# Discover the Swift install dynamically (do NOT hardcode a user name). The Swift.org
# Windows installer lays it out under %LOCALAPPDATA%\Programs\Swift; use the newest
# semver toolchain/runtime candidates with their required leaves. swift.exe needs the
# runtime bin (swiftCore.dll) on PATH and SDKROOT pointing at the Windows SDK, or it
# fails with DLL-not-found / "unable to load standard library".
$SwiftRoot   = Join-Path $env:LOCALAPPDATA 'Programs\Swift'
$SwiftTC     = Get-SwiftToolchainCandidate -SwiftRoot $SwiftRoot
$SwiftRT     = Get-SwiftRuntimeCandidate -SwiftRoot $SwiftRoot
$SwiftBin    = if ($SwiftTC) { Split-Path -Parent $SwiftTC.LeafPath } else { $null }
$SwiftRtBin  = if ($SwiftRT) { Split-Path -Parent $SwiftRT.LeafPath } else { $null }
if (-not $env:SDKROOT) {
    $SwiftSdk = Get-SwiftSdkPath -SwiftRoot $SwiftRoot -RuntimeCandidate $SwiftRT
    if ($SwiftSdk -and (Test-Path -LiteralPath $SwiftSdk -PathType Container)) { $env:SDKROOT = $SwiftSdk }
}

if (-not $env:VSCMD_VER) {
    if (-not (Test-Path $VsWhere)) { Write-Error "vswhere not found: $VsWhere"; exit 2 }
    $vcPath = & $VsWhere -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ([string]::IsNullOrWhiteSpace($vcPath)) { Write-Error 'VC x64 tools not found'; exit 2 }
    $vcvars = Join-Path $vcPath 'VC\Auxiliary\Build\vcvars64.bat'
    if (-not (Test-Path $vcvars)) { Write-Error "vcvars64.bat not found: $vcvars"; exit 2 }
    # vcvars64.bat invokes a bare `vswhere.exe`; ensure the VS Installer dir is on PATH
    # so the nested call resolves (fresh shells without it fail with "not recognized").
    $InstallerDir = Split-Path -Parent $VsWhere
    if ($env:PATH -notlike "*$InstallerDir*") { $env:PATH = "$InstallerDir;$env:PATH" }
    cmd /c "call `"$vcvars`" && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            try { Set-Item -Path "Env:\$($matches[1])" -Value $matches[2] -ErrorAction Stop } catch { }
        }
    }
    if (-not $env:VSCMD_VER) { Write-Error 'vcvars64 import failed'; exit 2 }
}

$VulkanMissing = @($VulkanRuntimeDllNames | Where-Object {
    -not (Test-Path (Join-Path $VulkanLlama $_) -PathType Leaf)
})
if ((Test-Path $VulkanLlama -PathType Container) -and $VulkanMissing.Count -gt 0) {
    Write-Error "Incomplete Vulkan runtime in $VulkanLlama; missing: $($VulkanMissing -join ', ')"
    exit 2
}
if ($VulkanMissing.Count -eq 0) {
    $VendorLlamaPath = (Resolve-Path $VulkanLlama).Path
    if (-not $env:NOSPACEKEY_ZENZAI_RUNTIME_DIR) { $env:NOSPACEKEY_ZENZAI_RUNTIME_DIR = $VendorLlamaPath }
} elseif (Test-Path $VendorLlama) {
    $VendorLlamaPath = (Resolve-Path $VendorLlama).Path
} else {
    $VendorLlamaPath = $null
}
if ($VendorLlamaPath) {
    $env:LIB = "$VendorLlamaPath;$env:LIB"   # link time: llama.lib
    # run time: the host/test binaries import llama.dll + ggml*.dll, so vendor\llama must
    # be on PATH too or `swift test`/the exe fail to load (abnormal(309) / DLL not found).
    if ($env:PATH -notlike "*$VendorLlamaPath*") { $env:PATH = "$VendorLlamaPath;$env:PATH" }
}

function Get-SwiftPMProductRuntimeDirectories {
    $buildRoot = Join-Path $RepoRoot 'engine-host\.build'
    if (-not (Test-Path $buildRoot -PathType Container)) { return }
    $targets = foreach ($triple in Get-ChildItem -LiteralPath $buildRoot -Directory -ErrorAction SilentlyContinue) {
        foreach ($configuration in @('debug', 'release')) {
            $target = Join-Path $triple.FullName $configuration
            if (Test-Path $target -PathType Container) { Get-Item -LiteralPath $target }
        }
    }
    return $targets
}

function Sync-VulkanBuildRuntime {
    if ($VulkanMissing.Count -ne 0) { return }
    $targets = Get-SwiftPMProductRuntimeDirectories
    foreach ($target in $targets) {
        foreach ($name in $VulkanRuntimeDllNames) {
            $source = Join-Path $VulkanLlama $name
            $destination = Join-Path $target.FullName $name
            if (-not (Test-Path -LiteralPath $destination -PathType Leaf) -or
                (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne
                (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash) {
                Copy-Item -LiteralPath $source -Destination $destination -Force
            }
        }
    }
}

Sync-VulkanBuildRuntime
foreach ($b in @($SwiftRtBin, $SwiftBin)) {
    if ($b) { Add-SwiftPathEntry -Path $b }
}

& $Cmd[0] @($Cmd[1..($Cmd.Count - 1)])
$exitCode = $LASTEXITCODE

# SwiftPM may copy CPU dependency DLLs while compiling the test bundle. Repair
# that executable-local closure before retrying test discovery.
if ($exitCode -ne 0 -and $Cmd[0] -eq 'swift' -and $Cmd.Count -gt 1 -and $Cmd[1] -eq 'test') {
    $stale = $false
    if ($VulkanMissing.Count -eq 0) {
        $vulkanHashes = @{}
        foreach ($name in $VulkanRuntimeDllNames) {
            $vulkanHashes[$name] = (Get-FileHash -LiteralPath (Join-Path $VulkanLlama $name) -Algorithm SHA256).Hash
        }
        foreach ($target in Get-SwiftPMProductRuntimeDirectories) {
            foreach ($name in $VulkanRuntimeDllNames) {
                $candidate = Join-Path $target.FullName $name
                if (-not (Test-Path -LiteralPath $candidate -PathType Leaf) -or
                    (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash -ne $vulkanHashes[$name]) {
                    $stale = $true
                    break
                }
            }
            if ($stale) { break }
        }
    }
    if ($stale) {
        Sync-VulkanBuildRuntime
        $testArgs = @('--skip-build')
        if ($Cmd.Count -gt 2) { $testArgs += @($Cmd[2..($Cmd.Count - 1)]) }
        & $Cmd[0] $Cmd[1] @testArgs
        $exitCode = $LASTEXITCODE
    }
}

exit $exitCode
