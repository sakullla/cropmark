# Cargo test/run wrapper. Copies the staged DirectML.dll next to the
# executable, then runs it unchanged. Cargo's working directory is the
# package root (src-tauri).
$ErrorActionPreference = "Continue"
if ($args.Length -lt 1) {
    Write-Error "directml-runner: missing executable"
    exit 1
}
$exe = $args[0]
$rest = @()
if ($args.Length -gt 1) {
    $rest = $args[1..($args.Length - 1)]
}
$dir = Split-Path -Parent $exe
$dest = Join-Path $dir "DirectML.dll"
if (-not (Test-Path -LiteralPath $dest)) {
    $source = $null
    $cursor = $dir
    while (-not $source -and $cursor) {
        $leaf = Split-Path -Leaf $cursor
        $candidate = Join-Path $cursor "DirectML.dll"
        if (($leaf -eq "debug" -or $leaf -eq "release") -and (Test-Path -LiteralPath $candidate)) {
            $source = $candidate
        }
        $parent = Split-Path -Parent $cursor
        if (-not $parent -or $parent -eq $cursor) {
            break
        }
        $cursor = $parent
    }
    if ($source -and ($source -ne $dest)) {
        Copy-Item -LiteralPath $source -Destination $dest -Force
    }
}
& $exe @rest
$code = $LASTEXITCODE
if (-not $? -and $null -eq $code) {
    exit 1
}
if ($null -ne $code) {
    exit $code
}
exit 0
