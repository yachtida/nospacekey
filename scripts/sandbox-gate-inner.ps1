<#
sandbox-gate-inner.ps1 - runs INSIDE Windows Sandbox.

Windows PowerShell 5.1 + ASCII-only on purpose: the sandbox image ships
WITHOUT pwsh 7, and 5.1 reads BOM-less files as the system ANSI codepage.
This is the ONLY 5.1-compatible script in scripts\ (everything else is
#Requires -Version 7.3).

Contract (invoked by the cmd watchdog launched from the .wsb LogonCommand
generated in run-gate.ps1 -Sandbox):
  0. For a build-tree development run only, disable Smart App Control in this
     disposable guest and reload Code Integrity policy. Signed distribution
     runs never request this bypass and keep the guest policy enforced.
  1. Copy the entire mapped BinDir into a GUID-named guest-local %TEMP% tree.
     Reject reparse points, missing/extra paths, file-count mismatches, copy
     failures, and SHA-256 mismatches before registration. The mapped folder
     remains the source of truth; all product execution uses the local copy.
  2. Register the local TIP DLL (the mapped folder is never registered).
  3. Run testbench with the SAFE SPAWN discipline - stdout/stderr to files,
     WaitForExit(timeout) on the process itself. NEVER pipe testbench output
     and NEVER use Start-Process -Wait: the --persist engine inherits the
     stdout handle and hangs the reader (see run-gate.ps1 / CLAUDE.md).
  3b. After the gate process exits, parse gate.out result rows (exact
     scenario/smoke/bare PASS|FAIL|ERROR status fields - the same regexes as
     Get-GateSummary in scripts\test-lib.ps1, duplicated inline because the
     sandbox image has no pwsh 7) and normalize the exit code BEFORE the
     unregister / log copy / exitcode steps:
       exit 0 + zero parsed rows        -> 2 (a zero exit is a claim, not
       exit 0 + any FAIL/ERROR row      -> 1  evidence; rows decide).
     Non-zero gate exits pass through unchanged.
  4. ONE completion path: staging/registration failure, missing testbench, spawn
     failure, timeout, or any unexpected exception only sets the exit state
     and falls into the same finally - no early exit, and no result write
     outside the finally.
  5. The finally unregisters the TIP when a regsvr32 registration was
     attempted (checked - any /u failure forces 2), copies the engine logs
     into the results map (copy failure forces 2), requests shutdown.exe
      /s /t 5 /f on EVERY path and CHECKS the
     request: shutdown.exe exits right after the request is accepted or
     rejected (never waiting the /t grace), so its non-zero exit means the
      sandbox will not close - launch failure OR rejection forces 2. Then it
      writes inner-exitcode.txt. The outer cmd watchdog retries shutdown and is
      the sole publisher of the host-visible exitcode.txt completion signal.
     logs the final status, and only then writes exitcode.txt LAST - exactly
     one result write; its appearance is the host's completion signal, and
     no Log or result write may follow it.
#>
param(
    [Parameter(Mandatory)][string]$BinDir,
    [Parameter(Mandatory)][string]$ResDir,
    [string]$TestbenchArgs = '--scenarios',
    [int]$TimeoutSec = 240,
    [switch]$DisableApplicationControlForDevelopment,
    [string]$PreviousBinDir = ''
)
$ErrorActionPreference = 'Continue'

$mappedBinDir = $BinDir
$mappedPreviousBinDir = $PreviousBinDir
$guestLocalRoot = $null
$guestBinDir = $null
$tipDll    = $null
$testbench = $null
$outFile   = Join-Path $ResDir 'gate.out'
$errFile   = Join-Path $ResDir 'gate.err'
$innerExitFile = Join-Path $ResDir 'inner-exitcode.txt'
$shutdownAcceptedFile = Join-Path $ResDir 'shutdown-accepted.txt'
$innerLog  = Join-Path $ResDir 'inner.log'
$systemDir = [Environment]::SystemDirectory
$regsvr32  = Join-Path $systemDir 'regsvr32.exe'
$regExe = Join-Path $systemDir 'reg.exe'
$ciToolExe = Join-Path $systemDir 'CiTool.exe'
$ctfmonExe = Join-Path $systemDir 'ctfmon.exe'
$taskkillExe = Join-Path $systemDir 'taskkill.exe'
$shutdownExe = Join-Path $systemDir 'shutdown.exe'
$powershellExe = Join-Path $systemDir 'WindowsPowerShell\v1.0\powershell.exe'

function Log([string]$t) {
    Write-Host "[inner] $t"
    Add-Content -Path $innerLog -Value ("[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss'), $t)
}

function Get-GuestRelativePath {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][string]$FullPath
    )
    $rootFull = [IO.Path]::GetFullPath($Root)
    $full = [IO.Path]::GetFullPath($FullPath)
    if ($rootFull.Length -gt 3) { $rootFull = $rootFull.TrimEnd('\') }
    $prefix = $rootFull + '\'
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "path escaped root: $FullPath"
    }
    return $full.Substring($prefix.Length).Replace('\', '/')
}

# Enumerate every directory and regular file below a root. Reparse points are
# rejected instead of followed: a mapped junction could escape the intended
# payload and make the inventory/hash evidence meaningless.
function Get-GuestTreeInventory {
    param([Parameter(Mandatory)][string]$Root)
    $rootItem = Get-Item -LiteralPath $Root -Force -ErrorAction Stop
    if (-not $rootItem.PSIsContainer) { throw "tree root is not a directory: $Root" }
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "reparse point is not allowed at tree root: $Root"
    }
    $rootFull = [IO.Path]::GetFullPath($rootItem.FullName)
    $pending = New-Object 'System.Collections.Generic.Stack[string]'
    $entries = New-Object 'System.Collections.Generic.List[object]'
    $pending.Push($rootFull)
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        foreach ($item in @(Get-ChildItem -LiteralPath $current -Force -ErrorAction Stop)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "reparse point is not allowed in tree: $($item.FullName)"
            }
            $entries.Add([pscustomobject]@{
                RelativePath = Get-GuestRelativePath -Root $rootFull -FullPath $item.FullName
                FullPath = [IO.Path]::GetFullPath($item.FullName)
                IsDirectory = [bool]$item.PSIsContainer
            })
            if ($item.PSIsContainer) { $pending.Push([IO.Path]::GetFullPath($item.FullName)) }
        }
    }
    return @($entries.ToArray() | Sort-Object RelativePath)
}

function Copy-GuestTree {
    param(
        [Parameter(Mandatory)][string]$SourceRoot,
        [Parameter(Mandatory)][string]$DestinationRoot
    )
    $sourceItem = Get-Item -LiteralPath $SourceRoot -Force -ErrorAction Stop
    if (-not $sourceItem.PSIsContainer) { throw "mapped BinDir is not a directory: $SourceRoot" }
    if (($sourceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "reparse point is not allowed at mapped BinDir: $SourceRoot"
    }
    if (Test-Path -LiteralPath $DestinationRoot) {
        throw "guest-local destination already exists: $DestinationRoot"
    }
    [void][IO.Directory]::CreateDirectory($DestinationRoot)
    $pending = New-Object 'System.Collections.Generic.Stack[string]'
    $pending.Push([IO.Path]::GetFullPath($SourceRoot))
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        foreach ($item in @(Get-ChildItem -LiteralPath $current -Force -ErrorAction Stop)) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "reparse point encountered while copying: $($item.FullName)"
            }
            $relative = Get-GuestRelativePath -Root $SourceRoot -FullPath $item.FullName
            $destination = Join-Path $DestinationRoot ($relative -replace '/', '\')
            if ($item.PSIsContainer) {
                [void][IO.Directory]::CreateDirectory($destination)
                $pending.Push([IO.Path]::GetFullPath($item.FullName))
            } else {
                $parent = Split-Path -Parent $destination
                if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
                    [void][IO.Directory]::CreateDirectory($parent)
                }
                try {
                    Copy-Item -LiteralPath $item.FullName -Destination $destination `
                        -Force -ErrorAction Stop
                } catch {
                    throw "copy failed for $relative`: $($_.Exception.Message)"
                }
            }
        }
    }
}

function Copy-AndVerify-GuestLocalBin {
    param([Parameter(Mandatory)][string]$SourceRoot)
    $sourceBefore = @(Get-GuestTreeInventory -Root $SourceRoot)
    $token = [Guid]::NewGuid().ToString('N')
    $localRoot = Join-Path $env:TEMP ("nospacekey-sandbox-gate-" + $token)
    $localBin = Join-Path $localRoot 'bin'
    Copy-GuestTree -SourceRoot $SourceRoot -DestinationRoot $localBin
    $sourceAfter = @(Get-GuestTreeInventory -Root $SourceRoot)
    $destination = @(Get-GuestTreeInventory -Root $localBin)

    if ($sourceBefore.Count -ne $sourceAfter.Count) {
        throw "mapped BinDir changed during copy (before=$($sourceBefore.Count) after=$($sourceAfter.Count))"
    }
    if ($sourceAfter.Count -ne $destination.Count) {
        throw "tree path count mismatch (source=$($sourceAfter.Count) destination=$($destination.Count))"
    }

    $sourceByPath = @{}
    $destinationByPath = @{}
    foreach ($entry in $sourceAfter) {
        if ($sourceByPath.ContainsKey($entry.RelativePath)) {
            throw "duplicate source relative path: $($entry.RelativePath)"
        }
        $sourceByPath[$entry.RelativePath] = $entry
    }
    foreach ($entry in $destination) {
        if ($destinationByPath.ContainsKey($entry.RelativePath)) {
            throw "duplicate destination relative path: $($entry.RelativePath)"
        }
        $destinationByPath[$entry.RelativePath] = $entry
    }
    foreach ($relative in $sourceByPath.Keys) {
        if (-not $destinationByPath.ContainsKey($relative)) {
            throw "destination is missing relative path: $relative"
        }
        if ([bool]$sourceByPath[$relative].IsDirectory -ne
            [bool]$destinationByPath[$relative].IsDirectory) {
            throw "source/destination type mismatch: $relative"
        }
    }
    foreach ($relative in $destinationByPath.Keys) {
        if (-not $sourceByPath.ContainsKey($relative)) {
            throw "destination has extra relative path: $relative"
        }
    }

    $sourceFiles = @($sourceAfter | Where-Object { -not $_.IsDirectory })
    $destinationFiles = @($destination | Where-Object { -not $_.IsDirectory })
    if ($sourceFiles.Count -ne $destinationFiles.Count) {
        throw "normal file count mismatch (source=$($sourceFiles.Count) destination=$($destinationFiles.Count))"
    }
    foreach ($entry in $sourceFiles) {
        $destinationEntry = $destinationByPath[$entry.RelativePath]
        $sourceHash = (Get-FileHash -LiteralPath $entry.FullPath -Algorithm SHA256 `
            -ErrorAction Stop).Hash.ToUpperInvariant()
        $destinationHash = (Get-FileHash -LiteralPath $destinationEntry.FullPath `
            -Algorithm SHA256 -ErrorAction Stop).Hash.ToUpperInvariant()
        if ($sourceHash -ne $destinationHash) {
            throw "SHA-256 mismatch: $($entry.RelativePath)"
        }
    }
    Log ("guest-local tree verified: paths={0} normal_files={1} root={2}" -f
        $sourceAfter.Count, $sourceFiles.Count, $localBin)
    return [pscustomobject]@{ Root = $localRoot; Bin = $localBin }
}

# Parse gate.out result rows with the SAME exact-match regexes as
# Get-GateSummary in scripts\test-lib.ps1 (scenario table rows, smoke rows,
# bare token rows; strict PASS|FAIL|ERROR status - SKIP/PASSED/etc. never
# count). Read UTF-8 explicitly: PS 5.1 otherwise uses the guest ANSI code page,
# whose decoder can consume the ASCII space after an emoji as a trail byte and
# erase the required separator before PASS/FAIL/ERROR.
# Returns parsed/failed counts plus numeric scenario IDs.
function Get-GateOutCounts([string]$Path) {
    $counts = @{ Parsed = 0; Failed = 0; ScenarioIds = @() }
    if (-not (Test-Path -LiteralPath $Path)) { return $counts }
    $scenarioRe = '^\s*(\d+|!)\s*\|\s*(?:[^\s|]+\s+)?(PASS|FAIL|ERROR)\s*\|'
    $smokeRe    = '^\S.*\s:\s(PASS|FAIL|ERROR)(?:\s|\(|$)'
    $bareRe     = '^\S+\s(PASS|FAIL|ERROR)\s*$'
    foreach ($line in @(Get-Content -LiteralPath $Path -Encoding UTF8 `
            -ErrorAction SilentlyContinue)) {
        if (-not $line) { continue }
        $status = $null
        if ($line -match $scenarioRe) {
            $key = $Matches[1]
            $status = $Matches[2]
            if ($key -ne '!') { $counts.ScenarioIds += [int]$key }
        }
        elseif ($line -match $smokeRe)    { $status = $Matches[1] }
        elseif ($line -match $bareRe)     { $status = $Matches[1] }
        if (-not $status) { continue }
        $counts.Parsed++
        if ($status -ne 'PASS') { $counts.Failed++ }
    }
    return $counts
}

function Get-NormalizedGateExit([int]$RawExit, $Counts) {
    if ($RawExit -ne 0) { return $RawExit }
    if ($null -eq $Counts -or $Counts.Parsed -eq 0) { return 2 }
    if ($Counts.Failed -gt 0) { return 1 }
    return 0
}

function Test-ExactScenarioIds($Ids) {
    # Retired scenario IDs (12, 16, 30, 41) were removed from the canonical set.
    $retired = @(12, 16, 30, 41)
    $idsArray = @($Ids)
    if ($idsArray.Count -ne 48) { return $false }
    $seen = New-Object 'bool[]' 53
    foreach ($id in $idsArray) {
        $n = [int]$id
        if ($n -lt 1 -or $n -gt 52 -or $retired -contains $n -or $seen[$n]) { return $false }
        $seen[$n] = $true
    }
    for ($n = 1; $n -le 52; $n++) { if (-not $seen[$n] -and -not ($retired -contains $n)) { return $false } }
    return $true
}

function Test-IsScenarioArgs($ArgsList) {
    $items = @($ArgsList)
    return $items.Count -gt 0 -and $items[0] -eq '--scenarios'
}



function Reset-SandboxCleanupTrace([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    try {
        Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
    } catch {
        if (Test-Path -LiteralPath $Path) {
            throw "cleanup trace reset failed: $($_.Exception.Message)"
        }
        return
    }
    if (Test-Path -LiteralPath $Path) { throw 'cleanup trace reset failed: stale trace remains' }
}

function New-SandboxCleanupFixtureRoot {
    $name = 'nsc-' + [Guid]::NewGuid().ToString('N').Substring(0, 12)
    $root = Join-Path ([IO.Path]::GetTempPath()) $name
    $item = New-Item -ItemType Directory -Path $root -ErrorAction Stop
    $temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    $parent = [IO.Path]::GetFullPath($item.Parent.FullName).TrimEnd('\')
    if (-not $parent.Equals($temp, [StringComparison]::OrdinalIgnoreCase) -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw 'cleanup fixture root ownership validation failed'
    }
    return $item.FullName
}

function Test-SandboxCleanupFixturePathBudget([string]$Root, $Manifest) {
    $quarantine = Join-Path (Join-Path $Root 'versions') ('.cleanup-' + ('0' * 32))
    $maximum = $quarantine.Length
    foreach ($file in @($Manifest.Files)) {
        $candidate = Join-Path $quarantine ([string]$file.Path).Replace('/', '\')
        if ($candidate.Length -gt $maximum) { $maximum = $candidate.Length }
    }
    if ($maximum -ge 248) { throw "cleanup fixture path budget exceeded: $maximum" }
    return $maximum
}

function Remove-SandboxCleanupFixtureRoot([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    $parent = [IO.Path]::GetFullPath([IO.Path]::GetDirectoryName($full)).TrimEnd('\')
    $item = Get-Item -LiteralPath $full -Force -ErrorAction Stop
    if (-not $parent.Equals($temp, [StringComparison]::OrdinalIgnoreCase) -or
        $item.Name -notmatch '\Ansc-[0-9a-f]{12}\z' -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw 'cleanup fixture teardown ownership validation failed'
    }
    Remove-Item -LiteralPath $full -Recurse -Force -ErrorAction Stop
    if (Test-Path -LiteralPath $full) { throw 'cleanup fixture teardown did not remove its owned root' }
}

function Invoke-BoundedSandboxProcess {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$ArgumentList = @(),
        [int]$TimeoutMs = 10000,
        [switch]$NoNewWindow,
        [switch]$CaptureOutput
    )
    $process = $null
    $killer = $null
    $stdoutTask = $null
    $stderrTask = $null
    $stdout = ''
    $stderr = ''
    $launched = $false
    $timedOut = $false
    $killConfirmed = $true
    $exitCode = 2
    $failure = ''
    try {
        if ($CaptureOutput) {
            $start = [Diagnostics.ProcessStartInfo]::new()
            $start.FileName = $FilePath
            # Callers already quote whitespace-bearing arguments for Start-Process.
            $start.Arguments = [string]::Join(' ', @($ArgumentList))
            $start.UseShellExecute = $false
            $start.CreateNoWindow = $true
            $start.RedirectStandardOutput = $true
            $start.RedirectStandardError = $true
            $process = [Diagnostics.Process]::new()
            $process.StartInfo = $start
            if (-not $process.Start()) { throw 'process start returned false' }
            $stdoutTask = $process.StandardOutput.ReadToEndAsync()
            $stderrTask = $process.StandardError.ReadToEndAsync()
        } else {
            $startArgs = @{
                FilePath = $FilePath
                ArgumentList = $ArgumentList
                PassThru = $true
                ErrorAction = 'Stop'
            }
            if ($NoNewWindow) { $startArgs.NoNewWindow = $true }
            else { $startArgs.WindowStyle = 'Hidden' }
            $process = Start-Process @startArgs
        }
        $launched = $true
        if ($process.WaitForExit($TimeoutMs)) {
            $exitCode = $process.ExitCode
        } else {
            $timedOut = $true
            $killConfirmed = $false
            try {
                $killer = Start-Process -FilePath $taskkillExe `
                    -ArgumentList '/PID', ([string]$process.Id), '/T', '/F' `
                    -PassThru -WindowStyle Hidden -ErrorAction Stop
                if (-not $killer.WaitForExit(5000)) { try { $killer.Kill() } catch { } }
            } catch { $failure = $_.Exception.Message }
            try {
                if (-not $process.HasExited) { $process.Kill() }
                $killConfirmed = $process.WaitForExit(5000) -and $process.HasExited
            } catch { $failure = $_.Exception.Message }
            if (-not $failure) { $failure = "timeout after ${TimeoutMs}ms" }
        }
    } catch {
        $failure = $_.Exception.Message
        if ($null -ne $process) {
            try {
                if (-not $process.HasExited) { $process.Kill() }
                $killConfirmed = $process.WaitForExit(5000) -and $process.HasExited
            } catch { $killConfirmed = $false }
        }
    }
    if ($CaptureOutput) {
        try {
            $captureWait = [Diagnostics.Stopwatch]::StartNew()
            $stdoutComplete = $null -ne $stdoutTask -and $stdoutTask.Wait(5000)
            $stderrWaitMs = [Math]::Max(0, 5000 - [int]$captureWait.ElapsedMilliseconds)
            $stderrComplete = $null -ne $stderrTask -and $stderrTask.Wait($stderrWaitMs)
            if (-not $stdoutComplete -or -not $stderrComplete) {
                throw 'captured output did not close within 5000ms'
            }
            $stdout = $stdoutTask.GetAwaiter().GetResult()
            $stderr = $stderrTask.GetAwaiter().GetResult()
        } catch {
            if (-not $failure) { $failure = "output capture failed: $($_.Exception.Message)" }
        }
    }
    $result = [pscustomobject]@{
        ExitCode = $exitCode
        Launched = $launched
        TimedOut = $timedOut
        KillConfirmed = $killConfirmed
        Failure = $failure
        Stdout = $stdout
        Stderr = $stderr
    }
    if ($null -ne $killer) { $killer.Dispose() }
    if ($null -ne $process) { $process.Dispose() }
    return $result
}

function Disable-SandboxApplicationControlForDevelopment {
    $policyPsPath = 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy'
    $policyRegPath = 'HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy'
    $valueName = 'VerifiedAndReputablePolicyState'

    try {
        $before = (Get-ItemProperty -LiteralPath $policyPsPath -Name $valueName `
            -ErrorAction Stop).$valueName
    } catch {
        throw "cannot read guest application-control state: $($_.Exception.Message)"
    }
    if ($before -eq 0) {
        Log 'development application-control bypass already active'
        return
    }

    Log "development gate: disabling guest application control (state=$before)"
    $write = Invoke-BoundedSandboxProcess -FilePath $regExe -ArgumentList @(
        'add', $policyRegPath, '/v', $valueName, '/t', 'REG_DWORD', '/d', '0', '/f') `
        -TimeoutMs 10000
    if (-not $write.Launched -or $write.TimedOut -or $write.ExitCode -ne 0) {
        throw ("guest application-control policy update failed: launched={0} timedOut={1} exit={2} failure={3}" -f
            $write.Launched, $write.TimedOut, $write.ExitCode, $write.Failure)
    }

    $reload = Invoke-BoundedSandboxProcess -FilePath $ciToolExe `
        -ArgumentList @('-r') -TimeoutMs 30000 -NoNewWindow
    if (-not $reload.Launched -or $reload.TimedOut -or -not $reload.KillConfirmed) {
        throw ("guest application-control policy reload failed: launched={0} timedOut={1} exit={2} failure={3}" -f
            $reload.Launched, $reload.TimedOut, $reload.ExitCode, $reload.Failure)
    }
    if ($null -ne $reload.ExitCode -and $reload.ExitCode -ne 0) {
        throw "guest application-control policy reload failed: exit=$($reload.ExitCode)"
    }
    Start-Sleep -Seconds 2

    try {
        $after = (Get-ItemProperty -LiteralPath $policyPsPath -Name $valueName `
            -ErrorAction Stop).$valueName
    } catch {
        throw "cannot verify guest application-control state: $($_.Exception.Message)"
    }
    if ($after -ne 0) {
        throw "guest application-control bypass was not applied (state=$after)"
    }
    Log 'development application-control bypass active for this disposable guest'
}

function Restart-SandboxTextServicesForDevelopment {
    Log 'development gate: refreshing text services after application-control change'
    # Kill by generation, not by disappearance: the guest's TSF watchdog
    # respawns ctfmon immediately, so "wait until no ctfmon exists" can never
    # succeed (observed: taskkill timed out at 10s and 30s, then a Stop-Process
    # disappearance poll timed out the same way -- v1.5.0 freeze). What the
    # refresh actually needs is that every pre-policy-change ctfmon is gone and
    # a post-change one is running. Stop-Process (.NET Process.Kill) is used
    # in-process; every wait below stays bounded and fail-closed.
    $oldIds = @(Get-Process -Name 'ctfmon' -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Id })
    foreach ($p in @(Get-Process -Name 'ctfmon' -ErrorAction SilentlyContinue)) {
        Stop-Process -InputObject $p -Force -ErrorAction SilentlyContinue
    }
    $killDeadline = [DateTime]::UtcNow.AddSeconds(30)
    while ($true) {
        $oldAlive = @(Get-Process -Name 'ctfmon' -ErrorAction SilentlyContinue |
            Where-Object { $oldIds -contains $_.Id })
        if ($oldAlive.Count -eq 0) { break }
        if ([DateTime]::UtcNow -ge $killDeadline) {
            throw ("guest text-service stop failed: pre-change ctfmon pids still running after 30s: {0}" -f
                (($oldAlive | ForEach-Object { $_.Id }) -join ','))
        }
        foreach ($p in $oldAlive) {
            Stop-Process -InputObject $p -Force -ErrorAction SilentlyContinue
        }
        Start-Sleep -Milliseconds 250
    }

    # Wait for the watchdog to respawn a post-change ctfmon; if it does not
    # within 15s, start one ourselves. Either way a running ctfmon is required.
    $respawnDeadline = [DateTime]::UtcNow.AddSeconds(15)
    while (@(Get-Process -Name 'ctfmon' -ErrorAction SilentlyContinue).Count -eq 0) {
        if ([DateTime]::UtcNow -ge $respawnDeadline) {
            try {
                Start-Process -FilePath $ctfmonExe -PassThru -WindowStyle Hidden `
                    -ErrorAction Stop | Out-Null
            } catch {
                throw "guest text-service restart failed: $($_.Exception.Message)"
            }
            break
        }
        Start-Sleep -Milliseconds 250
    }
    Start-Sleep -Seconds 2

    $running = @(Get-Process -Name 'ctfmon' -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) {
        throw 'guest text service did not remain running'
    }
    Log ("development text services refreshed: ctfmon pid={0}" -f
        (($running | ForEach-Object { $_.Id }) -join ','))
}

# Probe the loader without invoking DllRegisterServer. This is diagnostic only:
# a probe failure must never change the gate's existing fail-closed state.
function Invoke-SandboxLoadLibraryProbe {
    param(
        [Parameter(Mandatory)][string]$Path
    )
    $probeScript = Join-Path $guestLocalRoot ("loadlibrary-probe-{0}.ps1" -f
        [Guid]::NewGuid().ToString('N'))
    try {
        @'
param([Parameter(Mandatory)][string]$Path)
$ErrorActionPreference = 'Stop'
try {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class NospacekeySandboxProbeLoader
{
    [DllImport("kernel32.dll", EntryPoint = "LoadLibraryExW", CharSet = CharSet.Unicode, ExactSpelling = true, SetLastError = true)]
    public static extern IntPtr LoadLibraryExW(string fileName, IntPtr file, uint flags);

    [DllImport("kernel32.dll", EntryPoint = "FreeLibrary", ExactSpelling = true, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool FreeLibrary(IntPtr module);
}
"@ -ErrorAction Stop
} catch {
    Write-Output ("loadlibrary_probe_setup_error type={0}" -f $_.Exception.GetType().FullName)
    exit 2
}

foreach ($flags in @(0, 8)) {
    $module = [IntPtr]::Zero
    try {
        $module = [NospacekeySandboxProbeLoader]::LoadLibraryExW(
            $Path, [IntPtr]::Zero, [uint32]$flags)
        if ($module -eq [IntPtr]::Zero) {
            $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
            Write-Output ("loadlibrary_probe flags={0} ok=false win32_error={1}" -f $flags, $errorCode)
        } else {
            Write-Output ("loadlibrary_probe flags={0} ok=true win32_error=0" -f $flags)
        }
    } catch {
        Write-Output ("loadlibrary_probe_error flags={0} type={1}" -f $flags, $_.Exception.GetType().FullName)
    } finally {
        if ($module -ne [IntPtr]::Zero) {
            try {
                $freed = [NospacekeySandboxProbeLoader]::FreeLibrary($module)
                if (-not $freed) {
                    $errorCode = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                    Write-Output ("loadlibrary_probe_free flags={0} ok=false win32_error={1}" -f $flags, $errorCode)
                }
            } catch {
                Write-Output ("loadlibrary_probe_free_error flags={0} type={1}" -f $flags, $_.Exception.GetType().FullName)
            }
        }
    }
}
'@ | Set-Content -LiteralPath $probeScript -Encoding ascii -ErrorAction Stop
        $probe = Invoke-BoundedSandboxProcess -FilePath $powershellExe -ArgumentList @(
            '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File',
            ('"{0}"' -f $probeScript), '-Path', ('"{0}"' -f $Path)) `
            -TimeoutMs 10000 -CaptureOutput
        $stdout = [regex]::Replace(([string]$probe.Stdout), '[\r\n\t]+', ' ').Trim()
        $stderr = [regex]::Replace(([string]$probe.Stderr), '[\r\n\t]+', ' ').Trim()
        if ($stdout.Length -gt 2048) { $stdout = $stdout.Substring(0, 2048) + '<truncated>' }
        if ($stderr.Length -gt 2048) { $stderr = $stderr.Substring(0, 2048) + '<truncated>' }
        Log ("loadlibrary_probe_process launched={0} timedOut={1} exit={2} killConfirmed={3} failure={4} stdout={5} stderr={6}" -f
            $probe.Launched, $probe.TimedOut, $probe.ExitCode, $probe.KillConfirmed,
            $probe.Failure, $stdout, $stderr)
    } catch {
        Log ("loadlibrary_probe_process_error type={0}" -f $_.Exception.GetType().FullName)
    } finally {
        try { Remove-Item -LiteralPath $probeScript -Force -ErrorAction Stop } catch { }
    }
}

# Terminate the generated cmd wrapper and every known gate descendant, then
# confirm they are gone before finally copies logs and publishes exitcode.txt.
# This script runs in a dedicated Sandbox VM, so name-based cleanup cannot hit
# host processes or another gate run.
function Stop-SandboxGateAndConfirm {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Gate,
        [switch]$SkipNamedCleanup
    )
    $ok = $true
    $taskkill = $null
    try {
        $taskkill = Start-Process -FilePath $taskkillExe `
            -ArgumentList '/PID', ([string]$Gate.Id), '/T', '/F' `
            -PassThru -WindowStyle Hidden -ErrorAction Stop
        if (-not $taskkill.WaitForExit(5000)) {
            try { $taskkill.Kill() } catch { }
            $ok = $false
        } elseif ($taskkill.ExitCode -ne 0) {
            Log "taskkill tree exit=$($taskkill.ExitCode)"
            $ok = $false
        }
    } catch {
        Log "taskkill tree failed: $($_.Exception.Message)"
        $ok = $false
    } finally {
        if ($null -ne $taskkill) { $taskkill.Dispose() }
    }

    try {
        if (-not $Gate.HasExited) { $Gate.Kill() }
        if (-not $Gate.WaitForExit(2000) -or -not $Gate.HasExited) { $ok = $false }
    } catch {
        Log "cmd termination check failed: $($_.Exception.Message)"
        $ok = $false
    }

    if (-not $SkipNamedCleanup) {
        foreach ($name in @('testbench', 'NospacekeyEngineHost')) {
            try {
                foreach ($process in @([System.Diagnostics.Process]::GetProcessesByName($name))) {
                    if (-not $process.HasExited) { $process.Kill() }
                    if (-not $process.WaitForExit(2000)) { $ok = $false }
                }
                if (@([System.Diagnostics.Process]::GetProcessesByName($name)).Count -gt 0) {
                    Log "process remains after timeout cleanup: $name"
                    $ok = $false
                }
            } catch {
                Log "process cleanup failed ($name): $($_.Exception.Message)"
                $ok = $false
            }
        }
    }
    return $ok
}

function Get-BoundedCoexistDiagnosticText {
    param(
        [Parameter(Mandatory)][string]$Path,
        [int]$MaxBytes = 512
    )
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return '<missing>' }
    $stream = $null
    try {
        $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read,
            [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
        $length = $stream.Length
        $buffer = New-Object byte[] $MaxBytes
        $headCount = 0
        while ($headCount -lt $buffer.Length) {
            $read = $stream.Read($buffer, $headCount, $buffer.Length - $headCount)
            if ($read -eq 0) { break }
            $headCount += $read
        }
        $head = [Text.Encoding]::UTF8.GetString($buffer, 0, $headCount)
        $head = [regex]::Replace($head, '[\r\n\t]+', ' ')
        $head = [regex]::Replace($head, '[^\x20-\x7E]', '?').Trim()
        if ([string]::IsNullOrEmpty($head)) { $head = '<empty>' }
        if ($length -le $headCount) { return $head }

        $tailStart = [Math]::Max([int64]$headCount, $length - $MaxBytes)
        [void]$stream.Seek($tailStart, [IO.SeekOrigin]::Begin)
        $tailBuffer = New-Object byte[] $MaxBytes
        $tailCount = 0
        while ($tailCount -lt $tailBuffer.Length) {
            $read = $stream.Read($tailBuffer, $tailCount, $tailBuffer.Length - $tailCount)
            if ($read -eq 0) { break }
            $tailCount += $read
        }
        $tail = [Text.Encoding]::UTF8.GetString($tailBuffer, 0, $tailCount)
        $tail = [regex]::Replace($tail, '[\r\n\t]+', ' ')
        $tail = [regex]::Replace($tail, '[^\x20-\x7E]', '?').Trim()
        if ([string]::IsNullOrEmpty($tail)) { $tail = '<empty>' }
        return $head + '<truncated>' + $tail
    } catch {
        return '<unreadable:' + $_.Exception.GetType().Name + '>'
    } finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
}

function Add-CoexistOutputFile {
    param(
        [Parameter(Mandatory)][IO.Stream]$Destination,
        [Parameter(Mandatory)][string]$Source
    )
    $sourceStream = $null
    try {
        $sourceStream = [IO.File]::Open($Source, [IO.FileMode]::Open, [IO.FileAccess]::Read,
            [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
        $remaining = $sourceStream.Length
        $buffer = New-Object byte[] 65536
        while ($remaining -gt 0) {
            $requested = [Math]::Min([int64]$buffer.Length, $remaining)
            $read = $sourceStream.Read($buffer, 0, [int]$requested)
            if ($read -le 0) { throw "source ended before its length snapshot: $Source" }
            $Destination.Write($buffer, 0, $read)
            $remaining -= $read
        }
    } finally {
        if ($null -ne $sourceStream) { $sourceStream.Dispose() }
    }
}

function Get-CoexistReadyFailureEvidence {
    param(
        [Parameter(Mandatory)][ValidateSet('previous', 'current')][string]$Label,
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory)][string]$OutPath,
        [Parameter(Mandatory)][string]$ErrPath
    )
    $initial = 'unknown'
    $exitCode = 'unavailable'
    $termination = 'unconfirmed'
    try {
        $Process.Refresh()
        if ($Process.HasExited) {
            $initial = 'exited'
            $exitCode = [string]$Process.ExitCode
            $termination = 'not-needed'
        } else {
            $initial = 'running'
            $termination = if (Stop-SandboxGateAndConfirm `
                    -Gate $Process -SkipNamedCleanup) { 'confirmed' } else { 'unconfirmed' }
            try {
                $Process.Refresh()
                if ($Process.HasExited) { $exitCode = [string]$Process.ExitCode }
            } catch { }
        }
    } catch {
        $initial = 'unreadable'
        $termination = if (Stop-SandboxGateAndConfirm `
                -Gate $Process -SkipNamedCleanup) { 'confirmed' } else { 'unconfirmed' }
    }
    $stdout = Get-BoundedCoexistDiagnosticText -Path $OutPath
    $stderr = Get-BoundedCoexistDiagnosticText -Path $ErrPath
    return "label=$Label initial=$initial exit=$exitCode termination=$termination stdout=$stdout stderr=$stderr"
}

function Wait-CoexistMarker {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [int]$TimeoutMs = 30000
    )
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while ($watch.ElapsedMilliseconds -lt $TimeoutMs) {
        if ((Test-Path -LiteralPath $Path -PathType Leaf) -and
            (Get-Item -LiteralPath $Path).Length -gt 0) { return $true }
        if ($Process.HasExited) { return $false }
        Start-Sleep -Milliseconds 50
    }
    return $false
}

function Stop-ExactProcessAndConfirm {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [int]$TimeoutMs = 5000
    )
    try {
        if ($Process.HasExited) { return $true }
        $Process.Kill()
        return $Process.WaitForExit($TimeoutMs) -and $Process.HasExited
    } catch {
        try { if ($Process.HasExited) { return $true } } catch { }
        Log "exact process termination failed: $($_.Exception.Message)"
        return $false
    }
}

function Wait-CoexistProcesses {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process[]]$Processes,
        [int64]$TimeoutMs
    )
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while ($watch.ElapsedMilliseconds -lt $TimeoutMs) {
        if (@($Processes | Where-Object { -not $_.HasExited }).Count -eq 0) { return $true }
        Start-Sleep -Milliseconds 25
    }
    return @($Processes | Where-Object { -not $_.HasExited }).Count -eq 0
}

function Stop-CoexistProcessesAndConfirm {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process[]]$Processes,
        [switch]$SkipNamedCleanup
    )
    $ok = $true
    foreach ($process in $Processes) {
        if (-not $process.HasExited -and
            -not (Stop-SandboxGateAndConfirm $process -SkipNamedCleanup:$SkipNamedCleanup)) {
            $ok = $false
        }
    }
    foreach ($process in $Processes) {
        try {
            if (-not $process.HasExited -and -not $process.WaitForExit(2000)) { $ok = $false }
        } catch { $ok = $false }
    }
    return $ok
}

function Test-ProcessLoadedTipFrom {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory)][string]$ExpectedRoot
    )
    $prefix = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\') + '\'
    try {
        foreach ($module in @($Process.Modules)) {
            if ($module.ModuleName -ieq 'nospacekey_tip.dll' -and
                [IO.Path]::GetFullPath($module.FileName).StartsWith(
                    $prefix, [StringComparison]::OrdinalIgnoreCase)) {
                return $true
            }
        }
    } catch {
        Log "TIP module inspection failed pid=$($Process.Id): $($_.Exception.Message)"
    }
    return $false
}

function Test-EngineRunningFrom {
    param([Parameter(Mandatory)][string]$ExpectedRoot)
    $prefix = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\') + '\'
    foreach ($process in @([Diagnostics.Process]::GetProcessesByName('NospacekeyEngineHost'))) {
        try {
            $path = [IO.Path]::GetFullPath($process.MainModule.FileName)
            if ($path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                return $true
            }
        } catch { }
    }
    return $false
}

function Test-EngineProcessGone {
    param([Parameter(Mandatory)]$Process)
    try { return [bool]$Process.HasExited } catch { return $false }
}

function Get-EngineProcessObservations {
    param(
        [Parameter(Mandatory)][string]$ExpectedRoot,
        [scriptblock]$GetEngineProcesses,
        [switch]$FailOnError
    )
    $prefix = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\') + '\'
    $processes = if ($null -eq $GetEngineProcesses) {
        @([Diagnostics.Process]::GetProcessesByName('NospacekeyEngineHost'))
    } else {
        @(& $GetEngineProcesses)
    }
    $observations = New-Object 'System.Collections.Generic.List[object]'
    foreach ($process in $processes) {
        try {
            $fullPath = [IO.Path]::GetFullPath($process.MainModule.FileName)
            if (-not $fullPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                continue
            }
            $observations.Add([pscustomobject]@{
                Id = [int]$process.Id
                FullPath = $fullPath
                StartTimeUtcTicks = [long]$process.StartTime.ToUniversalTime().Ticks
            })
        } catch {
            if ($FailOnError -and -not (Test-EngineProcessGone -Process $process)) { throw }
        } finally {
            if ($process -is [IDisposable]) { $process.Dispose() }
        }
    }
    return $observations.ToArray()
}

function Get-PinnedEngineProcesses {
    param(
        [Parameter(Mandatory)][string]$ExpectedRoot,
        [scriptblock]$GetEngineProcesses
    )
    $prefix = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\') + '\'
    $processes = if ($null -eq $GetEngineProcesses) {
        @([Diagnostics.Process]::GetProcessesByName('NospacekeyEngineHost'))
    } else {
        @(& $GetEngineProcesses)
    }
    $targets = New-Object 'System.Collections.Generic.List[object]'
    foreach ($process in $processes) {
        $retained = $false
        try {
            $handle = [IntPtr]$process.Handle
            if ($handle -eq [IntPtr]::Zero) { throw 'Engine process handle is unavailable.' }
            $id = [int]$process.Id
            $fullPath = [IO.Path]::GetFullPath($process.MainModule.FileName)
            $startTimeUtcTicks = [long]$process.StartTime.ToUniversalTime().Ticks
            if ($fullPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
                $targets.Add([pscustomobject]@{
                    Process = $process
                    Id = $id
                    FullPath = $fullPath
                    StartTimeUtcTicks = $startTimeUtcTicks
                })
                $retained = $true
            }
        } catch {
            if (Test-EngineProcessGone -Process $process) { continue }
            foreach ($target in $targets) {
                if ($target.Process -is [IDisposable]) { $target.Process.Dispose() }
            }
            throw
        } finally {
            if (-not $retained -and $process -is [IDisposable]) { $process.Dispose() }
        }
    }
    return $targets.ToArray()
}

function Wait-EngineRunningFrom {
    param(
        [Parameter(Mandatory)][string]$ExpectedRoot,
        [Parameter(Mandatory)]$Gate,
        [int]$TimeoutMs = 30000,
        [scriptblock]$GetEngineProcesses
    )
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while ($watch.ElapsedMilliseconds -lt $TimeoutMs) {
        $targets = @(Get-EngineProcessObservations -ExpectedRoot $ExpectedRoot `
            -GetEngineProcesses $GetEngineProcesses)
        if ($targets.Count -gt 0) {
            Log "engine ready observed count=$($targets.Count) pids=$(@($targets | ForEach-Object { [string]$_.Id }) -join ',') root=$ExpectedRoot"
            return $targets
        }
        if ($Gate.HasExited) { return @() }
        Start-Sleep -Milliseconds 25
    }
    return @()
}

function Stop-EngineRunningFromAndConfirm {
    param(
        [Parameter(Mandatory)][string]$ExpectedRoot,
        [object[]]$PreviouslyObservedTargets = @(),
        [scriptblock]$GetEngineProcesses,
        [scriptblock]$StopProcessTree
    )
    $prefix = [IO.Path]::GetFullPath($ExpectedRoot).TrimEnd('\') + '\'
    $observedTargets = @($PreviouslyObservedTargets | Where-Object {
        try {
            [int]$_.Id -gt 0 -and [long]$_.StartTimeUtcTicks -gt 0 -and
                [IO.Path]::GetFullPath([string]$_.FullPath).StartsWith(
                    $prefix, [StringComparison]::OrdinalIgnoreCase)
        } catch { $false }
    })
    if ($observedTargets.Count -eq 0) {
        Log "engine absence observed count=0 root=$ExpectedRoot"
        return $false
    }
    $targetPids = @($observedTargets | ForEach-Object { [string]$_.Id }) -join ','
    Log "engine absence observed count=$($observedTargets.Count) pids=$targetPids root=$ExpectedRoot"
    try {
        $targets = @(Get-EngineProcessObservations -ExpectedRoot $ExpectedRoot `
            -GetEngineProcesses $GetEngineProcesses -FailOnError)
    } catch {
        Log "engine enumeration failed before stop: $($_.Exception.Message)"
        return $false
    }
    $terminationToolsConfirmed = $true
    foreach ($process in $targets) {
        $freshTargets = @()
        $target = $null
        try {
            $freshTargets = @(Get-PinnedEngineProcesses -ExpectedRoot $ExpectedRoot `
                -GetEngineProcesses $GetEngineProcesses)
            $target = @($freshTargets | Where-Object {
                $_.Id -eq $process.Id -and
                $_.StartTimeUtcTicks -eq $process.StartTimeUtcTicks -and
                [string]::Equals($_.FullPath, $process.FullPath,
                    [StringComparison]::OrdinalIgnoreCase)
            } | Select-Object -First 1)
            if ($target.Count -eq 0) {
                Log "engine stop skipped stale pid=$($process.Id); confirming final absence"
                continue
            }
            $target = $target[0]
            try {
                $handle = [IntPtr]$target.Process.Handle
                if ($handle -eq [IntPtr]::Zero) { throw 'Pinned Engine process handle was lost.' }
                $freshPath = [IO.Path]::GetFullPath($target.Process.MainModule.FileName)
                $freshStartTimeUtcTicks = [long]$target.Process.StartTime.ToUniversalTime().Ticks
            } catch {
                if (Test-EngineProcessGone -Process $target.Process) {
                    Log "engine stop skipped exited pid=$($process.Id); confirming final absence"
                    continue
                }
                throw
            }
            if ($target.Process.Id -ne $target.Id -or
                $freshStartTimeUtcTicks -ne $target.StartTimeUtcTicks -or
                -not [string]::Equals($freshPath, $target.FullPath,
                    [StringComparison]::OrdinalIgnoreCase)) {
                $terminationToolsConfirmed = $false
                continue
            }
            try {
                if ($null -ne $StopProcessTree) {
                    $result = & $StopProcessTree $target.Process
                    if ($null -eq $result -or ($result.TimedOut -and -not $result.KillConfirmed)) {
                        $terminationToolsConfirmed = $false
                    }
                    if ($null -ne $result -and [int]$result.ExitCode -ne 0) {
                        Log "engine taskkill raced pid=$($process.Id) exit=$($result.ExitCode); confirming final absence"
                    }
                    continue
                }
                $killer = $null
                try {
                    $killer = Start-Process -FilePath $taskkillExe -ArgumentList '/PID', `
                        ([string]$target.Id), '/T', '/F' -PassThru -WindowStyle Hidden -ErrorAction Stop
                    if (-not $killer.WaitForExit(5000)) {
                        try { $killer.Kill(); $killer.WaitForExit() } catch { $terminationToolsConfirmed = $false }
                        if (-not $killer.HasExited) { $terminationToolsConfirmed = $false }
                        Log "engine taskkill timed out pid=$($target.Id); confirming final absence"
                    } elseif ($killer.ExitCode -ne 0) {
                        Log "engine taskkill raced pid=$($target.Id) exit=$($killer.ExitCode); confirming final absence"
                    }
                } finally {
                    if ($killer -is [IDisposable]) { $killer.Dispose() }
                }
            } catch {
                Log "engine taskkill failed pid=$($process.Id): $($_.Exception.Message); confirming final absence"
                $terminationToolsConfirmed = $false
            }
        } catch {
            Log "engine identity failed pid=$($process.Id): $($_.Exception.Message); confirming final absence"
            $terminationToolsConfirmed = $false
        } finally {
            foreach ($freshTarget in $freshTargets) {
                if ($freshTarget.Process -is [IDisposable]) { $freshTarget.Process.Dispose() }
            }
        }
    }
    try {
        $remaining = @(Get-EngineProcessObservations -ExpectedRoot $ExpectedRoot `
            -GetEngineProcesses $GetEngineProcesses -FailOnError)
    } catch {
        Log "engine absence confirmation failed: $($_.Exception.Message)"
        return $false
    }
    if ($remaining.Count -gt 0) {
        Log "engine absence failed remaining=$(@($remaining | ForEach-Object { [string]$_.Id }) -join ',') root=$ExpectedRoot"
        return $false
    }
    Log "engine absence confirmed observed=$($observedTargets.Count) root=$ExpectedRoot"
    return $terminationToolsConfirmed
}

function Disable-GuestLocalEngineExecutable {
    param([Parameter(Mandatory)][string]$GuestBin)
    $original = Join-Path $GuestBin 'NospacekeyEngineHost.exe'
    $blocked = Join-Path $GuestBin 'NospacekeyEngineHost.acceptance-blocked'
    if (-not (Test-Path -LiteralPath $original -PathType Leaf) -or
        (Test-Path -LiteralPath $blocked)) { return $null }
    $hash = (Get-FileHash -LiteralPath $original -Algorithm SHA256 -ErrorAction Stop).Hash
    $state = [pscustomobject]@{ Original = $original; Blocked = $blocked; Hash = $hash }
    Move-Item -LiteralPath $original -Destination $blocked -ErrorAction Stop
    if ((Test-Path -LiteralPath $original) -or
        -not (Test-Path -LiteralPath $blocked -PathType Leaf)) {
        $null = Restore-GuestLocalEngineExecutable -State $state
        return $null
    }
    return $state
}

function Restore-GuestLocalEngineExecutable {
    param([Parameter(Mandatory)]$State)
    try {
        if (Test-Path -LiteralPath $State.Original) { return $false }
        if (-not (Test-Path -LiteralPath $State.Blocked -PathType Leaf)) { return $false }
        if ((Get-FileHash -LiteralPath $State.Blocked -Algorithm SHA256 -ErrorAction Stop).Hash -ne
            $State.Hash) { return $false }
        Move-Item -LiteralPath $State.Blocked -Destination $State.Original -ErrorAction Stop
        return (Test-Path -LiteralPath $State.Original -PathType Leaf) -and
            -not (Test-Path -LiteralPath $State.Blocked) -and
            (Get-FileHash -LiteralPath $State.Original -Algorithm SHA256 -ErrorAction Stop).Hash -eq
                $State.Hash
    } catch { return $false }
}

Log "start: BinDir=$BinDir ResDir=$ResDir args=$TestbenchArgs timeout=$TimeoutSec"

# Engine/TIP diagnostic logs land in the sandbox %TEMP%; keep them on.
$env:NOSPACEKEY_LOG = '1'

# Completion state, fail-closed until the gate proves otherwise: every
# failure below only sets state and falls into the finally, which owns the
# single result write and the single exit.
$gateExit              = 2
$loadLibraryProbeAttempted = $false
# True iff the regsvr32 registration process was LAUNCHED (whatever its
# exit code turns out to be): DllRegisterServer can leave partial registry
# state even on a non-zero exit, and the finally's idempotent /u cleans
# that up. A launch failure never flips it - nothing to unregister.
$registrationAttempted = $false
$pairCoexistence = $false
$versionCleanup = $false
$coexistGates = @()
$engineExecutableBlock = $null
$cleanupTipLoader = $null
$cleanupFixtureRoot = $null

try {
    if ($DisableApplicationControlForDevelopment) {
        Disable-SandboxApplicationControlForDevelopment
    }

    # 1) Copy and verify the mapped tree before any registry operation. The
    # local tree is deliberately kept until the sandbox shuts down; in
    # particular, it must never disappear before TIP unregister below.
    $localStage = Copy-AndVerify-GuestLocalBin -SourceRoot $mappedBinDir
    $guestLocalRoot = $localStage.Root
    $guestBinDir = $localStage.Bin
    $tipDll = Join-Path $guestBinDir 'nospacekey_tip.dll'
    $testbench = Join-Path $guestBinDir 'testbench.exe'
    Log "using guest-local product tree: $guestBinDir"

    $targs = @($TestbenchArgs -split '\s+' | Where-Object { $_ })
    $pairCoexistence = $targs.Count -eq 1 -and $targs[0] -eq '--pair-coexistence'
    $versionCleanup = $targs.Count -eq 1 -and $targs[0] -eq '--version-cleanup'

    if ($versionCleanup) {
        $cleanupFixtureRoot = New-SandboxCleanupFixtureRoot
        $fixtureRoot = $cleanupFixtureRoot
        $fixtureVersions = New-Item -ItemType Directory -Path (Join-Path $fixtureRoot 'versions') -Force
        $sourceManifest = Get-Content -Raw -LiteralPath (Join-Path $guestBinDir 'version-manifest.json') | ConvertFrom-Json
        $fixturePathLength = Test-SandboxCleanupFixturePathBudget `
            -Root $fixtureRoot -Manifest $sourceManifest
        Log "cleanup fixture root: $fixtureRoot worst_quarantine_path_length=$fixturePathLength"
        $activeVersion = [string]$sourceManifest.Version
        if ([string]::IsNullOrWhiteSpace($activeVersion)) { throw 'cleanup fixture build identity unavailable' }

        function Copy-CleanupFixtureTree([string]$Version) {
            $tree = Join-Path $fixtureVersions.FullName $Version
            New-Item -ItemType Directory -Path $tree -Force | Out-Null
            Copy-Item -Path (Join-Path $guestBinDir '*') -Destination $tree -Recurse -Force
            $manifestPath = Join-Path $tree 'version-manifest.json'
            $files = @(Get-ChildItem -LiteralPath $tree -File -Recurse | Where-Object {
                $_.Name -cne 'version-manifest.json'
            } | ForEach-Object {
                [ordered]@{
                    Path = $_.FullName.Substring($tree.Length).TrimStart('\').Replace('\', '/')
                    Sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
                }
            } | Sort-Object Path)
            [ordered]@{ Schema = 1; Version = $Version; Files = $files } |
                ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $manifestPath -Encoding utf8
            return $tree
        }

        $activeTree = Copy-CleanupFixtureTree $activeVersion
        $currentTree = Copy-CleanupFixtureTree '9.9.9-cleanup-current'
        $inactiveTree = Copy-CleanupFixtureTree '0.0.1-cleanup-inactive'
        $cleanupWorker = Join-Path $activeTree 'version-cleanup.ps1'
        # Each pass validates three roughly 3,472-file staged trees. The outer bound
        # also leaves room for the worker supervisor's bounded kill/reap path.
        $cleanupDeadlineMs = 15000
        $cleanupProcessTimeoutMs = 25000
        $engineFixtureId = [guid]::NewGuid().ToString('N')
        $engineReady = Join-Path $guestLocalRoot ("cleanup-engine-ready-$engineFixtureId.txt")
        $engineContinue = Join-Path $guestLocalRoot ("cleanup-engine-continue-$engineFixtureId.txt")
        $cleanupTrace = Join-Path $guestLocalRoot ("cleanup-worker-trace-$engineFixtureId.json")
        $engine = $null
        $engineReleased = $false
        try {
            $previousFixtureEnvironment = [Environment]::GetEnvironmentVariable(
                'NOSPACEKEY_TEST_FIXTURE', [EnvironmentVariableTarget]::Process)
            try {
                [Environment]::SetEnvironmentVariable(
                    'NOSPACEKEY_TEST_FIXTURE', '1', [EnvironmentVariableTarget]::Process)
                $engine = Start-Process -FilePath (Join-Path $activeTree 'NospacekeyEngineHost.exe') `
                    -ArgumentList @('--version-lifetime-fixture', ('"{0}"' -f $engineReady),
                        ('"{0}"' -f $engineContinue)) `
                    -PassThru -WindowStyle Hidden -ErrorAction Stop
            } finally {
                [Environment]::SetEnvironmentVariable(
                    'NOSPACEKEY_TEST_FIXTURE', $previousFixtureEnvironment,
                    [EnvironmentVariableTarget]::Process)
            }
            if (-not (Wait-CoexistMarker -Path $engineReady -Process $engine -TimeoutMs 10000) -or
                $engine.HasExited) {
                throw 'active cleanup fixture engine exited before lease observation'
            }
            Log ("cleanup active-engine evidence: {0}" -f
                (Get-Content -Raw -LiteralPath $engineReady))

            $cleanupArgs = @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',
                ('"{0}"' -f $cleanupWorker), '-InstallRoot', ('"{0}"' -f $fixtureRoot),
                '-TestFixture','-FixtureUseRealLeases','-FixtureCurrentTipPath',
                ('"{0}"' -f (Join-Path $currentTree 'nospacekey_tip.dll')),
                '-FixtureTracePath', ('"{0}"' -f $cleanupTrace),
                '-DeadlineMs', ([string]$cleanupDeadlineMs))
            $firstCleanup = Invoke-BoundedSandboxProcess -FilePath $powershellExe `
                -ArgumentList $cleanupArgs -TimeoutMs $cleanupProcessTimeoutMs
            if (-not $firstCleanup.Launched -or $firstCleanup.TimedOut -or $firstCleanup.ExitCode -ne 0) {
                throw ("active cleanup pass failed: launched=$($firstCleanup.Launched) " +
                    "timedOut=$($firstCleanup.TimedOut) exit=$($firstCleanup.ExitCode) " +
                    "killConfirmed=$($firstCleanup.KillConfirmed) failure=$($firstCleanup.Failure)")
            }
            if (-not (Test-Path -LiteralPath $activeTree -PathType Container) -or
                -not (Test-Path -LiteralPath $currentTree -PathType Container) -or
                (Test-Path -LiteralPath $inactiveTree)) {
                throw 'cleanup did not retain active/current and quarantine inactive tree'
            }
            $firstQuarantine = @(Get-ChildItem -LiteralPath $fixtureVersions.FullName -Directory -Filter '.cleanup-*')
            if ($firstQuarantine.Count -ne 1) { throw 'inactive tree quarantine evidence missing' }

            [IO.File]::WriteAllText($engineContinue, "continue`n", [Text.Encoding]::ASCII)
            if (-not $engine.WaitForExit(5000) -or $engine.ExitCode -ne 0) {
                throw 'active cleanup fixture engine did not release its lease cleanly'
            }
            $engineReleased = $true
            Log 'cleanup active-engine lifetime fixture released its lease'
        } finally {
            if ($null -ne $engine) {
                try {
                    if (-not $engineReleased -and -not $engine.HasExited) {
                        $engine.Kill()
                        if (-not $engine.WaitForExit(5000)) {
                            throw 'exact active cleanup fixture process did not stop'
                        }
                    }
                } catch {
                    Log "active cleanup fixture process cleanup failed: $($_.Exception.Message)"
                    throw
                } finally {
                    $engine.Dispose()
                }
            }
        }

        $loaderScript = Join-Path $guestLocalRoot 'cleanup-tip-loader.ps1'
        $loaderReady = Join-Path $guestLocalRoot 'cleanup-tip-loaded.txt'
        $loaderContinue = Join-Path $guestLocalRoot 'cleanup-tip-unload.txt'
        $loaderDone = Join-Path $guestLocalRoot 'cleanup-tip-unloaded.txt'
        $loaderResult = Join-Path $guestLocalRoot 'cleanup-tip-loader-result.txt'
        @'
param([string]$Dll, [string]$Ready, [string]$Continue, [string]$Done, [string]$Result, [switch]$Probe)
$ErrorActionPreference = 'Stop'
$stage = 'setup'
$win32 = 0
$module = [IntPtr]::Zero
function Write-LoaderResult([string]$Value) {
    [IO.File]::WriteAllText($Result, $Value, [Text.Encoding]::UTF8)
}
try {
  Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class CleanupTipLoader {
  [DllImport("kernel32.dll", EntryPoint="LoadLibraryExW", CharSet=CharSet.Unicode, ExactSpelling=true, SetLastError=true)]
  public static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
  [DllImport("kernel32.dll", ExactSpelling=true, SetLastError=true)]
  [return: MarshalAs(UnmanagedType.Bool)] public static extern bool FreeLibrary(IntPtr module);
}
"@
  if ($Probe) {
      $probeEvidence = @()
      foreach ($flags in @(0, 8)) {
          $stage = "probe-$flags"
          $module = [CleanupTipLoader]::LoadLibraryExW($Dll, [IntPtr]::Zero, [uint32]$flags)
          if ($module -eq [IntPtr]::Zero) {
              $win32 = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
              $probeEvidence += "flags=$flags ok=false win32_error=$win32"
          } else {
              $probeEvidence += "flags=$flags ok=true win32_error=0"
              if (-not [CleanupTipLoader]::FreeLibrary($module)) {
                  $win32 = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                  throw 'probe FreeLibrary failed'
              }
              $module = [IntPtr]::Zero
          }
          Write-LoaderResult ($probeEvidence -join ';')
      }
      exit 0
  }
  $stage = 'load'
  # LOAD_WITH_ALTERED_SEARCH_PATH makes dependencies follow the staged TIP directory.
  $module = [CleanupTipLoader]::LoadLibraryExW($Dll, [IntPtr]::Zero, [uint32]8)
  if ($module -eq [IntPtr]::Zero) {
      $win32 = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
      throw 'LoadLibraryExW returned null'
  }
  $stage = 'ready'
  Write-LoaderResult "stage=ready win32_error=0 pid=$PID path=$Dll"
  [IO.File]::WriteAllText($Ready, "pid=$PID path=$Dll", [Text.Encoding]::UTF8)
  $stage = 'wait'
  $limit = [Diagnostics.Stopwatch]::StartNew()
  while (-not (Test-Path -LiteralPath $Continue)) {
      if ($limit.ElapsedMilliseconds -gt 60000) { throw 'continue marker timeout' }
      Start-Sleep -Milliseconds 50
  }
  $stage = 'free'
  if (-not [CleanupTipLoader]::FreeLibrary($module)) {
      $win32 = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
      throw 'FreeLibrary failed'
  }
  $module = [IntPtr]::Zero
  [IO.File]::WriteAllText($Done, "pid=$PID path=$Dll", [Text.Encoding]::UTF8)
  Write-LoaderResult "stage=done win32_error=0 pid=$PID path=$Dll"
  exit 0
} catch {
  $hresult = $_.Exception.HResult
  $exceptionType = $_.Exception.GetType().FullName
  $message = $_.Exception.Message -replace '[\r\n]+', ' '
  $sentinelExists = Test-Path -LiteralPath (Join-Path (Split-Path -Parent $Dll) '.nospacekey-lifetime') -PathType Leaf
  Write-LoaderResult "stage=$stage win32_error=$win32 hresult=$hresult exception=$exceptionType sentinel_exists=$sentinelExists message=$message"
  exit 2
} finally {
  if ($module -ne [IntPtr]::Zero) {
      try { [void][CleanupTipLoader]::FreeLibrary($module) } catch { }
  }
}
'@ | Set-Content -LiteralPath $loaderScript -Encoding utf8
        $loader = Start-Process -FilePath $powershellExe -ArgumentList @(
            '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File', ('"{0}"' -f $loaderScript),
            '-Dll', ('"{0}"' -f (Join-Path $activeTree 'nospacekey_tip.dll')),
            '-Ready', ('"{0}"' -f $loaderReady), '-Continue', ('"{0}"' -f $loaderContinue),
            '-Done', ('"{0}"' -f $loaderDone), '-Result', ('"{0}"' -f $loaderResult)) `
            -PassThru -WindowStyle Hidden -ErrorAction Stop
        $cleanupTipLoader = $loader
        if (-not (Wait-CoexistMarker -Path $loaderReady -Process $loader)) {
            $loaderTerminationConfirmed = if ($loader.HasExited) { $true } else {
                Stop-ExactProcessAndConfirm -Process $loader -TimeoutMs 5000
            }
            $loaderExit = if ($loader.HasExited) { $loader.ExitCode } else { 'running' }
            $loaderResultEvidence = if (Test-Path -LiteralPath $loaderResult -PathType Leaf) {
                Get-Content -Raw -LiteralPath $loaderResult
            } else { 'missing' }
            if (-not $loaderTerminationConfirmed) {
                Log "cleanup loaded-tip failure: termination=unconfirmed loader_exit=$loaderExit loader_result=$loaderResultEvidence"
                throw "loaded TIP termination=unconfirmed; differential probe skipped"
            }
            $cleanupTipLoader.Dispose()
            $cleanupTipLoader = $null
            $loader = $null
            $probeResultPath = Join-Path $guestLocalRoot 'cleanup-tip-probe-result.txt'
            $probe = Invoke-BoundedSandboxProcess -FilePath $powershellExe -ArgumentList @(
                '-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File', ('"{0}"' -f $loaderScript),
                '-Dll', ('"{0}"' -f (Join-Path $activeTree 'nospacekey_tip.dll')),
                '-Ready', ('"{0}"' -f $loaderReady), '-Continue', ('"{0}"' -f $loaderContinue),
                '-Done', ('"{0}"' -f $loaderDone), '-Result', ('"{0}"' -f $probeResultPath), '-Probe') `
                -TimeoutMs 10000
            $probeResultEvidence = if (Test-Path -LiteralPath $probeResultPath -PathType Leaf) {
                Get-Content -Raw -LiteralPath $probeResultPath
            } else { 'missing' }
            Log ("cleanup loaded-tip failure: termination=confirmed loader_exit=$loaderExit " +
                "loader_result=$loaderResultEvidence probe_launched=$($probe.Launched) " +
                "probe_timedOut=$($probe.TimedOut) probe_exit=$($probe.ExitCode) " +
                "probe_killConfirmed=$($probe.KillConfirmed) probe_failure=$($probe.Failure) " +
                "probe_result=$probeResultEvidence")
            throw "loaded TIP did not publish its lease marker: loader_exit=$loaderExit loader_result=$loaderResultEvidence"
        }
        $loaderEvidence = Get-Content -Raw -LiteralPath $loaderReady
        $loaderResultEvidence = Get-Content -Raw -LiteralPath $loaderResult
        $tipLoaded = Test-ProcessLoadedTipFrom -Process $loader -ExpectedRoot $activeTree
        $engineAbsent = -not (Test-EngineRunningFrom -ExpectedRoot $activeTree)
        Log "cleanup loaded-tip evidence: $loaderEvidence result=$loaderResultEvidence loaded=$tipLoaded engine_absent=$engineAbsent"
        if (-not $tipLoaded -or -not $engineAbsent) { throw 'loaded TIP path or EngineHost absence evidence failed' }

        Reset-SandboxCleanupTrace -Path $cleanupTrace
        $loadedCleanup = Invoke-BoundedSandboxProcess -FilePath $powershellExe `
            -ArgumentList $cleanupArgs -TimeoutMs $cleanupProcessTimeoutMs -CaptureOutput
        if (-not $loadedCleanup.Launched -or $loadedCleanup.TimedOut -or $loadedCleanup.ExitCode -ne 0) {
            $loadedStdout = $loadedCleanup.Stdout -replace '[\r\n]+', ' '
            $loadedStderr = $loadedCleanup.Stderr -replace '[\r\n]+', ' '
            $loadedTrace = if (Test-Path -LiteralPath $cleanupTrace -PathType Leaf) {
                (Get-Content -Raw -LiteralPath $cleanupTrace) -replace '[\r\n]+', ' '
            } else { 'missing' }
            throw ("loaded-TIP cleanup pass failed: launched=$($loadedCleanup.Launched) " +
                "timedOut=$($loadedCleanup.TimedOut) exit=$($loadedCleanup.ExitCode) " +
                "killConfirmed=$($loadedCleanup.KillConfirmed) failure=$($loadedCleanup.Failure) " +
                "stdout=$loadedStdout stderr=$loadedStderr trace=$loadedTrace")
        }
        if (-not (Test-Path -LiteralPath $activeTree -PathType Container)) {
            throw 'DllMain lease did not retain the loaded old TIP tree'
        }
        [IO.File]::WriteAllText($loaderContinue, "unload`n", [Text.Encoding]::ASCII)
        $loaderExited = $loader.WaitForExit(5000)
        $loaderTerminationConfirmed = if ($loaderExited) { $true } else {
            Stop-ExactProcessAndConfirm -Process $loader -TimeoutMs 5000
        }
        if (-not $loaderTerminationConfirmed -or -not $loaderExited -or
            $loader.ExitCode -ne 0 -or -not (Test-Path $loaderDone)) {
            throw "loaded TIP did not unload within the bounded wait: termination=$loaderTerminationConfirmed"
        }
        Log ("cleanup unloaded-tip evidence: {0}" -f (Get-Content -Raw -LiteralPath $loaderDone))
        $cleanupTipLoader.Dispose()
        $cleanupTipLoader = $null
        $loader = $null

        $secondCleanup = Invoke-BoundedSandboxProcess -FilePath $powershellExe `
            -ArgumentList $cleanupArgs -TimeoutMs $cleanupProcessTimeoutMs
        if (-not $secondCleanup.Launched -or $secondCleanup.TimedOut -or $secondCleanup.ExitCode -ne 0) {
            throw ("inactive cleanup pass failed: launched={0} timedOut={1} exit={2} " +
                "killConfirmed={3} failure={4}" -f $secondCleanup.Launched,
                $secondCleanup.TimedOut, $secondCleanup.ExitCode,
                $secondCleanup.KillConfirmed, $secondCleanup.Failure)
        }
        if (Test-Path -LiteralPath $activeTree) { throw 'inactive old tree was not moved after lease release' }
        if (-not (Test-Path -LiteralPath $currentTree -PathType Container)) { throw 'current tree was reclaimed' }
        [IO.File]::WriteAllText($outFile, "version-cleanup PASS`r`n", [Text.Encoding]::ASCII)
        [IO.File]::WriteAllText($errFile, '', [Text.Encoding]::ASCII)
        $gateExit = 0
        Log 'version cleanup retained active old pair and reclaimed inactive old tree'
    } elseif ($pairCoexistence) {
        if ([string]::IsNullOrWhiteSpace($mappedPreviousBinDir)) {
            throw 'pair coexistence requires the previous read-only mapping'
        }
        $previousStage = Copy-AndVerify-GuestLocalBin -SourceRoot $mappedPreviousBinDir
        $previousGuestBin = $previousStage.Bin
        $previousTipDll = Join-Path $previousGuestBin 'nospacekey_tip.dll'
        if (-not (Test-Path -LiteralPath $previousTipDll -PathType Leaf)) {
            throw "previous TIP missing: $previousTipDll"
        }

        $oldReg = Invoke-BoundedSandboxProcess -FilePath $regsvr32 `
            -ArgumentList @('/s', "`"$previousTipDll`"") -TimeoutMs 10000
        $registrationAttempted = $oldReg.Launched
        if (-not $oldReg.Launched -or $oldReg.TimedOut -or $oldReg.ExitCode -ne 0) {
            throw "previous TIP registration failed: launched=$($oldReg.Launched) timedOut=$($oldReg.TimedOut) exit=$($oldReg.ExitCode)"
        }

        $oldReady = Join-Path $guestLocalRoot 'old-ready.txt'
        $oldContinue = Join-Path $guestLocalRoot 'old-continue.txt'
        $oldDone = Join-Path $guestLocalRoot 'old-done.txt'
        $oldExit = Join-Path $guestLocalRoot 'old-exit.txt'
        $oldOut = Join-Path $guestLocalRoot 'old-pair.out'
        $oldErr = Join-Path $guestLocalRoot 'old-pair.err'
        $oldRunner = Join-Path $guestLocalRoot 'old-pair.cmd'
        @(
            '@echo off',
            ('"{0}" --pair-hold "{1}" "{2}" "{3}" "{4}" > "{5}" 2> "{6}"' -f
                $testbench, $oldReady, $oldContinue, $oldDone, $oldExit, $oldOut, $oldErr),
            'exit /b %ERRORLEVEL%'
        ) | Set-Content -LiteralPath $oldRunner -Encoding ascii
        $oldGate = Start-Process -FilePath $oldRunner -PassThru -WindowStyle Hidden `
            -ErrorAction Stop
        $coexistGates = @($oldGate)
        if (-not (Wait-CoexistMarker -Path $oldReady -Process $oldGate)) {
            $readyEvidence = Get-CoexistReadyFailureEvidence -Label previous `
                -Process $oldGate -OutPath $oldOut -ErrPath $oldErr
            Log "previous pair ready failure: $readyEvidence"
            Invoke-SandboxLoadLibraryProbe -Path $previousTipDll
            throw "previous pair did not publish its ready marker ($readyEvidence)"
        }
        $oldReadyText = Get-Content -LiteralPath $oldReady -Raw
        if ($oldReadyText -notmatch '\Aready pid=(\d+)\s*\z') {
            throw 'previous pair ready marker did not contain a PID'
        }
        $oldHost = [Diagnostics.Process]::GetProcessById([int]$Matches[1])

        $reg = Invoke-BoundedSandboxProcess -FilePath $regsvr32 `
            -ArgumentList @('/s', "`"$tipDll`"") -TimeoutMs 10000
        $registrationAttempted = $registrationAttempted -or $reg.Launched
        if (-not $reg.Launched -or $reg.TimedOut -or $reg.ExitCode -ne 0) {
            throw "current TIP registration failed: launched=$($reg.Launched) timedOut=$($reg.TimedOut) exit=$($reg.ExitCode)"
        }

        $newReady = Join-Path $guestLocalRoot 'new-ready.txt'
        $newContinue = Join-Path $guestLocalRoot 'new-continue.txt'
        $newDone = Join-Path $guestLocalRoot 'new-done.txt'
        $newExit = Join-Path $guestLocalRoot 'new-exit.txt'
        $newOut = Join-Path $guestLocalRoot 'new-pair.out'
        $newErr = Join-Path $guestLocalRoot 'new-pair.err'
        $newRunner = Join-Path $guestLocalRoot 'new-pair.cmd'
        @(
            '@echo off',
            ('"{0}" --pair-hold "{1}" "{2}" "{3}" "{4}" > "{5}" 2> "{6}"' -f
                $testbench, $newReady, $newContinue, $newDone, $newExit, $newOut, $newErr),
            'exit /b %ERRORLEVEL%'
        ) | Set-Content -LiteralPath $newRunner -Encoding ascii
        $newGate = Start-Process -FilePath $newRunner -PassThru -WindowStyle Hidden `
            -ErrorAction Stop
        $coexistGates = @($oldGate, $newGate)
        if (-not (Wait-CoexistMarker -Path $newReady -Process $newGate)) {
            $readyEvidence = Get-CoexistReadyFailureEvidence -Label current `
                -Process $newGate -OutPath $newOut -ErrPath $newErr
            Log "current pair ready failure: $readyEvidence"
            Invoke-SandboxLoadLibraryProbe -Path $tipDll
            throw "current pair did not publish its ready marker ($readyEvidence)"
        }
        $newReadyText = Get-Content -LiteralPath $newReady -Raw
        if ($newReadyText -notmatch '\Aready pid=(\d+)\s*\z') {
            throw 'current pair ready marker did not contain a PID'
        }
        $newHost = [Diagnostics.Process]::GetProcessById([int]$Matches[1])

        $oldTipLoaded = Test-ProcessLoadedTipFrom -Process $oldHost `
            -ExpectedRoot $previousGuestBin
        $newTipLoaded = Test-ProcessLoadedTipFrom -Process $newHost `
            -ExpectedRoot $guestBinDir
        $oldEngineRunning = Test-EngineRunningFrom -ExpectedRoot $previousGuestBin
        $newEngineRunning = Test-EngineRunningFrom -ExpectedRoot $guestBinDir
        Log "coexist paths: old_tip=$oldTipLoaded new_tip=$newTipLoaded old_engine=$oldEngineRunning new_engine=$newEngineRunning"
        if (-not ($oldTipLoaded -and $newTipLoaded -and
                  $oldEngineRunning -and $newEngineRunning)) {
            throw 'old/new matching pair path evidence was incomplete'
        }

        $pairCompletionWatch = [Diagnostics.Stopwatch]::StartNew()
        $pairCompletionBudgetMs = [int64]$TimeoutSec * 1000
        # Each conversion has a 2.5-second deadline; do not let an old host consume its 120-second marker wait.
        $donePhaseCapMs = [int64]15000
        [IO.File]::WriteAllText($oldContinue, "continue`n", [Text.Encoding]::ASCII)
        $oldDoneWaitMs = [Math]::Min($donePhaseCapMs,
            [Math]::Max([int64]0, $pairCompletionBudgetMs - $pairCompletionWatch.ElapsedMilliseconds))
        if ($oldDoneWaitMs -le 0 -or -not (Wait-CoexistMarker -Path $oldDone `
                -Process $oldGate -TimeoutMs $oldDoneWaitMs) -or $oldGate.HasExited) {
            $stopped = Stop-CoexistProcessesAndConfirm -Processes @($oldGate, $newGate)
            throw "pair coexistence old done TIMEOUT (both stopped=$stopped)"
        }
        [IO.File]::WriteAllText($newContinue, "continue`n", [Text.Encoding]::ASCII)
        $newDoneWaitMs = [Math]::Min($donePhaseCapMs,
            [Math]::Max([int64]0, $pairCompletionBudgetMs - $pairCompletionWatch.ElapsedMilliseconds))
        if ($newDoneWaitMs -le 0 -or -not (Wait-CoexistMarker -Path $newDone `
                -Process $newGate -TimeoutMs $newDoneWaitMs) -or $newGate.HasExited) {
            $stopped = Stop-CoexistProcessesAndConfirm -Processes @($oldGate, $newGate)
            throw "pair coexistence new done TIMEOUT (both stopped=$stopped)"
        }
        [IO.File]::WriteAllText($oldExit, "exit`n", [Text.Encoding]::ASCII)
        [IO.File]::WriteAllText($newExit, "exit`n", [Text.Encoding]::ASCII)
        $exitWaitMs = [Math]::Max([int64]0,
            $pairCompletionBudgetMs - $pairCompletionWatch.ElapsedMilliseconds)
        if ($exitWaitMs -le 0 -or -not (Wait-CoexistProcesses -Processes @($oldGate, $newGate) `
                -TimeoutMs $exitWaitMs) -or
            $pairCompletionWatch.ElapsedMilliseconds -gt $pairCompletionBudgetMs) {
            $stopped = Stop-CoexistProcessesAndConfirm -Processes @($oldGate, $newGate)
            throw "pair coexistence exit phase TIMEOUT after ${TimeoutSec}s (both stopped=$stopped)"
        }
        $gateExit = if ($oldGate.ExitCode -eq 0 -and $newGate.ExitCode -eq 0) { 0 } else { 1 }
        $outStream = [IO.File]::Open($outFile, [IO.FileMode]::Create, [IO.FileAccess]::Write)
        try {
            foreach ($source in @($oldOut, $newOut)) {
                Add-CoexistOutputFile -Destination $outStream -Source $source
            }
        } finally { $outStream.Dispose() }
        $errStream = [IO.File]::Open($errFile, [IO.FileMode]::Create, [IO.FileAccess]::Write)
        try {
            foreach ($source in @($oldErr, $newErr)) {
                Add-CoexistOutputFile -Destination $errStream -Source $source
            }
        } finally { $errStream.Dispose() }
        Log "pair coexistence exits: old=$($oldGate.ExitCode) new=$($newGate.ExitCode) normalized_candidate=$gateExit"
    } else {
    # 2) register the TIP. The sandbox session is admin-by-default; if this
    #    fails the gate cannot activate the TIP at all - throw so the catch
    #    pins the exit at 2. $registrationAttempted flips as soon as the
    #    regsvr32 PROCESS exists, even if it goes on to exit non-zero: the
    #    partial registry state DllRegisterServer may leave behind still
    #    needs the finally's /u. A launch failure leaves it $false (no
    #    pointless /u) - hence -ErrorAction Stop + a dedicated try/catch,
    #    the same launch/exit separation as the testbench spawn below.
    $reg = Invoke-BoundedSandboxProcess -FilePath $regsvr32 `
        -ArgumentList @('/s', "`"$tipDll`"") -TimeoutMs 10000
    $registrationAttempted = $reg.Launched
    if (-not $reg.Launched) {
        throw "regsvr32 launch failed: $($reg.Failure)"
    }
    if ($reg.TimedOut) {
        throw "regsvr32 timeout (killConfirmed=$($reg.KillConfirmed)): $($reg.Failure)"
    }
    Log "regsvr32 exit=$($reg.ExitCode)"
    if ($reg.ExitCode -ne 0) {
        $loadLibraryProbeAttempted = $true
        Invoke-SandboxLoadLibraryProbe -Path $tipDll
        throw "regsvr32 failed exit=$($reg.ExitCode)"
    }
    if ($DisableApplicationControlForDevelopment) {
        # No ctfmon restart after the application-control change: regsvr32
        # above already loaded and registered the TIP DLL under the updated
        # CI policy, and the testbench below is likewise a new process whose
        # DLL loads are evaluated at load time. Killing ctfmon here left the
        # freshly respawned msctf server unable to activate the TIP: the
        # v1.5.0 freeze measured a 240s silent testbench hang (0-byte gate.out)
        # immediately after the generation swap.
    }

    # 3) run the gate (SAFE SPAWN - see the header contract).
    $env:NOSPACEKEY_TEST_SANDBOX = '1'
    if (-not (Test-Path -LiteralPath $testbench -PathType Leaf)) {
        throw "testbench not found: $testbench"
    }
    # Run the gate through a generated .cmd file:
    #  - PS 5.1 Start-Process + -RedirectStandardOutput + WaitForExit(timeout) can
    #    lose BOTH the stdout flush and the ExitCode (observed: gate.out 0B, exit null).
    #  - Passing the whole `testbench args > out 2> err` line as a cmd /c argument
    #    ALSO fails: PS 5.1 auto-quotes space-containing ArgumentList elements, which
    #    breaks cmd /c's own quote stripping (observed: instant exit 1, no files).
    # A generated .cmd holds the quoting inside the file, and the .cmd path itself
    # contains no spaces, so Start-Process -FilePath needs no quoting at all.
    # The .cmd (and thus testbench) is waited on via WaitForExit on the PROCESS ONLY
    # (never Start-Process -Wait, which would wait the whole --persist engine
    # descendant tree and hang).
    $engineAbsence = $targs.Count -eq 1 -and $targs[0] -eq '--engine-absence'
    if ($engineAbsence) {
        $absenceReady = Join-Path $guestLocalRoot 'engine-absence-ready.txt'
        $absenceContinue = Join-Path $guestLocalRoot 'engine-absence-continue.txt'
        $absenceDone = Join-Path $guestLocalRoot 'engine-absence-done.txt'
        $runnerArgs = @('--engine-absence', $absenceReady, $absenceContinue, $absenceDone)
    } else {
        $runnerArgs = $targs
    }
    $runner = Join-Path $guestBinDir 'run-gate-inner.cmd'
    @(
        '@echo off',
        ('"{0}" {1} > "{2}" 2> "{3}"' -f $testbench, ($runnerArgs -join ' '), $outFile, $errFile),
        'exit /b %ERRORLEVEL%'
    ) | Set-Content -LiteralPath $runner -Encoding ascii
    # Separate variable from $reg + try/catch: under EAP=Continue a failed
    # Start-Process leaves the variable holding the PREVIOUS process, and we
    # would then read regsvr32's exit 0 as the gate result (false PASS).
    # -ErrorAction Stop makes the catch unconditional: without it, error-kind or
    # version differences can surface Start-Process failures as non-terminating
    # errors that would skip the catch and leave $gate null.
    $gate = $null
    try {
        $gate = Start-Process -FilePath $runner -PassThru -WindowStyle Hidden -ErrorAction Stop
    } catch {
        throw "testbench spawn failed: $($_.Exception.Message)"
    }
    if ($engineAbsence) {
        if (-not (Wait-CoexistMarker -Path $absenceReady -Process $gate -TimeoutMs 30000)) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'engine absence gate did not publish its ready marker'
        }
        $observedEngineTargets = @(Wait-EngineRunningFrom -ExpectedRoot $guestBinDir `
            -Gate $gate -TimeoutMs 30000)
        if ($observedEngineTargets.Count -eq 0) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'guest-local EngineHost was not observed after the ready marker'
        }
        $engineExecutableBlock = Disable-GuestLocalEngineExecutable -GuestBin $guestBinDir
        if ($null -eq $engineExecutableBlock) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'guest-local EngineHost executable could not be disabled'
        }
        if (-not (Stop-EngineRunningFromAndConfirm -ExpectedRoot $guestBinDir `
                -PreviouslyObservedTargets $observedEngineTargets)) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'guest-local EngineHost absence injection was not confirmed'
        }
        [IO.File]::WriteAllText($absenceContinue, "continue`n", [Text.Encoding]::ASCII)
        if (-not (Wait-CoexistMarker -Path $absenceDone -Process $gate -TimeoutMs 30000)) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'engine absence gate did not publish its completion marker'
        }
        if ((Test-EngineRunningFrom -ExpectedRoot $guestBinDir) -or
            (Test-Path -LiteralPath $engineExecutableBlock.Original)) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'EngineHost became available during the absence key interval'
        }
        if (-not (Restore-GuestLocalEngineExecutable -State $engineExecutableBlock)) {
            $null = Stop-SandboxGateAndConfirm $gate
            throw 'guest-local EngineHost executable restoration failed'
        }
        $engineExecutableBlock = $null
        Log 'guest-local EngineHost remained absent for the complete key interval'
    }
    if (-not $gate.WaitForExit($TimeoutSec * 1000)) {
        $terminationConfirmed = Stop-SandboxGateAndConfirm $gate
        if ($terminationConfirmed) {
            throw "testbench TIMEOUT after ${TimeoutSec}s (process tree termination confirmed)"
        }
        throw "testbench TIMEOUT after ${TimeoutSec}s (process tree termination NOT confirmed)"
    }
    $gateExit = $gate.ExitCode
    if ($null -eq $gateExit -or $gateExit -lt 0) {
        # fail-closed: an unreadable exit code must never become an empty signal
        Log "exit code unavailable via cmd (got '$gateExit'); treating as 2"
        $gateExit = 2
    }
    Log "testbench exit=$gateExit"
    }

    # 2b) Evidence normalization (header contract): match the raw gate exit
    #     against the rows actually parsed from gate.out BEFORE unregister /
    #     log copy / exitcode, so exitcode.txt always carries the normalized code.
    $rowCounts = Get-GateOutCounts -Path $outFile
    if (-not (Test-Path -LiteralPath $outFile)) {
        Log "gate.out missing (no row evidence): $outFile"
    }
    Log ("gate.out rows: parsed={0} failed={1}" -f $rowCounts.Parsed, $rowCounts.Failed)
    $gateExit = Get-NormalizedGateExit -RawExit $gateExit -Counts $rowCounts
    if ((Test-IsScenarioArgs $targs) -and -not (Test-ExactScenarioIds $rowCounts.ScenarioIds)) {
        Log "scenario ID evidence does not match the canonical set"
        $gateExit = 2
    }
    Log "normalized exit=$gateExit"
} catch {
    # One catch for every handled failure (regsvr32 / missing testbench /
    # spawn / timeout - thrown above with descriptive messages) and for any
    # UNEXPECTED exception: set the state, never exit - the finally owns the
    # single completion path.
    if ($pairCoexistence -and $coexistGates.Count -gt 0) {
        $coexistStopped = Stop-CoexistProcessesAndConfirm -Processes $coexistGates
        Log "coexistence failure cleanup confirmed=$coexistStopped"
    }
    Log "gate aborted: $($_.Exception.Message)"
    $gateExit = 2
} finally {
    if ($coexistGates.Count -gt 0) {
        foreach ($coexistGate in $coexistGates) { $coexistGate.Dispose() }
        $coexistGates = @()
    }
    if ($null -ne $cleanupTipLoader) {
        try {
            if (-not (Stop-ExactProcessAndConfirm -Process $cleanupTipLoader -TimeoutMs 5000)) {
                Log 'cleanup TIP loader termination=unconfirmed during final drain'
                $gateExit = 2
            }
        } finally {
            $cleanupTipLoader.Dispose()
            $cleanupTipLoader = $null
        }
    }
    if ($null -ne $cleanupFixtureRoot) {
        try {
            Remove-SandboxCleanupFixtureRoot -Path $cleanupFixtureRoot
            Log "cleanup fixture root removed: $cleanupFixtureRoot"
            $cleanupFixtureRoot = $null
        } catch {
            Log "cleanup fixture root teardown failed: $($_.Exception.Message)"
            $gateExit = 2
        }
    }
    if ($null -ne $engineExecutableBlock) {
        if (Restore-GuestLocalEngineExecutable -State $engineExecutableBlock) {
            Log 'guest-local EngineHost executable restored during failure cleanup'
            $engineExecutableBlock = $null
        } else {
            Log 'guest-local EngineHost executable restoration failed during cleanup'
            $gateExit = 2
        }
    }
    # 3) unregister the TIP, CHECKED (fail-closed: an /u failure is abnormal
    #    cleanup and must never carry a clean code through) - but only when
    #    a regsvr32 registration process was actually LAUNCHED. /u is
    #    idempotent: it also removes the partial registry state
    #    DllRegisterServer can leave behind on a non-zero registration exit.
    if ($registrationAttempted) {
        try {
            $unreg = Invoke-BoundedSandboxProcess -FilePath $regsvr32 `
                -ArgumentList @('/u', '/s', "`"$tipDll`"") -TimeoutMs 10000
            Log "regsvr32 /u exit=$($unreg.ExitCode) timedOut=$($unreg.TimedOut) killConfirmed=$($unreg.KillConfirmed)"
            if (-not $unreg.Launched -or $unreg.ExitCode -ne 0) {
                if ($unreg.Launched -and $unreg.ExitCode -ne 0 -and
                    -not $loadLibraryProbeAttempted) {
                    $loadLibraryProbeAttempted = $true
                    Invoke-SandboxLoadLibraryProbe -Path $tipDll
                }
                Log "unregister failed; forcing exit 2"
                $gateExit = 2
            }
        } catch {
            Log "regsvr32 /u launch failed: $($_.Exception.Message); forcing exit 2"
            $gateExit = 2
        }
    }

    # The guest-local root is inside the disposable VM. Keep it present through
    # unregister so no registry path can point at a deleted DLL; Sandbox
    # destruction removes the whole local tree after this script exits.
    if ($guestLocalRoot) {
        Log "guest-local tree retained through unregister; Sandbox teardown owns cleanup: $guestLocalRoot"
    }

    # 4) surface engine logs for the host; a failed copy loses evidence, so
    #    it is logged and forces 2. (A source that never existed is not a
    #    copy failure - e.g. register aborted before the engine ever ran.)
    foreach ($n in @('nospacekey-tip.log', 'nospacekey-engine.log')) {
        $src = Join-Path $env:TEMP $n
        if (Test-Path -LiteralPath $src) {
            try {
                Copy-Item -LiteralPath $src -Destination (Join-Path $ResDir $n) -Force -ErrorAction Stop
                Log "copied $n"
            } catch {
                Log "copy failed for ${n}: $($_.Exception.Message); forcing exit 2"
                $gateExit = 2
            }
        }
    }

    # 5) Ask to close the sandbox FROM THE INSIDE on EVERY path. The outer cmd
    #    watchdog retries this after PowerShell exits, then publishes exitcode.txt.
    #    WindowsSandbox.exe on the host is only a launcher that has already
    #    exited; killing WindowsSandboxRemoteSession/Server from the host leaves the
    #    remote session table in a broken state and the NEXT launch fails with
    #    "the remote environment is logged off" / "too many sessions" (both observed
    #    2026-08-20). Shutting down inside the sandbox is the supported close - same
    #    as clicking the window's X - and keeps the host-side session table clean.
    #    The request is CHECKED: shutdown.exe is not testbench - it exits right
    #    after the request is accepted or rejected, never waiting the /t grace -
    #    the bounded helper therefore yields the verdict promptly. A rejected
    #    request (non-zero exit) means the sandbox will not close; it must not
    #    carry a clean code through to the signal.
    try {
        $shut = Invoke-BoundedSandboxProcess -FilePath $shutdownExe `
            -ArgumentList @('/s', '/t', '5', '/f') -TimeoutMs 10000
        Log "shutdown exit=$($shut.ExitCode)"
        if (-not $shut.Launched -or $shut.ExitCode -ne 0) {
            Log "shutdown request rejected; forcing exit 2"
            $gateExit = 2
        } else {
            Set-Content -Path $shutdownAcceptedFile -Value 1 -ErrorAction Stop
        }
    } catch {
        Log "shutdown request failed: $($_.Exception.Message); forcing exit 2"
        $gateExit = 2
    }

    # 6) Publish only the private inner status. The outer cmd watchdog attempts
    #    shutdown again before atomically moving this into the host-visible
    #    exitcode.txt signal.
    Log "done exit=$gateExit"
    Set-Content -Path $innerExitFile -Value $gateExit
}

# The ONLY exit - and only after the finally has published its private status.
exit $gateExit
