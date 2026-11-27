# M0.5 backend comparison

AMD Ryzen 5 5600G, Windows 11, MSVC, JDK 21.0.12. Raw rows: `results.txt` (runner: `run.ps1`). Latency values are medians of 3 runs.

Guest code: `fixtures/kernels.c`, built freestanding for arm64 and thumb (`fixtures/build.sh`).
- `empty`: returns 0 (per-call cost).
- `checksum`: 102,400 hash iterations over a 4 KiB buffer (ALU plus loads).
- `copy`: 204,800 byte copies (loads and stores).

The same images run on the Rust backend layer (`backendbench`) and through unidbg `eFunc` (`bench/baseline/KernelBench.java`).
Every run returned the host reference values (checksum 3090121304, copy 4375) on all 8 configurations.

## Latency (us, p50)

| Arch | Path | empty | checksum | copy | init |
|---|---|---|---|---|---|
| arm64 | Rust dynarmic | 0.1 | 372 | 796 | 24,448 |
| arm64 | unidbg dynarmic | 6.6 | 407 | 827 | 249,141 |
| arm64 | Rust unicorn | 0.3 | 820 | 17,197 | 1,283 |
| arm64 | unidbg unicorn2 | 4.5 | 831 | 6,386 | 223,888 |
| arm32 | Rust dynarmic | 0.1 | 306 | 702 | 14,914 |
| arm32 | unidbg dynarmic | 6.7 | 308 | 705 | 243,464 |
| arm32 | Rust unicorn | 0.6 | 714 | 16,899 | 4,555 |
| arm32 | unidbg unicorn2 | 4.7 | 695 | 7,506 | 219,851 |

The unidbg init includes building the Android emulator (linker, syscall handler); the Rust init is the backend only, so init columns are not comparable.
The first dynarmic call in a process costs 70 to 160 ms (one time per process, cause not yet identified).

## Throughput (empty calls/s, one thread per instance, 5 s)

| Path | k=1 | k=4 | k=8 |
|---|---|---|---|
| Rust dynarmic arm64 | 8,744,304 | 30,804,280 | 48,954,236 |
| Rust unicorn arm64 | 2,066,166 | 7,660,506 | 13,840,159 |
| Rust dynarmic arm32 | 6,631,519 | 23,016,819 | 40,292,387 |
| Rust unicorn arm32 | 1,832,224 | 5,773,747 | 10,537,174 |

## Memory (peak working set of the whole process)

| Path | 1 instance | 8 instances |
|---|---|---|
| Rust dynarmic arm64 | 25.9 MB | 161.9 MB |
| Rust unicorn arm64 | 6.3 MB | 15.4 MB |
| Rust dynarmic arm32 | 10.6 MB | 39.5 MB |
| Rust unicorn arm32 | 7.4 MB | 23.0 MB |

unidbg M0 (`bench/baseline`): 454 to 732 MB for one instance, JVM included.

## Findings

1. Guest execution speed is the backend's: on identical code, Rust and unidbg are within 0 to 9 % for dynarmic checksum/copy.
2. Per-call cost is the host layer's: an empty call costs 0.1 us on Rust dynarmic and 6.6 us through unidbg eFunc (66x).
3. dynarmic over unicorn on the Rust layer: checksum 2.2x (arm64) and 2.3x (arm32), copy 21.6x and 24.1x, calls/s 3.5x to 3.8x at k=8.
4. Both backends scale with instances on the Rust layer, arm32 included (dynarmic arm32 6.1x at k=8). unidbg's flat arm32 throughput in M0 (1,375 to 1,970 calls/s) therefore comes from its Java layer, not from the backends.
5. dynarmic costs more memory per instance (about 19 MB on arm64) than unicorn (about 1.3 MB).
6. Fixed during M0.5, measured before/after on the same machine:
   - dynarmic was compiled without `/O2` (the cmake crate replaces the release flags): JIT construction 90 ms to 24 ms.
   - Page table (36-bit, 128 MiB) zeroed eagerly: 20 ms to 15 us per instance.
   - A too-small code cache made dynarmic throw a C++ exception that aborted the process; the shim now returns an error.
   - dynarmic treats the remaining tick count as signed, so an unlimited budget stopped at once; the shim clamps it.
   - Instruction budget (`checksum_102400_budgeted`) costs nothing measurable on dynarmic.

## Decision

Default backend: dynarmic (confirmed by findings 1, 3, 4).

## Open items

- Rust unicorn `copy` is 2.3x to 2.7x slower than unidbg's unicorn2 on the same code. Not caused by the unmapped-memory hook (measured with and without). Cause not yet identified. Parity-only backend, so it does not affect the decision.
- unicorn budget: a finite `count` did not stop `checksum` in this wrapper (`budget_stop=Returned`). Cause not yet identified.
- dynarmic one-time first-call cost (70 to 160 ms) and per-instance memory (about 19 MB): candidates for the M3 pool.
