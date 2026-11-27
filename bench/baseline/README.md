# M0 baseline (unidbg)

unidbg 30911d7 (2026-11-19), JDK 21.0.12, AMD Ryzen 5 5600G, Windows 11. Module: `libtmessages.29.so` (unidbg example binaries).
Raw rows: `results.txt`. Harness: `Baseline.java`, runner: `run.ps1`. Medians of 3 runs.

| Backend | Arch | init ms | first call us | aesCbc16 p50 us | aesCbc16 p99 us | pbkdf2(1000) p50 ms | peak WS MB |
|---|---|---|---|---|---|---|---|
| unicorn2 | arm64 | 892 | 4623 | 231 | 724 | 27.4 | 454 |
| unicorn2 | arm32 | 881 | 6659 | 715 | 1102 | 99.8 | 459 |
| dynarmic | arm64 | 1451 | 12214 | 197 | 988 | 6.1 | 606 |
| dynarmic | arm32 | 1257 | 13420 | 716 | 1075 | 15.3 | 732 |

Throughput (aesCbc16 calls/s, one thread per instance, 10 s):

| Backend | Arch | k=1 | k=4 | k=8 |
|---|---|---|---|---|
| unicorn2 | arm64 | 3726 | 16482 | 30185 |
| unicorn2 | arm32 | 1375 | 1542 | 1518 |
| dynarmic | arm64 | 4231 | 8275 | 17605 |
| dynarmic | arm32 | 1150 | 1970 | 1819 |

Outputs are identical across all backends and arches (aesCbc16 `d5e581b4243ddfb6`, pbkdf2 `f6ea85e025a5a03b`, SHA-256 prefix).

Peak WS is the whole JVM process, not the guest; guest resident memory is 17.4 MB (arm64) and 14.6 MB (arm32).
