//! Thread contexts: flags, FP control and full vector registers survive a save, a clobber and a
//! load, and the guest sees them.

use backend::{Arch, Cpu, Dynarmic, Reg, Stop, load_context, save_context};
use machine::Perms;

fn round_trip(cpu: &mut dyn Cpu, arch: Arch) {
    let code = 0x1000_0000;
    cpu.map(code, 0x1000, Perms::RW, "code").unwrap();
    cpu.map(0xe000_0000, 0x10000, Perms::RW, "stack").unwrap();
    cpu.set_reg(Reg::Sp, 0xe000_ff00);
    let words: &[u32] = if arch == Arch::Arm64 {
        // cset x0, eq; umov x1, v1.d[1]; ret
        &[0x9a9f_17e0, 0x4e18_3c21, 0xd65f_03c0]
    } else {
        // moveq r0, #1; movne r0, #0; vmov r1, r2, d3; bx lr
        &[0x03a0_0001, 0x13a0_0000, 0xec52_1b13, 0xe12f_ff1e]
    };
    for (i, w) in words.iter().enumerate() {
        cpu.memory_mut().write_u32(code + i as u64 * 4, *w).unwrap();
    }
    cpu.protect(code, 0x1000, Perms::RX).unwrap();

    let user = if arch == Arch::Arm64 { 0 } else { 0x10 };
    let z = 0x4000_0000;
    // Round towards plus infinity: the same bits in FPCR and FPSCR.
    let fp_control = 0x0040_0000;
    cpu.set_reg(Reg::Flags, z | user);
    cpu.set_reg(Reg::FpControl, fp_control);
    cpu.set_reg(Reg::General(5), 0x55);
    cpu.set_reg(Reg::Tls, 0x7000);
    if arch == Arch::Arm64 {
        cpu.set_reg(Reg::Fp(1), 0x99);
        cpu.set_reg(Reg::VecHigh(1), 0x1122_3344_5566_7788);
    } else {
        cpu.set_reg(Reg::Fp(3), 0x1122_3344_5566_7788);
    }
    let saved = save_context(cpu);
    assert_eq!(saved.get(arch, Reg::General(5)), Some(0x55));

    // Another thread runs with other values.
    cpu.set_reg(Reg::Flags, user);
    cpu.set_reg(Reg::FpControl, 0);
    cpu.set_reg(Reg::General(5), 0);
    cpu.set_reg(Reg::Tls, 0);
    for i in 0..32 {
        cpu.set_reg(Reg::Fp(i), 0);
    }
    let (stop, flag) = cpu.call(code, &[], u64::MAX).unwrap();
    assert_eq!((stop, flag), (Stop::Returned, 0));

    load_context(cpu, &saved);
    assert_eq!(save_context(cpu), saved);
    assert_eq!(cpu.reg(Reg::FpControl), fp_control);
    assert_eq!(cpu.reg(Reg::General(5)), 0x55);
    assert_eq!(cpu.reg(Reg::Tls), 0x7000);
    let (stop, flag) = cpu.call(code, &[], u64::MAX).unwrap();
    assert_eq!((stop, flag), (Stop::Returned, 1), "Z flag restored");
    let high = if arch == Arch::Arm64 { cpu.reg(Reg::General(1)) } else { cpu.reg(Reg::General(2)) << 32 | cpu.reg(Reg::General(1)) };
    assert_eq!(high, 0x1122_3344_5566_7788);
    if arch == Arch::Arm64 {
        assert_eq!(cpu.reg(Reg::Fp(1)), 0x99, "low half kept when the high half is loaded");
    }
}

#[test]
fn dynarmic_context() {
    round_trip(&mut Dynarmic::new(Arch::Arm64).unwrap(), Arch::Arm64);
    round_trip(&mut Dynarmic::new(Arch::Arm32).unwrap(), Arch::Arm32);
}

#[cfg(feature = "unicorn")]
#[test]
fn unicorn_context() {
    round_trip(&mut backend::Unicorn::new(Arch::Arm64).unwrap(), Arch::Arm64);
    round_trip(&mut backend::Unicorn::new(Arch::Arm32).unwrap(), Arch::Arm32);
}
