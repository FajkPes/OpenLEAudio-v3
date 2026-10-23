[CmdletBinding(SupportsShouldProcess=$true)]
param()
$ErrorActionPreference = 'Stop'
$sourceDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).TrimEnd('\')
if ([IO.Path]::GetFileName($sourceDir) -eq 'OpenLEAudio v4') { Write-Host 'Already named OpenLEAudio v4.'; exit 0 }
if ([IO.Path]::GetFileName($sourceDir) -ne 'OpenLEAudio v3') { throw 'Unexpected source folder; nothing renamed.' }
$parentDir = [IO.Path]::GetDirectoryName($sourceDir)
$targetDir = [IO.Path]::GetFullPath((Join-Path $parentDir 'OpenLEAudio v4'))
if ([IO.Path]::GetDirectoryName($targetDir) -ne $parentDir) { throw 'Unexpected destination.' }
if (Test-Path -LiteralPath $targetDir) { throw 'Destination already exists; nothing overwritten.' }
if (-not (Test-Path -LiteralPath (Join-Path $sourceDir 'release\OpenLEAudio4\OpenLEAudio4.exe'))) { throw 'Complete v4 build missing.' }
Set-Location -LiteralPath $parentDir
if ($PSCmdlet.ShouldProcess($sourceDir, "Rename to $targetDir")) {
    Rename-Item -LiteralPath $sourceDir -NewName 'OpenLEAudio v4' -ErrorAction Stop
    Write-Host "Done: $targetDir"
}
