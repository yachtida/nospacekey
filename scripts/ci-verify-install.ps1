# Install the exact downloadable artifact on a fresh GitHub-hosted Windows VM.
# The VM is disposed after this job; no developer PC is registered or modified.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'This installer test is restricted to disposable GitHub-hosted Windows runners.'
}
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$reports = Join-Path $root 'artifacts/reports'
New-Item -ItemType Directory -Force $reports | Out-Null
$download = Join-Path $root 'artifacts/download'
$inputs = Join-Path $root 'artifacts/verification'
$info = Get-Content -Raw (Join-Path $download 'BUILD-INFO.json') | ConvertFrom-Json
$setup = Join-Path $download $info.installer
$installRoot = Join-Path $env:ProgramFiles 'nospacekey'
$installed = Join-Path $installRoot "versions/$($info.version)"
$results = [Collections.Generic.List[object]]::new()
$failure = $null

function Invoke-InstallerProcess([string]$Path, [string[]]$Arguments, [int]$TimeoutSeconds = 300) {
    $start = [Diagnostics.ProcessStartInfo]::new($Path)
    $start.UseShellExecute = $false
    foreach ($arg in $Arguments) { [void]$start.ArgumentList.Add($arg) }
    $process = [Diagnostics.Process]::Start($start)
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        $process.Kill($true)
        throw "Process timed out: $Path"
    }
    if ($process.ExitCode -ne 0) { throw "Process failed: $Path (exit $($process.ExitCode))" }
}

try {
    if ((Get-FileHash $setup -Algorithm SHA256).Hash -ine $info.sha256) { throw 'Installer SHA-256 mismatch' }
    $sig = Get-AuthenticodeSignature $setup
    if ($sig.SignerCertificate.Thumbprint -ine $info.signer_thumbprint) { throw 'Installer signer mismatch' }
    # Trust only this build's public certificate in this disposable VM.
    foreach ($store in @('Cert:\LocalMachine\Root', 'Cert:\LocalMachine\TrustedPublisher')) {
        Import-Certificate -FilePath (Join-Path $inputs 'ci-signing.cer') -CertStoreLocation $store | Out-Null
    }
    if ((Get-AuthenticodeSignature $setup).Status -ne 'Valid') { throw 'Installer signature validation failed' }
    Invoke-InstallerProcess $setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/TASKS=!zenzaimodel', "/LOG=$reports/install.log")
    $results.Add([ordered]@{ check = 'installer'; status = 'pass' })

    $manifest = Get-Content -Raw (Join-Path $inputs 'version-manifest.json') | ConvertFrom-Json
    if ($manifest.Version -cne $info.version -or @($manifest.Files).Count -eq 0) { throw 'Invalid expected payload manifest' }
    foreach ($file in $manifest.Files) {
        $path = [IO.Path]::GetFullPath((Join-Path $installed $file.Path))
        if (-not $path.StartsWith($installed.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Manifest path escapes installation' }
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Installed file missing: $($file.Path)" }
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ine $file.Sha256) { throw "Installed hash mismatch: $($file.Path)" }
    }
    $results.Add([ordered]@{ check = 'installed payload hashes'; status = 'pass'; files = @($manifest.Files).Count })

    $registered = (Get-Item 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Classes\CLSID\{B4B39227-EFF2-41DA-B357-0C3170A57875}\InprocServer32').GetValue('')
    if ([IO.Path]::GetFullPath($registered) -ine (Join-Path $installed 'nospacekey_tip.dll')) { throw 'COM registration does not point to the installed artifact' }
    $results.Add([ordered]@{ check = 'installed TIP registration'; status = 'pass' })

    # Keep the test program outside the product tree. DLL search uses the exact
    # installed payload; no Swift SDK or locally built engine is installed here.
    $env:PATH = "$installed;$env:PATH"
    $env:NOSPACEKEY_ZENZAI = 'off'
    $env:NOSPACEKEY_LEARNING = '0'
    $testbench = Join-Path $inputs 'testbench.exe'
    foreach ($scenario in @('--keymap-smoke', '--scenarios')) {
        & ./scripts/run-gate.ps1 -Testbench $testbench -TestbenchArgs $scenario -TimeoutSec 240
        $code = $LASTEXITCODE
        $status = if ($code -eq 0) { 'pass' } elseif ($code -eq 2) { 'unavailable' } else { 'fail' }
        $results.Add([ordered]@{ check = "TSF $scenario"; status = $status; exit_code = $code })
        if ($code -ne 0) { throw "TSF $scenario $status (exit $code)" }
    }
} catch {
    $failure = $_
    $results.Add([ordered]@{ check = 'verification'; status = 'fail'; detail = $_.Exception.Message })
} finally {
    # Keep all test evidence even when registration, input or cleanup fails.
    $gateRoot = Join-Path $env:TEMP 'nospacekey-gate'
    if (Test-Path $gateRoot) { Copy-Item $gateRoot (Join-Path $reports 'gates') -Recurse -Force }
    foreach ($name in @('nospacekey-tip.log', 'nospacekey-engine.log')) {
        $path = Join-Path $env:TEMP $name
        if (Test-Path $path) { Copy-Item $path $reports -Force }
    }
    $uninstaller = Join-Path $installRoot 'unins000.exe'
    if (Test-Path $uninstaller) {
        try {
            $config = Join-Path $installed 'NospacekeyConfig.exe'
            if (Test-Path $config) { Invoke-InstallerProcess $config @('--stop-engine') 30 }
            Invoke-InstallerProcess $uninstaller @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=$reports/uninstall.log")
            if (Test-Path 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Classes\CLSID\{B4B39227-EFF2-41DA-B357-0C3170A57875}\InprocServer32') { throw 'TIP registration remains after uninstall' }
            if (Test-Path $installed) { throw 'Installed version remains after uninstall (possibly pending reboot)' }
            $results.Add([ordered]@{ check = 'uninstall and unregister'; status = 'pass' })
        } catch {
            $results.Add([ordered]@{ check = 'uninstall'; status = 'fail'; detail = $_.Exception.Message })
            if (-not $failure) { $failure = $_ }
        }
    }
    [ordered]@{
        commit = $info.commit; installer_sha256 = $info.sha256
        runner_os = (Get-CimInstance Win32_OperatingSystem).Caption
        checks = $results.ToArray()
        not_tested = @('Physical GPU inference', 'Physical JIS keyboard', 'Word interaction', 'Upgrade from a previous version')
    } | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $reports 'verification.json') -Encoding utf8
    if ($env:GITHUB_STEP_SUMMARY) {
        '| Check | Result |' >> $env:GITHUB_STEP_SUMMARY
        '| --- | --- |' >> $env:GITHUB_STEP_SUMMARY
        foreach ($result in $results) { "| $($result.check) | $($result.status) |" >> $env:GITHUB_STEP_SUMMARY }
        '' >> $env:GITHUB_STEP_SUMMARY
        'Physical GPU inference, physical JIS keys, Word, and upgrades from an older version are not covered.' >> $env:GITHUB_STEP_SUMMARY
    }
}
if ($failure) { throw $failure }
