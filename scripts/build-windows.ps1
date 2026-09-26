# Builds Cropmark on Windows (NSIS installer, or a debug binary with -Debug).
param(
    [switch]$Debug,
    [switch]$RemapOnly
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

function Set-CropmarkRemapVar {
    param([string]$Name, [string]$Value)
    Set-Item -Path "Env:$Name" -Value $Value
    if ($env:GITHUB_ENV) {
        $utf8 = New-Object System.Text.UTF8Encoding $false
        [System.IO.File]::AppendAllText($env:GITHUB_ENV, "$Name=$Value`n", $utf8)
    }
}

# Keep flag order aligned with src-tauri/.cargo/config.toml (last match wins).
function Export-CropmarkReleasePathRemap {
    param([switch]$LinkPdbAltPath)

    if (-not $env:USERPROFILE) {
        throw "USERPROFILE is required to remap release build paths."
    }

    $repo = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
    $userHome = [System.IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\')
    if ($env:CARGO_HOME) {
        $cargoHome = [System.IO.Path]::GetFullPath($env:CARGO_HOME).TrimEnd('\')
    } else {
        $cargoHome = Join-Path $userHome ".cargo"
    }
    if ($env:RUSTUP_HOME) {
        $rustupHome = [System.IO.Path]::GetFullPath($env:RUSTUP_HOME).TrimEnd('\')
    } else {
        $rustupHome = Join-Path $userHome ".rustup"
    }

    $native = [ordered]@{
        CROPMARK_REMAP_HOME        = $userHome
        CROPMARK_REMAP_RUSTUP_HOME = $rustupHome
        CROPMARK_REMAP_CARGO_HOME  = $cargoHome
        CROPMARK_REMAP_REPO        = $repo
    }
    $to = @{
        CROPMARK_REMAP_HOME        = "/home"
        CROPMARK_REMAP_RUSTUP_HOME = "/rustup"
        CROPMARK_REMAP_CARGO_HOME  = "/cargo"
        CROPMARK_REMAP_REPO        = "/cropmark"
    }

    $flags = @("--remap-path-scope=all")
    foreach ($name in $native.Keys) {
        $path = $native[$name]
        $alt = $path -replace '\\', '/'
        if ($path -match '^[A-Za-z]:\\') {
            $verbatim = "\\?\$path"
        } else {
            $verbatim = "CROPMARK_REMAP_UNSET"
        }
        Set-CropmarkRemapVar "${name}_VERBATIM" $verbatim
        Set-CropmarkRemapVar "${name}_ALT" $alt
        Set-CropmarkRemapVar $name $path
        $flags += "--remap-path-prefix=${verbatim}=$($to[$name])"
        $flags += "--remap-path-prefix=${alt}=$($to[$name])"
        $flags += "--remap-path-prefix=${path}=$($to[$name])"
    }

    if ($LinkPdbAltPath) {
        # RUSTFLAGS replaces .cargo/config.toml rustflags, so this is the full set.
        $flags += "-Clink-arg=/PDBALTPATH:%_PDB%"
        Set-CropmarkRemapVar "RUSTFLAGS" ($flags -join " ")
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "scripts/build-windows.ps1 must run on Windows."
}

Export-CropmarkReleasePathRemap -LinkPdbAltPath:($RemapOnly -or -not $Debug)
if ($RemapOnly) {
    return
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
