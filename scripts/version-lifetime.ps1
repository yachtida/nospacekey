<# Shared staging contract for binaries that publish a version lifetime lease. #>

$VersionLifetimeSentinelName = '.nospacekey-lifetime'
$VersionLifetimeSentinelBytes = [Text.UTF8Encoding]::new($false).GetBytes(
    "nospacekey version lifetime sentinel`n")
function Install-VersionLifetimeSentinel([Parameter(Mandatory)][string]$Directory) {
    $root = [IO.Path]::GetFullPath($Directory).TrimEnd('\')
    $rootItem = Get-Item -LiteralPath $root -Force -ErrorAction Stop
    if (-not $rootItem.PSIsContainer -or
        (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "version lifetime directory is unsafe: $root"
    }
    $sentinel = Join-Path $root $VersionLifetimeSentinelName
    $temporary = Join-Path $root ($VersionLifetimeSentinelName + '.' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        if (-not (Test-Path -LiteralPath $sentinel)) {
            [IO.File]::WriteAllBytes($temporary, $VersionLifetimeSentinelBytes)
            try { [IO.File]::Move($temporary, $sentinel) } catch {
                if (-not (Test-Path -LiteralPath $sentinel -PathType Leaf)) { throw }
            }
        }
        $item = Get-Item -LiteralPath $sentinel -Force -ErrorAction Stop
        if ($item.PSIsContainer -or
            (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
            -not [Linq.Enumerable]::SequenceEqual(
                [byte[]]([IO.File]::ReadAllBytes($sentinel)),
                [byte[]]$VersionLifetimeSentinelBytes)) {
            throw "version lifetime sentinel is invalid: $sentinel"
        }
        return $sentinel
    } finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
}
