# Windows hosted-runner prerequisites. Product builds use with-dev-env.ps1.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$root = Split-Path -Parent $PSScriptRoot
$downloads = Join-Path $root '.ci-downloads'
New-Item -ItemType Directory -Force $downloads | Out-Null

function Get-Installer([string]$Name, [string]$Url) {
    $path = Join-Path $downloads $Name
    if (-not (Test-Path -LiteralPath $path)) {
        $partial = "$path.partial"
        & curl.exe --fail --location --retry 3 --output $partial $Url
        Move-Item -LiteralPath $partial -Destination $path -Force
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $path
    if ($signature.Status -ne 'Valid') {
        throw "Installer signature is not valid: $Name ($($signature.Status))"
    }
    return $path
}

function Install-Tool([string]$Path, [string[]]$Arguments) {
    $start = [Diagnostics.ProcessStartInfo]::new($Path)
    $start.UseShellExecute = $false
    foreach ($arg in $Arguments) { [void]$start.ArgumentList.Add($arg) }
    $process = [Diagnostics.Process]::Start($start)
    if (-not $process.WaitForExit(1200000)) {
        $process.Kill($true)
        throw "Tool installation timed out: $Path"
    }
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "Tool installation failed: $Path (exit $($process.ExitCode))"
    }
}

git config --global core.longpaths true
rustup toolchain install 1.96.0 --profile minimal --no-self-update

$swift = Get-Installer 'swift-6.3.2.exe' 'https://download.swift.org/swift-6.3.2-release/windows10/swift-6.3.2-RELEASE/swift-6.3.2-RELEASE-windows10.exe'
Install-Tool $swift @('/quiet', '/norestart', '/log', (Join-Path $downloads 'swift-install.log'))

$vulkan = Get-Installer 'VulkanSDK-1.3.296.0.exe' 'https://sdk.lunarg.com/sdk/download/1.3.296.0/windows/VulkanSDK-1.3.296.0-Installer.exe'
Install-Tool $vulkan @('--root', 'C:\VulkanSDK\1.3.296.0', '--accept-licenses', '--default-answer', '--confirm-command', 'install')
$sdk = 'C:\VulkanSDK\1.3.296.0'
foreach ($file in @('Include\vulkan\vulkan.h', 'Lib\vulkan-1.lib', 'Bin\glslc.exe')) {
    if (-not (Test-Path (Join-Path $sdk $file))) { throw "Vulkan SDK is missing $file" }
}
"VULKAN_SDK=$sdk" >> $env:GITHUB_ENV
"$sdk\Bin" >> $env:GITHUB_PATH

$iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) {
    choco install innosetup --version=6.4.3 --yes --no-progress
}
if (-not (Test-Path $iscc)) { throw 'Inno Setup compiler is unavailable' }
"NOSPACEKEY_ISCC=$iscc" >> $env:GITHUB_ENV

& (Join-Path $PSScriptRoot 'with-dev-env.ps1') @('swift', '--version')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
