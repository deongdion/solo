# JNI fixture

`libjnifix.so` registers `fix/Native.run` and `fix/Native.sum` in `JNI_OnLoad`, and exports
`Java_fix_Native_hello` (short name) and `Java_fix_Native_twice__I` (overload name). `run` drives
the JNI functions a typical protected library uses and returns everything it observed as a string.
The Java side (`fix/Host`) is implemented twice, in the same way: `JniRun.java` for unidbg and the
`Host` handler in `runtime/tests/jni.rs`.

`expected.txt` is the output a VM that follows the JNI specification produces. It is unidbg's
arm64 output (30911d7, dynarmic) with the six lines below corrected. The Rust runtime produces
exactly `expected.txt` on arm64 and armv7, with dynarmic and with unicorn.

| line | unidbg | expected | cause in unidbg |
|---|---|---|---|
| `version` | `10008` on arm64, `10006` on arm32 | `10006` | DalvikVM64.GetVersion returns JNI_VERSION_1_8; ART reports 1.6 |
| `mix` (third value, `CallIntMethodA`) | f = 0 | f = 3.0 | `JValueList` reads a `jfloat` element with `getDouble` |
| `scale` (armv7 only, `CallDoubleMethod(float, double)`) | `0.000000` (unicorn2), `9.27e84` (dynarmic) | `-3.125000` | `ArmVarArg32` aligns a promoted float by an offset parity that does not match the register position |
| `region` (`GetStringRegion`) | `e900 6c00 0` | `e9 6c 6c` | writes the characters big-endian and short |
| `rethrown` (`Throw` after `ExceptionClear`) | `0` | `1` | the thrown object is stored but `ExceptionCheck` still reports none |
| `refs` (`GetObjectRefType`) | local `2`, weak `2` | local `1`, weak `3` | reports a local and a weak global reference as global |

All six reproduce on both unidbg backends (`scale` on armv7 only). Every other line matches unidbg
byte for byte.

The fixture uses only JNI functions unidbg implements on both architectures. The others throw
`UnsupportedOperationException` in unidbg: 92 of the table's slots on arm64 and 95 on armv7, listed
in `unidbg_unsupported_arm64.txt` and `unidbg_unsupported_arm32.txt` (for example
`CallDoubleMethodV`, `CallFloatMethod`, every `CallNonvirtual*`, `GetDoubleField`, `ThrowNew` on
armv7). The Rust runtime implements every JNIEnv slot of Android's table.

unidbg also needs one handler override per call variant: overriding `callStaticLongMethod` does not
cover `CallStaticLongMethodV`, which fails as `UnsupportedOperationException` and surfaces as an
unrelated `NullPointerException` later (PLAN.md F1, F2). `JniRun.java` adds the `...V` overrides;
the Rust handler sees all variants through one `Jni::call`.

Two places where unidbg differs on purpose from the Rust runtime, not covered by the comparison:

- Strings: unidbg converts with standard UTF-8; JNI uses modified UTF-8 (U+0000 as `C0 80`,
  supplementary characters as two 3-byte surrogates), which the runtime implements. The fixture's
  strings stay in the BMP, where both agree.
- `CallStaticDoubleMethod` on a method bound with RegisterNatives: unidbg sends it to the Java
  handler (`JniRun` repeats the native's formula); the runtime calls the registered native, as ART
  does.

## Reading a HashMap (PLAN.md F11)

`Java_fix_Native_walk` reads a `java.util.Map` through `Map.size`, `Map.keySet`, `Set.iterator`,
`Iterator.hasNext/next`, `Map.get` and `Integer.intValue`, the way natives read request
parameters. `MapRun.java` passes a `HashMap` of ten strings under unidbg (arm64, dynarmic):

| unidbg setup | result |
|---|---|
| `AbstractJni` (map wrapped by `ProxyDvmObject.createObject`) | `UnsupportedOperationException: java/util/Map->size()I`; the native returns null |
| `ProxyClassFactory` | `InaccessibleObjectException`: `HashMap$KeySet.iterator()` cannot be made accessible (JDK 21 module system); the native returns null |
| `ProxyClassFactory` with `--add-opens java.base/java.util=ALL-UNNAMED` | works: `expected_map.txt` (arm64 and armv7) |

The Rust runtime produces `expected_map.txt` on both architectures with no handler code: the
built-in models (`java` crate) answer the calls, and HashMap iterates in OpenJDK's order.

## Threads (PLAN.md M3)

`Java_fix_Native_threads(n, rounds, parts)` runs, by bit of `parts`: (1) `n` pthreads adding to a
counter under a mutex `rounds` times each, then joined; (2) two threads alternating `rounds` times
each through a condition variable; (3, bit 4) a thread that spins until the caller sets a flag,
while the caller spins until the thread sets another, which completes only if the scheduler
preempts a spinning thread. `ThreadRun.java` runs it under unidbg (dynarmic, thread dispatcher
enabled; without the dispatcher even part 1 never returns), `n = 4`, `rounds = 1000`, 120 s limit:

| parts | unidbg arm64 | unidbg armv7 | Rust runtime (both, dynarmic and unicorn) |
|---|---|---|---|
| 1 | `counter 10000 joined 100;` | same | same |
| 2 | no result within 120 s | no result within 120 s | `pings 2000 turn 0;` |
| 4 | no result within 120 s | no result within 120 s | `spun 7;` |
| 7 | no result within 120 s | no result within 120 s | `counter 10000 joined 100; pings 2000 turn 0; spun 7;` |

The runtime's interleaving is deterministic: two runs make the same number of thread switches
(`runtime/tests/threads.rs`).

## Signals (PLAN.md M3)

`Java_fix_Native_signals()` installs a `SA_SIGINFO` handler for SIGUSR1 and `raise`s it twice;
installs a SIGSEGV handler that `siglongjmp`s out and probes a stack byte, address 16 and address
32 with a read (1 = readable); then blocks SIGUSR2, raises it, reads `sigpending`, and unblocks it.
`SignalRun.java` runs it under unidbg (dynarmic), 120 s limit:

| | output |
|---|---|
| unidbg arm64 | `usr1 0 signo 0; probe 1 1 1; blocked 0 pending 0 delivered 0` |
| unidbg armv7 | no result within 120 s (the SIGSEGV repeats) |
| Rust runtime (both architectures, dynarmic and unicorn) | `usr1 2 signo 10; probe 1 0 0; blocked 0 pending 1 delivered 1` (`expected_signals.txt`) |

## Hooks (PLAN.md M4)

`Java_fix_Native_hooked(a, b)` returns `fix_add(a, b) * 100000 + fix_global() * 10 +
strlen("hello")`; `fix_global` starts with `adrp` on arm64, `Java_fix_Native_step` calls
`fix_step`, whose first instruction is a branch to `fix_add` on both architectures.
`runtime/tests/hooks.rs` checks the runtime's hooks on them; `HookRun.java <arm64|arm32>
<libjnifix.so> <none|hookzz|hookzzpost|xhook>` times unidbg's HookZz on `fix_add` and xHook on
`strlen` (numbers in PLAN.md M4). `TraceRun.java <arm64|arm32> <libjnifix.so> [cold]` times unidbg's
`traceCode` on the module (unicorn2) and counts its lines; `cold` sets it before the first call; `read` / `readcold` do the same for `traceRead` (set TRACE_ECHO
to print its lines). `BreakRun.java <dynarmic|unicorn2> <arm64|arm32> <libjnifix.so> [cold]` times a
breakpoint whose callback continues.

Regenerate: `bash build.sh`, then compile and run `JniRun.java` as `bench/parity/run.ps1` does for
`ParityRun.java` (`JniRun <unicorn2|dynarmic> <arm64|arm32> <libjnifix.so> <out>`).

## Unpacker (PLAN.md M4)

`libpacked.so` is `packed.c` with its `pk_code` and `pk_data` sections XORed by `pack.py`;
`libpacked_plain.so` is the same build unencrypted. Its init function decrypts `pk_code` (on arm64
with 32-byte `stp q` stores) and clears the ELF magic in memory; the exported `unlock()` decrypts
`pk_data` after the load. `UnpackRun.java` runs unidbg's `ElfUnpacker` on it: with unicorn2 the dump
differs from `libpacked_plain.so` in 70 bytes (the cleared magic and the 66 bytes `unlock()`
decrypted); with dynarmic the load fails with `UnsupportedOperationException`. The runtime's dump
(`runtime/tests/unpack.rs`) equals `libpacked_plain.so`.

## Self-modifying code (PLAN.md M4)

`libsmc.so` is `smc.c`: `patch` writes a function returning 111 into an RWX buffer, runs it,
rewrites it to return 222, flushes the instruction cache (arm64 `IC IVAU`, arm32 the
`__ARM_NR_cacheflush` syscall), and runs it again. `runtime/tests/smc.rs` asserts the result is
111 then 222 on both backends; before the dynarmic cache-invalidation fix it returned 111 twice.
