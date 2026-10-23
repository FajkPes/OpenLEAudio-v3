$ErrorActionPreference = 'Stop'
$buildRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\build'))
$root = Join-Path $buildRoot ('publish-test-' + [Guid]::NewGuid().ToString('N'))
$publisher = Join-Path $PSScriptRoot '..\scripts\publish-stage.ps1'
$required = @('OpenLEAudio4.exe', 'OpenLEAudio4.dll', 'OpenLEAudio4.Client.exe',
    'OpenLEAudio4.pri', 'OpenLEAudio4.deps.json', 'OpenLEAudio4.runtimeconfig.json')
function MakePackage([string]$folder, [string]$content) {
    New-Item -ItemType Directory -Path $folder -Force | Out-Null
    foreach ($name in $required) { [IO.File]::WriteAllText((Join-Path $folder $name), $content) }
}
function Check([bool]$ok, [string]$message) { if (-not $ok) { throw $message } }
try {
    $stage = Join-Path $root '.publish-staging'
    $target = Join-Path $root 'OpenLEAudio4'
    MakePackage $target 'old'
    MakePackage $stage 'new'
    & $publisher -ReleaseRoot $root
    Check ((Get-Content -LiteralPath (Join-Path $target $required[0]) -Raw) -eq 'new') 'New package was not promoted.'
    Check (-not (Test-Path -LiteralPath $stage)) 'Stage was nested or left behind.'
    Check (@(Get-ChildItem -LiteralPath $root -Directory).Count -eq 1) 'Unexpected backup after successful promotion.'
    Write-Host 'PASS: complete directory promotion'

    MakePackage $stage 'next'
    $held = [IO.File]::Open((Join-Path $target $required[0]), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $refused = $false
        try { & $publisher -ReleaseRoot $root } catch { $refused = $true }
        Check $refused 'A locked package must refuse publication.'
        foreach ($name in $required) {
            Check ((Get-Content -LiteralPath (Join-Path $target $name) -Raw) -eq 'new') "Locked package was modified: $name"
        }
        Check (Test-Path -LiteralPath (Join-Path $stage $required[0])) 'Failed publish discarded the candidate.'
        Check (-not (Test-Path -LiteralPath (Join-Path $target '.publish-staging'))) 'Stage was nested in target.'
    } finally { $held.Dispose() }
    Write-Host 'PASS: locked package stays complete and candidate remains available'

    Remove-Item -LiteralPath (Join-Path $stage 'OpenLEAudio4.pri')
    $refused = $false
    try { & $publisher -ReleaseRoot $root } catch { $refused = $true }
    Check $refused 'Incomplete stages must be refused.'
    Check ((Get-Content -LiteralPath (Join-Path $target $required[0]) -Raw) -eq 'new') 'Incomplete publish modified target.'
    Write-Host 'PASS: incomplete package is refused'

    $refused = $false
    try { & $publisher -ReleaseRoot $root -StageName '..\outside' } catch { $refused = $true }
    Check $refused 'Paths outside release root must be refused.'
    Write-Host 'PASS: publish paths stay inside release root'
} finally {
    $resolved = [IO.Path]::GetFullPath($root)
    if ([IO.Path]::GetDirectoryName($resolved) -ne $buildRoot -or -not [IO.Path]::GetFileName($resolved).StartsWith('publish-test-')) {
        throw 'Refusing to clean unexpected test directory.'
    }
    if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
