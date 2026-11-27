# The M0 cases on this runtime (tools/tmbench), measured the way run.ps1 measures unidbg: one
# process per case, peak working set sampled from outside. Build first:
#   cargo build --release -p tmbench
param([int]$Repeats = 3, [int]$Seconds = 10)

$bin = Resolve-Path "$PSScriptRoot\..\..\target\release\tmbench.exe"
$results = "$PSScriptRoot\results_rust.txt"
$out = "$env:TEMP\tmbench_out.txt"

function Invoke-Case([string]$label, [string]$caseArgs) {
    $p = Start-Process -FilePath $bin -ArgumentList $caseArgs -NoNewWindow -PassThru -RedirectStandardOutput $out
    $null = $p.Handle
    $peak = 0
    while (-not $p.HasExited) {
        $p.Refresh()
        if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 }
        Start-Sleep -Milliseconds 100
    }
    $row = "$label exit=$($p.ExitCode) peak_ws_mb=$([math]::Round($peak / 1MB, 1)) | " + ((Get-Content $out) -join ' | ')
    Add-Content -Path $results -Value $row
    $row
}

"# $(Get-Date -Format s) $((Get-CimInstance Win32_Processor).Name.Trim()) tmbench" | Set-Content $results
foreach ($arch in "arm64", "arm32") {
    for ($i = 1; $i -le $Repeats; $i++) {
        Invoke-Case "latency dynarmic $arch run$i" "$arch dynarmic latency"
    }
    foreach ($k in 1, 4, 8) {
        Invoke-Case "throughput dynarmic $arch k=$k" "$arch dynarmic throughput $k $Seconds"
    }
}
