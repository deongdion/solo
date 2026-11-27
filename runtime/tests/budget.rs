//! The instruction budget covers a whole call, including the resumes between syscalls.

use backend::Arch;
use machine::Perms;
use runtime::{Emulator, Error, Options};

#[test]
fn syscall_loop_runs_out_of_budget() {
    let mut options = Options::new(Arch::Arm64);
    options.kernel.echo_output = false;
    options.budget = 1_000_000;
    let mut emu = Emulator::new(options).unwrap();
    let code = 0x4000_0000;
    emu.cpu.map(code, 0x1000, Perms::RW, "loop").unwrap();
    // mov x8, #172 (getpid); svc #0; b -8
    for (i, word) in [0xd280_1588u32, 0xd400_0001, 0x17ff_fffe].iter().enumerate() {
        emu.cpu.memory_mut().write_u32(code + i as u64 * 4, *word).unwrap();
    }
    emu.cpu.protect(code, 0x1000, Perms::RX).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(emu.call(code, &[]), Err(Error::Budget)));
    assert!(started.elapsed().as_secs() < 10);
}
