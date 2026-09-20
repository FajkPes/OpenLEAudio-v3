# Shared tools and validation for the exact INF/CAT pair being installed.
function Find-DriverSignTool {
    $tool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    if (-not $tool) { throw 'signtool.exe is missing. Install Windows SDK signing tools.' }
    return $tool
}

function Get-DriverInf2Cat {
    $cached = Join-Path (Split-Path $PSScriptRoot -Parent) 'runtime-data\wdk-tools\Inf2Cat.exe'
    if (Test-Path -LiteralPath $cached) { return $cached }
    $installed = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter Inf2Cat.exe -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    if ($installed) { return $installed }

    # Official Microsoft WDK NuGet package; local tools only, no system install.
    Write-Host 'Downloading Microsoft WDK catalog tools (about 106 MB)...'
    $runtime = Join-Path (Split-Path $PSScriptRoot -Parent) 'runtime-data'
    New-Item -ItemType Directory -Path $runtime -Force | Out-Null
    $archive = Join-Path $runtime 'microsoft.windows.wdk.x64.10.0.26100.1882.nupkg'
    $url = 'https://api.nuget.org/v3-flatcontainer/microsoft.windows.wdk.x64/10.0.26100.1882/microsoft.windows.wdk.x64.10.0.26100.1882.nupkg'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archive
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $prefix = 'c/bin/10.0.26100.0/x86/'
        $destination = Split-Path $cached -Parent
        New-Item -ItemType Directory -Path $destination -Force | Out-Null
        foreach ($entry in $zip.Entries) {
            if (-not $entry.FullName.StartsWith($prefix, [StringComparison]::Ordinal)) { continue }
            $leaf = $entry.FullName.Substring($prefix.Length)
            # This tool has flat companion DLLs. Do not extract arbitrary paths.
            if (-not $leaf -or $leaf.Contains('/') -or $leaf.Contains('\') -or $leaf -eq '..') { continue }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, (Join-Path $destination $leaf), $true)
        }
    } finally { $zip.Dispose() }
    if (-not (Test-Path -LiteralPath $cached)) { throw 'Inf2Cat was not found in the WDK package.' }
    return $cached
}

function Test-DriverPackage {
    param([string]$Inf, [string]$Catalog, [string]$SignTool)
    if (-not (Test-Path -LiteralPath $Inf) -or -not (Test-Path -LiteralPath $Catalog)) { return $false }
    # Catalog signature alone says nothing about whether this INF is included.
    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        & $SignTool verify /pa /c $Catalog $Inf *> $null
        return ($LASTEXITCODE -eq 0)
    } finally { $ErrorActionPreference = $previousPreference }
}
