param(
    [string]$ReleaseRoot = (Join-Path $PSScriptRoot '..\..\release'),
    [string]$StageName = '.publish-staging',
    [string]$TargetName = 'OpenLEAudio4'
)
$ErrorActionPreference = 'Stop'
$release = [IO.Path]::GetFullPath($ReleaseRoot).TrimEnd('\', '/')
function ChildPath([string]$name) {
    $path = [IO.Path]::GetFullPath((Join-Path $release $name))
    if ([IO.Path]::GetDirectoryName($path) -ne $release) { throw 'Publish paths must be direct children of the release directory.' }
    return $path
}
$stage = ChildPath $StageName
$target = ChildPath $TargetName
$backup = ChildPath ('.publish-backup-' + [Guid]::NewGuid().ToString('N'))
if ($stage -eq $target) { throw 'Stage and target must be different directories.' }
foreach ($path in @($stage, $target)) {
    if ((Test-Path -LiteralPath $path) -and ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Publish paths must not be directory links.'
    }
}
$required = @('OpenLEAudio4.exe', 'OpenLEAudio4.dll', 'OpenLEAudio4.Client.exe',
    'OpenLEAudio4.pri', 'OpenLEAudio4.deps.json', 'OpenLEAudio4.runtimeconfig.json')
foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $stage $name) -PathType Leaf)) { throw "Incomplete stage: missing $name" }
}
# A process list may be incomplete in a restricted execution environment.
# A mapped/running executable refuses write access even if it was not listed.
foreach ($name in $required) {
    $path = Join-Path $target $name
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        try {
            $probe = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            $probe.Dispose()
        } catch { throw "Published files are in use or not writable. Exit OpenLEAudio through its tray icon and retry. Original package preserved. ($name)" }
    }
}
$hadTarget = Test-Path -LiteralPath $target
if ($hadTarget) { [IO.Directory]::Move($target, $backup) }
try {
    # Directory.Move fails if target exists; it never nests stage inside target.
    [IO.Directory]::Move($stage, $target)
} catch {
    if ($hadTarget) { [IO.Directory]::Move($backup, $target) }
    throw
}
if ($hadTarget) {
    # Both absolute paths were validated above; removal is only after promotion.
    try { Remove-Item -LiteralPath $backup -Recurse -Force -ErrorAction Stop }
    catch { Write-Warning "New package is installed; previous package retained at $backup ($_)" }
}
Write-Host "Published complete package: $target"
