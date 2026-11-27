//! Disassembly of single instructions for traces and crash reports (yaxpeax-arm).

use backend::Arch;
use yaxpeax_arch::{Decoder, LengthedInstruction, U8Reader};

/// Decodes the instruction at the start of `bytes`: its length and text. Undecodable bytes give
/// the natural instruction size and `.inst 0x...`.
pub fn disassemble(arch: Arch, thumb: bool, bytes: &[u8]) -> (usize, String) {
    let fallback = |len: usize| {
        let word = bytes.iter().take(len).rev().fold(0u64, |v, b| (v << 8) | *b as u64);
        (len, format!(".inst {word:#x}"))
    };
    match arch {
        Arch::Arm64 => {
            let decoder = yaxpeax_arm::armv8::a64::InstDecoder::default();
            match decoder.decode(&mut U8Reader::new(bytes)) {
                Ok(insn) => (4, insn.to_string()),
                Err(_) => fallback(4),
            }
        }
        Arch::Arm32 => {
            let mut decoder = yaxpeax_arm::armv7::InstDecoder::armv7();
            decoder.set_thumb_mode(thumb);
            match decoder.decode(&mut U8Reader::new(bytes)) {
                Ok(insn) => {
                    let len = insn.len().to_const() as usize;
                    (len, insn.to_string())
                }
                Err(_) => {
                    // A 32-bit Thumb instruction starts with 0b11101, 0b11110 or 0b11111.
                    let wide = !thumb || (bytes.len() >= 2 && bytes[1] >> 3 >= 0b11101);
                    fallback(if wide { 4 } else { 2 })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_all_three_instruction_sets() {
        // stp x29, x30, [sp, #-0x20]!
        let (len, text) = disassemble(Arch::Arm64, false, &[0xfd, 0x7b, 0xbe, 0xa9]);
        assert_eq!(len, 4);
        assert!(text.starts_with("stp x29, x30, [sp"), "{text}");
        // push {r4, lr} (ARM)
        let (len, text) = disassemble(Arch::Arm32, false, &[0x10, 0x40, 0x2d, 0xe9]);
        assert_eq!(len, 4);
        assert!(text.contains("r4") && text.contains("lr"), "{text}");
        // push {r7, lr} (Thumb, 16 bits)
        let (len, text) = disassemble(Arch::Arm32, true, &[0x80, 0xb5]);
        assert_eq!(len, 2);
        assert!(text.contains("r7") && text.contains("lr"), "{text}");
        // b.w (Thumb, 32 bits)
        let (len, _) = disassemble(Arch::Arm32, true, &[0x00, 0xf0, 0x2a, 0xb8]);
        assert_eq!(len, 4);
    }
}
