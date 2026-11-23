# Builds Clipper and packages it into installer\Output\Clipper-Setup-<version>.exe
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value

Write-Host "`n[1/3] Building Clipper $version (release)..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$ff = Join-Path $root "installer\ffmpeg.exe"
if (Test-Path $ff) {
    Write-Host "`n[2/3] Using installer\ffmpeg.exe" -ForegroundColor Cyan
} else {
    Write-Host "`n[2/3] Downloading ffmpeg (bundled into the installer)..." -ForegroundColor Cyan
    $zip = Join-Path $env:TEMP "ffmpeg-release-essentials.zip"
    $tmp = Join-Path $env:TEMP "ffmpeg-release-essentials"
    Invoke-WebRequest "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip" -OutFile $zip
    if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $tmp
    $found = Get-ChildItem $tmp -Recurse -Filter "ffmpeg.exe" | Select-Object -First 1
    if (-not $found) { throw "ffmpeg.exe not found in the downloaded archive" }
    Copy-Item $found.FullName $ff
}

Write-Host "`n[3/3] Building the installer..." -ForegroundColor Cyan
function Find-Iscc {
    $c = @(
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    )
    return $c | Where-Object { Test-Path $_ } | Select-Object -First 1
}
$iscc = Find-Iscc
if (-not $iscc) {
    Write-Host "Inno Setup not found; installing it with winget..." -ForegroundColor Yellow
    winget install --id JRSoftware.InnoSetup -e --accept-source-agreements --accept-package-agreements
    $iscc = Find-Iscc
}
if (-not $iscc) { throw "Inno Setup not found. Install it from https://jrsoftware.org/isdl.php and run this again." }

& $iscc "/DAppVersion=$version" "installer\clipper.iss"
if ($LASTEXITCODE -ne 0) { throw "Inno Setup failed" }

$setup = Join-Path $root "installer\Output\Clipper-Setup-$version.exe"
Write-Host "`nDone: $setup" -ForegroundColor Green
if (Test-Path $setup) { Start-Process explorer.exe "/select,`"$setup`"" }
