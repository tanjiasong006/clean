[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,
    [string] $Binary = "target\release\clean.exe",
    [string] $OutDir = "dist"
)

$ErrorActionPreference = "Stop"

if (-not (Get-Command makensis.exe -ErrorAction SilentlyContinue)) {
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
$scriptPath = (Resolve-Path "packaging\windows\clean.nsi").Path
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$outPath = Join-Path (Resolve-Path $OutDir).Path "Clean-${Version}-windows-x86_64-setup.exe"

& makensis.exe `
    "/DAPP_VERSION=$Version" `
    "/DAPP_VERSION_4=$version4" `
    "/DAPP_BINARY=$binaryPath" `
    "/DAPP_OUT_FILE=$outPath" `
    $scriptPath
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Output $outPath
