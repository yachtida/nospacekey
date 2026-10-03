<#
Crash-containment supervisor for nospacekey validation scripts.

Run the target below this process after assigning it to a KILL_ON_JOB_CLOSE Job.
Admin-required targets additionally perform owned registry cleanup. Validation-
only targets use the same containment without crossing an elevation boundary.
#>
#Requires -Version 7.3
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$InvocationBase64,
    [ValidateRange(1, 86400)][int]$TimeoutSec = 3600,
    [switch]$ValidationOnly,
    # Lets an isolated fixture test only the timeout/process-tree contract.
    [switch]$SkipSystemCleanupForTesting
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'test-lib.ps1')

function New-AdminCleanupDirectory {
    param(
        [Parameter(Mandatory)][string]$Token,
        [switch]$ForTesting
    )
    return New-ValidationCleanupDirectory -Token $Token -ForTesting:$ForTesting
}

function Remove-AdminCleanupDirectory {
    param([string]$Path, [switch]$ForTesting)
    return Remove-ValidationCleanupDirectory -Path $Path -ForTesting:$ForTesting
}

function New-PinnedTipSnapshot {
    param(
        [Parameter(Mandatory)]$PinnedTip,
        [Parameter(Mandatory)][string]$CleanupDirectory
    )
    return New-ProtectedTipSnapshot -PinnedTip $PinnedTip `
        -CleanupDirectory $CleanupDirectory
}

function Start-ArgumentProcess {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][string[]]$ArgumentList,
        $Job = $null,
        $RecoveryJob = $null
    )
    if ($null -ne $Job) {
        return $Job.StartProcess($FilePath, [string[]]$ArgumentList, $RecoveryJob)
    }
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $FilePath
    $psi.UseShellExecute = $false
    foreach ($arg in $ArgumentList) { [void]$psi.ArgumentList.Add($arg) }
    $process = [System.Diagnostics.Process]::Start($psi)
    if ($null -eq $process) { throw "process start returned null: $FilePath" }
    return $process
}

function Stop-ProcessAndConfirm {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [int]$WaitMs = 5000
    )
    try {
        if (-not $Process.HasExited) { $Process.Kill($true) }
        return $Process.WaitForExit($WaitMs) -and $Process.HasExited
    } catch {
        Write-Host "[FAIL] process termination could not be confirmed (PID $($Process.Id)): $($_.Exception.Message)"
        return $false
    }
}

function Stop-NamedProcessesAndConfirm {
    param([Parameter(Mandatory)][string]$Name)
    $ok = $true
    $sessionId = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
    try {
        $processes = @([System.Diagnostics.Process]::GetProcessesByName($Name) |
            Where-Object { $_.SessionId -eq $sessionId })
    }
    catch {
        Write-Host "[FAIL] process enumeration failed ($Name): $($_.Exception.Message)"
        return $false
    }
    foreach ($process in $processes) {
        if (-not (Stop-ProcessAndConfirm -Process $process)) { $ok = $false }
    }
    try {
        $remaining = @([System.Diagnostics.Process]::GetProcessesByName($Name) |
            Where-Object { $_.SessionId -eq $sessionId })
        if ($remaining.Count -gt 0) { $ok = $false }
    } catch { $ok = $false }
    return $ok
}

function Invoke-BoundedProcess {
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [Parameter(Mandatory)][string[]]$ArgumentList,
        [int]$TimeoutMs = 10000,
        $Job = $null
    )
    $process = $null
    try {
        $process = Start-ArgumentProcess -FilePath $FilePath `
            -ArgumentList $ArgumentList -Job $Job
        if ($process.WaitForExit($TimeoutMs)) { return $process.ExitCode }
        [void](Stop-ProcessAndConfirm -Process $process)
        Write-Host "[FAIL] helper process timed out: $FilePath"
        return 2
    } catch {
        Write-Host "[FAIL] helper process failed ($FilePath): $($_.Exception.Message)"
        if ($null -ne $process) { [void](Stop-ProcessAndConfirm -Process $process) }
        return 2
    }
}

function Invoke-OwnedSystemCleanup {
    param(
        [Parameter(Mandatory)][string]$MarkerPath,
        [Parameter(Mandatory)][string]$Token,
        [Parameter(Mandatory)][string]$Secret,
        [Parameter(Mandatory)][string]$ExpectedTipDll,
        [Parameter(Mandatory)][string]$TrustedRoot,
        [Parameter(Mandatory)][string]$CleanupDirectory,
        [switch]$ForTesting
    )
    if (-not (Test-Path -LiteralPath $MarkerPath)) {
        if (Test-TipRegistrationAbsent) { return $true }
        Write-Host '[FAIL] cleanup marker is missing while machine TIP registration remains'
        return $false
    }

    $ok = $true
    $pinned = $null
    try {
        if (-not (Test-ValidationCleanupDirectory -Path $CleanupDirectory `
            -ForTesting:$ForTesting)) {
            throw 'cleanup directory failed ACL/reparse validation'
        }
        $expectedMarker = [IO.Path]::GetFullPath((Join-Path $CleanupDirectory 'marker.json'))
        if (-not [IO.Path]::GetFullPath($MarkerPath).Equals(
            $expectedMarker, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'cleanup marker path is outside the owned directory'
        }
        $markerItem = Get-Item -LiteralPath $MarkerPath -Force -ErrorAction Stop
        if ($markerItem.PSIsContainer -or $markerItem.Length -gt 16384 -or
            ($markerItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'cleanup marker is not a bounded regular file'
        }
        $record = Get-Content -LiteralPath $MarkerPath -Raw -ErrorAction Stop | ConvertFrom-Json
        if ([int]$record.Version -ne 2) { throw 'cleanup marker version mismatch' }
        if ([string]$record.Token -cne $Token) { throw 'cleanup marker token mismatch' }
        $sourceTipDll = [IO.Path]::GetFullPath([string]$record.SourceTipDll)
        $expected = [IO.Path]::GetFullPath($ExpectedTipDll)
        if (-not $sourceTipDll.Equals($expected, [StringComparison]::OrdinalIgnoreCase)) {
            throw "unexpected TIP source in cleanup marker: $sourceTipDll"
        }
        $registeredTipDll = [IO.Path]::GetFullPath([string]$record.RegisteredTipDll)
        $expectedRegistered = [IO.Path]::GetFullPath(
            (Join-Path $CleanupDirectory 'nospacekey_tip.dll'))
        if (-not $registeredTipDll.Equals(
            $expectedRegistered, [StringComparison]::OrdinalIgnoreCase)) {
            throw "unexpected protected TIP cleanup target: $registeredTipDll"
        }
        $recordHash = ([string]$record.Sha256).ToLowerInvariant()
        $recordMac = ([string]$record.Mac).ToLowerInvariant()
        if ($recordHash -notmatch '^[0-9a-f]{64}$' -or $recordMac -notmatch '^[0-9a-f]{64}$') {
            throw 'cleanup marker hash/MAC is malformed'
        }
        $expectedMac = Get-AdminCleanupMarkerMac -Token $Token `
            -SourceTipDll ([string]$record.SourceTipDll) `
            -RegisteredTipDll ([string]$record.RegisteredTipDll) `
            -Sha256 ([string]$record.Sha256) -Secret $Secret
        if (-not [Security.Cryptography.CryptographicOperations]::FixedTimeEquals(
            [Convert]::FromHexString($recordMac), [Convert]::FromHexString($expectedMac))) {
            throw 'cleanup marker MAC mismatch'
        }
        # The elevated target registered this exact ACL-protected snapshot. Use
        # the same immutable file for /u; never reopen the workspace source.
        $pinned = Open-PinnedValidationTipDll -TipDll $registeredTipDll `
            -ExpectedPath $expectedRegistered -TrustedRoot $CleanupDirectory
        if ($pinned.Sha256 -cne $recordHash) {
            throw 'protected TIP snapshot changed after registration'
        }
    } catch {
        Write-Host "[FAIL] TIP cleanup marker is invalid: $($_.Exception.Message)"
        if ($null -ne $pinned) { $pinned.Stream.Dispose() }
        return $false
    }

    try {
        $unregister = Invoke-BoundedRegsvr32 -TipDll $pinned.Path -Unregister
        if ($unregister.ExitCode -ne 0 -or -not (Test-TipRegistrationAbsent)) {
            Write-Host "[FAIL] TIP unregister failed during timeout cleanup (exit $($unregister.ExitCode))"
            $ok = $false
        }
    } finally {
        $pinned.Stream.Dispose()
    }

    # Registration is machine-wide. Host validation is serialized by the mutex
    # held by this elevated supervisor (or by a direct elevated target), and this
    # branch is reached only after this run wrote an authenticated marker.
    if (-not (Stop-NamedProcessesAndConfirm -Name 'NospacekeyEngineHost')) { $ok = $false }
    # Do not delete marker.json or individual runtime files here. The outer
    # Remove-ValidationCleanupDirectory transaction removes the complete tree
    # and keeps/restores the authenticated marker on any failure.
    return $ok
}

function Read-ValidationInvocation {
    param([Parameter(Mandatory)][string]$Base64)
    try {
        $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Base64))
        $payload = ConvertFrom-Json -InputObject $json
    } catch {
        throw "validation invocation is not valid Base64 JSON: $($_.Exception.Message)"
    }
    if ([int]$payload.Version -ne 1 -or
        [string]::IsNullOrWhiteSpace([string]$payload.Target)) {
        throw 'validation invocation version/target is invalid'
    }
    $seen = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase)
    foreach ($record in @($payload.Parameters)) {
        $name = [string]$record.Name
        $kind = [string]$record.Kind
        if ($name -notmatch '^[A-Za-z][A-Za-z0-9]*$' -or -not $seen.Add($name)) {
            throw "validation invocation contains an invalid/duplicate parameter: $name"
        }
        switch ($kind) {
            'Switch' {
                if ($record.Value -isnot [bool]) {
                    throw "validation switch value is not boolean: $name"
                }
            }
            'Scalar' {
                if ($null -eq $record.PSObject.Properties['Value']) {
                    throw "validation scalar value is missing: $name"
                }
            }
            'StringArray' {
                foreach ($value in @($record.Values)) {
                    if ($value -isnot [string]) {
                        throw "validation array contains a non-string value: $name"
                    }
                }
            }
            'Null' { }
            default { throw "validation parameter kind is invalid: $kind" }
        }
    }
    return $payload
}

$recoveryScope = if ($ValidationOnly) { 'User' } else { 'Admin' }
if (-not (Enter-ValidationGateLock -RequireOwnership -RecoveryScope $recoveryScope)) {
    Write-Host '[FAIL] another nospacekey validation is already running'
    exit 2
}

$target = $null
$cleanupDirectory = $null
$cleanupComplete = $false
$retainRegistration = $false
$supervisorExit = 2
try {
    if (-not $ValidationOnly -and -not $SkipSystemCleanupForTesting -and
        -not (Test-IsAdmin)) {
        throw 'admin validation supervisor must be started from an elevated PowerShell'
    }
    $payload = Read-ValidationInvocation -Base64 $InvocationBase64
    $allowedTargetNames = if ($ValidationOnly) {
        @('run-gate.ps1', 'verify-sp6c.ps1', 'verify-sp7.ps1')
    } else {
        @('verify-manual.ps1', 'verify-harness.ps1', 'verify-sp6b.ps1')
    }
    $allowedTargets = @($allowedTargetNames | ForEach-Object {
        [IO.Path]::GetFullPath((Join-Path $PSScriptRoot $_))
    })
    $decodedTarget = [IO.Path]::GetFullPath([string]$payload.Target)
    if (-not @($allowedTargets | Where-Object {
        $_.Equals($decodedTarget, [StringComparison]::OrdinalIgnoreCase)
    })) {
        throw "decoded invocation requested an unrecognized target: $decodedTarget"
    }

    if (-not $ValidationOnly) {
        $cleanupToken = [Guid]::NewGuid().ToString('N')
        $cleanupSecretBytes = [Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
        try { $cleanupSecret = [Convert]::ToBase64String($cleanupSecretBytes) }
        finally { [Array]::Clear($cleanupSecretBytes, 0, $cleanupSecretBytes.Length) }
        $cleanupDirectory = New-AdminCleanupDirectory -Token $cleanupToken `
            -ForTesting:$SkipSystemCleanupForTesting
        $cleanupMarker = Join-Path $cleanupDirectory 'marker.json'
        $env:NOSPACEKEY_ADMIN_CLEANUP_DIRECTORY = $cleanupDirectory
        $env:NOSPACEKEY_ADMIN_CLEANUP_TOKEN = $cleanupToken
        $env:NOSPACEKEY_ADMIN_CLEANUP_SECRET = $cleanupSecret
        $env:NOSPACEKEY_ADMIN_CLEANUP_MARKER = $cleanupMarker
    }

    $leaseId = [Guid]::NewGuid().ToString('N')
    $env:NOSPACEKEY_VALIDATION_GATE_LEASE_ID = $leaseId
    $env:NOSPACEKEY_VALIDATION_GATE_SUPERVISOR_PID = [string]$PID
    $env:NOSPACEKEY_VALIDATION_GATE_SCOPE = $recoveryScope
    $env:NOSPACEKEY_VALIDATION_INVOCATION_BASE64 = $InvocationBase64
    $pwshExe = (Get-Process -Id $PID).Path
    # The target bootstrap is fixed code. Target path and bound parameters stay
    # data in the already-validated JSON payload, including string-array values.
    $bootstrap = @'
$ErrorActionPreference = 'Stop'
try {
    $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String(
        [string]$env:NOSPACEKEY_VALIDATION_INVOCATION_BASE64))
    $payload = ConvertFrom-Json -InputObject $json
    $parameters = @{}
    foreach ($record in @($payload.Parameters)) {
        $name = [string]$record.Name
        switch ([string]$record.Kind) {
            'Switch' { $parameters[$name] = [bool]$record.Value }
            'Scalar' { $parameters[$name] = [string]$record.Value }
            'StringArray' { $parameters[$name] = [string[]]@($record.Values) }
            'Null' { $parameters[$name] = $null }
            default { throw "invalid validation parameter kind: $($record.Kind)" }
        }
    }
    Remove-Item Env:\NOSPACEKEY_VALIDATION_INVOCATION_BASE64 -ErrorAction SilentlyContinue
    $global:LASTEXITCODE = 0
    & ([string]$payload.Target) @parameters
    $targetSucceeded = $?
    $targetExitCode = [int]$LASTEXITCODE
    # `exit N` inside an invoked .ps1 returns control here with
    # $LASTEXITCODE=N; propagate it instead of turning every target into exit 0.
    if (-not $targetSucceeded -and $targetExitCode -eq 0) { exit 1 }
    exit $targetExitCode
} catch {
    Write-Host "[FAIL] validation target bootstrap failed: $($_.Exception.Message)"
    exit 2
}
'@
    $bootstrapBase64 = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($bootstrap))
    $targetArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass',
        '-OutputFormat', 'Text', '-EncodedCommand', $bootstrapBase64)
    # Start suspended, assign to the unnamed crash-containment Job and the fixed
    # recovery Job, then resume. The target cannot execute even one instruction
    # outside supervisor ownership.
    $target = Start-ArgumentProcess -FilePath $pwshExe -ArgumentList $targetArgs `
        -Job $script:ValidationGateJob -RecoveryJob $script:ValidationGateRecoveryJob

    if (-not $ValidationOnly) {
        $cleanupArgs = @{
            MarkerPath = $cleanupMarker
            Token = $cleanupToken
            Secret = $cleanupSecret
            ExpectedTipDll = $script:ValidationTipDll
            TrustedRoot = $script:ValidationRepoRoot
            CleanupDirectory = $cleanupDirectory
        }
    }
    if ($target.WaitForExit($TimeoutSec * 1000)) {
        $targetExit = $target.ExitCode
        # WaitForExit covers only the target, not its descendants. Terminate and
        # positively drain the target phase before any registry cleanup helper.
        if (-not (Reset-ValidationGateJob)) {
            Write-Host '[FAIL] validation target job did not drain; cleanup skipped'
            $supervisorExit = 2
        } elseif ($ValidationOnly) {
            $supervisorExit = $targetExit
        } elseif ($targetExit -ne 0 -and (Test-Path -LiteralPath $cleanupMarker)) {
            if ($SkipSystemCleanupForTesting -or (Invoke-OwnedSystemCleanup @cleanupArgs)) {
                $cleanupComplete = $true
                $supervisorExit = $targetExit
            } else {
                $supervisorExit = 2
            }
        } elseif ($targetExit -eq 0 -and (Test-Path -LiteralPath $cleanupMarker)) {
            # -KeepRegistered is represented by a successful target leaving its
            # authenticated marker. Keep the protected DLL path that is actually
            # recorded in InprocServer32; deleting it would corrupt registration.
            $retainRegistration = $true
            $supervisorExit = 0
            Write-Host "[OK] protected TIP registration retained: $cleanupDirectory"
        } else {
            if (Test-TipRegistrationAbsent) {
                $cleanupComplete = $true
                $supervisorExit = $targetExit
            } else {
                Write-Host '[FAIL] cleanup marker is missing while machine TIP registration remains; protected evidence retained'
                $supervisorExit = 2
            }
        }
    } else {
        Write-Host "[FAIL] validation target timed out after ${TimeoutSec}s; cleaning its process tree"
        $cleanupOk = Reset-ValidationGateJob
        if ($cleanupOk -and -not $ValidationOnly) {
            if ($SkipSystemCleanupForTesting -or (Invoke-OwnedSystemCleanup @cleanupArgs)) {
                $cleanupComplete = $true
            } else {
                $cleanupOk = $false
            }
        }
        if ($cleanupOk) { Write-Host '[OK] validation timeout cleanup confirmed' }
        else { Write-Host '[FAIL] validation timeout cleanup could not be fully confirmed' }
        $supervisorExit = 2
    }
} catch {
    Write-Host "[FAIL] validation supervisor failed: $($_.Exception.Message)"
    $supervisorExit = 2
} finally {
    if (-not $ValidationOnly -and $cleanupComplete -and -not $retainRegistration) {
        if (-not (Remove-AdminCleanupDirectory -Path $cleanupDirectory `
            -ForTesting:$SkipSystemCleanupForTesting)) {
            $supervisorExit = 2
        }
    } elseif (-not $ValidationOnly -and
        -not [string]::IsNullOrWhiteSpace($cleanupDirectory)) {
        Write-Host "[WARN] protected cleanup evidence retained: $cleanupDirectory"
    }
    try { Exit-ValidationGateLock } catch { $supervisorExit = 2 }
}
exit $supervisorExit
