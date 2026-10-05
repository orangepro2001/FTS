# fts one-shot build script.
# Usage:
#   .\build.ps1              # build release + collect artifacts to dist\
#   .\build.ps1 -Test        # build + run unit tests
#   .\build.ps1 -SkipBuild   # only re-collect dist\
#   .\build.ps1 -Updater     # additionally build the updater GUI (Tauri, dev machine only)
param(
    [switch]$Test,
    [switch]$SkipBuild,
    [switch]$Updater
)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

if (-not $SkipBuild) {
    Write-Host "==> cargo build --release"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

    if ($Test) {
        Write-Host "==> cargo test --release"
        cargo test --release
        if ($LASTEXITCODE -ne 0) { throw "cargo test failed (exit $LASTEXITCODE)" }
    }
}

# Collect distributable artifacts (everything needed to run, copy-and-use) into dist\
New-Item -ItemType Directory -Force -Path dist | Out-Null
Copy-Item target\release\fts.exe dist\ -Force
Copy-Item target\release\fts.dll dist\ -Force -ErrorAction SilentlyContinue
Copy-Item include\fts.h dist\ -Force
Copy-Item config.json dist\ -Force
Copy-Item fts-daemon.ps1 dist\ -Force
Copy-Item fts-daemon.cmd dist\ -Force
Copy-Item DEPLOY.txt dist\ -Force

# W9:dist 版本号(更新器 GUI 比对目标机 Capabilities.fts_version 用)
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
Set-Content -Path dist\VERSION -Value $version -NoNewline -Encoding ascii

Write-Host ""
Write-Host "==> artifacts in dist\:"
Get-ChildItem dist | Format-Table Name, Length -AutoSize
Write-Host "done. Deploy = copy the whole dist\ folder to the target machine."

if ($Updater) {
    Write-Host ""
    Write-Host "==> building updater GUI (updater\, dev machine only) ..."
    Push-Location updater
    npm install
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "npm install failed" }
    npx tauri build
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "tauri build failed" }
    Pop-Location
    Write-Host "updater exe: updater\src-tauri\target\release\fts-updater.exe (安装包在 target\release\bundle\nsis\)"
}
