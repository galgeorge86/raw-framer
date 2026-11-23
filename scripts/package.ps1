# Builds a portable release zip for GitHub Releases:
#   dist\raw-framer-<version>-windows-x64.zip  (+ .sha256)
# Layout inside the zip (unzip anywhere and run raw-framer.exe):
#   raw-framer.exe, rf-braw-probe.exe, README.md, licenses\...
#   BlackmagicRawAPI\  Blackmagic RAW runtime (BlackmagicRawAPI.dll + CUDA/OpenCL decoders),
#                      found automatically next to the exe. Redistributed under the
#                      Blackmagic RAW SDK license, clause 1.1(d).
# Needs the Blackmagic RAW SDK installed (or BRAW_SDK_DIR) because the app is built against it.
# Nothing is uploaded: the script prints the `gh release create` command to run yourself.
param([switch]$SkipBuild)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$sdk = if ($env:BRAW_SDK_DIR) { $env:BRAW_SDK_DIR } else { "C:\Program Files (x86)\Blackmagic Design\Blackmagic RAW\Blackmagic RAW SDK" }
$libs = Join-Path $sdk "Win\Libraries"
if (-not (Test-Path (Join-Path $libs "BlackmagicRawAPI.dll"))) { throw "Blackmagic RAW SDK not found at $sdk (set BRAW_SDK_DIR)" }

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "raw-framer-$version-windows-x64"

if (-not $SkipBuild) {
    # A separate target dir so a running copy of the app doesn't block the build.
    $env:CARGO_TARGET_DIR = Join-Path $root "target\dist-build"
    cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
$bin = if ($env:CARGO_TARGET_DIR) { Join-Path $env:CARGO_TARGET_DIR "release" } else { Join-Path $root "target\dist-build\release" }

$dist = Join-Path $root "dist"
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force (Join-Path $stage "BlackmagicRawAPI"), (Join-Path $stage "licenses") | Out-Null

Copy-Item (Join-Path $bin "raw-framer.exe"), (Join-Path $bin "rf-braw-probe.exe") $stage
Copy-Item README.md $stage
Copy-Item images (Join-Path $stage "images") -Recurse
# x64 runtime only (the ARM64 sub-folders are not needed).
Get-ChildItem $libs -File -Filter *.dll | Copy-Item -Destination (Join-Path $stage "BlackmagicRawAPI")
$docs = Join-Path $sdk "Documentation"
foreach ($f in "License.rtf", "Third Party Licenses.rtf") {
    $src = Join-Path $docs $f
    if (Test-Path $src) { Copy-Item $src (Join-Path $stage "licenses\Blackmagic RAW - $f") }
}
Get-ChildItem app\assets\fonts -File | Where-Object { $_.Extension -eq ".txt" } | Copy-Item -Destination (Join-Path $stage "licenses")
Copy-Item LICENSE (Join-Path $stage "LICENSE.txt")
Copy-Item USE-PERMISSION.md (Join-Path $stage "USE-PERMISSION.md")
# Notices for the Rust libraries compiled into the exe (MIT / Apache-2.0 etc. require them).
if (-not (Get-Command cargo-about -ErrorAction SilentlyContinue)) { throw "install cargo-about first: cargo install --locked cargo-about --features cli" }
cargo about generate --workspace -c (Join-Path $PSScriptRoot "about.toml") -o (Join-Path $stage "licenses\THIRD_PARTY_LICENSES.md") (Join-Path $PSScriptRoot "about.hbs")
if ($LASTEXITCODE -ne 0) { throw "cargo about failed" }

$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
"$hash  $name.zip" | Out-File -Encoding ascii (Join-Path $dist "$name.zip.sha256")

Write-Host ""
Write-Host "Packaged $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"
Write-Host "SHA256 $hash"
Write-Host ""
Write-Host "To publish (after committing and pushing):"
Write-Host "  gh release create v$version `"$zip`" `"$zip.sha256`" --title `"Raw Framer $version`" --notes-file <notes.md>"
