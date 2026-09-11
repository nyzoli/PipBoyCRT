# Builds the release exe and zips it with the files it expects next to it.
# Usage: pwsh scripts/package.ps1   →  dist/pipboy-windows-x64.zip
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$stage = Join-Path "dist" "pipboy"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $stage "vault") | Out-Null
Copy-Item "target/release/pipboy.exe" $stage
Copy-Item "config.toml" $stage
Copy-Item "vault/CLAUDE.md" (Join-Path $stage "vault")
Copy-Item "README.md", "LICENSE" $stage
$zip = Join-Path "dist" "pipboy-windows-x64.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Get-Item $zip | Select-Object FullName, Length
