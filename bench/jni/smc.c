// Self-modifying code for the SMC test (runtime/tests/smc.rs). patch() writes a function that
// returns 111 into a buffer, runs it, then overwrites the return value with 222, flushes the
// instruction cache the way bionic's __clear_cache does, and runs it again. A JIT that does not
// re-translate on the cache flush returns 111 both times.
#include <stdint.h>

typedef int (*fn)(void);

// Flush the instruction cache for [start, end) the way bionic's __clear_cache does, without
// linking libc: arm64 runs the cache-maintenance instructions, arm32 makes the __ARM_NR_cacheflush
// syscall (0xf0002).
static void clear_cache(void *start, void *end) {
#if defined(__aarch64__)
    uintptr_t p = (uintptr_t)start & ~(uintptr_t)63;
    for (; p < (uintptr_t)end; p += 64) __asm__ volatile("dc cvau, %0" ::"r"(p) : "memory");
    __asm__ volatile("dsb ish" ::: "memory");
    p = (uintptr_t)start & ~(uintptr_t)63;
    for (; p < (uintptr_t)end; p += 64) __asm__ volatile("ic ivau, %0" ::"r"(p) : "memory");
    __asm__ volatile("dsb ish\n\tisb" ::: "memory");
#else
    register long r0 __asm__("r0") = (long)start;
    register long r1 __asm__("r1") = (long)end;
    register long r2 __asm__("r2") = 0;
    __asm__ volatile("push {r7}\n\tmovw r7, #0x0002\n\tmovt r7, #0x000f\n\tsvc #0\n\tpop {r7}"
                     : "+r"(r0)
                     : "r"(r1), "r"(r2)
                     : "memory");
#endif
}

// Writes `movz w0, #imm; ret` (arm64) or `movs r0, #imm; bx lr` (thumb) into code[].
static void emit(uint32_t *code, int imm) {
#if defined(__aarch64__)
    code[0] = 0x52800000u | ((uint32_t)(imm & 0xffff) << 5); // movz w0, #imm
    code[1] = 0xd65f03c0u;                                   // ret
#else
    uint16_t *c = (uint16_t *)code;
    c[0] = 0x2000u | (imm & 0xff); // movs r0, #imm
    c[1] = 0x4770u;                // bx lr
#endif
}

// code must point at an RWX buffer the caller allocated. Returns first*1000 + second.
int patch(uint32_t *code) {
    emit(code, 111);
    clear_cache(code, (char *)code + 8);
#if defined(__aarch64__)
    fn f = (fn)code;
#else
    fn f = (fn)((uintptr_t)code | 1); // Thumb
#endif
    int first = f();
    emit(code, 222);
    clear_cache(code, (char *)code + 8);
    int second = f();
    return first * 1000 + second;
}
