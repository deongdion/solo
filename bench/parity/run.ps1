# Regenerates expected.txt: runs libparity.so under unidbg (both backends, both architectures),
# checks that all four outputs agree, and writes the common output.
$root = Resolve-Path "$PSScriptRoot\..\.."
$work = "$root\bench\work"
$java = "C:\Program Files\Eclipse Adoptium\jdk-21.0.12.101-hotspot\bin"
$cp = Get-Content "$work\fullcp.txt" -Raw
& "$java\javac.exe" -cp $cp -d "$work\bcls" "$PSScriptRoot\ParityRun.java"
$outputs = @()
Push-Location "$work\unidbg\unidbg-android"
foreach ($arch in "arm64", "arm32") {
    foreach ($backend in "unicorn2", "dynarmic") {
        $out = "$work\parity_${backend}_$arch.txt"
        & "$java\java.exe" -cp $cp ParityRun $backend $arch "$PSScriptRoot\$arch\libparity.so" $out 2>$null | Select-Object -Last 1
        $outputs += $out
    }
}
Pop-Location
$first = [IO.File]::ReadAllBytes($outputs[0])
foreach ($o in $outputs) {
    if ([Convert]::ToBase64String([IO.File]::ReadAllBytes($o)) -ne [Convert]::ToBase64String($first)) { throw "unidbg outputs differ: $o" }
}
[IO.File]::WriteAllBytes("$PSScriptRoot\expected.txt", $first)
"expected.txt: $($first.Length) bytes, identical across $($outputs.Count) unidbg runs"
