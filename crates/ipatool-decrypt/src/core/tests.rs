use super::*;

/// A thin little-endian Mach-O header with `commands` as its load commands.
fn macho(cpu: u32, subtype: u32, commands: &[&[u8]]) -> Vec<u8> {
    let magic = if cpu == CPU_TYPE_ARM64 {
        MACHO_MAGIC_64_LE
    } else {
        MACHO_MAGIC_LE
    };
    let mut out = Vec::new();
    for word in [magic, cpu, subtype, 2] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let ncmds = u32::try_from(commands.len()).expect("few commands");
    let sizeofcmds: usize = commands.iter().map(|command| command.len()).sum();
    out.extend_from_slice(&ncmds.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(sizeofcmds)
            .expect("small commands")
            .to_le_bytes(),
    );
    out.extend_from_slice(&0u32.to_le_bytes());
    if cpu == CPU_TYPE_ARM64 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    for command in commands {
        out.extend_from_slice(command);
    }
    out.resize(0x3000, 0);
    out
}

fn encryption_command(cmd: u32, cryptoff: u32, cryptsize: u32, cryptid: u32) -> Vec<u8> {
    let size = if cmd == LC_ENCRYPTION_INFO_64 { 24 } else { 20 };
    let mut out = Vec::new();
    for word in [cmd, size, cryptoff, cryptsize, cryptid] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.resize(size as usize, 0);
    out
}

#[test]
fn rejects_non_arm_macho() {
    let err = arm_encryption_info(b"\x7fELF\0\0\0\0").unwrap_err();
    assert_eq!(err.to_string(), "not a supported ARM Mach-O");
}

#[test]
fn rejects_empty_and_short_input() {
    assert_eq!(
        arm_encryption_info(&[]).unwrap_err().to_string(),
        "not a supported ARM Mach-O"
    );
    // A thin header needs 28 bytes.
    assert_eq!(
        arm_encryption_info(b"\xcf\xfa\xed\xfe\x0c\0\0\x01")
            .unwrap_err()
            .to_string(),
        "not a supported ARM Mach-O"
    );
}

#[test]
fn rejects_bad_fat_tables() {
    let mut fat = b"\xca\xfe\xba\xbe\0\0\0\0".to_vec();
    assert_eq!(
        arm_encryption_info(&fat).unwrap_err().to_string(),
        "invalid FAT32 architecture table"
    );
    fat[3] = 0xbf;
    fat[7] = 2;
    assert_eq!(
        arm_encryption_info(&fat).unwrap_err().to_string(),
        "invalid FAT64 architecture table"
    );
}

#[test]
fn thin_x86_has_no_arm_slice() {
    let data = macho(0x0100_0007, 3, &[]);
    assert_eq!(
        arm_encryption_info(&data).unwrap_err().to_string(),
        "expected one ARM slice to decrypt, found 0"
    );
}

#[test]
fn encryption_info_of_thin_slices() {
    let command = encryption_command(LC_ENCRYPTION_INFO_64, 0x1000, 0x2000, 1);
    let info = arm_encryption_info(&macho(CPU_TYPE_ARM64, 0x8000_0002, &[&command]))
        .expect("parses")
        .expect("encrypted");
    assert_eq!(
        info,
        MachoInfo {
            base: 0,
            size: 0x3000,
            cpu: CPU_TYPE_ARM64,
            subtype: 0x8000_0002,
            cryptoff: 0x1000,
            cryptsize: 0x2000,
            cryptid_offset: 32 + 16,
        }
    );
    // arm64_32 and 32-bit ARM use the 32-bit header and LC_ENCRYPTION_INFO.
    let command = encryption_command(LC_ENCRYPTION_INFO, 0x1000, 0x1000, 1);
    for cpu in [CPU_TYPE_ARM64_32, CPU_TYPE_ARM] {
        let info = arm_encryption_info(&macho(cpu, 9, &[&command]))
            .expect("parses")
            .expect("encrypted");
        assert_eq!(info.cryptid_offset, 28 + 16);
    }
    // A 64-bit command in a 32-bit header is no encryption command at all.
    let command = encryption_command(LC_ENCRYPTION_INFO_64, 0x1000, 0x1000, 1);
    assert_eq!(
        arm_encryption_info(&macho(CPU_TYPE_ARM, 9, &[&command])).expect("parses"),
        None
    );
}

#[test]
fn encryption_command_errors() {
    let unencrypted = encryption_command(LC_ENCRYPTION_INFO_64, 0x1000, 0x1000, 0);
    assert_eq!(
        arm_encryption_info(&macho(CPU_TYPE_ARM64, 0, &[&unencrypted])).expect("parses"),
        None
    );
    let command = encryption_command(LC_ENCRYPTION_INFO_64, 0x1000, 0x1000, 1);
    let twice = macho(CPU_TYPE_ARM64, 0, &[&command, &command]);
    assert_eq!(
        arm_encryption_info(&twice).unwrap_err().to_string(),
        "expected one encryption command, found 2"
    );
    let odd = encryption_command(LC_ENCRYPTION_INFO_64, 0x1000, 0x800, 1);
    assert_eq!(
        arm_encryption_info(&macho(CPU_TYPE_ARM64, 0, &[&odd]))
            .unwrap_err()
            .to_string(),
        "unsupported cryptid/size: 1, 2048"
    );
    let past_end = encryption_command(LC_ENCRYPTION_INFO_64, 0x2000, 0x2000, 1);
    assert_eq!(
        arm_encryption_info(&macho(CPU_TYPE_ARM64, 0, &[&past_end]))
            .unwrap_err()
            .to_string(),
        "invalid encrypted region bounds"
    );
    let mut wrong_magic = macho(CPU_TYPE_ARM64, 0, &[&command]);
    wrong_magic[4..8].copy_from_slice(&CPU_TYPE_ARM.to_le_bytes());
    assert_eq!(
        arm_encryption_info(&wrong_magic).unwrap_err().to_string(),
        "invalid Mach-O slice header"
    );
}

#[test]
fn rejects_bad_supf_format() {
    let err =
        arm_supf_record_offset(b"\x05999\0\0\0\0\0\0\0\x01", 1, CPU_TYPE_ARM64, 0).unwrap_err();
    assert_eq!(err.to_string(), "unsupported SUPF format");
    let err =
        arm_supf_record_offset(b"\x04510\0\0\0\0\0\0\0\x01", 1, CPU_TYPE_ARM64, 0).unwrap_err();
    assert_eq!(err.to_string(), "unsupported SUPF format");
}

#[test]
fn rejects_short_supf() {
    let err = arm_supf_record_offset(b"\x05510", 1, CPU_TYPE_ARM64, 0).unwrap_err();
    assert_eq!(err.to_string(), "unsupported SUPF format");
    let err = arm_supf_record_offset(b"", 1, CPU_TYPE_ARM64, 0).unwrap_err();
    assert_eq!(err.to_string(), "unsupported SUPF format");
}

#[test]
fn accepts_every_supf_format_ipadecrypt_does() {
    for format in SUPF_FORMATS {
        let mut supf = vec![5];
        supf.extend_from_slice(format);
        supf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        // One arm64e record block, matched without its capability bits.
        for word in [12 + 32, CPU_TYPE_ARM64, 0x8000_0002, 1] {
            supf.extend_from_slice(&word.to_be_bytes());
        }
        supf.extend_from_slice(&[0; 32]);
        assert_eq!(
            arm_supf_record_offset(&supf, 1, CPU_TYPE_ARM64, 2).expect("record"),
            12 + 4 + 12
        );
    }
}

#[test]
fn supp_offsets() {
    let mut v4 = vec![4, b'5', b'0', b'7'];
    v4.extend_from_slice(&2u32.to_be_bytes());
    v4.extend_from_slice(&[0; 64]);
    assert_eq!(pclass_page_key_offset(&v4, 0).expect("page 0"), 8);
    assert_eq!(pclass_page_key_offset(&v4, 1).expect("page 1"), 40);
    assert_eq!(
        pclass_page_key_offset(&v4, 2).unwrap_err().to_string(),
        "SUPP page key missing"
    );
    let mut v1 = vec![1, b'5', b'0', b'7'];
    v1.extend_from_slice(&[0; 20]);
    v1.extend_from_slice(&1u32.to_be_bytes());
    v1.extend_from_slice(&[0; 32]);
    assert_eq!(pclass_page_key_offset(&v1, 0).expect("page 0"), 28);
    assert_eq!(
        pclass_page_key_offset(&v1[..20], 0)
            .unwrap_err()
            .to_string(),
        "truncated SUPP header"
    );
    v1.truncate(40);
    assert_eq!(
        pclass_page_key_offset(&v1, 0).unwrap_err().to_string(),
        "invalid SUPP page count"
    );
    assert_eq!(
        pclass_page_key_offset(b"\x03507\0\0\0\0", 0)
            .unwrap_err()
            .to_string(),
        "unsupported SUPP format"
    );
}

fn atom(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(8 + body.len())
        .expect("small atom")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    out
}

#[test]
fn arcade_sinf_is_key_field_4() {
    let sinf = |key: &[u8]| atom(*b"sinf", &atom(*b"schi", &atom(*b"key ", key)));
    assert!(is_arcade_sinf(&sinf(&[4, 0, 0, 1])));
    assert!(!is_arcade_sinf(&sinf(&[3, 0, 0, 1])));
    assert!(!is_arcade_sinf(&sinf(&[4, 0, 0])));
    assert!(!is_arcade_sinf(b"not a sinf"));
}

#[test]
fn sinf_field_errors_quote_like_go() {
    let sinf = atom(*b"sinf", &atom(*b"schi", &atom(*b"name", b"x")));
    assert_eq!(
        derive_work16(&[0; 16], &sinf).unwrap_err().to_string(),
        "expected one \"iviv\" field"
    );
    let two_schi = atom(
        *b"sinf",
        &[atom(*b"schi", &[]), atom(*b"schi", &[])].concat(),
    );
    assert_eq!(
        derive_work16(&[0; 16], &two_schi).unwrap_err().to_string(),
        "expected one schi container"
    );
    assert_eq!(
        derive_work16(&[0; 16], &atom(*b"frma", &[]))
            .unwrap_err()
            .to_string(),
        "expected outer sinf atom"
    );
    assert!(!credential_decodes(&[0; 16], &sinf));
}

#[test]
fn source_blocks_layout() {
    let wrapped: [u8; 16] = core::array::from_fn(|i| u8::try_from(i).expect("0..16"));
    let (first, second, third) = source_blocks(wrapped, 1);
    // page_offset = (1 << 12) big-endian over 8 bytes.
    let page_offset = [0, 0, 0, 0, 0, 0, 0x10, 0];
    assert_eq!(&first[..8], &wrapped[8..16]);
    assert_eq!(&first[8..], &page_offset);
    assert_eq!(second, wrapped);
    assert_eq!(&third[..8], &page_offset);
    assert_eq!(&third[8..], &wrapped[..8]);
}
