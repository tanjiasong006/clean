[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,
    [string] $Binary = "target\release\clean.exe",
    [string] $OutDir = "dist"
)

$ErrorActionPreference = "Stop"

$makensis = Get-Command makensis.exe -ErrorAction SilentlyContinue | Select-Object -First 1
$makensisPath = $null
if ($makensis) {
    $makensisPath = $makensis.Source
} else {
    $candidates = @()
    if (${env:ProgramFiles(x86)}) { $candidates += Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe" }
    if ($env:ProgramFiles) { $candidates += Join-Path $env:ProgramFiles "NSIS\makensis.exe" }
    $makensisPath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $makensisPath) {
    throw "makensis.exe was not found. Install NSIS and add it to PATH."
}

$numeric = $Version -replace '^v', ''
if ($numeric -match '^\d+(\.\d+){0,3}') {
    $numeric = $Matches[0]
} else {
    $numeric = '0.0.0'
}
$parts = @($numeric.Split('.'))
while ($parts.Count -lt 4) { $parts += '0' }
$version4 = $parts[0..3] -join '.'

$binaryPath = (Resolve-Path $Binary).Path
$iconPath = (Resolve-Path "packaging\icons\clean.ico").Path
$scriptPath = (Resolve-Path "packaging\windows\clean.nsi").Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = Join-Path (Resolve-Path $OutDir).Path "Clean-${Version}-windows-x86_64-setup.exe"

& $makensisPath `
    "/DAPP_VERSION=$Version" `
    "/DAPP_VERSION_4=$version4" `
    "/DAPP_BINARY=$binaryPath" `
    "/DAPP_ICON=$iconPath" `
    "/DAPP_OUT_FILE=$outPath" `
    $scriptPath
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Output $outPath
