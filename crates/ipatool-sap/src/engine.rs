//! Smoke module, replaced by the engine port.

#[cfg(test)]
mod tests {
    use unicorn_engine::{Arch, Mode, Prot, Unicorn};

    #[test]
    fn runs_arm64() {
        let mut uc = Unicorn::new(Arch::ARM64, Mode::LITTLE_ENDIAN).unwrap();
        uc.mem_map(0x1000, 0x1000, Prot::ALL).unwrap();
        // mov x0, #42 ; brk #0 is avoided: stop at the end address instead.
        uc.mem_write(0x1000, &[0x40, 0x05, 0x80, 0xd2]).unwrap();
        uc.emu_start(0x1000, 0x1004, 0, 0).unwrap();
        assert_eq!(uc.reg_read(unicorn_engine::RegisterARM64::X0).unwrap(), 42);
    }
}
