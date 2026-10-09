
$ErrorActionPreference = "Stop"

function Get-WorkspaceVersion {
    param([string]$CargoTomlPath = "Cargo.toml")
    if (-not (Test-Path $CargoTomlPath)) { return $null }
    $content = Get-Content $CargoTomlPath -Raw
    $match = [regex]::Match($content, '(?m)^\s*version\s*=\s*\"([^\"]+)\"')
    if ($match.Success) { return $match.Groups[1].Value }
    return $null
}

$Version = $Env:ULTRASEARCH_VERSION
if (-not $Version) {
    $Version = Get-WorkspaceVersion
}
if (-not $Version) {
    $Version = "0.1.0"
}

Write-Host "Building release binaries (version $Version)..."
$TargetTriple = "x86_64-pc-windows-msvc"
cargo build --locked --release --target $TargetTriple -p service -p index-worker -p ui -p launcher
if ($LASTEXITCODE -ne 0) { throw "Release binary build failed (exit $LASTEXITCODE)" }

# Honor the caller's target directory, including the managed RCH/DSR buildroot.
$TargetRoot = if ($Env:CARGO_TARGET_DIR) { $Env:CARGO_TARGET_DIR } else { "target" }
$BinDir = Join-Path (Join-Path $TargetRoot $TargetTriple) "release"
foreach ($Binary in @("service.exe", "index-worker.exe", "ui.exe", "launcher.exe")) {
    if (-not (Test-Path -LiteralPath (Join-Path $BinDir $Binary) -PathType Leaf)) {
        throw "Required installer binary missing: $(Join-Path $BinDir $Binary)"
    }
}

Write-Host "Checking for WiX Toolset..."
if (-not (Get-Command "candle.exe" -ErrorAction SilentlyContinue)) {
    Write-Warning "WiX Toolset (candle.exe/light.exe) not found in PATH."
    Write-Warning "Please install WiX Toolset v3.11 or v4: https://wixtoolset.org/releases/"
    throw "WiX is required to build the MSI; no installer was produced."
}

$WxsFile = "ultrasearch\wix\main.wxs"
$WixDir = Join-Path $TargetRoot "wix"
$ObjFile = Join-Path $WixDir "main.wixobj"
$MsiFile = Join-Path $WixDir "UltraSearch-$Version.msi"

New-Item -ItemType Directory -Force -Path $WixDir | Out-Null

Write-Host "Compiling WiX source..."
candle.exe -nologo -out $ObjFile $WxsFile -arch x64 -ext WixUtilExtension -dProductVersion="$Version" -dCargoTargetBinDir="$BinDir"
if ($LASTEXITCODE -ne 0) { throw "WiX compilation failed (exit $LASTEXITCODE)" }

Write-Host "Linking MSI..."
light.exe -nologo -out $MsiFile $ObjFile -ext WixUtilExtension -cultures:en-us
if ($LASTEXITCODE -ne 0) { throw "MSI linking failed (exit $LASTEXITCODE)" }
if (-not (Test-Path -LiteralPath $MsiFile -PathType Leaf)) { throw "MSI was not produced: $MsiFile" }

Write-Host "Success! MSI created at: $MsiFile"
