# Builds Cropmark on Windows (NSIS installer, or a debug binary with -Debug).
param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

if ($env:OS -ne "Windows_NT") {
    throw "scripts/build-windows.ps1 must run on Windows."
}

if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    throw "npm is required. Install Node.js, then retry."
}

if (-not (Test-Path (Join-Path $Root "node_modules"))) {
    npm install
    if ($LASTEXITCODE -ne 0) { throw "npm install failed." }
}

$tauriArgs = @("run", "tauri", "--", "build")
if ($Debug) {
    $tauriArgs += "--debug"
} else {
    $tauriArgs += @("--bundles", "nsis")
}

npm @tauriArgs
if ($LASTEXITCODE -ne 0) { throw "Cropmark Windows build failed." }

if ($Debug) {
    Write-Host "Cropmark debug output: src-tauri/target/debug/cropmark.exe"
} else {
    Write-Host "Cropmark NSIS output: src-tauri/target/release/bundle/nsis/"
}
