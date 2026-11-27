# Parity fixture

`libparity.so` exercises bionic with deterministic calls and writes every result into a buffer
the caller passes to `parity_run(buffer, size)`. The text is compared byte for byte:

- `expected.txt` is unidbg's output (upstream 30911d7). `run.ps1` regenerates it with unicorn2
  and dynarmic on arm64 and armv7, and fails unless all four runs agree.
- `runtime/tests/parity.rs` runs the same binaries here and compares against `expected.txt`.

What it covers: printf formats (integers, widths, 64-bit, floats, `%a`, truncation), string
functions, strtol/strtoul/strtod/sscanf, qsort/bsearch, srand/rand, gmtime_r/strftime/timegm at a
fixed time, open errno and strerror, getenv, calloc/realloc/malloc, dlopen/dlsym/dlerror on libm.
Results go through a buffer and not stdout because unidbg writes its own log lines to the same
`System.out` as the guest's stdout.

Variants (`build.sh` builds all; the binaries are committed so the test needs no LLVM):

| file | dynamic section | unidbg | here |
|---|---|---|---|
| libparity.so | DT_HASH and DT_GNU_HASH, packed relocations | expected.txt | equal |
| libparitygnu.so | DT_GNU_HASH only | equal | equal |
| libparityplt.so | no DT_REL/DT_RELENT, only DT_JMPREL | armv7 fails: `mem_read address=0x14cc`, returns -1 | equal |

The armv7 failure: `net.fornwall.jelf.ElfDynamicStructure` defaults the relocation entry size to
12 bytes (RELA) for 32-bit files when DT_RELENT is absent, but ARM uses 8-byte REL entries, so
`.rel.plt` is read with the wrong stride and the PLT is never bound. lld omits DT_RELENT whenever
a library has no data relocations. The fix is to take the entry size from DT_PLTREL.

The fixture links against stub libc.so/libdl.so with the right sonames because lld rejects the
sdk23 libc.so (version names appear as local symbols in the global part of .dynsym); at run time
both emulators bind the names to the real sdk23 bionic.
