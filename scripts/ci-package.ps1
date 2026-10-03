# CI builds are downloadable test builds, not automatically published releases.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
. (Join-Path $PSScriptRoot 'release-lib.ps1')

$identityJson = & node (Join-Path $PSScriptRoot 'ci-identity.mjs') begin-package
if ($LASTEXITCODE -ne 0) { throw 'Packaging identity already used or invalid; rerun the build job for a new version' }
$identity = $identityJson | ConvertFrom-Json
$version = $identity.version
$sha = $identity.commit
$stem = [IO.Path]::GetFileNameWithoutExtension($identity.installer)
$download = Join-Path $root 'artifacts/download'
$verification = Join-Path $root 'artifacts/verification'

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

$buildJson = & node (Join-Path $PSScriptRoot 'ci-identity.mjs') finish-package $signature.SignerCertificate.Thumbprint
if ($LASTEXITCODE -ne 0) { throw 'Failed to seal the installer identity; do not upload incomplete outputs' }
$build = $buildJson | ConvertFrom-Json
$hash = $build.sha256
if ($env:GITHUB_STEP_SUMMARY) {
    @"
Download **nospacekey-windows-x64-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT** from this run's Artifacts.

- Version: $version
- Source: $sha
- Installer SHA-256: $($hash.ToLowerInvariant())
- Signing: development certificate generated for this build (SmartScreen warnings are expected).
- Installation and TSF results appear in the separate verification job. GPU inference and physical keyboard/Word acceptance are not covered.
"@ >> $env:GITHUB_STEP_SUMMARY
}
