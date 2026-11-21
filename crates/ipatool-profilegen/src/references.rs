//! Port of Go `profilegen/references.go`.

use crate::collect::adrp;
use crate::source::le32;

/// Go's `pageReference`: an ADRP-derived address within a basic block. Unresolved uses keep
/// a conservative window; understood arithmetic and memory operands keep only the pages they
/// address.
#[derive(Clone, Copy, Default)]
struct PageReference {
    address: u64,
    valid: bool,
    resolved: bool,
}

struct Registers<'f> {
    registers: [PageReference; 32],
    add: &'f mut dyn FnMut(u64),
}

impl Registers<'_> {
    fn forget(&mut self, register: u32) {
        let reference = self.registers[register as usize];
        if reference.valid && !reference.resolved {
            for page in 0..16u64 {
                (self.add)(reference.address.wrapping_add(page * 4096));
            }
        }
        self.registers[register as usize] = PageReference::default();
    }

    /// An earlier understood access does not prove that a surviving address won't be
    /// indexed differently in another basic block.
    fn uncertain(&mut self, register: u32) {
        self.registers[register as usize].resolved = false;
        self.forget(register);
    }

    fn flush(&mut self) {
        for register in 0..32 {
            self.uncertain(register);
        }
    }

    fn access(&mut self, register: u32, offset: i64, size: u64) {
        let reference = &mut self.registers[register as usize];
        if reference.valid {
            let address = reference.address.wrapping_add_signed(offset);
            reference.resolved = true;
            (self.add)(address);
            (self.add)(address.wrapping_add(size).wrapping_sub(1));
        }
    }
}

/// Go's `dataReferences`: decodes immediate ARM64 address construction without executing a
/// particular path, so collection also covers server-dependent branches.
pub(crate) fn data_references(code: &[u8], pc: u64, add: &mut dyn FnMut(u64)) {
    let mut state = Registers {
        registers: [PageReference::default(); 32],
        add,
    };
    let mut offset = 0usize;
    while offset + 4 <= code.len() {
        let word = le32(&code[offset..]);
        let here = pc + offset as u64;
        offset += 4;
        let (rd, rn) = (word & 31, word >> 5 & 31);
        if word & 0x9f00_0000 == 0x9000_0000 {
            // ADRP
            state.forget(rd);
            let address = adrp(word, here);
            (state.add)(address);
            state.registers[rd as usize] = PageReference {
                address,
                valid: rd != 31,
                resolved: false,
            };
        } else if word & 0x9f00_0000 == 0x1000_0000 {
            // ADR (signed, byte-relative)
            state.forget(rd);
            let mut immediate = i64::from(word >> 5 & 0x7ffff) << 2 | i64::from(word >> 29 & 3);
            if immediate & (1 << 20) != 0 {
                immediate -= 1 << 21;
            }
            let address = here.wrapping_add_signed(immediate);
            (state.add)(address);
            state.registers[rd as usize] = PageReference {
                address,
                valid: rd != 31,
                resolved: true,
            };
        } else if word & 0x9f00_0000 == 0x9100_0000 {
            // 64-bit ADD/SUB immediate
            let mut reference = state.registers[rn as usize];
            if reference.valid {
                let mut immediate = i64::from(word >> 10 & 0xfff);
                if word & (1 << 22) != 0 {
                    immediate <<= 12;
                }
                if word & (1 << 30) != 0 {
                    immediate = -immediate;
                }
                state.access(rn, immediate, 1);
                reference.address = reference.address.wrapping_add_signed(immediate);
                reference.resolved = true;
            }
            state.forget(rd);
            state.registers[rd as usize] = reference;
        } else if word & 0x3b00_0000 == 0x3900_0000 {
            // unsigned-immediate LDR/STR (including SIMD)
            let mut scale = word >> 30;
            if word & (1 << 26) != 0 && word & (1 << 23) != 0 {
                scale = 4;
            }
            state.access(rn, i64::from(word >> 10 & 0xfff) << scale, 1 << scale);
            if word & (1 << 26) == 0 && word & 0x00c0_0000 != 0 {
                state.forget(rd);
            }
        } else if word & 0x3b20_0000 == 0x3800_0000 {
            // unscaled, pre- or post-indexed load/store
            let immediate = i64::from((word << 11).cast_signed() >> 23);
            let mode = word >> 10 & 3;
            // Post-indexing uses the original address.
            state.access(rn, if mode == 1 { 0 } else { immediate }, 16);
            if (mode == 1 || mode == 3) && state.registers[rn as usize].valid {
                let register = &mut state.registers[rn as usize];
                register.address = register.address.wrapping_add_signed(immediate);
            }
            if word & (1 << 26) == 0 && word & 0x00c0_0000 != 0 {
                state.forget(rd);
            }
        } else if word & 0x3a00_0000 == 0x2800_0000 {
            // load/store pair
            let mut scale = 2 + (word >> 31);
            if word & (1 << 26) != 0 {
                scale = 2 + (word >> 30);
            }
            let immediate = i64::from((word << 10).cast_signed() >> 25) << scale;
            let mode = word >> 23 & 3;
            state.access(rn, if mode == 1 { 0 } else { immediate }, 2 << scale);
            if (mode == 1 || mode == 3) && state.registers[rn as usize].valid {
                let register = &mut state.registers[rn as usize];
                register.address = register.address.wrapping_add_signed(immediate);
            }
            if word & (1 << 26) == 0 && word & (1 << 22) != 0 {
                state.forget(rd);
                state.forget(word >> 10 & 31);
            }
        } else if word & 0x1f80_0000 == 0x1280_0000 {
            // MOVN/MOVZ/MOVK immediate
            state.forget(rd);
        } else if word == 0xd503_201f {
            // NOP
        } else if word & 0x1c00_0000 == 0x1400_0000 {
            // branch, exception or system instruction
            state.flush();
        } else {
            // Registers independent of an unknown instruction survive it; its potential
            // operands fall back instead of propagating through unrecognized arithmetic.
            state.uncertain(rn);
            state.uncertain(word >> 16 & 31);
            state.forget(rd);
        }
    }
    for register in 0..32 {
        state.forget(register);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Page addresses collected from synthetic instructions, independent of the pinned build.
    fn reference_pages(words: &[u32]) -> Vec<u64> {
        let code: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let mut pages = BTreeSet::new();
        data_references(&code, 0x10_0000, &mut |address| {
            pages.insert(address & !4095);
        });
        pages.into_iter().collect()
    }

    #[test]
    fn retains_the_addressed_page_for_an_adrp_and_add_pair() {
        // add x0, x0, #256
        assert_eq!(reference_pages(&[0x9000_0000, 0x9104_0000]), [0x10_0000]);
    }

    #[test]
    fn tracks_addresses_across_independent_instructions() {
        // mov x1, #1; nop; add x0, x0, #256
        assert_eq!(
            reference_pages(&[0x9000_0000, 0xd280_0021, 0xd503_201f, 0x9104_0000]),
            [0x10_0000]
        );
    }

    #[test]
    fn includes_scaled_load_offsets_beyond_the_adrp_page() {
        // ldr x0, [x0, #16376]
        assert_eq!(
            reference_pages(&[0x9000_0000, 0xf95f_fc00]),
            [0x10_0000, 0x10_3000]
        );
    }

    #[test]
    fn includes_shifted_immediate_address_arithmetic() {
        // add x0, x0, #2, lsl #12
        assert_eq!(
            reference_pages(&[0x9000_0000, 0x9140_0800]),
            [0x10_0000, 0x10_2000]
        );
    }

    #[test]
    fn includes_both_pages_of_an_unaligned_load() {
        // add x0, x0, #4092; ldr x0, [x0]
        assert_eq!(
            reference_pages(&[0x9000_0000, 0x913f_f000, 0xf940_0000]),
            [0x10_0000, 0x10_1000]
        );
    }

    #[test]
    fn sign_extends_unscaled_negative_load_offsets() {
        // ldur x0, [x0, #-8]
        assert_eq!(
            reference_pages(&[0x9000_0000, 0xf85f_8000]),
            [0xf_f000, 0x10_0000]
        );
    }

    #[test]
    fn retains_the_full_conservative_range_for_an_unknown_consumer() {
        let pages = reference_pages(&[0x9000_0000, 0x8b01_0000]);
        assert_eq!(pages.len(), 16);
        assert_eq!(pages[0], 0x10_0000);
        assert_eq!(pages[15], 0x10_f000);
    }

    #[test]
    fn keeps_a_conservative_range_for_addresses_live_at_a_branch() {
        // ldr x1, [x0]; b next
        assert_eq!(
            reference_pages(&[0x9000_0000, 0xf940_0001, 0x1400_0001]).len(),
            16
        );
    }

    #[test]
    fn does_not_propagate_register_values_across_branches() {
        assert_eq!(
            reference_pages(&[0x9000_0000, 0x1400_0001, 0xf97f_fc00]).len(),
            16
        );
    }
}
