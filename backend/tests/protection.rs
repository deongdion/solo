//! Execute permission: jumping into data must fault, not translate it.

use backend::{Arch, Cpu, Dynarmic, Stop};
use machine::Perms;

fn jump_into_data(arch: Arch) {
    let mut cpu = Dynarmic::new(arch).unwrap();
    let data = 0x1000_0000;
    // 1 MiB of zeros: `andeq r0, r0, r0` on ARM, an undefined encoding on A64.
    cpu.map(data, 1 << 20, Perms::RW, "data").unwrap();
    cpu.map(0xe000_0000, 0x10000, Perms::RW, "stack").unwrap();
    cpu.set_reg(backend::Reg::Sp, 0xe000_ff00);
    let (stop, _) = cpu.call(data, &[], u64::MAX).unwrap();
    assert_eq!(stop, Stop::Fault { address: data, write: false });

    // Making the page executable lets the same address run (and be retranslated).
    cpu.protect(data, 0x1000, Perms::RX).unwrap();
    let ret: u32 = if arch == Arch::Arm64 { 0xd65f_03c0 } else { 0xe12f_ff1e };
    cpu.memory_mut().write_u32(data, ret).unwrap();
    cpu.invalidate(data, 4);
    let (stop, _) = cpu.call(data, &[], u64::MAX).unwrap();
    assert_eq!(stop, Stop::Returned);
}

#[test]
fn arm64_data_is_not_executable() {
    jump_into_data(Arch::Arm64);
}

#[test]
fn arm32_data_is_not_executable() {
    jump_into_data(Arch::Arm32);
}
