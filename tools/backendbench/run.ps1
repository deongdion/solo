# M0.5 runner: the same kernels on the Rust backend layer and through unidbg's eFunc, 3 runs each.
param([int]$Repeats = 3)
$root = Resolve-Path "$PSScriptRoot\..\.."
$exe = "$root\target\release\backendbench.exe"
$work = "$root\bench\work"
$java = "C:\Program Files\Eclipse Adoptium\jdk-21.0.12.101-hotspot\bin\java.exe"
$cp = Get-Content "$work\fullcp.txt" -Raw
$fixtures = "$PSScriptRoot\fixtures"
$out = "$PSScriptRoot\results.txt"
"# $(Get-Date -Format s) $((Get-CimInstance Win32_Processor).Name.Trim())" | Set-Content $out
foreach ($arch in "arm64", "arm32") {
    foreach ($pair in @(@("dynarmic", "dynarmic"), @("unicorn", "unicorn2"))) {
        for ($i = 1; $i -le $Repeats; $i++) {
            $rust = (& $exe $pair[0] $arch latency) -join ' | '
            Add-Content $out "rust $($pair[0]) $arch run$i | $rust"
            Push-Location "$work\unidbg\unidbg-android"
            $unidbg = (& $java -cp $cp KernelBench $pair[1] $arch $fixtures 2>$null | Where-Object { $_ -match '^(init|checksum|empty|copy)' }) -join ' | '
            Pop-Location
            Add-Content $out "unidbg $($pair[1]) $arch run$i | $unidbg"
        }
        foreach ($k in 1, 4, 8) {
            Add-Content $out "rust $($pair[0]) $arch throughput | $(& $exe $pair[0] $arch throughput $k 5)"
        }
        foreach ($n in 1, 8) {
            $p = Start-Process -FilePath $exe -ArgumentList "$($pair[0]) $arch instances $n" -NoNewWindow -PassThru -RedirectStandardOutput "$work\inst.txt"
            $null = $p.Handle; $peak = 0
            while (-not $p.HasExited) { $p.Refresh(); if ($p.PeakWorkingSet64 -gt $peak) { $peak = $p.PeakWorkingSet64 }; Start-Sleep -Milliseconds 50 }
            Add-Content $out "rust $($pair[0]) $arch memory | $(Get-Content "$work\inst.txt") peak_ws_mb=$([math]::Round($peak / 1MB, 1))"
        }
    }
}
