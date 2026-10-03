# CI builds are downloadable test builds, not automatically published releases.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
. (Join-Path $PSScriptRoot 'release-lib.ps1')

$version = Get-IssVersion (Get-Content -Raw installer/version.iss)
if (-not $version) { throw 'Installer version is missing' }
$sha = (& git rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Cannot determine source commit' }
$suffix = if ($env:GITHUB_RUN_NUMBER) { $env:GITHUB_RUN_NUMBER } else { 'local' }
$stem = "nospacekey-setup-$version-ci.$suffix-$($sha.Substring(0, 8))-devsigned"
$download = Join-Path $root 'artifacts/download'
$verification = Join-Path $root 'artifacts/verification'
New-Item -ItemType Directory -Force $download, $verification | Out-Null

& ./scripts/sign-dist.ps1 -Testbench (Join-Path $root 'target/release/testbench.exe')
if ($LASTEXITCODE -ne 0) { throw 'Payload signing failed' }
# Signing changes file hashes. Recreate ownership metadata before packaging.
[void](Write-VersionOwnershipManifest -Root (Join-Path $root 'dist') -Version $version)
Copy-Item dist/version-manifest.json (Join-Path $verification 'version-manifest.json')
Copy-Item target/release/testbench.exe $verification

$iscc = $env:NOSPACEKEY_ISCC
if (-not $iscc) { $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6/ISCC.exe' }
& $iscc "/O$download" "/F$stem" installer/nospacekey.iss
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
$setup = Join-Path $download "$stem.exe"
& ./scripts/sign-dist.ps1 -SetupExe $setup
if ($LASTEXITCODE -ne 0) { throw 'Installer signing failed' }
$signature = Get-AuthenticodeSignature -LiteralPath $setup
if (-not $signature.SignerCertificate) { throw 'Signed installer has no certificate' }
# Only the public certificate leaves the ephemeral build machine.
Export-Certificate -Cert $signature.SignerCertificate -FilePath (Join-Path $verification 'ci-signing.cer') | Out-Null

$hash = (Get-FileHash -LiteralPath $setup -Algorithm SHA256).Hash
New-Sha256SumsLine $hash "$stem.exe" | Set-Content (Join-Path $download 'SHA256SUMS.txt') -Encoding ascii
$build = [ordered]@{
    version = $version
    source_version = $env:NOSPACEKEY_CI_BASE_VERSION
    commit = $sha
    ref = $env:GITHUB_REF
    run_url = "$env:GITHUB_SERVER_URL/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID"
    installer = "$stem.exe"
    sha256 = $hash.ToLowerInvariant()
    signing = 'ephemeral development certificate'
    signer_thumbprint = $signature.SignerCertificate.Thumbprint
    verification = 'See the separate verify-install job and windows-verification-report artifact.'
    limitations = @('No physical GPU inference test', 'No physical keyboard or Word interaction test')
}
$build | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $download 'BUILD-INFO.json') -Encoding utf8
if ($env:GITHUB_STEP_SUMMARY) {
    @"
Download **nospacekey-windows-x64-$env:GITHUB_RUN_NUMBER-$env:GITHUB_RUN_ATTEMPT** from this run's Artifacts.

- Version: $version
- Source: $sha
- Installer SHA-256: $($hash.ToLowerInvariant())
- Signing: development certificate generated for this build (SmartScreen warnings are expected).
- Installation and TSF results appear in the separate verification job. GPU inference and physical keyboard/Word acceptance are not covered.
"@ >> $env:GITHUB_STEP_SUMMARY
}
