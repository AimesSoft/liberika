param(
    [Parameter(Mandatory)][string]$Hdc,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [Parameter(Mandatory)][int]$ApiVersion,
    [Parameter(Mandatory)][string]$OperatingSystem,
    [string]$TargetSerial = '127.0.0.1:5555',
    [int]$TimeoutSeconds = 600
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
$previous = ''
do {
    $complete = 0
    $progress = @()
    foreach ($bridge in @('arkts', 'flutter')) {
        $destination = Join-Path $OutputDirectory "ohos-$bridge-review.jsonl"
        & $Hdc -t $TargetSerial file recv "/data/app/el2/100/base/com.aimesoft.erika.review/haps/entry/files/$bridge-review.jsonl" $destination | Out-Null
        if ($LASTEXITCODE -ne 0 -or !(Test-Path -LiteralPath $destination)) { continue }
        try { $rows = @(Get-Content -LiteralPath $destination | ConvertFrom-Json) }
        catch { continue } # A snapshot may race the app's JSONL rewrite.
        if ($rows.stage -contains 'FAILED') { throw "$bridge failed: $(($rows | Where-Object stage -eq 'FAILED').value)" }
        $stage = ($rows | Where-Object stage -ne 'event' | Select-Object -Last 1).stage
        $progress += "$bridge=$stage"
        if ($rows.stage -contains 'COMPLETE' -and $rows.stage -contains 'pause-settled') { $complete++ }
    }
    $current = $progress -join ', '
    if ($current -ne $previous) { Write-Output $current; $previous = $current }
    if ($complete -eq 2) {
        & python "$PSScriptRoot/summarize-ohos-live.py" $OutputDirectory --api $ApiVersion --os $OperatingSystem
        if ($LASTEXITCODE -ne 0) { throw 'Actual playback assertions failed' }
        exit 0
    }
    Start-Sleep -Seconds 10
} while ((Get-Date) -lt $deadline)
throw "Playback did not complete within $TimeoutSeconds seconds; emulator remains running"
