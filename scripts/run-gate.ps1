<#
.SYNOPSIS
  Run the nospacekey headless gate (testbench) WITHOUT the recurring
  wrapper-hang traps, and print a pass/fail summary. pwsh 7.3+ only.

.DESCRIPTION
  Two traps have repeatedly wasted hours (2026-07-06..09), both in the WRAPPER,
  not the gate itself (the gate finishes in ~30s):

    1. Piping testbench output ( `testbench ... | Where-Object` ) hangs forever:
       the TIP-spawned `--persist` engine inherits testbench's stdout pipe handle
       and never exits, so the pipe never reaches EOF.
    2. `Start-Process -Wait` ALSO hangs: PowerShell's -Wait waits for the whole
       descendant process tree, which includes the persist engine.

  This script avoids both: stdout/stderr go to files, and we wait ONLY on the
  testbench process itself via .NET WaitForExit(timeout). The spawn discipline
  lives in scripts/test-lib.ps1 (Invoke-Testbench) — do NOT hand-roll a wrapper.

  Foreground trap (the gate all-FAILs while the user is typing/mousing, because
  msctf then never activates the TIP — item 1 "activate" failing is the tell):
    -WaitIdle  wait for a >=15s no-input window (GetLastInputInfo), then run.
    -Sandbox   run the whole gate inside Windows Sandbox: a separate session
               whose input is fully isolated, so the host user may keep typing.
               Artifacts are staged to %TEMP%\nospacekey-sandbox-gate\<run-guid>\;
               results land next to them. The sandbox is disposable, so the
               TIP registration inside is always from-scratch (deterministic).
               All Sandbox runs disable the guest's Smart App Control policy
               for that disposable session: local dev binaries are unsigned,
               and current distributions are dev-signed (self-rooted), which
               an enforcing guest image blocks at LoadLibrary.
               -DistributionDir makes a signed distribution tree the sole
               product-payload source; the legacy build-tree closure is not
               mixed into that run. testbench.exe remains a separate input.

  Use this for EVERY testbench invocation (--scenarios, --keymap-smoke, ...).
  Do not hand-roll a wrapper: both traps above are silent and cost hours.

  Exit codes are FAIL-CLOSED against the parsed result rows (see
  Resolve-GateExit in test-lib.ps1): the child's exit 0 is only trusted when
  the evidence agrees. exit 0 + zero parsed rows -> 2 (no evidence); exit 0 +
  FAIL/ERROR rows -> 1. Nonzero child exits pass through unchanged.

.EXAMPLE
  pwsh -File scripts\run-gate.ps1                                  # default --scenarios
  pwsh -File scripts\run-gate.ps1 -TestbenchArgs '--keymap-smoke'  # any testbench mode
  pwsh -File scripts\run-gate.ps1 -WaitIdle                        # auto-wait for no-input window
  pwsh -File scripts\run-gate.ps1 -Sandbox                         # immune to user input
  pwsh -File scripts\run-gate.ps1 -Sandbox -DistributionDir dist # signed payload gate
  pwsh -File scripts\run-gate.ps1 -Sandbox -DistributionDir dist `
#>
#Requires -Version 7.3
[CmdletBinding()]
param(
    # Path to testbench.exe (empty = sibling target\release of this repo).
    [string]$Testbench = '',
    # Scenario mode / extra args for testbench. Elements must NOT contain spaces:
    # in -Sandbox mode they cross the .wsb LogonCommand boundary as one joined
    # string and are re-split on whitespace inside the sandbox.
    [string[]]$TestbenchArgs = @('--scenarios'),
    # Hard timeout for the testbench process itself (sandbox mode adds a boot
    # buffer on top). Do not shorten below 240: item24 gets cut off at 45s.
    [int]$TimeoutSec = 240,
    # Reuse an engine that was already running instead of name-killing it before
    # and after the gate. Any engine spawned by this validation run is still a
    # Job descendant and is drained by the crash-containment supervisor.
    [switch]$ReuseExistingEngine,
    # Wait for a >=15s user-idle window before running (foreground trap mitigation).
    [switch]$WaitIdle,
    # Idle threshold in seconds for -WaitIdle (default 15; the "input -> idle
    # just-transitioned" window passes most reliably).
    [int]$IdleSeconds = 15,
    # Run the gate inside Windows Sandbox (needs Pro/Enterprise + the Windows
    # Sandbox feature enabled; see the hint printed when it is missing).
    [switch]$Sandbox,
    # Signed distribution tree to use as the product payload in -Sandbox mode.
    # testbench.exe is supplied separately by -Testbench and is never part of
    # this tree. An empty value preserves the historical build-tree staging.
    # Keep new optional parameters after the legacy positional parameters.
    [string]$DistributionDir = '',
    # Previous signed distribution used only by --pair-coexistence. The guest
    # keeps this TIP loaded while registering and starting DistributionDir.
    [string]$PreviousDistributionDir = '',
    # Use the current Store CLI when the legacy WindowsSandbox.exe launcher fails.
    [switch]$SandboxCli
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'test-lib.ps1')
. (Join-Path $PSScriptRoot 'zenzai-runtime-manifest.ps1')
. (Join-Path $PSScriptRoot 'swift-toolchain.ps1')
. (Join-Path $PSScriptRoot 'release-lib.ps1')

# The first invocation immediately hands control to the same-integrity
# supervisor. The supervised child returns here only after parent/job/mutex
# ownership has been proven by Enter-ValidationGateLock.
Invoke-ValidationRelaunch -ScriptPath $PSCommandPath -BoundParameters $PSBoundParameters

# ----------------------------------------------------------------------------
# Windows Sandbox gate (input-isolated session; see .DESCRIPTION)
# Defined before the main flow because PowerShell evaluates top-to-bottom.
# ----------------------------------------------------------------------------
function Resolve-SandboxTimeoutSec {
    param(
        [int]$RequestedTimeoutSec,
        [switch]$Sandbox,
        [string[]]$TestbenchArgs,
        [switch]$TimeoutExplicit
    )
    if ($RequestedTimeoutSec -lt 1) { throw 'TimeoutSec must be positive.' }
    if ($Sandbox -and -not $TimeoutExplicit -and $TestbenchArgs.Count -eq 1 -and
        $TestbenchArgs[0] -eq '--async-stress') {
        return 420
    }
    return $RequestedTimeoutSec
}

function Get-SandboxValidatedPath {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Label
    )
    try {
        $full = [IO.Path]::GetFullPath($Path)
        $root = [IO.Path]::GetPathRoot($full)
        if ([string]::IsNullOrWhiteSpace($root)) { throw 'path has no filesystem root' }
        $rootItem = Get-Item -LiteralPath $root -Force -ErrorAction Stop
        if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point is not allowed at path root: $root"
        }
        $current = $root
        $relative = $full.Substring($root.Length)
        foreach ($component in @($relative -split '[\\/]+' | Where-Object { $_ })) {
            $current = Join-Path -Path $current -ChildPath $component
            $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "reparse point is not allowed in path component: $current"
            }
        }
        return $full
    } catch {
        throw "$Label path component validation failed: $Path ($($_.Exception.Message))"
    }
}

function Test-SandboxPathWithinRoot {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][string]$Candidate
    )
    $rootFull = [IO.Path]::GetFullPath($Root)
    $rootIsPathRoot = $rootFull.Equals([IO.Path]::GetPathRoot($rootFull), [StringComparison]::OrdinalIgnoreCase)
    if (-not $rootIsPathRoot) { $rootFull = $rootFull.TrimEnd('\') }
    $candidateFull = [IO.Path]::GetFullPath($Candidate)
    $prefix = if ($rootIsPathRoot) { $rootFull } else { $rootFull + [IO.Path]::DirectorySeparatorChar }
    return $candidateFull.Equals($rootFull, [StringComparison]::OrdinalIgnoreCase) -or
        $candidateFull.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-SandboxPayloadManifest {
    param(
        [Parameter(Mandatory)][string]$StagedRoot,
        [Parameter(Mandatory)][string]$ManifestPath,
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$GateOwnedRelativePaths
    )

    # The manifest describes product files only.  Sandbox orchestration files
    # live in the same mapped bin directory, so keep their paths in a separate
    # allowlist while checking every other path against the manifest exactly.
    $rootItem = Get-Item -LiteralPath $StagedRoot -Force -ErrorAction Stop
    if (-not $rootItem.PSIsContainer) { throw "staged payload root is not a directory: $StagedRoot" }
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "reparse point is not allowed at staged payload root: $StagedRoot"
    }
    $manifestItem = Get-Item -LiteralPath $ManifestPath -Force -ErrorAction Stop
    if ($manifestItem.PSIsContainer) { throw "host-only payload manifest is not a file: $ManifestPath" }
    if (($manifestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "reparse point is not allowed at host-only payload manifest: $ManifestPath"
    }

    $rootFull = [IO.Path]::GetFullPath($rootItem.FullName)
    $rootIsPathRoot = $rootFull.Equals([IO.Path]::GetPathRoot($rootFull), [StringComparison]::OrdinalIgnoreCase)
    if (-not $rootIsPathRoot) { $rootFull = $rootFull.TrimEnd('\') }

    function Get-ManifestRelativePath {
        param([Parameter(Mandatory)][string]$Root, [Parameter(Mandatory)][string]$FullPath)
        $rootFull = [IO.Path]::GetFullPath($Root)
        $rootIsPathRoot = $rootFull.Equals([IO.Path]::GetPathRoot($rootFull), [StringComparison]::OrdinalIgnoreCase)
        if (-not $rootIsPathRoot) { $rootFull = $rootFull.TrimEnd('\') }
        $full = [IO.Path]::GetFullPath($FullPath)
        $prefix = if ($rootIsPathRoot) { $rootFull } else { $rootFull + [IO.Path]::DirectorySeparatorChar }
        if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "staged payload path escaped root: $FullPath"
        }
        return $full.Substring($prefix.Length).Replace('\', '/')
    }

    function Normalize-ManifestRelativePath {
        param([Parameter(Mandatory)][string]$Path)
        if ([string]::IsNullOrWhiteSpace($Path) -or
            $Path.Contains('\') -or $Path.Contains('//') -or
            $Path.StartsWith('/') -or $Path -match '\A[A-Za-z]:' -or
            $Path -match '(^|/)\.\.?(?:/|$)' -or $Path.EndsWith('/')) {
            throw "invalid payload manifest relative path: $Path"
        }
        return $Path
    }

    $expected = [Collections.Generic.Dictionary[string,string]]::new(
        [StringComparer]::OrdinalIgnoreCase)
    foreach ($line in @(Get-Content -LiteralPath $ManifestPath -ErrorAction Stop)) {
        if ($line -notmatch '\A(?<Hash>[A-Fa-f0-9]{64})  (?<Path>.+)\z') {
            throw "invalid payload manifest line"
        }
        $manifestHash = $Matches.Hash
        $manifestRelative = $Matches.Path
        $relative = Normalize-ManifestRelativePath -Path $manifestRelative
        if ($expected.ContainsKey($relative)) {
            throw "duplicate payload manifest path: $relative"
        }
        $expected.Add($relative, $manifestHash.ToUpperInvariant())
    }

    $gateOwned = [Collections.Generic.Dictionary[string,bool]]::new(
        [StringComparer]::OrdinalIgnoreCase)
    foreach ($path in @($GateOwnedRelativePaths)) {
        $relative = Normalize-ManifestRelativePath -Path $path
        if ($expected.ContainsKey($relative)) {
            throw "payload manifest contains gate-owned path: $relative"
        }
        if (-not $gateOwned.ContainsKey($relative)) { $gateOwned.Add($relative, $true) }
    }

    # Inventory all entries explicitly so a reparse point, an unexpected
    # directory, or a file substituted for an expected directory cannot be
    # hidden by a recursive Get-FileHash call.
    $pending = [Collections.Generic.Stack[string]]::new()
    $entries = [Collections.Generic.List[object]]::new()
    $pending.Push($rootFull)
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        foreach ($item in @(Get-ChildItem -LiteralPath $current -Force -ErrorAction Stop)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "reparse point is not allowed in staged payload: $($item.FullName)"
            }
            $entry = [pscustomobject]@{
                RelativePath = Get-ManifestRelativePath -Root $rootFull -FullPath $item.FullName
                FullPath = [IO.Path]::GetFullPath($item.FullName)
                IsDirectory = [bool]$item.PSIsContainer
            }
            $entries.Add($entry)
            if ($entry.IsDirectory) { $pending.Push($entry.FullPath) }
        }
    }

    $actual = [Collections.Generic.Dictionary[string,object]]::new(
        [StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in $entries) {
        if ($actual.ContainsKey($entry.RelativePath)) {
            throw "duplicate staged payload path: $($entry.RelativePath)"
        }
        $actual.Add($entry.RelativePath, $entry)
    }

    # Gate-owned files are allowed only at their explicitly named paths.  All
    # other files must be manifest entries; directories are allowed only when
    # they are ancestors of a declared product/gate-owned path.
    foreach ($entry in $entries) {
        $relative = $entry.RelativePath
        if ($gateOwned.ContainsKey($relative)) {
            if ($entry.IsDirectory) { throw "gate-owned path has wrong type: $relative" }
            continue
        }
        if ($expected.ContainsKey($relative)) {
            if ($entry.IsDirectory) { throw "manifest file path is a directory: $relative" }
            continue
        }
        if (-not $entry.IsDirectory) {
            throw "unexpected staged product path: $relative"
        }
        $ancestorPrefix = $relative + '/'
        $isAncestor = @($expected.Keys + $gateOwned.Keys | Where-Object {
            $_.StartsWith($ancestorPrefix, [StringComparison]::OrdinalIgnoreCase)
        }).Count -gt 0
        if (-not $isAncestor) { throw "unexpected staged product directory: $relative" }
    }

    $productFiles = @($entries | Where-Object {
        -not $_.IsDirectory -and -not $gateOwned.ContainsKey($_.RelativePath)
    })
    if ($productFiles.Count -ne $expected.Count) {
        throw "staged product file count mismatch: expected=$($expected.Count) actual=$($productFiles.Count)"
    }
    foreach ($relative in $expected.Keys) {
        if (-not $actual.ContainsKey($relative)) {
            throw "staged product path missing: $relative"
        }
        $entry = $actual[$relative]
        if ($entry.IsDirectory) { throw "manifest file path is a directory: $relative" }
        # Re-read the item immediately before hashing.  This closes the
        # inventory/hash gap for an accidental replacement with a reparse point
        # or a directory and turns all read failures into a hard failure.
        $currentItem = Get-Item -LiteralPath $entry.FullPath -Force -ErrorAction Stop
        if ($currentItem.PSIsContainer) { throw "manifest file path is a directory: $relative" }
        if (($currentItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point is not allowed in staged payload: $relative"
        }
        $actualHash = (Get-FileHash -LiteralPath $entry.FullPath -Algorithm SHA256 `
            -ErrorAction Stop).Hash.ToUpperInvariant()
        if ($actualHash -ne $expected[$relative]) {
            throw "staged product SHA-256 mismatch: $relative"
        }
    }
    return [pscustomobject]@{
        FileCount = $productFiles.Count
        ManifestPath = [IO.Path]::GetFullPath($ManifestPath)
    }
}

function Get-SandboxApplicationControlArgument {
    # Signed-distribution gates also need the disposable guest SAC bypass:
    # while no public certificate exists, distributions ship dev-signed
    # (self-rooted) binaries that an enforcing Smart App Control image blocks
    # at LoadLibrary (win32 4551), killing regsvr32 with exit 3. Host-side
    # signature revalidation is unaffected. Drop the bypass once releases
    # carry a publicly trusted signature.
    param([bool]$UseSignedDistribution)
    return ' -DisableApplicationControlForDevelopment'
}

function Invoke-SandboxGate {
    param(
        [Parameter(Mandatory)][string]$Testbench,
        [string[]]$TestbenchArgs = @('--scenarios'),
        [int]$TimeoutSec = 240,
        [string]$OutFile = '',
        [string]$DistributionDir = '',
        [string]$PreviousDistributionDir = '',
        [switch]$SandboxCli
    )
    $wsbExe = Join-Path $env:WINDIR 'System32\WindowsSandbox.exe'
    if (-not (Test-Path $wsbExe)) {
        Write-FailMsg 'Windows Sandbox is not enabled on this machine.'
        Write-Host  '  Enable it from an ADMIN prompt (reboot if asked), then retry:' -ForegroundColor Yellow
        Write-Host  '    DISM /Online /Enable-Feature /FeatureName:Containers-DisposableClientVm /All' -ForegroundColor Yellow
        Write-Host  '  Until then, use:  run-gate.ps1 -WaitIdle' -ForegroundColor Yellow
        exit 2
    }
    if ($TimeoutSec -lt 1) {
        Write-FailMsg 'sandbox timeout must be at least one second'
        exit 2
    }
    foreach ($argument in $TestbenchArgs) {
        # The sandbox contract deliberately re-splits one whitespace-free
        # string. Reject cmd/XML metacharacters rather than interpolating them
        # into the generated guest watchdog command line.
        if ([string]::IsNullOrWhiteSpace($argument) -or
            $argument -notmatch '\A[A-Za-z0-9_./:\\=,+@-]+\z') {
            Write-FailMsg "unsafe sandbox testbench argument: $argument"
            exit 2
        }
    }
    $pairCoexistence = $TestbenchArgs.Count -eq 1 -and
        $TestbenchArgs[0] -eq '--pair-coexistence'
    $versionCleanup = $TestbenchArgs.Count -eq 1 -and
        $TestbenchArgs[0] -eq '--version-cleanup'

    function Get-SandboxHostRelativePath {
        param(
            [Parameter(Mandatory)][string]$Root,
            [Parameter(Mandatory)][string]$FullPath
        )
        $rootFull = [IO.Path]::GetFullPath($Root)
        $rootIsPathRoot = $rootFull.Equals([IO.Path]::GetPathRoot($rootFull), [StringComparison]::OrdinalIgnoreCase)
        if (-not $rootIsPathRoot) { $rootFull = $rootFull.TrimEnd('\') }
        $full = [IO.Path]::GetFullPath($FullPath)
        $prefix = if ($rootIsPathRoot) { $rootFull } else { $rootFull + [IO.Path]::DirectorySeparatorChar }
        if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "path escaped root: $FullPath"
        }
        return $full.Substring($prefix.Length).Replace('\', '/')
    }

    # Enumerate a host tree without following reparse points. The signed
    # distribution is a payload boundary: a junction/symlink would make the
    # apparent tree differ from the bytes that are actually staged.
    function Get-SandboxHostTree {
        param([Parameter(Mandatory)][string]$Root)
        $rootItem = Get-Item -LiteralPath $Root -Force -ErrorAction Stop
        if (-not $rootItem.PSIsContainer) { throw "distribution is not a directory: $Root" }
        if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "reparse point is not allowed at distribution root: $Root"
        }
        $rootFull = [IO.Path]::GetFullPath($rootItem.FullName)
        $pending = New-Object 'System.Collections.Generic.Stack[string]'
        $entries = New-Object 'System.Collections.Generic.List[object]'
        $pending.Push($rootFull)
        while ($pending.Count -gt 0) {
            $current = $pending.Pop()
            foreach ($item in @(Get-ChildItem -LiteralPath $current -Force -ErrorAction Stop)) {
                if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                    throw "reparse point is not allowed in distribution: $($item.FullName)"
                }
                $entries.Add([pscustomobject]@{
                    RelativePath = Get-SandboxHostRelativePath -Root $rootFull -FullPath $item.FullName
                    FullPath = [IO.Path]::GetFullPath($item.FullName)
                    Name = $item.Name
                    IsDirectory = [bool]$item.PSIsContainer
                })
                if ($item.PSIsContainer) { $pending.Push([IO.Path]::GetFullPath($item.FullName)) }
            }
        }
        return @($entries.ToArray() | Sort-Object RelativePath)
    }

    function Stage-SignedSandboxPayload {
        param(
            [Parameter(Mandatory)][string]$SourceRoot,
            [Parameter(Mandatory)][string]$DestinationRoot,
            [Parameter(Mandatory)][string]$ManifestPath
        )
        $entries = @(Get-SandboxHostTree -Root $SourceRoot)
        $collisionNames = @(
            'testbench.exe', 'sandbox-gate-inner.ps1', 'sandbox-gate-watchdog.cmd',
            'run-gate-inner.cmd')
        $collisions = @($entries | Where-Object {
            $collisionNames -contains $_.Name.ToLowerInvariant()
        })
        if ($collisions.Count -gt 0) {
            throw ("distribution payload collides with gate-owned file(s): {0}" -f
                (($collisions | ForEach-Object { $_.RelativePath }) -join ', '))
        }
        foreach ($required in @('nospacekey_tip.dll', 'NospacekeyEngineHost.exe')) {
            $matches = @($entries | Where-Object {
                -not $_.IsDirectory -and $_.RelativePath -ieq $required
            })
            if ($matches.Count -ne 1) {
                throw "signed distribution is missing root payload file: $required"
            }
        }

        foreach ($entry in @($entries | Where-Object IsDirectory | Sort-Object RelativePath)) {
            $destination = Join-Path $DestinationRoot ($entry.RelativePath -replace '/', '\')
            [void][IO.Directory]::CreateDirectory($destination)
        }
        foreach ($entry in @($entries | Where-Object { -not $_.IsDirectory })) {
            $destination = Join-Path $DestinationRoot ($entry.RelativePath -replace '/', '\')
            $parent = Split-Path -Parent $destination
            if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
                [void][IO.Directory]::CreateDirectory($parent)
            }
            Copy-Item -LiteralPath $entry.FullPath -Destination $destination -Force -ErrorAction Stop
        }

        $manifestLines = New-Object 'System.Collections.Generic.List[string]'
        $fileEntries = @($entries | Where-Object { -not $_.IsDirectory })
        foreach ($entry in $fileEntries) {
            $destination = Join-Path $DestinationRoot ($entry.RelativePath -replace '/', '\')
            $sourceHash = (Get-FileHash -LiteralPath $entry.FullPath -Algorithm SHA256 -ErrorAction Stop).Hash.ToUpperInvariant()
            $stagedHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256 -ErrorAction Stop).Hash.ToUpperInvariant()
            if ($sourceHash -ne $stagedHash) {
                throw "staged payload hash mismatch: $($entry.RelativePath)"
            }
            [void]$manifestLines.Add(('{0}  {1}' -f $stagedHash, $entry.RelativePath))
        }
        [IO.File]::WriteAllLines($ManifestPath, [string[]]$manifestLines.ToArray(), [Text.Encoding]::UTF8)
        return [pscustomobject]@{ Entries = $entries; FileCount = $fileEntries.Count }
    }

    $repoRoot = Split-Path -Parent $PSScriptRoot
    $testbenchSource = $null
    try {
        $validatedTestbench = Get-SandboxValidatedPath -Path $Testbench -Label 'testbench'
        $testbenchSource = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $validatedTestbench -ErrorAction Stop).Path)
    } catch {
        Write-FailMsg "testbench not found: $Testbench"
        exit 2
    }
    if (-not (Test-Path -LiteralPath $testbenchSource -PathType Leaf)) {
        Write-FailMsg "testbench not found: $Testbench"
        exit 2
    }
    $Testbench = $testbenchSource

    $distributionRoot = $null
    $previousDistributionRoot = $null
    $useSignedDistribution = -not [string]::IsNullOrWhiteSpace($DistributionDir)
    if ($useSignedDistribution) {
        try {
            $validatedDistribution = Get-SandboxValidatedPath -Path $DistributionDir -Label 'distribution'
            $distributionRoot = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $validatedDistribution -ErrorAction Stop).Path)
            if (-not (Test-Path -LiteralPath $distributionRoot -PathType Container)) {
                throw "distribution is not a directory: $DistributionDir"
            }
            Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $distributionRoot | Out-Null
            if (Test-SandboxPathWithinRoot -Root $distributionRoot -Candidate $testbenchSource) {
                throw 'testbench path is inside the signed distribution root'
            }
        } catch {
            Write-FailMsg "invalid signed distribution directory: $DistributionDir ($($_.Exception.Message))"
            exit 2
        }
    } else {
        $tipDll      = Join-Path $repoRoot 'target\release\nospacekey_tip.dll'
        # Prefer the co-located engine (target\release), but fall back to the swift
        # build output: verify-manual's teardown REMOVES the co-located exe after a
        # completed run, and the sandbox stages into its own bin\ anyway, so the
        # .build copy is sufficient here and the hint below stays self-resolving.
        $engineExe   = Join-Path $repoRoot 'target\release\NospacekeyEngineHost.exe'
        if (-not (Test-Path -LiteralPath $engineExe)) {
            $engineExe = Join-Path $repoRoot 'engine-host\.build\x86_64-unknown-windows-msvc\release\NospacekeyEngineHost.exe'
        }
        $vendorLlama = Join-Path $repoRoot 'engine-host\vendor\llama\vulkan'
        $modelsDir   = Join-Path $repoRoot 'engine-host\models'
        foreach ($f in @($Testbench, $tipDll, $engineExe)) {
            if (-not (Test-Path -LiteralPath $f)) {
                Write-FailMsg "artifact not found: $f"
                # verify-harness builds the DEBUG testbench and the DEBUG engine, but
                # this gate needs the RELEASE set (testbench + engine + .resources) —
                # verify-manual -Rebuild is the entry that actually produces it.
                Write-Host  '  Build the release gate set first:' -ForegroundColor Yellow
                Write-Host  '    pwsh -File scripts\verify-manual.ps1 -Suite sp1 -Rebuild   # TIP DLL + release engine + .resources' -ForegroundColor Yellow
                Write-Host  '    pwsh -File scripts\with-dev-env.ps1 cargo build -p testbench --release   # release testbench (from repo root)' -ForegroundColor Yellow
                exit 2
            }
        }
    }
    if ($pairCoexistence) {
        if (-not $useSignedDistribution -or
            [string]::IsNullOrWhiteSpace($PreviousDistributionDir)) {
            Write-FailMsg '--pair-coexistence requires both -DistributionDir and -PreviousDistributionDir.'
            exit 2
        }
        try {
            $validatedPrevious = Get-SandboxValidatedPath -Path $PreviousDistributionDir `
                -Label 'previous distribution'
            $previousDistributionRoot = [IO.Path]::GetFullPath(
                (Resolve-Path -LiteralPath $validatedPrevious -ErrorAction Stop).Path)
            if (-not (Test-Path -LiteralPath $previousDistributionRoot -PathType Container)) {
                throw "previous distribution is not a directory: $PreviousDistributionDir"
            }
            if ($previousDistributionRoot.Equals($distributionRoot,
                    [StringComparison]::OrdinalIgnoreCase)) {
                throw 'previous and current distribution roots must be different'
            }
            Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $previousDistributionRoot | Out-Null
            $currentTipVersion = (Get-Item -LiteralPath `
                (Join-Path $distributionRoot 'nospacekey_tip.dll') `
                -ErrorAction Stop).VersionInfo.ProductVersion
            $previousTipVersion = (Get-Item -LiteralPath `
                (Join-Path $previousDistributionRoot 'nospacekey_tip.dll') `
                -ErrorAction Stop).VersionInfo.ProductVersion
            if ([string]::IsNullOrWhiteSpace($currentTipVersion) -or
                [string]::IsNullOrWhiteSpace($previousTipVersion)) {
                throw 'both TIPs must carry a ProductVersion identity'
            }
            if ($previousTipVersion -eq $currentTipVersion) {
                throw "old/new product versions must differ: $currentTipVersion"
            }
            Write-Ok "coexistence versions: previous=$previousTipVersion current=$currentTipVersion"
        } catch {
            Write-FailMsg "invalid previous signed distribution directory: $PreviousDistributionDir ($($_.Exception.Message))"
            exit 2
        }
    } elseif (-not [string]::IsNullOrWhiteSpace($PreviousDistributionDir)) {
        Write-FailMsg '-PreviousDistributionDir is valid only with --pair-coexistence.'
        exit 2
    }

    # ---- stage artifacts (mirrors stage-dist.ps1 §1-4c: the sandbox image is a
    # CLEAN Windows -- no Swift toolchain, no VC++ redist, no SPM bundles. Missing
    # any of these = the engine dies 0xC0000135 inside the sandbox and every
    # conversion item FAILs) ----
    $run = New-RunOwnedDirectory -Root (Join-Path $env:TEMP 'nospacekey-sandbox-gate')
    $root = $run.Path
    $binDir = Join-Path $root 'bin'
    $previousBinDir = Join-Path $root 'previous-bin'
    $resDir = Join-Path $root 'results'
    New-Item -ItemType Directory -Path $binDir, $resDir -ErrorAction Stop | Out-Null

    $payloadManifest = $null
    $payloadGateOwnedPaths = @(
        'testbench.exe', 'sandbox-gate-inner.ps1', 'sandbox-gate-watchdog.cmd',
        'run-gate-inner.cmd')
    Write-Step "staging artifacts -> $binDir"
    if ($useSignedDistribution) {
        try {
            # Only bin/results are mapped into the guest; this file stays host-only.
            $payloadManifest = Join-Path $root 'payload-manifest.txt'
            $payload = Stage-SignedSandboxPayload -SourceRoot $distributionRoot `
                -DestinationRoot $binDir -ManifestPath $payloadManifest
            $productLifetimeEntries = @($payload.Entries | Where-Object {
                $_.RelativePath -ieq '.nospacekey-lifetime'
            })
            if ($productLifetimeEntries.Count -eq 0) {
                $payloadGateOwnedPaths += '.nospacekey-lifetime'
            } elseif ($productLifetimeEntries.Count -ne 1 -or
                $productLifetimeEntries[0].IsDirectory) {
                throw 'signed distribution lifetime sentinel is not a root file'
            }
            if (-not (Test-Path -LiteralPath $payloadManifest -PathType Leaf)) {
                throw "host-only signed distribution manifest was not written: $payloadManifest"
            }
            $testbenchDestination = Join-Path $binDir 'testbench.exe'
            if (Test-Path -LiteralPath $testbenchDestination) {
                throw 'testbench.exe collision in staged distribution payload'
            }
            Copy-Item -LiteralPath $Testbench -Destination $testbenchDestination `
                -Force -ErrorAction Stop
            Write-Ok ("staged signed distribution payload: {0} files" -f $payload.FileCount)
            Write-Ok "staged testbench separately: $Testbench"
            if ($pairCoexistence) {
                New-Item -ItemType Directory -Path $previousBinDir -ErrorAction Stop | Out-Null
                $previousPayloadManifest = Join-Path $root 'previous-payload-manifest.txt'
                $previousPayload = Stage-SignedSandboxPayload `
                    -SourceRoot $previousDistributionRoot `
                    -DestinationRoot $previousBinDir `
                    -ManifestPath $previousPayloadManifest
                Write-Ok ("staged previous signed distribution payload: {0} files" -f
                    $previousPayload.FileCount)
            }
        } catch {
            Write-FailMsg "signed distribution staging failed: $($_.Exception.Message)"
            exit 2
        }
    } else {
        Copy-Item -LiteralPath $Testbench, $tipDll, $engineExe $binDir -Force

        # SPM .resources bundles (dictionary/tokenizer/hub): the engine loads them from
        # next to the exe; without them it cannot start / converts to void.
        $engineRel = Join-Path $repoRoot 'engine-host\.build\x86_64-unknown-windows-msvc\release'
        foreach ($r in @(
            'AzooKeyKanaKanjiConverter_KanaKanjiConverterModuleWithDefaultDictionary.resources',
            'AzooKeyKanaKanjiConverter_EfficientNGram.resources',
            'swift-transformers_Hub.resources'
        )) {
            $src = Join-Path $engineRel $r
            if (Test-Path -LiteralPath $src) { Copy-Item -LiteralPath $src -Destination (Join-Path $binDir $r) -Recurse -Force }
            else { Write-Warn "resources bundle missing: $src (engine may fail inside the sandbox)" }
        }

        # Swift runtime DLLs (the clean sandbox image has no Swift toolchain;
        # same semver/required-leaf contract as stage-dist.ps1 / find-swift.ps1).
        $swiftRoot = Join-Path $env:LOCALAPPDATA 'Programs\Swift'
        $swiftRtRoot = Join-Path $swiftRoot 'Runtimes'
        $swiftRtDir = Get-SwiftRuntimeCandidate -SwiftRoot $swiftRoot
        if ($swiftRtDir) {
            $swiftDlls = @(Get-ChildItem -LiteralPath (Split-Path -Parent $swiftRtDir.LeafPath) -Filter *.dll -File)
            foreach ($d in $swiftDlls) { Copy-Item -LiteralPath $d.FullName -Destination $binDir -Force }
            Write-Ok ("staged {0} Swift runtime DLLs" -f $swiftDlls.Count)
        } else {
            Write-Warn "Swift runtime with swiftCore.dll not found under $swiftRtRoot\<semver>\usr\bin (engine will die 0xC0000135 in the sandbox)."
        }

        try {
            $zenzaiRuntime = Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $vendorLlama `
                -RequireExactDllSet
            foreach ($dll in $zenzaiRuntime.Dlls) {
                Copy-Item -LiteralPath $dll.Path -Destination $binDir -Force -ErrorAction Stop
            }
            Copy-Item -LiteralPath $zenzaiRuntime.ManifestPath -Destination $binDir -Force -ErrorAction Stop
            Copy-Item -LiteralPath $zenzaiRuntime.ReceiptPath -Destination $binDir -Force -ErrorAction Stop
            Assert-ZenzaiVulkanRuntimeBundle -RuntimeDirectory $binDir | Out-Null
            Write-Ok 'staged exact Vulkan Zenzai runtime (five DLLs + receipt + manifest)'
        } catch {
            Write-FailMsg "Vulkan Zenzai runtime staging failed: $($_.Exception.Message)"
            exit 2
        }
        # VC++ redist DLLs the PEs dynamically import (clean image has no VC++ redist;
        # the Swift runtime copy above may already have landed some of them).
        foreach ($vc in @('vcomp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll')) {
            if (Test-Path -LiteralPath (Join-Path $binDir $vc)) { continue }
            $dll = Find-VcRedistDll -Name $vc
            if ($dll) { Copy-Item -LiteralPath $dll.FullName -Destination $binDir -Force; Write-Ok "staged $vc (VC++ redist)" }
            else { Write-Warn "$vc not found (PEs may fail with 0xC0000135 in the sandbox). Install the VC++ redist / VS C++ workload." }
        }
        if (Test-Path -LiteralPath $modelsDir) {
            New-Item -ItemType Directory -Force (Join-Path $binDir 'models') | Out-Null
            Copy-Item -Path (Join-Path $modelsDir '*.gguf') -Destination (Join-Path $binDir 'models') -Force -ErrorAction SilentlyContinue
        }
        if ($versionCleanup) {
            foreach ($name in @('NospacekeyConfig.exe','NospacekeyUpdateChecker.exe')) {
                $source = Join-Path $repoRoot "target\release\$name"
                if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
                    Write-FailMsg "version-cleanup artifact missing: $source"
                    exit 2
                }
                Copy-Item -LiteralPath $source -Destination $binDir -Force
            }
            Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'version-cleanup.ps1') `
                -Destination $binDir -Force
            $workspaceVersion = ([regex]::Match(
                (Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'Cargo.toml')),
                '(?m)^version = "([^"]+)"\r?$')).Groups[1].Value
            if ([string]::IsNullOrWhiteSpace($workspaceVersion)) {
                Write-FailMsg 'version-cleanup build identity unavailable'
                exit 2
            }
            Write-VersionOwnershipManifest -Root $binDir -Version $workspaceVersion | Out-Null
            Write-Ok 'staged build-tree version cleanup fixture manifest'
        }
    }
    Copy-Item (Join-Path $PSScriptRoot 'sandbox-gate-inner.ps1') $binDir -Force
    Install-VersionLifetimeSentinel -Directory $binDir | Out-Null

    # ---- generate the guest watchdog and .wsb ----
    # Sandbox maps host folders onto its default user desktop (WDAGUtilityAccount).
    # The .wsb launches cmd, not the inner PowerShell directly. If that
    # PowerShell hard-crashes, cmd remains alive long enough to request a clean
    # guest shutdown and publish a fail-closed completion signal. The inner
    # runner writes only its private status; cmd is the sole host-signal writer.
    $desk = 'C:\Users\WDAGUtilityAccount\Desktop'
    $argsJoined = ($TestbenchArgs -join ' ')
    $applicationControlArgument =
        Get-SandboxApplicationControlArgument -UseSignedDistribution $useSignedDistribution
    $innerCommand = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File' +
        " `"$desk\bin\sandbox-gate-inner.ps1`"" +
        " -BinDir `"$desk\bin`" -ResDir `"$desk\results`"" +
        " -TestbenchArgs `"$argsJoined`" -TimeoutSec $TimeoutSec" +
        $applicationControlArgument
    $guestPreviousBin = "$desk\previous-bin"
    if ($pairCoexistence) {
        $innerCommand += " -PreviousBinDir `"$guestPreviousBin`""
    }
    Write-Warn 'Sandbox gate: disabling guest Smart App Control for this disposable session (unsigned dev tree or dev-signed distribution)'
    $guestExit = "$desk\results\exitcode.txt"
    $guestExitTemp = "$guestExit.tmp"
    $guestInnerExit = "$desk\results\inner-exitcode.txt"
    $guestShutdownAccepted = "$desk\results\shutdown-accepted.txt"
    $guestShutdownFailed = "$desk\results\shutdown-failed.txt"
    $guestWatchdogLog = "$desk\results\watchdog.log"
    $watchdogPath = Join-Path $binDir 'sandbox-gate-watchdog.cmd'
    @(
        '@echo off',
        'setlocal DisableDelayedExpansion',
        $innerCommand,
        'set "innerRunnerExit=%ERRORLEVEL%"',
        'set "watchdogAttempts=0"',
        ':watchdog_shutdown',
        'set /a watchdogAttempts+=1 >nul',
        '"%SystemRoot%\System32\shutdown.exe" /s /t 5 /f',
        'set "watchdogShutdownExit=%ERRORLEVEL%"',
        'if "%watchdogShutdownExit%"=="0" goto watchdog_publish',
        'if "%watchdogAttempts%"=="3" goto watchdog_exhausted',
        '"%SystemRoot%\System32\ping.exe" -n 2 127.0.0.1 >nul',
        'goto watchdog_shutdown',
        ':watchdog_exhausted',
        ('if exist "{0}" for %%I in ("{0}") do if %%~zI GTR 0 goto watchdog_publish' -f $guestShutdownAccepted),
        ('> "{0}" echo guest shutdown was not accepted after %watchdogAttempts% attempts' -f $guestShutdownFailed),
        ('> "{0}" echo inner exit=%innerRunnerExit%; shutdown exit=%watchdogShutdownExit%; attempts=%watchdogAttempts%' -f $guestWatchdogLog),
        'exit /b 2',
        ':watchdog_publish',
        ('> "{0}" echo inner exit=%innerRunnerExit%; shutdown exit=%watchdogShutdownExit%; attempts=%watchdogAttempts%' -f $guestWatchdogLog),
        ('if exist "{0}" copy /y "{0}" "{1}" >nul' -f $guestInnerExit, $guestExitTemp),
        ('if not exist "{0}" > "{0}" echo 2' -f $guestExitTemp),
        ('for %%I in ("{0}") do if %%~zI LEQ 0 > "{0}" echo 2' -f $guestExitTemp),
        ('move /y "{0}" "{1}" >nul' -f $guestExitTemp, $guestExit),
        'exit /b 2'
    ) | Set-Content -LiteralPath $watchdogPath -Encoding ascii
    $logon = "cmd.exe /d /c $desk\bin\sandbox-gate-watchdog.cmd"
    # XML-escape host paths and the deliberately simple LogonCommand.
    $escBin   = [System.Security.SecurityElement]::Escape($binDir)
    $escRes   = [System.Security.SecurityElement]::Escape($resDir)
    $escLogon = [System.Security.SecurityElement]::Escape($logon)
    $wsbPath = Join-Path $root 'gate.wsb'
    $mappedFolders = @(
        "    <MappedFolder><HostFolder>$escBin</HostFolder><ReadOnly>true</ReadOnly></MappedFolder>",
        "    <MappedFolder><HostFolder>$escRes</HostFolder><ReadOnly>false</ReadOnly></MappedFolder>"
    )
    if ($pairCoexistence) {
        $escPreviousBin = [System.Security.SecurityElement]::Escape($previousBinDir)
        $escGuestPreviousBin = [System.Security.SecurityElement]::Escape($guestPreviousBin)
        $mappedFolders += "    <MappedFolder><HostFolder>$escPreviousBin</HostFolder>" +
            "<SandboxFolder>$escGuestPreviousBin</SandboxFolder><ReadOnly>true</ReadOnly></MappedFolder>"
    }
    @(
        '<Configuration>',
        '  <MappedFolders>',
        $mappedFolders,
        '  </MappedFolders>',
        '  <Networking>Default</Networking>',
        "  <LogonCommand><Command>$escLogon</Command></LogonCommand>",
        '</Configuration>'
    ) | Set-Content -Path $wsbPath -Encoding utf8

    if ($useSignedDistribution) {
        try {
            # The manifest is host-only.  Verify the mapped product tree after
            # every gate-owned file has been added and immediately before the
            # Sandbox launch; the guest never receives this evidence file.
            $preLaunchPayload = Assert-SandboxPayloadManifest -StagedRoot $binDir `
                -ManifestPath $payloadManifest -GateOwnedRelativePaths $payloadGateOwnedPaths
            Write-Ok ("signed product payload revalidated before Sandbox launch: {0} files" -f
                $preLaunchPayload.FileCount)
            if ($pairCoexistence) {
                $preLaunchPreviousPayload = Assert-SandboxPayloadManifest `
                    -StagedRoot $previousBinDir `
                    -ManifestPath $previousPayloadManifest `
                    -GateOwnedRelativePaths @()
                Write-Ok ("previous signed payload revalidated before Sandbox launch: {0} files" -f
                    $preLaunchPreviousPayload.FileCount)
            }
        } catch {
            Write-FailMsg "signed product payload pre-launch revalidation failed: $($_.Exception.Message)"
            exit 2
        }
    }

    $sandboxExit = Join-Path $resDir 'exitcode.txt'
    $sandboxShutdownFailed = Join-Path $resDir 'shutdown-failed.txt'
    if (Test-Path -LiteralPath $sandboxExit) {
        throw "fresh run-owned result path unexpectedly exists: $sandboxExit"
    }
    Write-Step "launching Windows Sandbox (results: $resDir)"
    if ($SandboxCli) {
        $cli = Get-Command wsb.exe -ErrorAction Stop
        $sandboxId = [Guid]::NewGuid().ToString()
        $startResult = & $cli.Source start --id $sandboxId --config ([IO.File]::ReadAllText($wsbPath)) --raw
        $startExit = $LASTEXITCODE
        $startResult | Set-Content -LiteralPath (Join-Path $root 'sandbox-cli-start.json') -Encoding utf8
        if ($startExit -ne 0) {
            Write-FailMsg "Sandbox CLI start failed ($startExit); session id $sandboxId"
            exit 2
        }
        Write-Step "connecting Sandbox CLI session $sandboxId"
        $p = Start-Process -FilePath $cli.Source -ArgumentList @('connect', '--id', $sandboxId, '--raw') -WindowStyle Hidden -PassThru
    } else {
        $p = Start-Process -FilePath $wsbExe -ArgumentList "`"$wsbPath`"" -PassThru
    }

    # Completion signal = exitcode.txt appearing WITH CONTENT in the writable map
    # (the outer watchdog publishes it LAST). Boot budget: sandbox cold start can take
    # ~60s. NOTE: do NOT break on $p.HasExited - WindowsSandbox.exe is a LAUNCHER
    # that exits right after handing off (observed live: the sandbox kept running
    # under WindowsSandboxRemoteSession/Server while the launcher was gone at 64s).
    $limitMs = ($TimeoutSec + 240) * 1000
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $limitMs) {
        if ((Test-Path $sandboxExit) -and ((Get-Item $sandboxExit).Length -gt 0)) { break }
        if ((Test-Path $sandboxShutdownFailed) -and
            ((Get-Item $sandboxShutdownFailed).Length -gt 0)) { break }
        Start-Sleep -Seconds 2
    }
    if ((Test-Path $sandboxShutdownFailed) -and
        ((Get-Item $sandboxShutdownFailed).Length -gt 0)) {
        $shutdownFailure = Get-Content -LiteralPath $sandboxShutdownFailed -Raw `
            -ErrorAction SilentlyContinue
        Write-FailMsg "sandbox guest teardown was not confirmed: $shutdownFailure"
        Write-Host '  The sandbox was NOT killed. Close its window normally before retrying.' `
            -ForegroundColor Yellow
        exit 2
    }
    if (-not ((Test-Path $sandboxExit) -and ((Get-Item $sandboxExit).Length -gt 0))) {
        # Never force-kill Windows Sandbox from the host. Doing so corrupts the
        # host session table (observed: the next launch fails with "the remote
        # environment is logged off" / "too many sessions"). Leave a stuck VM
        # available for normal UI closure or inner shutdown and fail this run.
        Write-FailMsg ("sandbox gate produced no results within {0:f0}s (boot+gate). See {1}" -f $sw.Elapsed.TotalSeconds, $resDir)
        Write-Host  '  The sandbox was NOT killed. Close it normally, or wait for its inner shutdown.' -ForegroundColor Yellow
        exit 2
    }
    Start-Sleep -Seconds 3   # let the mapped-folder writes settle

    $gateOut = Join-Path $resDir 'gate.out'
    $exit = 0
    $rawExit = Get-Content $sandboxExit -Raw -ErrorAction SilentlyContinue
    if ([string]::IsNullOrWhiteSpace($rawExit)) { $exit = 2 }
    else { try { $exit = [int]$rawExit.Trim() } catch { $exit = 2 } }
    # Double fail-closed: exit=0 without gate.out means the inner runner never
    # really ran the gate (e.g. a spawn failure reported the wrong code).
    if ($exit -eq 0 -and -not (Test-Path $gateOut)) {
        Write-Warn 'exitcode=0 but gate.out missing — treating as error (fail-closed).'
        $exit = 2
    }
    $gateLines = if (Test-Path $gateOut) { @(Get-Content $gateOut -ErrorAction SilentlyContinue) } else { @() }
    $summary = Get-GateSummary -Lines $gateLines
    if ($useSignedDistribution) {
        try {
            # exitcode.txt is the watchdog's accepted completion signal.  A
            # second host-side check at that point proves the product evidence
            # remained unchanged throughout the guest run.
            $postResultPayload = Assert-SandboxPayloadManifest -StagedRoot $binDir `
                -ManifestPath $payloadManifest -GateOwnedRelativePaths $payloadGateOwnedPaths
            Write-Ok ("signed product payload revalidated after Sandbox result acceptance: {0} files" -f
                $postResultPayload.FileCount)
            # This stable host-only line is the release procedure's evidence
            # capture contract. It is emitted only after post-result revalidation.
            Write-Ok ("signed distribution manifest (host-only): {0}" -f $payloadManifest)
            if ($pairCoexistence) {
                $postPreviousPayload = Assert-SandboxPayloadManifest `
                    -StagedRoot $previousBinDir `
                    -ManifestPath $previousPayloadManifest `
                    -GateOwnedRelativePaths @()
                Write-Ok ("previous signed payload revalidated after Sandbox result acceptance: {0} files" -f
                    $postPreviousPayload.FileCount)
            }
        } catch {
            Write-FailMsg "signed product payload post-result revalidation failed: $($_.Exception.Message)"
            $exit = 2
        }
    }
    # Fail-closed, BEFORE the summary so the displayed exit matches the final
    # one: normalize exit 0 + zero parsed rows -> 2, exit 0 + FAIL/ERROR rows
    # -> 1. Nonzero child exits are preserved as-is.
    $exit = Resolve-GateExit -Summary $summary -Exit $exit
    if (Test-IsScenarioInvocation -ArgumentList $TestbenchArgs) {
        $scenarioEvidence = Test-ScenarioIdSet -Summary $summary
        if (-not $scenarioEvidence.Ok) {
            Write-Warn "scenario evidence invalid: $($scenarioEvidence.Why)"
            $exit = 2
        }
    }
    Write-GateSummary -Summary $summary -Exit $exit -OutFile $gateOut
    if ($OutFile -and (Test-Path -LiteralPath $gateOut -PathType Leaf)) {
        Copy-Item -LiteralPath $gateOut -Destination $OutFile -Force
    }
    # The guest watchdog requests shutdown before publishing exitcode.txt. Do not
    # kill the VM processes here: force-killing
    # corrupts the host session table (observed: next launch fails with
    # "the remote environment is logged off" / "too many sessions").
    exit $exit
}

# ----------------------------------------------------------------------------
# main flow
# ----------------------------------------------------------------------------

if (-not $Testbench) {
    $Testbench = Join-Path (Split-Path $PSScriptRoot -Parent) 'target\release\testbench.exe'
}
if (-not (Test-Path -LiteralPath $Testbench -PathType Leaf)) {
    Write-FailMsg "testbench not found: $Testbench"
    exit 2
}
if (-not [string]::IsNullOrWhiteSpace($DistributionDir) -and -not $Sandbox) {
    Write-FailMsg '-DistributionDir is only valid with -Sandbox (the signed payload gate).'
    exit 2
}
if ($SandboxCli -and -not $Sandbox) {
    Write-FailMsg '-SandboxCli is only valid with -Sandbox.'
    exit 2
}
if (-not [string]::IsNullOrWhiteSpace($PreviousDistributionDir) -and -not $Sandbox) {
    Write-FailMsg '-PreviousDistributionDir is only valid with -Sandbox.'
    exit 2
}
$TimeoutSec = Resolve-SandboxTimeoutSec -RequestedTimeoutSec $TimeoutSec `
    -Sandbox:$Sandbox -TestbenchArgs $TestbenchArgs `
    -TimeoutExplicit:$PSBoundParameters.ContainsKey('TimeoutSec')
$run = New-RunOwnedDirectory -Root (Join-Path $env:TEMP 'nospacekey-gate')
$outDir = $run.Path
$outFile = Join-Path $outDir 'gate.out'
$errFile = Join-Path $outDir 'gate.err'

if ($Sandbox) {
    # Sandbox 内の入力はホストから完全隔離 → foreground trap が原理的に起きない。
    # エンジン kill も不要（sandbox は使い捨てで、常にクリーンな登録から始まる）。
    try {
        Invoke-SandboxGate -Testbench $Testbench -DistributionDir $DistributionDir `
            -PreviousDistributionDir $PreviousDistributionDir -SandboxCli:$SandboxCli `
            -TestbenchArgs $TestbenchArgs `
            -TimeoutSec $TimeoutSec -OutFile $outFile
    } catch {
        Write-FailMsg "sandbox gate infrastructure failure: $($_.Exception.Message)"
        exit 2
    }
    # Invoke-SandboxGate は戻らない（ゲートの exit code で終了する）。
}

if ($WaitIdle) {
    $idle = Wait-UserIdle -IdleSeconds $IdleSeconds
    if (-not $idle) { exit 2 }
}

if (-not $ReuseExistingEngine) {
    # stale engine（前回実行の env を引き継いだ常駐）を残したままゲートを回さない:
    # stop 失敗はインフラエラーとして exit 2（fail-closed）。
    if (-not (Stop-EngineHost)) { Write-FailMsg 'pre-gate engine stop failed (stale engine remains)'; exit 2 }
    Start-Sleep -Milliseconds 300
}

try {
    $r = Invoke-Testbench -Testbench $Testbench -ArgumentList $TestbenchArgs `
        -OutFile $outFile -ErrFile $errFile -TimeoutSec $TimeoutSec
} catch {
    Write-FailMsg "$($_.Exception.Message)"
    # runner 例外（起動失敗 / WaitForExit・ExitCode 取得失敗）でも stale engine を
    # 残さない（timeout 経路と同規律）。停止失敗は表示のみで exit 2 は維持。
    if (-not $ReuseExistingEngine -and -not (Stop-EngineHost)) {
        Write-FailMsg 'engine stop after runner exception also failed (stale engine remains)'
    }
    exit 2
}
if ($r.TimedOut) {
    if ($r.KillConfirmed) {
        Write-FailMsg "testbench did not finish in ${TimeoutSec}s (kill confirmed). Output: $outFile"
    } else {
        Write-FailMsg "testbench did not finish in ${TimeoutSec}s and its termination could not be confirmed. Output: $outFile"
    }
    if (-not $ReuseExistingEngine -and -not (Stop-EngineHost)) {
        Write-FailMsg 'engine stop after timeout also failed (stale engine remains)'
    }
    exit 2
}
$exit = $r.ExitCode
if (-not $ReuseExistingEngine -and -not (Stop-EngineHost)) {
    Write-FailMsg 'post-run engine stop failed (stale engine remains) — infrastructure error'
    exit 2
}

$lines = @(Get-Content $outFile -ErrorAction SilentlyContinue)
$summary = Get-GateSummary -Lines $lines
# fail-closed（Sandbox 経路と同じ規律）: 子が exit 0 を主張していても、解析行ゼロ（→2）
# や FAIL/ERROR 行（→1）と矛盾するなら正規化する。非0 の子 exit はそのまま維持。
# Write-GateSummary の前に置き、表示する exit と最終 exit を一致させる。
$exit = Resolve-GateExit -Summary $summary -Exit $exit
if (Test-IsScenarioInvocation -ArgumentList $TestbenchArgs) {
    $scenarioEvidence = Test-ScenarioIdSet -Summary $summary
    if (-not $scenarioEvidence.Ok) {
        Write-Warn "scenario evidence invalid: $($scenarioEvidence.Why)"
        $exit = 2
    }
}
Write-GateSummary -Summary $summary -Exit $exit -OutFile $outFile
exit $exit
