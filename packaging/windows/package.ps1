# Builds and optionally signs the koe desktop MSIX package.
#
# Environment:
#   KOE_RELEASE_VERSION          semver, optional leading "v"
#   KOE_RELEASE_TARGET           Rust target triple (default x86_64 MSVC)
#   KOE_SIGNING_CERT_SHA1        thumbprint already imported in CurrentUser\My
#   KOE_MSIX_PUBLISHER           certificate subject (default CN=mokmok-dev)
#   KOE_REQUIRE_SIGNING          1 to reject an unsigned package
#   KOE_WINDOWS_KIT_ROOT         Windows SDK root (optional)
#   KOE_SKIP_BUILD               1 when binaries were already built and signed
$ErrorActionPreference = "Stop"

function Get-SdkTool {
    param([string]$Name)
    $sdkRoot = $env:KOE_WINDOWS_KIT_ROOT
    if (-not $sdkRoot) {
        $sdkRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    }
    $tool = Get-ChildItem -Path $sdkRoot -Recurse -Filter $Name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $tool) { throw "$Name not found under $sdkRoot; install the Windows SDK." }
    return $tool.FullName
}

function Convert-ToMsixVersion {
    param([string]$Version)
    $numeric = $Version.TrimStart('v').Split('-')[0]
    $parts = @($numeric.Split('.'))
    if ($parts.Count -lt 3 -or $parts.Count -gt 4) {
        throw "KOE_RELEASE_VERSION must be a three- or four-part semantic version"
    }
    while ($parts.Count -lt 4) { $parts += '0' }
    foreach ($part in $parts) {
        $value = 0
        if (-not [int]::TryParse($part, [ref]$value) -or $value -lt 0 -or $value -gt 65535) {
            throw "invalid MSIX version component: $part"
        }
    }
    return ($parts -join '.')
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$releaseVersion = if ($env:KOE_RELEASE_VERSION) { $env:KOE_RELEASE_VERSION } else { "0.0.0" }
$target = if ($env:KOE_RELEASE_TARGET) { $env:KOE_RELEASE_TARGET } elseif ($env:CARGO_BUILD_TARGET) { $env:CARGO_BUILD_TARGET } else { "x86_64-pc-windows-msvc" }
$msixVersion = Convert-ToMsixVersion $releaseVersion
$publisher = if ($env:KOE_MSIX_PUBLISHER) { $env:KOE_MSIX_PUBLISHER } else { "CN=mokmok-dev" }
$architecture = if ($target.StartsWith('aarch64')) { 'arm64' } else { 'x64' }
$releaseDir = Join-Path $repoRoot "target\$target\release"
$distDir = Join-Path $repoRoot "dist"
$staging = Join-Path $releaseDir "msix"
$output = Join-Path $distDir "koe-$releaseVersion-$architecture.msix"

Push-Location $repoRoot
try {
    if ($env:KOE_SKIP_BUILD -ne '1') {
        cargo build --release --locked --target $target -p koe-desktop
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    }

    [xml]$manifest = Get-Content "packaging/windows/AppxManifest.xml" -Raw
    $manifest.Package.Identity.SetAttribute('Version', $msixVersion)
    $manifest.Package.Identity.SetAttribute('Publisher', $publisher)

    if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
    New-Item -ItemType Directory -Path (Join-Path $staging "Assets") -Force | Out-Null
    New-Item -ItemType Directory -Path $distDir -Force | Out-Null
    Copy-Item (Join-Path $releaseDir "koe-desktop.exe") $staging
    Copy-Item "packaging/windows/Assets/*.png" (Join-Path $staging "Assets")
    $manifest.Save((Join-Path $staging "AppxManifest.xml"))

    $makeappx = Get-SdkTool "MakeAppx.exe"
    & $makeappx pack /d $staging /p $output /o
    if ($LASTEXITCODE -ne 0) { throw "MakeAppx failed" }

    $thumbprint = $env:KOE_SIGNING_CERT_SHA1
    if ($thumbprint) {
        if (-not (Test-Path "Cert:\CurrentUser\My\$thumbprint")) {
            throw "KOE_SIGNING_CERT_SHA1 is not present in CurrentUser\\My"
        }
        $signtool = Get-SdkTool "signtool.exe"
        & $signtool sign /sha1 $thumbprint /s My /fd SHA256 `
            /tr "https://timestamp.digicert.com" /td SHA256 $output
        if ($LASTEXITCODE -ne 0) { throw "signtool sign failed" }
        & $signtool verify /pa /v $output
        if ($LASTEXITCODE -ne 0) { throw "signtool verify failed" }
    } elseif ($env:KOE_REQUIRE_SIGNING -eq '1') {
        throw "KOE_SIGNING_CERT_SHA1 is required for a production package"
    } else {
        Write-Warning "KOE_SIGNING_CERT_SHA1 unset; MSIX left unsigned for local smoke testing."
    }
} finally {
    Pop-Location
}
Write-Host "wrote $output"
