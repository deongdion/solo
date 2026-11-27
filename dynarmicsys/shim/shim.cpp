#include "shim.h"

#include <array>
#include <atomic>
#include <cstring>
#include <memory>
#include <optional>

#include <dynarmic/interface/A32/a32.h>
#include <dynarmic/interface/A32/config.h>
#include <dynarmic/interface/A32/coprocessor.h>
#include <dynarmic/interface/A64/a64.h>
#include <dynarmic/interface/A64/config.h>
#include <dynarmic/interface/exclusive_monitor.h>

namespace {

constexpr uint64_t PAGE_BITS = 12;
constexpr uint64_t PAGE_MASK = (1ull << PAGE_BITS) - 1;

// Memory access and event plumbing shared by both instruction sets.
template <typename VAddr>
struct Common {
    uint8_t** page_table;
    uint64_t page_count;
    DynHooks hooks;
    uint64_t budget = 0;
    uint64_t executed = 0;
    std::atomic<bool>* halt_requested = nullptr;
    void (*halt)(void* jit, Dynarmic::HaltReason reason) = nullptr;
    void* jit = nullptr;

    uint8_t* host(uint64_t address) const {
        uint64_t index = address >> PAGE_BITS;
        if (index >= page_count) return nullptr;
        uint8_t* page = page_table[index];
        return page ? page + (address & PAGE_MASK) : nullptr;
    }

    void fault(uint64_t address, uint32_t size, uint32_t is_write) {
        if (hooks.on_fault) hooks.on_fault(hooks.user, address, size, is_write);
        // MemoryAbort makes dynarmic stop at the faulting instruction (check_halt_on_memory_access).
        halt(jit, static_cast<Dynarmic::HaltReason>(DYN_HALT_FAULT) | Dynarmic::HaltReason::MemoryAbort);
    }

    template <typename T>
    T read(VAddr address) {
        T value{};
        uint8_t* bytes = reinterpret_cast<uint8_t*>(&value);
        for (size_t i = 0; i < sizeof(T);) {
            uint64_t at = static_cast<uint64_t>(address) + i;
            uint8_t* src = host(at);
            if (!src) {
                fault(at, sizeof(T), 0);
                return T{};
            }
            size_t chunk = std::min<size_t>(sizeof(T) - i, (PAGE_MASK + 1) - (at & PAGE_MASK));
            std::memcpy(bytes + i, src, chunk);
            i += chunk;
        }
        return value;
    }

    template <typename T>
    void write(VAddr address, T value) {
        const uint8_t* bytes = reinterpret_cast<const uint8_t*>(&value);
        for (size_t i = 0; i < sizeof(T);) {
            uint64_t at = static_cast<uint64_t>(address) + i;
            uint8_t* dst = host(at);
            if (!dst) {
                fault(at, sizeof(T), 1);
                return;
            }
            size_t chunk = std::min<size_t>(sizeof(T) - i, (PAGE_MASK + 1) - (at & PAGE_MASK));
            std::memcpy(dst, bytes + i, chunk);
            i += chunk;
        }
    }

    // A null result makes dynarmic raise NoExecuteFault when the block is reached, so a jump into
    // data faults instead of translating an unbounded run of non-branch words.
    std::optional<uint32_t> code(VAddr address) {
        uint64_t at = static_cast<uint64_t>(address);
        uint8_t* src = host(at);
        if (!src || (hooks.can_execute && hooks.can_execute(hooks.user, at) == 0)) return std::nullopt;
        uint32_t value;
        std::memcpy(&value, src, sizeof value);  // 4-byte aligned, so within one page
        return value;
    }

    void svc(uint32_t imm) {
        if (hooks.on_svc && hooks.on_svc(hooks.user, imm) != 0) {
            halt(jit, static_cast<Dynarmic::HaltReason>(DYN_HALT_SVC));
        }
    }

    void exception(uint64_t pc, uint32_t kind) {
        if (hooks.on_exception) hooks.on_exception(hooks.user, pc, kind);
        halt(jit, static_cast<Dynarmic::HaltReason>(DYN_HALT_EXCEPTION));
    }

    void add_ticks(uint64_t ticks) {
        executed += ticks;
        budget = ticks >= budget ? 0 : budget - ticks;
    }
};

struct Callbacks64 final : Dynarmic::A64::UserCallbacks, Common<uint64_t> {
    using V = Dynarmic::A64::VAddr;
    uint64_t tpidr_el0 = 0;
    uint64_t tpidrro_el0 = 0;
    Dynarmic::A64::Jit* cpu = nullptr;

    uint8_t MemoryRead8(V a) override { return read<uint8_t>(a); }
    uint16_t MemoryRead16(V a) override { return read<uint16_t>(a); }
    uint32_t MemoryRead32(V a) override { return read<uint32_t>(a); }
    uint64_t MemoryRead64(V a) override { return read<uint64_t>(a); }
    Dynarmic::A64::Vector MemoryRead128(V a) override { return read<Dynarmic::A64::Vector>(a); }
    void MemoryWrite8(V a, uint8_t v) override { write(a, v); }
    void MemoryWrite16(V a, uint16_t v) override { write(a, v); }
    void MemoryWrite32(V a, uint32_t v) override { write(a, v); }
    void MemoryWrite64(V a, uint64_t v) override { write(a, v); }
    void MemoryWrite128(V a, Dynarmic::A64::Vector v) override { write(a, v); }
    // Single guest core: an exclusive store always succeeds.
    bool MemoryWriteExclusive8(V a, uint8_t v, uint8_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive16(V a, uint16_t v, uint16_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive32(V a, uint32_t v, uint32_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive64(V a, uint64_t v, uint64_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive128(V a, Dynarmic::A64::Vector v, Dynarmic::A64::Vector) override { write(a, v); return true; }

    void InterpreterFallback(V pc, size_t) override { exception(pc, 0xffff); }
    void CallSVC(uint32_t imm) override { svc(imm); }
    std::optional<std::uint32_t> MemoryReadCode(V vaddr) override { return code(vaddr); }

    void ExceptionRaised(V pc, Dynarmic::A64::Exception e) override {
        using E = Dynarmic::A64::Exception;
        // Hints with no effect on a single guest thread.
        if (e == E::Yield || e == E::SendEvent || e == E::SendEventLocal) return;
        cpu->SetPC(pc);
        exception(pc, static_cast<uint32_t>(e));
    }
    void AddTicks(uint64_t ticks) override { add_ticks(ticks); }
    uint64_t GetTicksRemaining() override { return budget; }
    uint64_t GetCNTPCT() override { return executed; }
};

struct Callbacks32 final : Dynarmic::A32::UserCallbacks, Common<uint32_t> {
    using V = Dynarmic::A32::VAddr;
    Dynarmic::A32::Jit* cpu = nullptr;

    uint8_t MemoryRead8(V a) override { return read<uint8_t>(a); }
    uint16_t MemoryRead16(V a) override { return read<uint16_t>(a); }
    uint32_t MemoryRead32(V a) override { return read<uint32_t>(a); }
    uint64_t MemoryRead64(V a) override { return read<uint64_t>(a); }
    void MemoryWrite8(V a, uint8_t v) override { write(a, v); }
    void MemoryWrite16(V a, uint16_t v) override { write(a, v); }
    void MemoryWrite32(V a, uint32_t v) override { write(a, v); }
    void MemoryWrite64(V a, uint64_t v) override { write(a, v); }
    bool MemoryWriteExclusive8(V a, uint8_t v, uint8_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive16(V a, uint16_t v, uint16_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive32(V a, uint32_t v, uint32_t) override { write(a, v); return true; }
    bool MemoryWriteExclusive64(V a, uint64_t v, uint64_t) override { write(a, v); return true; }

    void InterpreterFallback(V pc, size_t) override { exception(pc, 0xffff); }
    void CallSVC(uint32_t imm) override { svc(imm); }
    std::optional<std::uint32_t> MemoryReadCode(V vaddr) override { return code(vaddr); }

    void ExceptionRaised(V pc, Dynarmic::A32::Exception e) override {
        using E = Dynarmic::A32::Exception;
        // Hints with no effect on a single guest thread.
        if (e == E::Yield || e == E::SendEvent || e == E::SendEventLocal || e == E::PreloadData ||
            e == E::PreloadDataWithIntentToWrite || e == E::PreloadInstruction) {
            return;
        }
        cpu->Regs()[15] = pc;
        exception(pc, static_cast<uint32_t>(e));
    }
    void AddTicks(uint64_t ticks) override { add_ticks(ticks); }
    uint64_t GetTicksRemaining() override { return budget; }
};

/// CP15 as user mode sees it: the thread ID registers and the legacy barrier operations.
class Cp15 final : public Dynarmic::A32::Coprocessor {
public:
    std::uint32_t tpidruro = 0;
    std::uint32_t tpidrurw = 0;

    using Reg = Dynarmic::A32::CoprocReg;

    std::optional<Callback> CompileInternalOperation(bool, unsigned, Reg, Reg, Reg, unsigned) override { return std::nullopt; }

    CallbackOrAccessOneWord CompileSendOneWord(bool two, unsigned opc1, Reg CRn, Reg CRm, unsigned opc2) override {
        if (two || opc1 != 0) return std::monostate{};
        if (CRn == Reg::C13 && CRm == Reg::C0 && opc2 == 2) return &tpidrurw;
        // c7: CP15ISB (c5, 4), CP15DSB (c10, 4), CP15DMB (c10, 5). A single guest thread needs no barrier.
        if (CRn == Reg::C7 && ((CRm == Reg::C5 && opc2 == 4) || (CRm == Reg::C10 && (opc2 == 4 || opc2 == 5)))) {
            return Callback{[](void*, std::uint32_t, std::uint32_t) -> std::uint64_t { return 0; }, std::nullopt};
        }
        return std::monostate{};
    }

    CallbackOrAccessTwoWords CompileSendTwoWords(bool, unsigned, Reg) override { return std::monostate{}; }

    CallbackOrAccessOneWord CompileGetOneWord(bool two, unsigned opc1, Reg CRn, Reg CRm, unsigned opc2) override {
        if (two || opc1 != 0 || CRn != Reg::C13 || CRm != Reg::C0) return std::monostate{};
        if (opc2 == 3) return &tpidruro;
        if (opc2 == 2) return &tpidrurw;
        return std::monostate{};
    }

    CallbackOrAccessTwoWords CompileGetTwoWords(bool, unsigned, Reg) override { return std::monostate{}; }
    std::optional<Callback> CompileLoadWords(bool, bool, Reg, std::optional<std::uint8_t>) override { return std::nullopt; }
    std::optional<Callback> CompileStoreWords(bool, bool, Reg, std::optional<std::uint8_t>) override { return std::nullopt; }
};

}  // namespace

struct DynCpu {
    bool is64;
    // One guest thread per JIT: a private monitor. Declared before the JITs so it outlives them.
    Dynarmic::ExclusiveMonitor monitor{1};
    std::unique_ptr<Callbacks64> cb64;
    std::unique_ptr<Dynarmic::A64::Jit> jit64;
    std::unique_ptr<Callbacks32> cb32;
    std::unique_ptr<Dynarmic::A32::Jit> jit32;
    std::shared_ptr<Cp15> cp15 = std::make_shared<Cp15>();
};

static DynCpu* create(int32_t is64, uint8_t** page_table, uint32_t address_bits, uint64_t code_cache_size,
                      uint32_t flags, const DynHooks* hooks) {
    auto cpu = std::make_unique<DynCpu>();
    cpu->is64 = is64 != 0;
    if (cpu->is64) {
        auto cb = std::make_unique<Callbacks64>();
        cb->page_table = page_table;
        cb->page_count = 1ull << (address_bits - PAGE_BITS);
        cb->hooks = *hooks;
        Dynarmic::A64::UserConfig config;
        config.callbacks = cb.get();
        config.global_monitor = &cpu->monitor;
        config.tpidr_el0 = &cb->tpidr_el0;
        config.tpidrro_el0 = &cb->tpidrro_el0;
        config.page_table = reinterpret_cast<void**>(page_table);
        config.page_table_address_space_bits = address_bits;
        config.silently_mirror_page_table = false;
        config.absolute_offset_page_table = false;
        config.detect_misaligned_access_via_page_table = 16 | 32 | 64 | 128;
        config.only_detect_misalignment_via_page_table_on_page_boundary = true;
        config.define_unpredictable_behaviour = true;
        config.enable_cycle_counting = true;
        config.check_halt_on_memory_access = (flags & DYN_PRECISE_FAULTS) != 0;
        if (code_cache_size != 0) config.code_cache_size = code_cache_size;
        cpu->jit64 = std::make_unique<Dynarmic::A64::Jit>(config);
        cb->cpu = cpu->jit64.get();
        cb->jit = cpu->jit64.get();
        cb->halt = [](void* jit, Dynarmic::HaltReason r) { static_cast<Dynarmic::A64::Jit*>(jit)->HaltExecution(r); };
        cpu->cb64 = std::move(cb);
    } else {
        auto cb = std::make_unique<Callbacks32>();
        cb->page_table = page_table;
        cb->page_count = Dynarmic::A32::UserConfig::NUM_PAGE_TABLE_ENTRIES;
        cb->hooks = *hooks;
        Dynarmic::A32::UserConfig config;
        config.callbacks = cb.get();
        config.global_monitor = &cpu->monitor;
        config.page_table =
            reinterpret_cast<std::array<uint8_t*, Dynarmic::A32::UserConfig::NUM_PAGE_TABLE_ENTRIES>*>(page_table);
        config.absolute_offset_page_table = false;
        config.detect_misaligned_access_via_page_table = 16 | 32 | 64 | 128;
        config.only_detect_misalignment_via_page_table_on_page_boundary = true;
        config.define_unpredictable_behaviour = true;
        config.enable_cycle_counting = true;
        config.always_little_endian = true;
        config.check_halt_on_memory_access = (flags & DYN_PRECISE_FAULTS) != 0;
        config.coprocessors[15] = cpu->cp15;
        if (code_cache_size != 0) config.code_cache_size = code_cache_size;
        cpu->jit32 = std::make_unique<Dynarmic::A32::Jit>(config);
        cb->cpu = cpu->jit32.get();
        cb->jit = cpu->jit32.get();
        cb->halt = [](void* jit, Dynarmic::HaltReason r) { static_cast<Dynarmic::A32::Jit*>(jit)->HaltExecution(r); };
        cpu->cb32 = std::move(cb);
    }
    return cpu.release();
}

extern "C" {

// C++ exceptions must not cross into Rust: every entry point that can throw catches here.
DynCpu* dyn_new(int32_t is64, uint8_t** page_table, uint32_t address_bits, uint64_t code_cache_size,
                uint32_t flags, const DynHooks* hooks) {
    try {
        return create(is64, page_table, address_bits, code_cache_size, flags, hooks);
    } catch (...) {
        return nullptr;
    }
}

void dyn_free(DynCpu* cpu) { delete cpu; }

static uint32_t run(DynCpu* cpu, uint64_t budget, uint64_t* executed) {
    // dynarmic keeps the remaining tick count signed; clamp so "unlimited" stays positive.
    constexpr uint64_t max_budget = 1ull << 62;
    if (budget > max_budget) budget = max_budget;
    Dynarmic::HaltReason reason;
    if (cpu->is64) {
        cpu->cb64->budget = budget;
        cpu->cb64->executed = 0;
        reason = cpu->jit64->Run();
        cpu->jit64->ClearHalt(static_cast<Dynarmic::HaltReason>(0xffffffffu));
        if (executed) *executed = cpu->cb64->executed;
    } else {
        cpu->cb32->budget = budget;
        cpu->cb32->executed = 0;
        reason = cpu->jit32->Run();
        cpu->jit32->ClearHalt(static_cast<Dynarmic::HaltReason>(0xffffffffu));
        if (executed) *executed = cpu->cb32->executed;
    }
    return static_cast<uint32_t>(reason);
}

uint32_t dyn_run(DynCpu* cpu, uint64_t budget, uint64_t* executed) {
    try {
        return run(cpu, budget, executed);
    } catch (...) {
        return DYN_HALT_ERROR;
    }
}

void dyn_halt(DynCpu* cpu) {
    auto reason = static_cast<Dynarmic::HaltReason>(DYN_HALT_EXTERNAL);
    if (cpu->is64) cpu->jit64->HaltExecution(reason);
    else cpu->jit32->HaltExecution(reason);
}

uint64_t dyn_reg(const DynCpu* cpu, uint32_t index) {
    if (cpu->is64) {
        const auto& j = *cpu->jit64;
        if (index <= 30) return j.GetRegister(index);
        switch (index) {
            case 31: return j.GetSP();
            case 32: return j.GetPC();
            case 33: return j.GetPstate();
            case 34: return cpu->cb64->tpidr_el0;
            case 35: return cpu->cb64->tpidrro_el0;
        }
        return 0;
    }
    auto& j = *cpu->jit32;
    if (index <= 15) return j.Regs()[index];
    switch (index) {
        case 16: return j.Cpsr();
        case 17: return j.Fpscr();
        case 18: return cpu->cp15->tpidruro;
    }
    return 0;
}

void dyn_set_reg(DynCpu* cpu, uint32_t index, uint64_t value) {
    if (cpu->is64) {
        auto& j = *cpu->jit64;
        if (index <= 30) { j.SetRegister(index, value); return; }
        switch (index) {
            case 31: j.SetSP(value); break;
            case 32: j.SetPC(value); break;
            case 33: j.SetPstate(static_cast<uint32_t>(value)); break;
            case 34: cpu->cb64->tpidr_el0 = value; break;
            case 35: cpu->cb64->tpidrro_el0 = value; break;
        }
        return;
    }
    auto& j = *cpu->jit32;
    if (index <= 15) { j.Regs()[index] = static_cast<uint32_t>(value); return; }
    switch (index) {
        case 16: j.SetCpsr(static_cast<uint32_t>(value)); break;
        case 17: j.SetFpscr(static_cast<uint32_t>(value)); break;
        case 18: cpu->cp15->tpidruro = static_cast<uint32_t>(value); break;
    }
}

void dyn_invalidate(DynCpu* cpu, uint64_t address, uint64_t length) {
    if (cpu->is64) cpu->jit64->InvalidateCacheRange(address, length);
    else cpu->jit32->InvalidateCacheRange(static_cast<uint32_t>(address), length);
}

}  // extern "C"
