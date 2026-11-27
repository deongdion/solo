// A library shaped like a packed one, for the unpacker tests (runtime/tests/unpack.rs).
// pack.py XORs the pk_code and pk_data sections of the built file; at run time:
// - an init_array function makes pk_code writable, decrypts it, and clears the ELF magic in
//   memory (the anti-dump step packers use);
// - unlock(), an ordinary exported call made after loading, decrypts pk_data.
// Both loops store 16 bytes at a time at -O2 on arm64. lld puts everything here in one R-X
// segment, so the protections are restored to R-X.

#include <stddef.h>
#include <stdint.h>

#define KEY 0x5a
#define PAGE 4096u

extern char __start_pk_code[], __stop_pk_code[];
extern char __start_pk_data[], __stop_pk_data[];
extern char __ehdr_start[];

static long sys_mprotect(void *address, size_t length, long prot) {
#if defined(__aarch64__)
    register long x0 __asm__("x0") = (long)address;
    register long x1 __asm__("x1") = (long)length;
    register long x2 __asm__("x2") = prot;
    register long x8 __asm__("x8") = 226;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory");
    return x0;
#else
    register long r0 __asm__("r0") = (long)address;
    register long r1 __asm__("r1") = (long)length;
    register long r2 __asm__("r2") = prot;
    // r7 is the Thumb frame pointer, so it is saved around the call instead of bound.
    __asm__ volatile("push {r7}\n\tmovs r7, #125\n\tsvc #0\n\tpop {r7}" : "+r"(r0) : "r"(r1), "r"(r2) : "memory");
    return r0;
#endif
}

static void protect(char *start, char *end, long prot) {
    uintptr_t first = (uintptr_t)start & ~(uintptr_t)(PAGE - 1);
    uintptr_t last = ((uintptr_t)end + PAGE - 1) & ~(uintptr_t)(PAGE - 1);
    sys_mprotect((void *)first, last - first, prot);
}

static void xor_range(char *start, char *end) {
    for (char *p = start; p < end; p++) *p ^= KEY;
}

__attribute__((section("pk_code"), noinline, visibility("default"))) int secret(int x) {
    int value = 12345;
    for (int i = 0; i < x; i++) value = value * 31 + i;
    return value;
}

__attribute__((section("pk_data"), used)) const char pk_message[] =
    "decrypted after load by an exported call, not by an init function";

__attribute__((constructor)) static void decrypt_code(void) {
    protect(__start_pk_code, __stop_pk_code, 7);
    xor_range(__start_pk_code, __stop_pk_code);
    protect(__start_pk_code, __stop_pk_code, 5);
    protect(__ehdr_start, __ehdr_start + 4, 7);
    for (int i = 0; i < 4; i++) ((volatile char *)__ehdr_start)[i] = 0;
    protect(__ehdr_start, __ehdr_start + 4, 5);
}

__attribute__((visibility("default"))) const char *unlock(void) {
    protect(__start_pk_data, __stop_pk_data, 7);
    xor_range(__start_pk_data, __stop_pk_data);
    protect(__start_pk_data, __stop_pk_data, 5);
    return pk_message;
}
