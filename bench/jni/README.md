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

Regenerate: `bash build.sh`, then compile and run `JniRun.java` as `bench/parity/run.ps1` does for
`ParityRun.java` (`JniRun <unicorn2|dynarmic> <arm64|arm32> <libjnifix.so> <out>`).
