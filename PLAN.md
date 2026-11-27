# Plan

Rust reimplementation of the Android side of unidbg (github.com/zhkl0228/unidbg), to be merged into redex later.

## Rules

- Scope: features unidbg already has, improved. No features unidbg does not have.
- iOS is out of scope.
- No project name in code, crate names, identifiers or docs. No hyphens in file or folder names.
- All code text in English. Library crates use `thiserror` only.
- An improvement counts only when measured against the baseline. Unmeasured claims are hypotheses.
- Builds on Windows, Linux and macOS.

## License and backend

| Item | Decision | Reason |
|---|---|---|
| License | Private, all rights reserved, same as redex | No conflict when merging |
| GPL code | Never in a distributed build | Static unicorn (GPL-2.0) would make the merged redex GPL |
| Default backend | dynarmic (0BSD), pinned fork behind a C shim | Already a unidbg backend; upstream suggests it for #310 and #198. Upstream dynarmic is archived, so the fork is ours to maintain |
| unicorn | Optional feature, local parity runs only | Reference for output equality |
| HVF, KVM | Later; kept out of the default CI gate | Existing unidbg backends, fragile |

The default backend is confirmed or revised by the M0.5 measurement.

Porting unidbg code (Apache-2.0) requires keeping its NOTICE and marking changed files. Do not copy AndroidNativeEmu (GPL-3.0) code.

## unidbg features

| unidbg feature | Plan | Improvement | Evidence |
|---|---|---|---|
| ARM64, ARM32/Thumb | Keep (M1-M2) | Word-size generic layers | about 1/3 of 2024-2026 issues are 32-bit |
| Backends | dynarmic default, unicorn parity, HVF/KVM later | Capabilities exposed by the backend trait | |
| ELF loader | Keep | Lenient mode (overlapping or out-of-range segments, missing DT_SYMTAB, RWX), 16KB pages | #753, #793, 45 packer issues |
| ElfUnpacker | Keep | Dump decrypted modules | #750 |
| Linux syscalls | Keep | List of unhandled syscalls, selectable policy | 47 issues |
| Allocator, mmap | Keep | calloc zeroing and partial munmap tests | #754, #599 |
| IOResolver (VFS, /proc, /dev) | Keep | Reject paths escaping the rootfs; /proc/self/maps matches the real map | #772, #795, #40 |
| System properties, device info | Keep | One device profile (properties, packages, certificates) | anti-emulation issues |
| Bundled system libs | Keep | Current API level bionic and libc++ | #735 |
| Signals | Keep | | 11 issues |
| Threads (futex, clone) | Keep | Instruction-budget preemption, virtual time, reproducible runs | 33 issues |
| DalvikVM JNI | Keep | Reentrant calls, Java exceptions into native, reflection (FromReflectedMethod) | #593, #432, #771, #775, #699 |
| AbstractJni | Keep | Same signature strings ("com/x/Y->m(I)V") so scripts port easily | |
| ProxyClassFactory, default Java methods, FallbackJni | Keep | Built-in java.lang/util/security models; Context, PackageManager, AssetManager, ActivityThread, Settings stubs | #211, #62, #332 |
| Signing certificates v1-v3 | Keep | | recent upstream commits |
| Virtual modules (libandroid, jnigraphics, mediandk) | Keep | Complete NDK AAsset API | #62 |
| Code and memory trace | Keep | Zero cost when off, ring buffer, per-module log filters | #310, #729, #504, #767 |
| Console debugger, gdb, IDA stubs | Keep | | #737, #744 |
| Unwinder | Keep | Backtrace in the crash report | |
| Exception output | Keep | Crash report: module+offset, last instructions, last syscalls | 91 crash issues |
| Hooks (xHook, Dobby, HookZz, Whale, inline) | Merge into one API | PLT and inline hook API; drop the four guest hook frameworks | low usage of HookZz/Whale (hypothesis) |
| WorkerPool | Keep | Worker recycling; memory, instruction and time limits; cancellation | #198 |
| Serialization | Keep | Save and restore the state after JNI_OnLoad to cut pool init cost | |
| MCP debugger | Keep, later | | |
| iOS | Drop | | |

## Field experience (pain points hit in real use)

| # | Pain point in unidbg | Improvement | Crate | Milestone |
|---|---|---|---|---|
| F1 | An unimplemented JNI callback throws inside native code and is swallowed; the visible symptom is unrelated (`Illegal JNI version: 0xffffffff`) | Always warn `unhandled JNI callback: <class>-><sig>`; selectable policy: raise, or return null/zero and continue | javavm | M2 |
| F2 | `CallStaticXxxMethod` (varargs), `...V` (va_list) and `...A` (jvalue array) each need their own override; missing one fails silently | All variants decode into one call `on_call(signature, args)`; one handler covers every variant | javavm | M2 |
| F3 | The debugger reads stdin on failure and dies with `No line found` when there is no terminal (CI, gradle) | Never prompt without a terminal; dump registers and backtrace and return the error; explicit setting to turn the debugger off | inspect | M1 |
| F4 | `debugger break at svc #0x16e` looks like an unhandled syscall, but it is a JNI callback error passing through the trampoline | Stop reasons are distinct kinds: trampoline/callback error (with the callback signature), unhandled syscall (with name and args), memory fault | machine, inspect | M1 |
| F5 | Seeing which files native code opens needs a rootfs and reading failure logs | File I/O trace toggle: one line per open/read/write/stat/access with path, flags and result | kernel, inspect | M1 |
| F6 | Extracting a runtime-decrypted dex/zip/ELF means walking the mappings by hand | One call: scan mapped memory for magic (`dex\n`, `PK\x03\x04`, `\x7fELF`) and dump each hit with its region; extends the existing module dump | machine, inspect | M1 |
| F7 | Unresolved symbols and unhandled syscalls/JNI calls are scattered through the log | Summary at the end of a run: unresolved symbols, unhandled syscalls, unhandled JNI signatures, with counts and first caller | inspect | M1 (syscalls, symbols), M2 (JNI) |
| F8 | Multi-stage loaders (one module loading another via dlopen) are hard to follow | Module timeline: load, init_array, JNI_OnLoad, RegisterNatives, dlsym, with the requesting module and caller address | linker, javavm, inspect | M2 |
| F9 | On Windows, java.exe broke CJK paths (JVM argument encoding, not unidbg) | Host paths stay `OsStr`/`Path` end to end; CI test from a Korean path on Windows | all | M1 |
| F11 | Native code reads input `HashMap`s through `keySet()`, `iterator()`, `next()`, `get()`, `size()`, `containsKey()`, `toArray()`; users re-implement the Map/Set/Iterator/Map.Entry protocol by hand. In unidbg's proxy layer the method is looked up on the object's runtime class; for `HashMap$KeySet` and similar private classes the reflective call throws `IllegalAccessException` (measured on JDK 21 outside unidbg; that this is the unidbg failure path is a hypothesis) | Built-in marshaling for `Map`, `HashMap`, `Set`, `List`, `Iterator`, `Map$Entry`, boxed primitives: resolve methods by the interface named in `GetMethodID`, not by the runtime class | java, javavm | M2 |
| F13 | An armv7 library without data relocations has no DT_RELENT (lld omits it); unidbg then reads `.rel.plt` with a 12-byte stride and leaves the PLT unbound (reproduced: `bench/parity/libparityplt.so`, `mem_read address=0x14cc`) | Entry size from DT_PLTREL (REL 8/16, RELA 12/24), never a guessed default | linker | M1 (done) |
| F14 | unidbg's JNI table: 92 slots (arm64) / 95 (armv7) throw `UnsupportedOperationException`; of the implemented ones, `GetStringRegion` writes big-endian, `Throw` does not set the pending exception, `GetObjectRefType` reports locals and weak refs as global, `CallXxxMethodA` reads `jfloat` as double, armv7 varargs misalign promoted floats, arm64 `GetVersion` says 1.8 (all reproduced: `bench/jni/README.md`) | Every slot of Android's table, per the JNI specification; byte-checked by `runtime/tests/jni.rs` on both architectures and both backends | javavm | M2 (done) |
| F12 | `fstatat(fd, NULL, buf, AT_EMPTY_PATH)` (stat the fd itself) crashes unidbg: both handlers call `pathname.getString(0)` before any check (NPE on null), and a dirfd other than `AT_FDCWD` with an empty or relative path throws `BackendException` (checked in `ARM64SyscallHandler.fstatat64` and `ARM32SyscallHandler.fstatat64` at upstream 30911d7) | All `*at` syscalls share one path resolver: null or empty path with `AT_EMPTY_PATH` resolves to the fd itself; relative paths resolve against the dirfd; errors become errno, never a host exception. Covered by fixtures | kernel | M1 |
| F10 | Signing orchestration (which cmd/args reach `doCommandNative`) lives in a plugin dex that is only decrypted in memory at run time; unidbg cannot run it, so the commands must be guessed or the dex read by hand | Load a dex from memory (like `InMemoryDexClassLoader`) and run its bytecode in the VM, calling into native through the same JNI layer. **Exception to the scope rule: unidbg has no dex loading or bytecode execution (checked in its source).** Needs a Dalvik interpreter on top of the redex `engine` dex parser | javavm, java, redex engine | M6 (after merge) |

Not planned (not in unidbg): HTTP or gRPC server, Python bindings, standalone CLI, auto-generated JNI stubs from dex, real-device diff tool, crypto constant detection, coverage export, icicle backend. Run commands go into the redex `cli` after the merge.

## Crates

Dependencies point downward: machine <- backend <- linker <- kernel <- javavm <- java, device <- runtime.

| Crate | Role | At merge |
|---|---|---|
| machine | Guest memory, registers, errors | as is |
| backend | dynarmic, unicorn (crates.io `unicorn-engine`, optional), HVF, KVM | as is |
| tools/backendbench | Same guest kernels on every backend and through unidbg | as is |
| dynarmicsys | FFI to the pinned dynarmic fork (submodule) through a C shim | as is |
| linker | ELF loader, unpacker | as is |
| kernel | Syscalls, VFS, signals, threads | as is |
| javavm | JNI, reference tables, exceptions | as is |
| java | Java and Android stubs | as is |
| device | Device profile, rootfs, APK metadata | APK and dex parsing replaced by redex `engine` |
| inspect | Trace, hooks, debugger, MCP | as is |
| runtime | Entry point, worker pool | used by redex `cli` |

No `cli` or `dex` crate: both already exist in redex.

## Milestones

| Step | Content | Done when (measured) |
|---|---|---|
| M0 | unidbg baseline | p50/p99 call latency over 10k warm calls, RSS per instance, time to end of JNI_OnLoad, throughput at K instances; fixed hardware |
| M0.5 | dynarmic vs unicorn | Done: dynarmic confirmed (tools/backendbench/README.md) |
| M1 | ELF, libc, syscalls, VFS, arm64 and armv7, crash report, memory magic scan and dump (F6) | Output byte-equal to unidbg |
| M2 | JNI, java stubs, reflection, exceptions | RegisterNatives and static-registered modules pass |
| M3 | Threads, signals, save/restore, worker pool | Beats M0; 24h RSS soak |
| M4 | Trace, hooks, debugger, unpacker, device profile | Feature fixtures pass |
| M5 | Merge into redex | |
| M6 | Dex bytecode execution (F10) | Plugin dex loaded from memory drives the native calls with no hand-written orchestration |


### M1 progress

Done (tests in `runtime/tests/libc.rs`, both architectures, dynarmic):
- Linker: ELF32/64 parse, placement from `MMAP_BASE`, REL/RELA/RELR, GNU and SysV hash lookup, DT_SYMBOLIC, BFS local group, init order (dependencies first, init_array read after relocation), missing libraries and unresolved symbols recorded instead of failing.
- Kernel: syscall tables for arm64 and ARM EABI, rootfs VFS that cannot escape the root, overlay writes, `/proc/self/{maps,cmdline,status}`, file trace (F5), `*at` path resolver with AT_EMPTY_PATH (F12), unhandled syscall counts.
- Runtime: unidbg memory layout, TLS slots and KernelArgumentBlock for `__libc_preinit`, host stubs (`svc #0x100+n`) for libdl, reporting stubs for unresolved symbols, crash report with module+offset (F4), end-of-run summary (F7) with the dlopen timeline (F8).
- Host memory limit per address space (default 4 GiB): a guest that maps too much gets an error instead of exhausting the host.
- Sdk23 bionic libc init runs; malloc, strlen, sprintf, puts, fflush and getpid return the expected values.

Fixed while bringing libc up: arm64 TLS is TPIDR_EL0 (was TPIDRRO_EL0); dynarmic needs an exclusive monitor (LDXR/STXR) and a CP15 coprocessor (TPIDRURO on A32); allocations are keyed by host address, because a partly unmapped range can be remapped at the start of its old allocation.

libtmessages.29.so (M0 library): loads with its dependencies and runs its init functions on both architectures (JNI_OnLoad waits for M2). Measured on this machine: arm64 load+init 243 ms, arm32 209 ms; 8 modules; 5 missing libraries (jnigraphics, EGL, GLESv2, android, OpenSLES) and 48 unresolved symbols, all reported in the summary. Not comparable with the M0 init numbers, which include JNI_OnLoad and the JVM.

Hardening from that run:
- Android packed relocations (APS2, `DT_ANDROID_REL[A]`) are applied; armv7 libc++ uses them. Without them the guest jumped to unrelocated addresses.
- dynarmic code fetch honours the execute bit and unmapped pages: a jump into data or unmapped memory is a fetch fault. Before, the fetch callback returned 0 after a fault and dynarmic kept translating the next word, growing one block until the host ran out of memory (3.4 GB in 2 s, measured). `protect` invalidates translations.
- The instruction budget covers a whole call; before, each resume after a syscall got a fresh budget, so a syscall loop never stopped.
- A32 hint instructions (PLD, PLDW, PLI, SEV, SEVL) no longer stop execution.
- Unresolved data symbols bind to zeroed storage instead of a code stub; library names with `+` also resolve with `p` (unidbg's file names).
- Benchmark after the code fetch check (3 runs, p50): arm64 checksum 362-384 us (before 359-379), copy 778-841 us (before 796-803); no measurable change.

F6 memory magic scan and dump (`inspect::scan`): dex, zip and ELF images found in readable mappings, each validated and sized from its own header and bounded by its mapping; dumps go to `<kind>_<address>.<ext>` under any host path (tested with a Korean directory name, F9). After loading libtmessages it finds all 8 modules at their bases. Release build, 3 runs each: naive window search 96-185 ms for 11.5-14.1 MiB, memchr::memmem 4.8-6.9 ms.

F3 failure dump: a crash returns `Error::Crash` with registers, module+offset+symbol for pc and lr, the fault region, and a frame-pointer backtrace (x29 on arm64, r7 on Thumb, bounded to writable mappings and 32 frames). Nothing ever waits for input.

Precise fault PCs: dynarmic stops at a faulting data access only with `check_halt_on_memory_access`; otherwise the PC is the translated block's (seen: `vdprintf+0x2420` instead of `strlen+0x10`). Cost, p50 median of 5 alternating runs in one build:

| kernel | precise | block PC | cost |
|---|---|---|---|
| arm64 checksum | 437 us | 392 us | +11% |
| arm64 copy | 963 us | 835 us | +15% |
| arm32 checksum | 383 us | 301 us | +27% |
| arm32 copy | 776 us | 727 us | +7% |

So it is an option (`Options::precise_faults`, backendbench `dynarmic_precise`), off by default; the crash report says when the PC is only the block start. Fetch faults are always exact.

Parity with unidbg (`bench/parity`, `runtime/tests/parity.rs`): the fixture's output is byte-equal to unidbg's on arm64 and armv7, for all three dynamic-section variants. unidbg itself agrees across unicorn2 and dynarmic and both architectures. Found on the way: unidbg cannot bind the armv7 PLT of a library without DT_RELENT (12-byte default stride for 32-bit REL entries); recorded in bench/parity/README.md as an improvement to the existing loader.

### M2 progress

JNI (`javavm`, `runtime/src/jni.rs`): every JNIEnv and JavaVM slot as host stubs; varargs, `va_list` and `jvalue` arguments decoded per AAPCS64 and the 32-bit softfp convention, FP registers on both backends; local/global/weak reference tables with stale-reference detection; modified UTF-8; Get/Release buffers in a guest arena that is reused and unmapped; a mark-and-sweep Java heap collected between top-level calls; RegisterNatives and `Java_` symbols (short and overload names); a registered native called back through `CallXxxMethod` runs in the guest. One `Jni::call` handles every call variant (F2); an unanswered call stops with its signature and the native caller, or returns zero or throws, by policy (F1); the summary lists unhandled JNI signatures and the JNI_OnLoad/RegisterNatives timeline (F7, F8).

Measured against unidbg with `bench/jni` (`runtime/tests/jni.rs`): equal output except six lines where unidbg breaks the JNI specification (F14); the Rust runtime gives the specified output on arm64 and armv7, dynarmic and unicorn.

libtmessages.29.so JNI_OnLoad runs on both architectures with no unhandled JNI call (10 RegisterNatives). The M0 benchmark on this runtime (`tools/tmbench`, `bench/baseline/runrust.ps1`, results in `bench/baseline/results_rust.txt`) produces the same output hashes as unidbg (aesCbc16 `d5e581b4243ddfb6`, pbkdf2 `f6ea85e025a5a03b`). Same machine, same method (one process per case, peak working set sampled from outside), dynarmic on both sides, 3 runs:

| case | unidbg 30911d7 | this runtime |
|---|---|---|
| arm64 init (load + JNI_OnLoad) | 1387-1530 ms | 167-197 ms |
| arm64 aesCbc16 p50 / p99 | 191-198 / 859-1234 us | 5.4-5.6 / 12.9-16.6 us |
| arm64 pbkdf2_1000 p50 | 5869-6370 us | 2887-3066 us |
| arm64 peak working set (latency) | 604-637 MB | 44 MB |
| arm64 throughput k=1 / 4 / 8 | 4231 / 8275 / 17605 calls/s | 131698 / 388598 / 541562 calls/s |
| arm32 init | 1257-1283 ms | 129-267 ms |
| arm32 aesCbc16 p50 | 716-733 us | 5.4-10.3 us |
| arm32 pbkdf2_1000 p50 | 15103-15382 us | 13307-17304 us (no gain) |
| arm32 peak working set (latency) | 731-740 MB | 27 MB |
| arm32 throughput k=1 / 4 / 8 | 1150 / 1970 / 1819 calls/s | 86102 / 280076 / 517478 calls/s |

aesCbc16 is dominated by per-call cost (16 bytes of work), pbkdf2 by guest computation. arm32 pbkdf2 shows no gain: both run the same dynarmic A32 JIT; why A32 guest code is 4-5x slower than A64 here is not investigated yet (hypothesis: A32 JIT options, not measured).

Next: built-in java.lang/java.util models (F11).

## Build

The dynarmic backend needs Boost headers: set `DYNARMIC_BOOST_INCLUDE` to a Boost source tree (the directory that contains `boost/`). The dynarmic fork is a git submodule (`git submodule update --init --recursive`).

## Corpus

1. NDK fixtures, one per feature, arm64 and armv7.
2. Redistributable open-source modules (Telegram, libsignal, SQLCipher), hashes pinned.
3. unidbg's own test cases.
4. Commercial app modules: local only under `corpus/local`, never committed.

## Production requirements

- Per-call instruction budget and deadline, cancellation from the host.
- Guest memory and fd limits.
- `catch_unwind` at every FFI callback boundary.
- Untrusted modules run in a sandboxed worker process.
- Fuzzing of the ELF loader and syscall argument decoding.
- CI: windows-msvc, linux x64 and arm64, macos arm64; `cargo deny`.

## Prior art

rnidbg (github.com/fuqiuluo/rnidbg): an earlier Rust port of unidbg that stalled in 2024 with an incomplete JNI layer and few syscalls. Read it before M1.
