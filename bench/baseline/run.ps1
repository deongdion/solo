# M0 baseline runner. Each configuration runs in its own JVM; peak working set is sampled from outside.
param([string]$Work = "$PSScriptRoot\..\work", [int]$Repeats = 3, [int]$Seconds = 10)

$java = "C:\Program Files\Eclipse Adoptium\jdk-21.0.12.101-hotspot\bin\java.exe"
$cp = Get-Content "$Work\fullcp.txt" -Raw
$results = "$PSScriptRoot\results.txt"
Set-Location "$Work\unidbg\unidbg-android"

function Invoke-Case([string]$label, [string]$caseArgs) {
    $p = Start-Process -FilePath $java -ArgumentList "-cp `"$cp`" Baseline $caseArgs" -NoNewWindow -PassThru `
        -RedirectStandardOutput "$Work\case_out.txt" -RedirectStandardError "$Work\case_err.txt"
    $null = $p.Handle
    $peak = 0
    while (-not $p.HasExited) {
        $p.Refresh()
        if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 }
        Start-Sleep -Milliseconds 100
    }
    $lines = Get-Content "$Work\case_out.txt" | Where-Object { $_ -match '^(init|aesCbc16|pbkdf2|guest|instances)' }
    $row = "$label exit=$($p.ExitCode) peak_ws_mb=$([math]::Round($peak / 1MB, 1)) | " + ($lines -join ' | ')
    Add-Content -Path $results -Value $row
    $row
}

"# $(Get-Date -Format s) $((Get-CimInstance Win32_Processor).Name.Trim()) java=21.0.12" | Set-Content $results
foreach ($backend in "unicorn2", "dynarmic") {
    foreach ($arch in "arm64", "arm32") {
        for ($i = 1; $i -le $Repeats; $i++) {
            Invoke-Case "latency $backend $arch run$i" "$backend $arch latency"
        }
        foreach ($k in 1, 4, 8) {
            Invoke-Case "throughput $backend $arch k=$k" "$backend $arch throughput $k $Seconds"
        }
    }
}
