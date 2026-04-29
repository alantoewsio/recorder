param(
    [string]$Target = "",
    [string]$ArtifactName = "",
    [string]$OutRoot = "dist"
)

$ErrorActionPreference = "Stop"

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repo
try {
    $cargoArgs = @("build", "-p", "recorder-sdk", "--release")
    if (-not [string]::IsNullOrWhiteSpace($Target)) {
        $cargoArgs += @("--target", $Target)
    }

    cargo @cargoArgs

    $releaseDir = if ([string]::IsNullOrWhiteSpace($Target)) {
        Join-Path $repo "target\release"
    } else {
        Join-Path $repo "target\$Target\release"
    }

    if ([string]::IsNullOrWhiteSpace($ArtifactName)) {
        if ([string]::IsNullOrWhiteSpace($Target)) {
            if ($IsWindows -or $env:OS -eq "Windows_NT") {
                $ArtifactName = "recorder-sdk-windows-x64"
            } elseif ($IsMacOS) {
                $ArtifactName = "recorder-sdk-macos-x64"
            } else {
                $ArtifactName = "recorder-sdk-linux-x64"
            }
        } else {
            $ArtifactName = "recorder-sdk-$Target"
        }
    }

    $dist = Join-Path $repo (Join-Path $OutRoot $ArtifactName)
    if (Test-Path -LiteralPath $dist) {
        Remove-Item -LiteralPath $dist -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $dist "include") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $dist "lib") | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $dist "bin") | Out-Null

    Copy-Item -LiteralPath "crates\recorder-sdk\include\recorder_sdk.h" -Destination (Join-Path $dist "include")

    $patterns = @(
        "recorder_sdk.dll",
        "recorder_sdk.dll.lib",
        "recorder_sdk.lib",
        "librecorder_sdk.dylib",
        "librecorder_sdk.so",
        "librecorder_sdk.a"
    )
    foreach ($pattern in $patterns) {
        Get-ChildItem -LiteralPath $releaseDir -Filter $pattern -ErrorAction SilentlyContinue | ForEach-Object {
            if ($_.Extension -eq ".dll" -or $_.Extension -eq ".dylib" -or $_.Extension -eq ".so") {
                Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $dist "bin")
            } else {
                Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $dist "lib")
            }
        }
    }

    $lame = Join-Path $repo "third_party\lame\windows-x64\libmp3lame.dll"
    if (Test-Path -LiteralPath $lame) {
        Copy-Item -LiteralPath $lame -Destination (Join-Path $dist "bin")
    }

    $zip = Join-Path $repo (Join-Path $OutRoot "$ArtifactName.zip")
    if (Test-Path -LiteralPath $zip) {
        Remove-Item -LiteralPath $zip -Force
    }
    Compress-Archive -Path (Join-Path $dist "*") -DestinationPath $zip -Force
    Write-Host "Created $zip"
}
finally {
    Pop-Location
}
