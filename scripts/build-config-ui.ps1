[CmdletBinding()]
param([switch]$SkipInstall)
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$frontend = Join-Path $repoRoot 'crates\config\frontend'
$lockfile = Join-Path $frontend 'package-lock.json'
if (-not (Test-Path -LiteralPath $lockfile -PathType Leaf)) {
    throw "config UI lockfile is missing: $lockfile"
}

Push-Location $frontend
try {
    if (-not $SkipInstall) {
        & npm.cmd ci --ignore-scripts --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) { throw "npm ci failed with exit code $LASTEXITCODE" }
    }
    & npm.cmd run build
    if ($LASTEXITCODE -ne 0) { throw "config UI build failed with exit code $LASTEXITCODE" }

    $inputs = Get-ChildItem -LiteralPath $frontend -Recurse -File |
        Where-Object {
            $_.FullName -notlike "*\node_modules\*" -and
            $_.FullName -notlike "*\dist\*" -and
            $_.Name -notlike '*.tsbuildinfo' -and
            $_.Name -ne 'vite.config.js' -and
            $_.Name -ne 'vite.config.d.ts'
        } |
        Sort-Object FullName
    $frontendPrefix = (Resolve-Path -LiteralPath $frontend).Path.TrimEnd('\') + '\'
    $manifest = foreach ($input in $inputs) {
        $relative = $input.FullName.Substring($frontendPrefix.Length).Replace('\', '/')
        $hash = (Get-FileHash -LiteralPath $input.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$relative|$hash"
    }
    [IO.File]::WriteAllLines(
        (Join-Path $frontend 'dist\.source-hash'),
        [string[]]$manifest,
        (New-Object Text.UTF8Encoding($false)))
} finally {
    Pop-Location
}
