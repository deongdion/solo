#!/usr/bin/env bash
# Builds libjnifix.so for arm64 and armv7 (Thumb). Requires LLVM and a JDK for jni.h (the JDK
# jni_md.h is replaced by include/jni_md.h). Links against a stub libc.so as bench/parity does.
set -euo pipefail
cd "$(dirname "$0")"
LLVM="${LLVM:-/c/Program Files/LLVM/bin}"
JDK_INCLUDE="${JDK_INCLUDE:-/c/Program Files/Eclipse Adoptium/jdk-21.0.12.101-hotspot/include}"
# armv7 bionic also exports the EABI helpers the compiler calls for 64-bit conversions.
LIBC="vsnprintf strlen __aeabi_l2d pthread_create pthread_join pthread_mutex_lock pthread_mutex_unlock pthread_cond_wait pthread_cond_broadcast sched_yield sigaction raise sigprocmask sigpending sigemptyset sigaddset sigismember sigsetjmp siglongjmp"
build() {
    local out=$1 target=$2; shift 2
    local stubs symbol
    stubs=$(mktemp -d)
    : > "$stubs/libc.c"
    for symbol in $LIBC; do echo "void $symbol(void) {}" >> "$stubs/libc.c"; done
    "$LLVM/clang" --target="$target" -fPIC -shared -nostdlib -fuse-ld=lld -fno-builtin -w "$stubs/libc.c" \
        -Wl,-soname,libc.so -o "$stubs/libc.so"
    mkdir -p "$out"
    "$LLVM/clang" --target="$target" -O2 -fPIC -shared -nostdlib -fuse-ld=lld -fno-stack-protector \
        -Iinclude -I"$JDK_INCLUDE" "$@" jnifix.c -L"$stubs" -lc -Wl,-soname,libjnifix.so -Wl,--no-undefined \
        -o "$out/libjnifix.so"
    rm -r "$stubs"
}
build arm64 aarch64-linux-android23
build arm32 armv7a-linux-androideabi23 -mthumb

# libpacked.so: packed.c with its pk_code and pk_data sections encrypted by pack.py, next to the
# unencrypted build (libpacked_plain.so) that an unpacked dump must equal.
packed() {
    local out=$1 target=$2; shift 2
    "$LLVM/clang" --target="$target" -O2 -fPIC -shared -nostdlib -fuse-ld=lld -fno-stack-protector \
        "$@" packed.c -Wl,-soname,libpacked.so -Wl,--no-undefined -o "$out/libpacked_plain.so"
    cp "$out/libpacked_plain.so" "$out/libpacked.so"
    python pack.py "$out/libpacked.so"
}
packed arm64 aarch64-linux-android23
packed arm32 armv7a-linux-androideabi23 -mthumb
