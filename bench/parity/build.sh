#!/usr/bin/env bash
# Builds libparity.so for arm64 and armv7. Requires LLVM (clang, ld.lld, llvm-nm).
# The sdk23 libc.so lists version names as local symbols in the global part of .dynsym, which lld
# rejects, so the fixture links against stub libraries with the same sonames; at run time both
# emulators bind the names to the real sdk23 bionic.
set -euo pipefail
cd "$(dirname "$0")"
LLVM="${LLVM:-/c/Program Files/LLVM/bin}"
LIBC="vsnprintf snprintf sscanf strlen strcmp strncmp strstr strchr strrchr strcpy strcat strdup memcmp memset memcpy memmove strtol strtoul strtod atoi qsort bsearch srand rand malloc calloc realloc free getenv __errno strerror open close gmtime_r strftime timegm toupper isdigit"
LIBDL="dlopen dlsym dlerror"
stub() {
    local dir=$1 target=$2 soname=$3; shift 3
    local src="$dir/$soname.c"
    : > "$src"
    local symbol
    for symbol in "$@"; do echo "void $symbol(void) {}" >> "$src"; done
    "$LLVM/clang" --target="$target" -fPIC -shared -nostdlib -fuse-ld=lld -fno-builtin -w "$src" \
        -Wl,-soname,"$soname" -o "$dir/$soname"
    rm "$src"
}
build() {
    local out=$1 target=$2 name=$3 hash=$4; shift 4
    local stubs
    stubs=$(mktemp -d)
    stub "$stubs" "$target" libc.so $LIBC
    stub "$stubs" "$target" libdl.so $LIBDL
    mkdir -p "$out"
    "$LLVM/clang" --target="$target" -O2 -fPIC -shared -nostdlib -fuse-ld=lld -fno-stack-protector "$@" parity.c \
        -L"$stubs" -lc -ldl -Wl,-soname,"$name" -Wl,--no-undefined -Wl,--hash-style="$hash" -o "$out/$name"
    rm -r "$stubs"
}
# libparity.so: DT_HASH and DT_GNU_HASH, packed relocations. Its unidbg output is expected.txt.
build arm64 aarch64-linux-android23 libparity.so both
build arm32 armv7a-linux-androideabi23 libparity.so both -mthumb
# libparitygnu.so: DT_GNU_HASH only, as current NDKs emit.
build arm64 aarch64-linux-android23 libparitygnu.so gnu
build arm32 armv7a-linux-androideabi23 libparitygnu.so gnu -mthumb
# libparityplt.so: no data relocations, so lld emits no DT_REL/DT_RELENT, only DT_JMPREL.
# unidbg then reads armv7 .rel.plt with a 12-byte stride (ElfDynamicStructure assumes RELA
# entries when DT_RELENT is absent) and the PLT is never bound; only the Rust test runs it.
build arm64 aarch64-linux-android23 libparityplt.so both -DPARITY_NO_DATA_RELOCATIONS
build arm32 armv7a-linux-androideabi23 libparityplt.so both -DPARITY_NO_DATA_RELOCATIONS -mthumb
