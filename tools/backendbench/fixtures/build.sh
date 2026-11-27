#!/usr/bin/env bash
# Rebuilds the flat guest images and their symbol tables. Requires LLVM (clang, ld.lld, llvm-objcopy, llvm-nm).
set -euo pipefail
cd "$(dirname "$0")"
LLVM="${LLVM:-/c/Program Files/LLVM/bin}"
build() {
    local name=$1 target=$2; shift 2
    "$LLVM/clang" --target="$target" -O2 -ffreestanding -fno-builtin -fno-vectorize -fno-slp-vectorize -nostdlib -fPIC "$@" -c kernels.c -o "$name.o"
    "$LLVM/ld.lld" -static -nostdlib --no-dynamic-linker -e 0 --image-base=0x10000 -Ttext=0x10000 "$name.o" -o "$name.elf"
    "$LLVM/llvm-objcopy" -O binary --only-section=.text "$name.elf" "$name.bin"
    "$LLVM/llvm-nm" "$name.elf" | awk '$2 == "T" { print $3, $1 }' | sort > "$name.sym"
    rm -f "$name.o" "$name.elf"
}
build arm64 aarch64-linux-android
build thumb armv7a-linux-androideabi -mthumb
