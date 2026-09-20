[CmdletBinding(SupportsShouldProcess=$true)]
param()
$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).TrimEnd('\')
$prefix = $workspace + '\'
# Explicit build-output allowlist. Never discover deletion targets by filename patterns.
$relativeTargets = @(
    'dev\core\target',
    'dev\app\OpenLEAudio\bin',
    'dev\app\OpenLEAudio\obj',
    'dev\tests\.discovery-regression\bin',
    'dev\tests\.discovery-regression\obj'
)
# Refuse cleanup until a complete published application exists.
$published = @('release\OpenLEAudio') | ForEach-Object {
    $folder = Join-Path $workspace $_
    if ((Test-Path -LiteralPath (Join-Path $folder 'OpenLEAudio2.exe')) -and
        (Test-Path -LiteralPath (Join-Path $folder 'OpenLEAudio2.Client.exe')) -and
        (Test-Path -LiteralPath (Join-Path $folder 'OpenLEAudio2.pri'))) { $folder }
}
if (-not $published) { throw 'No complete published application found; nothing removed.' }
# Published releases remain usable, including the currently running version.
$running = @(Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
    try { if ($_.Path) { $_.Path } } catch { }
})
$totalBytes = 0L
foreach ($relative in $relativeTargets) {
    $target = [IO.Path]::GetFullPath((Join-Path $workspace $relative))
    if (-not $target.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw "Outside workspace: $target" }
    if (-not (Test-Path -LiteralPath $target)) { continue }
    # Reject junctions/symlinks in the parent chain or inside a target.
    $cursor = Get-Item -LiteralPath $target -Force
    while ($cursor -and $cursor.FullName.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        if ($cursor.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point: $($cursor.FullName)" }
        $cursor = $cursor.Parent
    }
    $contents = @(Get-ChildItem -LiteralPath $target -Recurse -Force)
    if ($contents | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }) { throw "Reparse point inside: $target" }
    if ($running | Where-Object { $_.StartsWith($target + '\', [StringComparison]::OrdinalIgnoreCase) }) {
        Write-Host "Kept (running process): $relative"
        continue
    }
    $bytes = ($contents | Where-Object { -not $_.PSIsContainer } | Measure-Object -Property Length -Sum).Sum
    if ($PSCmdlet.ShouldProcess($target, 'Remove generated build outputs')) {
        Remove-Item -LiteralPath $target -Recurse -Force
        $totalBytes += $bytes
    }
}
Write-Host ('Removed: {0:N2} GiB. Published apps, source, settings, bonds and backups preserved.' -f ($totalBytes / 1GB))
