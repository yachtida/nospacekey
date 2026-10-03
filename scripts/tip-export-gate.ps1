param([Alias('Path')][string]$TipDllPath)

$script:RequiredTipExports = @(
    'DllCanUnloadNow',
    'DllGetClassObject',
    'DllInstall',
    'DllRegisterServer',
    'DllUnregisterServer'
)

function Get-TipExportNamesFromText {
    param(
        [Parameter(Mandatory = $true)][ValidateSet('Dumpbin', 'LlvmReadobj')][string]$Kind,
        [Parameter(Mandatory = $true)][string]$Text
    )

    $names = @()
    if ($Kind -eq 'Dumpbin') {
        if ($Text -notmatch '(?im)^\s*ordinal\s+hint\s+RVA\s+name\s*$') {
            throw 'dumpbin output has no export table header'
        }
        foreach ($line in ($Text -split "`r?`n")) {
            if ($line -match '^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\S+)(?:\s+=.*)?\s*$') {
                $names += $Matches[1]
            }
        }
    } else {
        if ($Text -notmatch '(?m)^\s*Export\s*\{\s*$') {
            throw 'llvm-readobj output has no export records'
        }
        foreach ($line in ($Text -split "`r?`n")) {
            if ($line -match '^\s*Name:\s*(\S+)\s*$') {
                $names += $Matches[1]
            }
        }
    }
    if ($names.Count -eq 0) { throw "$Kind output contained no parseable exports" }
    @($names | Sort-Object -Unique)
}

function Assert-ExactTipExports {
    param([Parameter(Mandatory = $true)][string[]]$Names)
    $actual = @($Names | Sort-Object -Unique)
    # Rust's cdylib may expose symbols from statically linked dependencies. The installer
    # contract is that each COM entrypoint exists under its exact undecorated name.
    $missing = @($script:RequiredTipExports | Where-Object { $actual -cnotcontains $_ })
    if ($missing.Count -ne 0) {
        throw "TIP DLL is missing exact required exports: $($missing -join ', ')"
    }
}

function Resolve-TipExportTool {
    $candidates = @()
    if ($env:VCToolsInstallDir) {
        $candidates += [pscustomobject]@{ Kind = 'Dumpbin'; Path = (Join-Path $env:VCToolsInstallDir 'bin\Hostx64\x64\dumpbin.exe') }
    }
    $dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($dumpbin) { $candidates += [pscustomobject]@{ Kind = 'Dumpbin'; Path = $dumpbin.Source } }

    $programFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
    $vswhere = Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $found = @(& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' 2>$null)
        if ($LASTEXITCODE -eq 0) {
            foreach ($item in $found) { $candidates += [pscustomobject]@{ Kind = 'Dumpbin'; Path = [string]$item } }
        }
    }
    $llvm = Get-Command llvm-readobj.exe -ErrorAction SilentlyContinue
    if ($llvm) { $candidates += [pscustomobject]@{ Kind = 'LlvmReadobj'; Path = $llvm.Source } }

    foreach ($candidate in $candidates) {
        if ($candidate.Path -and (Test-Path -LiteralPath $candidate.Path -PathType Leaf)) { return $candidate }
    }
    throw 'no PE export reader found (dumpbin.exe or llvm-readobj.exe is required)'
}

function Assert-NospacekeyTipExports {
    param([Parameter(Mandatory = $true)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "TIP DLL not found: $Path" }
    $tool = Resolve-TipExportTool
    if ($tool.Kind -eq 'Dumpbin') {
        $lines = @(& $tool.Path /nologo /exports $Path 2>&1)
    } else {
        $lines = @(& $tool.Path --coff-exports $Path 2>&1)
    }
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) { throw "$($tool.Kind) failed with exit code $exitCode" }
    $names = @(Get-TipExportNamesFromText -Kind $tool.Kind -Text ($lines -join "`n"))
    Assert-ExactTipExports $names
}

if ($MyInvocation.InvocationName -ne '.') {
    if ([string]::IsNullOrWhiteSpace($TipDllPath)) { throw 'usage: tip-export-gate.ps1 -Path <nospacekey_tip.dll>' }
    Assert-NospacekeyTipExports -Path $TipDllPath
    Write-Host "OK: TIP exports verified: $TipDllPath"
}
