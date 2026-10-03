<#
  sync-version.ps1 - propagate the single-source version + verify consistency.

  Source of truth: [workspace.package] version in the root Cargo.toml. Cargo
  crates inherit it via `version.workspace = true`. The three files Cargo cannot
  reach are checked-in generated artifacts this script rewrites:
    - crates/config/tauri.conf.json                          (top-level "version")
    - installer/version.iss                                  (display + numeric file versions)
    - engine-host/Sources/NospacekeyEngineCore/BuildInfo.swift  (BuildInfo.version)

  Usage:
    pwsh -File scripts\sync-version.ps1          # rewrite the 3 generated files
    pwsh -File scripts\sync-version.ps1 -Check   # verify only; exit 1 on drift

  ASCII-only on purpose (matches with-dev-env.ps1): no Japanese -> no BOM trap.
  Generated files are written UTF-8 WITHOUT BOM. `Set-Content -Encoding UTF8`
  adds a BOM on PS5.1, and a BOM breaks the tauri.conf.json JSON parse in
  crates/config/tests/version_consistency.rs -- so use WriteAllText + UTF8(false).
#>
param([switch]$Check)
$ErrorActionPreference = 'Stop'

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$CargoPath = Join-Path $RepoRoot 'Cargo.toml'
$ConfPath  = Join-Path $RepoRoot 'crates\config\tauri.conf.json'
$IssPath   = Join-Path $RepoRoot 'installer\version.iss'
$SwiftPath = Join-Path $RepoRoot 'engine-host\Sources\NospacekeyEngineCore\BuildInfo.swift'
$Utf8NoBom = [System.Text.UTF8Encoding]::new($false)

# Read `version = "x"` from a named TOML section (scan until the next [ header).
# All file reads in this script go through [System.IO.File] (UTF-8 by default on
# both engines): WinPS 5.1's Get-Content without -Encoding reads BOM-less UTF-8 as
# ANSI, and a Japanese comment's UTF-8 continuation byte can act as a Shift-JIS
# lead byte and swallow the next character (observed eating a JSON closing quote).
function Get-SectionVersion {
    param([string]$Path, [string]$Header)
    $inSection = $false
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        $t = $line.Trim()
        if ($t -eq $Header) { $inSection = $true; continue }
        if ($inSection) {
            if ($t.StartsWith('[')) { break }
            $m = [regex]::Match($t, '^version\s*=\s*"([^"]+)"')
            if ($m.Success) { return $m.Groups[1].Value }
        }
    }
    return $null
}

# Return the [package] section lines of a crate manifest (until the next [ header).
function Get-PackageLines {
    param([string]$Path)
    $out = New-Object System.Collections.Generic.List[string]
    $inSection = $false
    foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
        $t = $line.Trim()
        if ($t -eq '[package]') { $inSection = $true; continue }
        if ($inSection) {
            if ($t.StartsWith('[')) { break }
            $out.Add($line)
        }
    }
    return $out
}

$ver = Get-SectionVersion -Path $CargoPath -Header '[workspace.package]'
if (-not $ver) { Write-Error 'workspace.package.version not found in root Cargo.toml'; exit 2 }
$baseVer = $ver.Split('-')[0]
if ($baseVer -notmatch '^\d+\.\d+\.\d+$') { Write-Error "workspace.package.version '$ver' has an unsupported core version"; exit 2 }
$fileVer = "$baseVer.0"

$Crates = @('tip', 'ipc', 'ids', 'settings', 'testbench', 'config', 'update')

if ($Check) {
    $errors = New-Object System.Collections.Generic.List[string]

    # tauri.conf.json -- read the TOP-LEVEL version via JSON parse (robust against any
    # nested "version" a future plugin/bundle/updater sub-config might introduce; a
    # first-match regex would validate the wrong key).
    try { $confVer = ([System.IO.File]::ReadAllText($ConfPath) | ConvertFrom-Json).version }
    catch { $confVer = $null }
    if (-not $confVer) { $errors.Add('tauri.conf.json: no top-level "version"') }
    elseif ($confVer -ne $ver) { $errors.Add("tauri.conf.json: version '$confVer' != '$ver'") }

    # installer/version.iss
    if (-not (Test-Path -LiteralPath $IssPath)) { $errors.Add('installer/version.iss: missing') }
    else {
        $iss = [System.IO.File]::ReadAllText($IssPath)
        if ($iss -notmatch [regex]::Escape("#define MyAppVersion `"$ver`"")) { $errors.Add("version.iss: does not define MyAppVersion `"$ver`"") }
        if ($iss -notmatch [regex]::Escape("#define MyAppFileVersion `"$fileVer`"")) { $errors.Add("version.iss: does not define MyAppFileVersion `"$fileVer`"") }
    }

    # engine-host BuildInfo.swift
    if (-not (Test-Path -LiteralPath $SwiftPath)) { $errors.Add('BuildInfo.swift: missing') }
    else {
        $bi = [System.IO.File]::ReadAllText($SwiftPath)
        if ($bi -notmatch [regex]::Escape("version = `"$ver`"")) { $errors.Add("BuildInfo.swift: does not set version `"$ver`"") }
    }

    # Each crate's [package] must inherit (version.workspace = true) and not hardcode.
    # Scope to the [package] section only -- dependency tables carry `version = "0.62"`
    # etc. that are NOT the crate version and must never trip this check.
    foreach ($c in $Crates) {
        $path = Join-Path $RepoRoot "crates\$c\Cargo.toml"
        $pkg = Get-PackageLines -Path $path
        if (-not ($pkg | Where-Object { $_.Trim() -eq 'version.workspace = true' })) {
            $errors.Add("crates/${c}: [package] missing 'version.workspace = true'")
        }
        if ($pkg | Where-Object { $_.TrimStart() -match '^version\s*=\s*"' }) {
            $errors.Add("crates/${c}: [package] has a hardcoded 'version = ...'")
        }
    }

    if ($errors.Count -gt 0) {
        Write-Host "version drift (source of truth = $ver):"
        $errors | ForEach-Object { Write-Host "  - $_" }
        exit 1
    }
    Write-Host "version OK: all declarations = $ver"
    exit 0
}

# --- write mode: rewrite the 3 generated files (UTF-8, no BOM) ---

# tauri.conf.json: replace ONLY the top-level "version" value, anchored to its 2-space
# top-level indent so a future nested "version" in a sub-config is never clobbered. Keep
# a string replace (not ConvertFrom/ConvertTo-Json) to avoid reflowing the whole file. If
# the top-level key ever stops matching (reindented file), no replacement happens and the
# subsequent -Check fails loudly rather than corrupting a nested value.
$conf = [System.IO.File]::ReadAllText($ConfPath)
$conf = [regex]::Replace($conf, '(?m)^  "version"\s*:\s*"[^"]*"', "  `"version`": `"$ver`"")
[System.IO.File]::WriteAllText($ConfPath, $conf, $Utf8NoBom)

$issText = @"
; AUTO-GENERATED by scripts/sync-version.ps1 -- do not edit by hand.
; The source of truth is [workspace.package] version in the root Cargo.toml.
#define MyAppVersion "$ver"
#define MyAppFileVersion "$fileVer"
"@
[System.IO.File]::WriteAllText($IssPath, $issText + "`n", $Utf8NoBom)

$swiftText = @"
// AUTO-GENERATED by scripts/sync-version.ps1 -- do not edit by hand.
// The source of truth is [workspace.package] version in the root Cargo.toml.
public enum BuildInfo {
    public static let version = "$ver"
}
"@
[System.IO.File]::WriteAllText($SwiftPath, $swiftText + "`n", $Utf8NoBom)

Write-Host "synced version $ver -> tauri.conf.json, installer/version.iss, BuildInfo.swift"
