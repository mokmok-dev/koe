# Windows HIL gate for the exact signed MSIX and CLI release artifacts.
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$bin = $env:KOE_HIL_BIN
$msix = $env:KOE_HIL_PACKAGE
$metadata = $env:KOE_HIL_UPDATE_METADATA
$expiredMetadata = $env:KOE_HIL_EXPIRED_UPDATE_METADATA
$tamperMetadata = $env:KOE_HIL_TAMPER_UPDATE_METADATA
$systemWav = if ($env:KOE_HIL_SYSTEM_TEST_WAV) { $env:KOE_HIL_SYSTEM_TEST_WAV } else { $env:KOE_HIL_TEST_WAV }
$micWav = $env:KOE_HIL_MIC_TEST_WAV
$micPlayer = $env:KOE_HIL_MIC_PLAYER
$offlineRunner = $env:KOE_HIL_OFFLINE_RUNNER
$modelSelector = $env:KOE_HIL_MODEL_SELECTOR
$platformGates = $env:KOE_HIL_PLATFORM_GATES
$root = Join-Path ([IO.Path]::GetTempPath()) "koe-hil-$([guid]::NewGuid())"
$durationSecs = if ($env:KOE_HIL_DURATION_SECS) { [int]$env:KOE_HIL_DURATION_SECS } else { 3600 }
if ($durationSecs -lt 3600) { throw "hil-windows: release soak must run for at least 3600 seconds" }
foreach ($file in @($bin, $msix, $metadata, $expiredMetadata, $tamperMetadata, $systemWav, $micWav, $micPlayer, $offlineRunner, $platformGates)) {
    if (-not $file -or -not (Test-Path $file -PathType Leaf)) { throw "hil-windows: missing required artifact/fixture: $file" }
}
New-Item -ItemType Directory -Path $root -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $repoRoot "target\hil") -Force | Out-Null

$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter signtool.exe |
    Where-Object FullName -Match '\\x64\\' | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) { throw "signtool.exe is required" }
& $signtool.FullName verify /pa /v $msix
if ($LASTEXITCODE -ne 0) { throw "MSIX signature verification failed" }
& $signtool.FullName verify /pa /v $bin
if ($LASTEXITCODE -ne 0) { throw "CLI signature verification failed" }

# Presence is mandatory: install, launch registration smoke, then uninstall.
Add-AppxPackage -Path $msix
try {
    $package = Get-AppxPackage -Name "org.mokmok.koe"
    if (-not $package) { throw "MSIX install verification failed" }
    $manifest = Get-AppxPackageManifest -Package $package
    if (-not $manifest.Package.Applications.Application) { throw "MSIX has no launchable application" }
    $before = @(Get-Process -Name "koe-desktop" -ErrorAction SilentlyContinue).Id
    Start-Process "explorer.exe" "shell:AppsFolder\$($package.PackageFamilyName)!koe"
    $launched = $null
    for ($attempt = 0; $attempt -lt 40 -and -not $launched; $attempt++) {
        Start-Sleep -Milliseconds 250
        $launched = Get-Process -Name "koe-desktop" -ErrorAction SilentlyContinue |
            Where-Object { $_.Id -notin $before } | Select-Object -First 1
    }
    if (-not $launched) { throw "MSIX application activation failed" }
    Stop-Process -Id $launched.Id -Force
    & $platformGates $bin $msix $root
    if ($LASTEXITCODE -ne 0) { throw "permission/hot-plug/sleep/recovery platform gates failed" }
} finally {
    $package = Get-AppxPackage -Name "org.mokmok.koe"
    if ($package) { Remove-AppxPackage -Package $package.PackageFullName }
}
if (Get-AppxPackage -Name "org.mokmok.koe") { throw "MSIX uninstall verification failed" }

& $bin update --data-root $root verify --metadata $metadata --target $msix --target-name (Split-Path $msix -Leaf)
if ($LASTEXITCODE -ne 0) { throw "pinned MSIX inventory verification failed" }
& $bin --output-format json update --data-root $root apply --metadata $metadata --target $bin --consent |
    Out-File (Join-Path $root "update.json") -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "pinned update verification failed" }
& $bin --output-format json update --data-root $root launch -- capabilities |
    Out-File (Join-Path $root "launched-capabilities.json") -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "updated executable launch failed" }
& $bin --output-format json update --data-root $root rollback |
    Out-File (Join-Path $root "rollback.json") -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "rollback failed" }
& $bin --output-format json update --data-root $root launch -- capabilities |
    Out-File (Join-Path $root "rollback-capabilities.json") -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "rolled-back executable launch failed" }
function Assert-UpdateRejected {
    param([string]$ExpectedCode, [string]$MetadataPath, [string]$TargetPath)
    $stderr = Join-Path $root "rejection.err"
    & $bin update --data-root $root apply --metadata $MetadataPath --target $TargetPath --consent 2> $stderr
    if ($LASTEXITCODE -eq 0) { throw "update rejection $ExpectedCode unexpectedly succeeded" }
    if (-not (Select-String -Path $stderr -SimpleMatch $ExpectedCode -Quiet)) {
        throw "update rejection did not report $ExpectedCode"
    }
}
Assert-UpdateRejected "KOE-UPDATE-REPLAY" $metadata $bin
Assert-UpdateRejected "KOE-UPDATE-EXPIRED" $expiredMetadata $bin
$tampered = Join-Path $root "tampered-update.exe"
Copy-Item $bin $tampered
[IO.File]::AppendAllText($tampered, "x")
Assert-UpdateRejected "KOE-UPDATE-TARGET-DIGEST-MISMATCH" $tamperMetadata $tampered
& $bin --output-format json doctor --data-root $root | Out-File (Join-Path $root "doctor.json") -Encoding utf8
if (-not $modelSelector) { throw "KOE_HIL_MODEL_SELECTOR is required" }
$modelInstallPath = Join-Path $root "model-install.json"
& $bin --output-format json models --data-root $root install $modelSelector --network |
    Out-File $modelInstallPath -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw "model install failed" }
$modelId = (Get-Content $modelInstallPath -Raw | ConvertFrom-Json).id
if (-not $modelId) { throw "installed model ID is absent" }
$microphones = @(& $bin --output-format json devices list --source mic | ConvertFrom-Json)
$systems = @(& $bin --output-format json devices list --source system | ConvertFrom-Json)
if ($microphones.Count -eq 0 -or $systems.Count -eq 0) { throw "microphone and loopback devices are required" }

$recordArguments = @(
    $bin, "record", "--mic", $microphones[0].id, "--system", $systems[0].id,
    "--model", $modelSelector, "--output", $root, "--consent",
    "--duration-seconds", "$durationSecs", "--sample-rate", "48000", "--channels", "1"
)
$recordJob = Start-Job -ScriptBlock {
    param($runner, $arguments)
    & $runner @arguments
    if ($LASTEXITCODE -ne 0) { throw "recording failed with exit code $LASTEXITCODE" }
} -ArgumentList $offlineRunner, (, $recordArguments)
$ready = $null
for ($attempt = 0; $attempt -lt 120 -and -not $ready; $attempt++) {
    Start-Sleep -Milliseconds 250
    $ready = Get-ChildItem (Join-Path $root "sessions") -Filter "session.json" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { (Get-Content $_.FullName -Raw | ConvertFrom-Json).state -eq "recording" } |
        Select-Object -First 1
}
if (-not $ready) {
    Stop-Job $recordJob
    throw "hil-windows: capture did not become ready"
}
$micProcess = Start-Process -FilePath $micPlayer -ArgumentList @($micWav) -PassThru
$systemPlayer = New-Object System.Media.SoundPlayer $systemWav
$systemPlayer.Play()
try {
    Wait-Job $recordJob | Out-Null
    Receive-Job $recordJob
    if ($recordJob.State -ne "Completed") { throw "recording job failed: $($recordJob.State)" }
} finally {
    $systemPlayer.Stop()
    if (-not $micProcess.HasExited) { $micProcess.Kill() }
    Remove-Job $recordJob -Force
}

$session = Get-ChildItem (Join-Path $root "sessions") -Directory | Select-Object -First 1
if (-not $session) { throw "hil-windows: no session produced" }
$metrics = Join-Path $repoRoot "target\hil\metrics.json"
python (Join-Path $repoRoot "scripts\hil\report_metrics.py") $session.FullName $metrics `
    --system-expected $systemWav --mic-expected $micWav
if ($LASTEXITCODE -ne 0) { throw "HIL metric validation failed" }
if (-not (Test-Path (Join-Path $session.FullName "transcript\final.json"))) { throw "offline transcript is absent" }
& $bin models --data-root $root remove $modelId
if ($LASTEXITCODE -ne 0) { throw "model unload/remove lifecycle failed" }
