//! 64-bit floating-point registers are readable and writable around a guest call.

use backend::{Arch, Cpu, Dynarmic, Reg, Stop};
use machine::Perms;

fn add_doubles(cpu: &mut dyn Cpu, arch: Arch) {
    let code = 0x1000_0000;
    cpu.map(code, 0x1000, Perms::RW, "code").unwrap();
    cpu.map(0xe000_0000, 0x10000, Perms::RW, "stack").unwrap();
    cpu.set_reg(Reg::Sp, 0xe000_ff00);
    // fadd d0, d0, d1; ret  /  vadd.f64 d0, d0, d1; bx lr
    let words: [u32; 2] = if arch == Arch::Arm64 { [0x1e61_2800, 0xd65f_03c0] } else { [0xee30_0b01, 0xe12f_ff1e] };
    for (i, w) in words.iter().enumerate() {
        cpu.memory_mut().write_u32(code + i as u64 * 4, *w).unwrap();
    }
    cpu.protect(code, 0x1000, Perms::RX).unwrap();
    cpu.set_reg(Reg::Fp(0), 1.5f64.to_bits());
    cpu.set_reg(Reg::Fp(1), 2.25f64.to_bits());
    cpu.set_reg(Reg::Fp(31), 7.0f64.to_bits());
    let (stop, _) = cpu.call(code, &[], u64::MAX).unwrap();
    assert_eq!(stop, Stop::Returned);
    assert_eq!(f64::from_bits(cpu.reg(Reg::Fp(0))), 3.75);
    assert_eq!(f64::from_bits(cpu.reg(Reg::Fp(31))), 7.0);
}

#[test]
fn arm64_fp_registers() {
    add_doubles(&mut Dynarmic::new(Arch::Arm64).unwrap(), Arch::Arm64);
}

#[test]
fn arm32_fp_registers() {
    add_doubles(&mut Dynarmic::new(Arch::Arm32).unwrap(), Arch::Arm32);
}

#[cfg(feature = "unicorn")]
#[test]
fn unicorn_fp_registers() {
    add_doubles(&mut backend::Unicorn::new(Arch::Arm64).unwrap(), Arch::Arm64);
    add_doubles(&mut backend::Unicorn::new(Arch::Arm32).unwrap(), Arch::Arm32);
}
