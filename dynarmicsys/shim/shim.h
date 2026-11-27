// C interface over the dynarmic A64 and A32 JITs.
#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct DynCpu DynCpu;

// Halt reason bits returned by dyn_run. Zero means the instruction budget ran out.
#define DYN_HALT_EXTERNAL 0x01000000u
#define DYN_HALT_SVC 0x02000000u
#define DYN_HALT_EXCEPTION 0x04000000u
#define DYN_HALT_FAULT 0x08000000u
// The JIT threw (for example the code cache is too small). The CPU state is undefined.
#define DYN_HALT_ERROR 0x80000000u

typedef struct DynHooks {
    void* user;
    // Supervisor call. Return nonzero to halt after the instruction.
    int32_t (*on_svc)(void* user, uint32_t imm);
    // Undefined instruction, breakpoint and similar. Execution halts.
    void (*on_exception)(void* user, uint64_t pc, uint32_t kind);
    // Access to an unmapped page. Execution halts.
    void (*on_fault)(void* user, uint64_t address, uint32_t size, uint32_t is_write);
    // Whether the page holding `address` may be executed. Null allows every mapped page.
    // Called while translating, not per executed instruction.
    int32_t (*can_execute)(void* user, uint64_t address);
} DynHooks;

// page_table: one host pointer per 4 KiB guest page (null when unmapped).
// For A64 it holds 1 << (address_bits - 12) entries; for A32 it holds 1 << 20 entries.
// flags: DYN_PRECISE_FAULTS stops at the faulting instruction (its PC is reported) at the cost of
// a check after every data access.
#define DYN_PRECISE_FAULTS 0x1u
DynCpu* dyn_new(int32_t is64, uint8_t** page_table, uint32_t address_bits, uint64_t code_cache_size,
                uint32_t flags, const DynHooks* hooks);
void dyn_free(DynCpu* cpu);

// Runs until halted or until `budget` instructions have executed. Stores the executed count.
uint32_t dyn_run(DynCpu* cpu, uint64_t budget, uint64_t* executed);
// Requests a halt; safe to call from another thread.
void dyn_halt(DynCpu* cpu);

// A64: 0..30 general registers, 31 sp, 32 pc, 33 pstate, 34 tpidr_el0, 35 tpidrro_el0.
// A32: 0..15 general registers, 16 cpsr, 17 fpscr, 18 tpidruro.
uint64_t dyn_reg(const DynCpu* cpu, uint32_t index);
void dyn_set_reg(DynCpu* cpu, uint32_t index, uint64_t value);

void dyn_invalidate(DynCpu* cpu, uint64_t address, uint64_t length);

#ifdef __cplusplus
}
#endif
