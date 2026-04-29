#Requires -Version 5.1
<#
.SYNOPSIS
  Activate Visual Studio 18 (2026) from the new layout under Program Files, then build recorder-ui with VST.

.DESCRIPTION
  1. Prefers Microsoft's current layout: `Program Files\Microsoft Visual Studio\18\` (e.g. `...\18\Community`)
     where `VC\Auxiliary\Build\vcvars64.bat` lives - this is not always returned ahead of older Build Tools by vswhere.
  2. If a VS 18 install root exists but the C++ workload is not installed (no vcvars64.bat / no VC\Tools), the script
     errors out with an actionable message instead of silently falling back to older Build Tools. Pass
     -AllowLegacyFallback (or set $env:RECORDER_ALLOW_LEGACY_VS = '1') to permit falling back to whatever vswhere -latest
     returns (e.g. BuildTools 2022).
  3. If no VS 18 install root is found at all, the script uses vswhere for installation 18.x, then -latest with VC++ x64.
  4. Runs vcvars64.bat, prepends MSVC Hostx64\x64 + optional repo CMake 3.31, sets CMAKE_GENERATOR, runs cargo release vst build.

  Repo root = parent of the `scripts` folder.
#>
[CmdletBinding()]
param(
    [switch]$AllowLegacyFallback
)

$ErrorActionPreference = 'Stop'

if (-not $AllowLegacyFallback -and $env:RECORDER_ALLOW_LEGACY_VS -eq '1') {
    $AllowLegacyFallback = $true
}

function Find-VsInstanceRootFromBase {
    param([string]$BasePath)
    if ([string]::IsNullOrWhiteSpace($BasePath) -or -not (Test-Path -LiteralPath $BasePath)) {
        return $null
    }
    $direct = Join-Path $BasePath 'VC\Auxiliary\Build\vcvars64.bat'
    if (Test-Path -LiteralPath $direct) {
        return (Resolve-Path -LiteralPath $BasePath).Path
    }
    # Do not use ForEach-Object { return ... }: return exits only the scriptblock, not this function.
    foreach ($dir in (Get-ChildItem -LiteralPath $BasePath -Directory -ErrorAction SilentlyContinue)) {
        $vcvars = Join-Path $dir.FullName 'VC\Auxiliary\Build\vcvars64.bat'
        if (Test-Path -LiteralPath $vcvars) {
            return $dir.FullName
        }
    }
    return $null
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $RepoRoot

function Find-Vs18RootAny {
    param([string]$BasePath)
    if ([string]::IsNullOrWhiteSpace($BasePath) -or -not (Test-Path -LiteralPath $BasePath)) {
        return $null
    }
    if (Test-Path -LiteralPath (Join-Path $BasePath 'Common7\IDE')) {
        return (Resolve-Path -LiteralPath $BasePath).Path
    }
    foreach ($dir in (Get-ChildItem -LiteralPath $BasePath -Directory -ErrorAction SilentlyContinue)) {
        if (Test-Path -LiteralPath (Join-Path $dir.FullName 'Common7\IDE')) {
            return $dir.FullName
        }
    }
    return $null
}

# --- 1) Explicit VS 18 "channel" folder (64-bit Program Files), ahead of vswhere / old Build Tools ---
$explicit18Bases = @(
    (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\18')
    'C:\Program Files\Microsoft Visual Studio\18'
) | ForEach-Object { $_.TrimEnd('\') } | Select-Object -Unique

$install = $null
$usedVs2026 = $false
$vs18RootButNoCpp = $null

foreach ($base in $explicit18Bases) {
    $foundWithCpp = Find-VsInstanceRootFromBase -BasePath $base
    if ($null -ne $foundWithCpp) {
        $install = $foundWithCpp
        $usedVs2026 = $true
        Write-Host "Selected: Visual Studio 18 under $base -> $install" -ForegroundColor Green
        break
    }
    if ($null -eq $vs18RootButNoCpp) {
        $rootAny = Find-Vs18RootAny -BasePath $base
        if ($null -ne $rootAny) { $vs18RootButNoCpp = $rootAny }
    }
}

if ($null -eq $install -and $null -ne $vs18RootButNoCpp -and -not $AllowLegacyFallback) {
    $expectedVcvars = Join-Path $vs18RootButNoCpp 'VC\Auxiliary\Build\vcvars64.bat'
    $msg = @"
Visual Studio 18 (2026) is installed at:
  $vs18RootButNoCpp
but the C++ build tools are NOT installed under it.
Expected file is missing:
  $expectedVcvars

Fix:
  1) Open 'Visual Studio Installer'
  2) Click 'Modify' on 'Visual Studio Community 2026' (or your VS 18 edition)
  3) Check 'Desktop development with C++' (or at minimum the component
     'Microsoft.VisualStudio.Component.VC.Tools.x86.x64'), then Install
  4) Re-run this script

To intentionally build against an older Visual Studio install (e.g. Build Tools 2022)
without modifying VS 18, re-run with:
  -AllowLegacyFallback
or set the environment variable RECORDER_ALLOW_LEGACY_VS=1
"@
    throw $msg
}

# --- 2) vswhere (x86 installer), prefer catalog 18.x ---
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if ($null -eq $install) {
    if (-not (Test-Path -LiteralPath $vswhere)) {
        throw "vswhere.exe not found at $vswhere. Install Visual Studio Installer, or install VS under Program Files\Microsoft Visual Studio\18\ with the 'Desktop development with C++' workload."
    }

    $vsBaseArgs = @('-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64')
    $argsVs18 = @(
        '-version', '[18.0,19.0)',
        '-property', 'installationPath',
        '-latest'
    ) + $vsBaseArgs

    $vs18Lines = @(& $vswhere @argsVs18 2>$null)
    $vs18Install = if ($vs18Lines.Count -gt 0 -and $null -ne $vs18Lines[0]) { $vs18Lines[0].ToString().Trim() } else { '' }
    $usedVs2026 = -not [string]::IsNullOrWhiteSpace($vs18Install)

    if ($usedVs2026) {
        $install = $vs18Install
        Write-Host "Selected: Visual Studio 2026 (vswhere installation 18.x): $install" -ForegroundColor Green
    } elseif ($AllowLegacyFallback) {
        Write-Warning 'No VS 18.x with C++ tools via vswhere; -AllowLegacyFallback is set, using latest install with MSVC.'
        $fallbackArgs = @('-latest', '-property', 'installationPath') + $vsBaseArgs
        $fbLines = @(& $vswhere $fallbackArgs 2>$null)
        $install = if ($fbLines.Count -gt 0 -and $null -ne $fbLines[0]) { $fbLines[0].ToString().Trim() } else { '' }
        if (-not [string]::IsNullOrWhiteSpace($install)) {
            Write-Host "Selected (legacy fallback): $install" -ForegroundColor Yellow
        }
    } else {
        throw @"
No Visual Studio 18 install with the C++ workload was found, and no VS 18 install root
was detected under Program Files\Microsoft Visual Studio\18\. Install VS 18 with the
'Desktop development with C++' workload, or pass -AllowLegacyFallback to use whatever
vswhere returns (e.g. Build Tools 2022).
"@
    }
}

if ([string]::IsNullOrWhiteSpace($install)) {
    throw 'No Visual Studio with MSVC x64 tools found. Expected e.g. C:\Program Files\Microsoft Visual Studio\18\Community with the C++ workload.'
}

$vcvars = Join-Path $install 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path -LiteralPath $vcvars)) {
    throw "Missing vcvars64.bat: $vcvars"
}

# CMake probe: VS-bundled first, then portable, then PATH
$portableCmake = Join-Path $RepoRoot 'tools\cmake-3.31.5-windows-x86_64\bin\cmake.exe'
$vsCmake = Join-Path $install 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
$probeCmake = $null
if (Test-Path -LiteralPath $vsCmake) { $probeCmake = $vsCmake }
elseif (Test-Path -LiteralPath $portableCmake) { $probeCmake = $portableCmake }
else {
    $cmdCMake = Get-Command cmake -ErrorAction SilentlyContinue
    if ($cmdCMake) { $probeCmake = $cmdCMake.Source }
}
if (-not $probeCmake -or -not (Test-Path -LiteralPath $probeCmake)) {
    throw 'cmake.exe not found. Install CMake, add it to PATH, or place tools/cmake-3.31.5-windows-x86_64 under the repo.'
}

$cmakeHelp = & $probeCmake --help 2>&1 | Out-String
$hasVs18Gen = $cmakeHelp -match 'Visual Studio 18 2026'
if ($usedVs2026 -and $hasVs18Gen) {
    $cmakeGenerator = 'Visual Studio 18 2026'
} else {
    $cmakeGenerator = 'Visual Studio 17 2022'
    if ($usedVs2026 -and -not $hasVs18Gen) {
        Write-Warning "CMake at $probeCmake does not list 'Visual Studio 18 2026'. Using '$cmakeGenerator'."
    }
}

$prependParts = @((Split-Path -Parent $probeCmake))
if (Test-Path -LiteralPath $portableCmake) {
    $prependParts += (Split-Path -Parent $portableCmake)
}

$msvcRoot = Join-Path $install 'VC\Tools\MSVC'
$latestMsvc = Get-ChildItem -LiteralPath $msvcRoot -Directory -ErrorAction SilentlyContinue | Sort-Object { $_.Name } -Descending | Select-Object -First 1
if ($null -eq $latestMsvc) {
    throw "No MSVC toolset under $msvcRoot"
}
$msvcBin = Join-Path $latestMsvc.FullName 'bin\Hostx64\x64'
if (-not (Test-Path -LiteralPath (Join-Path $msvcBin 'cl.exe'))) {
    throw "cl.exe not found under $msvcBin"
}
$prependParts += $msvcBin

$prependJoined = ($prependParts -join ';')
if ($prependJoined) { $prependJoined += ';' }

$cmdLine = "call `"$vcvars`" >nul && set `"PATH=$prependJoined%PATH%`" && set `"CMAKE=$probeCmake`" && set `"CMAKE_GENERATOR=$cmakeGenerator`" && cd /d `"$RepoRoot`" && cargo build -p recorder-ui --features vst --release"

Write-Host "Visual Studio (instance root): $install" -ForegroundColor Cyan
Write-Host "CMAKE_GENERATOR=$cmakeGenerator (cmake: $probeCmake)" -ForegroundColor Cyan
Write-Host 'cargo build -p recorder-ui --features vst --release' -ForegroundColor DarkGray

cmd.exe /c $cmdLine
exit $LASTEXITCODE
