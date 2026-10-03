<#
  sign-dist.ps1 -- dev Authenticode code-signing for the nospacekey distribution.
  ASCII-only on purpose (no Japanese -> no UTF-8 BOM requirement).

  WHAT IT SIGNS
    The four first-party PEs in the staged dist tree:
      dist\nospacekey_tip.dll
      dist\NospacekeyEngineHost.exe
      dist\NospacekeyConfig.exe
      dist\NospacekeyUpdateChecker.exe
    plus the bundled llama/ggml runtime DLLs in dist\ (llama*.dll, ggml*.dll;
    the Swift runtime DLLs are NOT signed -- they ship as-is from the toolchain).
    With -SetupExe <path> it signs ONLY the produced installer.  The payload must
    already be signed before ISCC compiles nospacekey.iss; this keeps the bytes
    packed into the installer identical to the bytes left in dist\.
    With -Testbench <path> it signs an explicit formal-gate testbench input.  The
    testbench MUST be outside dist\ and is never copied into the installer.

  CERTIFICATE (cert-agnostic -- same pipeline for dev and prod)
    -Thumbprint <hex> : use an existing cert in the cert store by thumbprint
                        (this is the future OV/production path).
    (omitted)         : create or reuse a dev self-signed code-signing cert
                        "CN=nospacekey Dev" in Cert:\CurrentUser\My.

  USAGE
    Stage first:   powershell -File scripts\stage-dist.ps1 -Rebuild
    Sign payload:  powershell -File scripts\sign-dist.ps1
    Sign gate input: powershell -File scripts\sign-dist.ps1 -Testbench target\release\testbench.exe -Thumbprint <OV-cert-thumbprint>
    Sign setup:    powershell -File scripts\sign-dist.ps1 -SetupExe installer\Output\nospacekey-setup.exe
    Prod cert:     powershell -File scripts\sign-dist.ps1 -Thumbprint <OV-cert-thumbprint>

  NOTE
    A self-signed cert is valid Authenticode but UNTRUSTED on other machines.
    signtool verify will report a chain warning for it -- that is EXPECTED for
    dev. On the test VM, import the public cert into the machine trust stores
    (instructions are printed at the end). Swapping in a real OV cert via
    -Thumbprint removes the warning with no other change.
#>
[CmdletBinding()]
param(
    [string]$Thumbprint,
    [string]$SetupExe,
    [Alias('FormalTestbench')]
    [string]$Testbench,
    [string]$DistDir = "dist"
)
$ErrorActionPreference = 'Stop'

function Write-Step([string]$t) { Write-Host ">> $t" -ForegroundColor Yellow }
function Write-Ok([string]$t)   { Write-Host "   [OK] $t" -ForegroundColor Green }
function Write-Note([string]$t) { Write-Host "   [..] $t" -ForegroundColor DarkGray }
function Fail([string]$t)       { Write-Host "   [FAIL] $t" -ForegroundColor Red; exit 1 }

$RepoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'release-lib.ps1')
. (Join-Path $PSScriptRoot 'zenzai-runtime-manifest.ps1')

# Resolve the dist dir relative to the repo root unless an absolute path is given.
if ([System.IO.Path]::IsPathRooted($DistDir)) {
    $Dist = $DistDir
} else {
    $Dist = Join-Path $RepoRoot $DistDir
}
if (-not (Test-Path -LiteralPath $Dist -PathType Container)) { Fail "dist dir not found: $Dist  (run scripts\stage-dist.ps1 first)" }
$Dist = (Resolve-Path -LiteralPath $Dist).Path
$DistRoot = [IO.Path]::GetFullPath($Dist)
$distRootIsPathRoot = $DistRoot.Equals([IO.Path]::GetPathRoot($DistRoot), [StringComparison]::OrdinalIgnoreCase)
if (-not $distRootIsPathRoot) { $DistRoot = $DistRoot.TrimEnd('\') }

function Resolve-ExternalInput([string]$Path, [string]$Label) {
    if ([string]::IsNullOrWhiteSpace($Path)) { return $null }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Fail "$Label not found: $Path"
    }
    try {
        $full = Resolve-NoReparsePath -Path $Path -Label $Label
    } catch {
        Fail $_.Exception.Message
    }
    $distPrefix = if ($distRootIsPathRoot) { $DistRoot } else { $DistRoot + [IO.Path]::DirectorySeparatorChar }
    if ($full.Equals($DistRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $full.StartsWith($distPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        Fail "$Label must be outside dist: $full"
    }
    return $full
}

$TestbenchPath = Resolve-ExternalInput $Testbench 'formal gate testbench'

# --- 1) Resolve signtool.exe: newest of the Win10/11 SDK x64 builds. ---
Write-Step 'resolve signtool.exe'
# base from the env var (x86 Program Files may not be on C:), version segment globbed -> newest.
$SignToolGlob = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin\*\x64\signtool.exe'
$SignTool = Get-ChildItem -Path $SignToolGlob -ErrorAction SilentlyContinue |
    Sort-Object { [version]($_.Directory.Parent.Name) } -Descending |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $SignTool) { $SignTool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source }  # VS Dev prompt / PATH fallback
if (-not $SignTool) { Fail "signtool.exe not found under: $SignToolGlob (install the Windows 10/11 SDK)" }
Write-Ok "signtool: $SignTool"

# --- 2) Resolve the signing certificate (existing thumbprint, or dev self-signed). ---
$HasExplicitThumbprint = -not [string]::IsNullOrWhiteSpace($Thumbprint)
if ($HasExplicitThumbprint -and -not $SetupExe -and -not $TestbenchPath) {
    Fail 'payload signing with explicit -Thumbprint requires -Testbench <path> outside dist'
}
if ($HasExplicitThumbprint) {
    Write-Step "use existing certificate by thumbprint"
    $clean = ($Thumbprint -replace '[^0-9A-Fa-f]', '')
    $cert = Get-ChildItem -Path Cert:\CurrentUser\My, Cert:\LocalMachine\My -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $clean } | Select-Object -First 1
    if (-not $cert) { Fail "certificate with thumbprint $clean not found in CurrentUser\My or LocalMachine\My" }
    $Thumb = $cert.Thumbprint
    Write-Ok "certificate: $($cert.Subject) ($Thumb)"
} else {
    Write-Step 'create or reuse dev self-signed code-signing certificate (CN=nospacekey Dev)'
    $cert = Get-ChildItem -Path Cert:\CurrentUser\My -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq 'CN=nospacekey Dev' } |
        Sort-Object NotAfter -Descending | Select-Object -First 1
    if ($cert) {
        Write-Note "reusing existing dev cert"
    } else {
        Write-Note "no dev cert found; creating a new one"
        $cert = New-SelfSignedCertificate -Type CodeSigningCert `
            -Subject 'CN=nospacekey Dev' `
            -CertStoreLocation Cert:\CurrentUser\My `
            -KeyUsage DigitalSignature `
            -KeyExportPolicy Exportable
    }
    $Thumb = $cert.Thumbprint
    Write-Ok "dev certificate: $($cert.Subject) ($Thumb)"
}

# --- 3) Collect the PEs to sign. ---
$TimestampUrl = 'http://timestamp.digicert.com'

$OwnPes = @(
    Join-Path $Dist 'nospacekey_tip.dll'
    Join-Path $Dist 'NospacekeyEngineHost.exe'
    Join-Path $Dist 'NospacekeyConfig.exe'
    Join-Path $Dist 'NospacekeyUpdateChecker.exe'
)
# Bundled llama/ggml DLLs (NOT the ~32 Swift runtime DLLs). Match recursively because the
$LlamaDlls = @(Get-ChildItem -Path $Dist -Filter '*.dll' -File -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'llama*.dll' -or $_.Name -like 'ggml*.dll' -or $_.Name -eq 'mtmd.dll' } |
    Select-Object -ExpandProperty FullName)

$PayloadTargets = @($OwnPes + $LlamaDlls)
$Targets = @()

# SetupExe is deliberately setup-only.  The default invocation remains the
# existing payload-only operation; an explicit testbench can be combined with
# either operation, but it is always an external input and never a dist member.
if ($SetupExe) {
    if (-not (Test-Path -LiteralPath $SetupExe -PathType Leaf)) { Fail "setup exe not found: $SetupExe" }
    try {
        $Targets += [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $SetupExe -ErrorAction Stop).Path)
    } catch {
        Fail "setup exe could not be resolved: $SetupExe"
    }
} else {
    $Targets += $PayloadTargets
}
if ($TestbenchPath) {
    $Targets += $TestbenchPath
}

# Sanity: the payload targets must exist for the payload-only invocation.  A
# setup-only call intentionally does not touch or require dist payload files.
if (-not $SetupExe) {
    try {
        # Verify the build receipt before touching any payload bytes. The
        # manifest alone is not an accepted provenance source.
        Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $Dist | Out-Null
        Write-Ok 'Vulkan Zenzai provenance receipt and manifest verified before payload signing'
    } catch {
        Fail "unsigned Vulkan Zenzai runtime manifest verification failed: $($_.Exception.Message)"
    }
    foreach ($p in $PayloadTargets) {
        if (-not (Test-Path -LiteralPath $p -PathType Leaf)) {
            Fail "expected payload missing (run stage-dist.ps1): $p"
        }
    }
}

Write-Step ("sign {0} file(s)" -f $Targets.Count)
foreach ($f in $Targets) { Write-Note (Split-Path -Leaf $f) }

# --- 4) Sign each target: SHA256 file digest + RFC3161 timestamp (SHA256). ---
$signed = 0
foreach ($f in $Targets) {
    & $SignTool sign /fd SHA256 /sha1 $Thumb /tr $TimestampUrl /td SHA256 $f
    if ($LASTEXITCODE -ne 0) { Fail "signtool sign failed (exit $LASTEXITCODE): $f" }
    $signed++
}
Write-Ok "$signed file(s) signed"

# Authenticode appends bytes to each PE/DLL. Refresh the distribution manifest
# after payload signing so its current DLL rows describe the bytes that will be
# packed while its build_artifacts remain bound to the verified receipt.
if (-not $SetupExe) {
    try {
        Write-ZenzaiVulkanRuntimeManifest -RuntimeDirectory $Dist -Signed | Out-Null
        Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $Dist | Out-Null
        Write-Ok 'Vulkan Zenzai manifest refreshed after payload signing (receipt retained)'
        $workspaceVersion = ([regex]::Match(
            (Get-Content -Raw -LiteralPath (Join-Path $RepoRoot 'Cargo.toml')),
            '(?m)^version = "([^"]+)"\r?$')).Groups[1].Value
        if ([string]::IsNullOrWhiteSpace($workspaceVersion)) { throw 'workspace version unavailable' }
        $ownershipCount = Write-VersionOwnershipManifest -Root $Dist -Version $workspaceVersion
        Write-Ok "ownership manifest refreshed after payload signing ($ownershipCount files)"
    } catch {
        Fail "signed Vulkan Zenzai runtime manifest verification failed: $($_.Exception.Message)"
    }
}

# --- 5) Verify each signature. A dev self-signed cert shows a chain warning. ---
Write-Step 'verify signatures (signtool verify /pa /v)'
foreach ($f in $Targets) {
    & $SignTool verify /pa /v $f
    if ($LASTEXITCODE -ne 0) {
        # Untrusted chain (self-signed) -> nonzero exit but the signature IS
        # present.  An explicit thumbprint is the production path, so it is
        # fail-closed here; release.ps1 repeats the strict check at G2/G4.
        if ($HasExplicitThumbprint) {
            Fail "signtool verify /pa failed for explicit -Thumbprint target: $f"
        }
        $sig = Get-AuthenticodeSignature -FilePath $f
        if (@('Valid', 'UnknownError') -notcontains $sig.Status -or -not $sig.SignerCertificate) {
            Fail "signtool verify /pa failed and Authenticode signature is unusable for dev target: $f (status '$($sig.Status)')"
        }
        Write-Note ("chain not trusted for: {0} (signature status={1}; signer present)" -f (Split-Path -Leaf $f), $sig.Status)
    } else {
        Write-Ok ("verified: {0}" -f (Split-Path -Leaf $f))
    }
}

# --- 6) Summary. ---
Write-Host ''
Write-Host '=== sign-dist summary ===' -ForegroundColor Cyan
Write-Host ("  signtool   : {0}" -f $SignTool)
Write-Host ("  certificate: {0} ({1})" -f $cert.Subject, $Thumb)
Write-Host ("  timestamp  : {0}" -f $TimestampUrl)
Write-Host ("  dist dir   : {0}" -f $Dist)
if ($SetupExe) {
    $scope = 'setup.exe only'
} else {
    $scope = ('{0} payload PE/DLLs' -f $PayloadTargets.Count)
}
if ($TestbenchPath) { $scope += ' + formal gate testbench' }
Write-Host ("  signed     : {0} file(s)  ({1})" -f $Targets.Count, $scope)
if (-not $HasExplicitThumbprint) {
    Write-Host ''
    Write-Host '  This is a DEV self-signed certificate. The chain is untrusted on' -ForegroundColor DarkYellow
    Write-Host '  other machines (signtool verify reports a chain warning -- expected).' -ForegroundColor DarkYellow
    Write-Host '  On the test VM, export the public cert and import it into the machine' -ForegroundColor DarkYellow
    Write-Host '  trust stores so the signature validates:' -ForegroundColor DarkYellow
    Write-Host ''
    Write-Host ('    $c = Get-ChildItem Cert:\CurrentUser\My\{0}' -f $Thumb) -ForegroundColor Gray
    Write-Host '    Export-Certificate -Cert $c -FilePath nospacekey-dev.cer' -ForegroundColor Gray
    Write-Host '    # then, on the VM (as admin):' -ForegroundColor Gray
    Write-Host '    Import-Certificate -FilePath nospacekey-dev.cer -CertStoreLocation Cert:\LocalMachine\Root' -ForegroundColor Gray
    Write-Host '    Import-Certificate -FilePath nospacekey-dev.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher' -ForegroundColor Gray
    Write-Host ''
    Write-Host '  For production, re-run with -Thumbprint <OV-cert-thumbprint>; no other' -ForegroundColor DarkYellow
    Write-Host '  change is needed (the pipeline is cert-agnostic).' -ForegroundColor DarkYellow
}
Write-Host ''
Write-Ok 'done'
exit 0
