# Re-populates third_party/lame: upstream LAME license texts + Windows x64 libmp3lame.dll
# Run from repo root: pwsh scripts/vendor_lame.ps1
$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$dest = Join-Path $root "third_party\lame"
$srcTar = "https://downloads.sourceforge.net/project/lame/lame/3.100/lame-3.100.tar.gz"
$dllZip = "https://www.rarewares.org/files/mp3/libmp3lame-3.100x64.zip"

New-Item -ItemType Directory -Force -Path $dest | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $dest "windows-x64") | Out-Null

$tarPath = Join-Path $dest "lame-3.100.tar.gz"
Write-Host "Downloading LAME 3.100 source tarball..."
curl.exe -sL $srcTar -o $tarPath
tar -xzf $tarPath -C $dest
Copy-Item (Join-Path $dest "lame-3.100\LICENSE") (Join-Path $dest "LICENSE") -Force
Copy-Item (Join-Path $dest "lame-3.100\COPYING") (Join-Path $dest "COPYING") -Force
Remove-Item (Join-Path $dest "lame-3.100") -Recurse -Force
Remove-Item $tarPath -Force

$zipPath = Join-Path $dest "windows-x64\libmp3lame.zip"
Write-Host "Downloading Windows x64 libmp3lame (RareWares)..."
curl.exe -sL $dllZip -o $zipPath
$expand = Join-Path $dest "windows-x64\_extract"
Remove-Item $expand -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -Path $zipPath -DestinationPath $expand -Force
Get-ChildItem $expand -Recurse -Filter "libmp3lame.dll" | Select-Object -First 1 | ForEach-Object {
    Copy-Item $_.FullName (Join-Path $dest "windows-x64\libmp3lame.dll") -Force
}
Remove-Item $expand -Recurse -Force
Remove-Item $zipPath -Force

Write-Host "Done. See third_party/lame/README.md"
