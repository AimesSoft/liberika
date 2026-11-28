param(
    [string]$ReviewRoot = (Join-Path $env:LOCALAPPDATA 'Temp\erika-pr-review-20261005'),
    [string]$StudioRoot = (Join-Path $env:LOCALAPPDATA 'ErikaReviewTools\DevEcoStudio-26.0.0.851'),
    [string]$WorktreeRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..\..')).Path,
    [switch]$Baseline
)
$ErrorActionPreference = 'Stop'
$env:DEVECO_CLI_STUDIO_PATH = $StudioRoot
$cli = Join-Path $ReviewRoot 'deveco-cli\node_modules\.bin\devecocli.cmd'
$appName = if ($Baseline) { 'ohos-baseline-app' } else { 'ohos-live-app' }
$bundle = if ($Baseline) { 'com.aimesoft.erika.baseline' } else { 'com.aimesoft.erika.review' }
$app = Join-Path $ReviewRoot $appName
if ($Baseline) {
    $baselineSource = Join-Path $ReviewRoot 'ohos-baseline-src'
    New-Item -ItemType Directory -Force -Path $baselineSource | Out-Null
    foreach ($bridge in @('erika_ohos', 'erika_flutter_plugin')) {
        $relative = if ($bridge -eq 'erika_ohos') {
            'packages/erika_ohos/src/main/cpp/erika_ohos.cpp'
        } else { 'packages/erika_flutter/ohos/src/main/cpp/erika_flutter_plugin.cpp' }
        $revision = '4390c78f4467e2047e19690700fbead6a18d8e31:' + $relative
        $original = (& git -C $WorktreeRoot show $revision) -join "`n"
        if ($LASTEXITCODE -ne 0) { throw 'Could not read baseline bridge' }
        [IO.File]::WriteAllText((Join-Path $baselineSource "$bridge.cpp"), $original + "`n", [Text.UTF8Encoding]::new($false))
    }
}
if (-not (Test-Path -LiteralPath (Join-Path $app 'build-profile.json5'))) {
    & $cli create --project-path $app --app-name ErikaReview --bundle-name $bundle --api-level 26
    if ($LASTEXITCODE -ne 0) { throw 'DevEco project creation failed' }
}
$profilePath = Join-Path $app 'build-profile.json5'
$profile = (Get-Content -LiteralPath $profilePath -Raw).
    Replace('"compatibleSdkVersion": "26.0.0"', '"compatibleSdkVersion": "5.1.0(18)"').
    Replace('"signingConfig": "default",', '')
[IO.File]::WriteAllText($profilePath, $profile, [Text.UTF8Encoding]::new($false))
$entry = Join-Path $app 'entry'
$source = if ($Baseline) { 'baseline-Index.ets' } else { 'Index.ets' }
Copy-Item -LiteralPath (Join-Path $PSScriptRoot $source) -Destination (Join-Path $entry 'src\main\ets\pages\Index.ets')
$raw = Join-Path $entry 'src\main\resources\rawfile'
New-Item -ItemType Directory -Force -Path $raw, (Join-Path $entry 'libs\x86_64') | Out-Null
foreach ($fixture in @('bbb-av-10s.mp4', 'cover-5s.mp3')) {
    Copy-Item -LiteralPath (Join-Path $ReviewRoot $fixture) -Destination $raw
}
foreach ($bridge in @('ohos', 'flutter')) {
    $types = Join-Path $entry "src\main\cpp\types\liberika_$bridge"
    New-Item -ItemType Directory -Force -Path $types | Out-Null
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "liberika_$bridge\index.d.ts") -Destination $types
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "liberika_$bridge\oh-package.json5") -Destination $types
}
$packagePath = Join-Path $entry 'oh-package.json5'
$package = Get-Content -LiteralPath $packagePath -Raw
if (-not $package.Contains('liberika_ohos.so')) {
    $package = $package.Replace('"dependencies": {', '"dependencies": {"liberika_ohos.so":"file:./src/main/cpp/types/liberika_ohos", "liberika_flutter.so":"file:./src/main/cpp/types/liberika_flutter",')
    [IO.File]::WriteAllText($packagePath, $package, [Text.UTF8Encoding]::new($false))
}
Write-Output "Prepared $app. Build and stage native libraries, then run devecocli build from this directory."
