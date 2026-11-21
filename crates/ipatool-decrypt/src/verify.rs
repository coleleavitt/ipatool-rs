//! The check after decrypting: every Mach-O in the output must have `cryptid` 0 and a
//! non-zero encrypted region.
//!
//! A port of the source-free part of ipadecrypt's `internal/pipeline/verify.go` and the
//! `internal/macho` helpers it uses (MIT). The source byte-compare (`--extra-verify`) is
//! not ported.

use crate::core::{be32, be64, le32};
use crate::error::Result;

const MAGIC_LE: u32 = 0xFEED_FACE;
const MAGIC_64_LE: u32 = 0xFEED_FACF;
const MAGIC_BE: u32 = 0xCEFA_EDFE;
const MAGIC_64_BE: u32 = 0xCFFA_EDFE;
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_CIGAM: u32 = 0xBEBA_FECA;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;
const FAT_CIGAM_64: u32 = 0xBFBA_FECA;
const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2C;

/// What [`verify_members`] found (`verify.go`'s `VerifyResult` without the source compare).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Verification {
    /// Mach-Os parsed in the output.
    pub scanned: usize,
    /// Mach-Os with `cryptid` != 0: decryption did not write through.
    pub still_encrypted: Vec<String>,
    /// Mach-Os with `cryptid` 0 whose encrypted region is all zeros: no plaintext.
    pub all_zero_crypt: Vec<String>,
    /// Mach-Os whose load commands did not parse.
    pub skipped: Vec<String>,
}

impl Verification {
    /// `VerifyResult.OK`: no still-encrypted and no zero-filled binary.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.still_encrypted.is_empty() && self.all_zero_crypt.is_empty()
    }
}

/// `macho.IsMagic`: whether `data` starts with a thin or fat Mach-O magic.
fn is_macho(data: &[u8]) -> bool {
    le32(data, 0).is_ok_and(|magic| {
        matches!(
            magic,
            MAGIC_LE
                | MAGIC_64_LE
                | MAGIC_BE
                | MAGIC_64_BE
                | FAT_MAGIC
                | FAT_CIGAM
                | FAT_MAGIC_64
                | FAT_CIGAM_64
        )
    })
}

/// `verify.go`'s `Verify` over the output's members, given as `(name, contents)` in
/// archive order. With `skip_appex`, `.appex` members are ignored: they were left
/// encrypted on purpose.
///
/// # Errors
/// If a member cannot be read.
pub fn verify_members<I>(members: I, skip_appex: bool) -> Result<Verification>
where
    I: IntoIterator<Item = Result<(String, Vec<u8>)>>,
{
    let mut result = Verification::default();
    for member in members {
        let (name, data) = member?;
        if !name.starts_with("Payload/") || name.ends_with('/') {
            continue;
        }
        if skip_appex && name.contains(".appex/") {
            continue;
        }
        if !is_macho(&data) {
            continue;
        }
        result.scanned += 1;
        match slice_has_cryptid(&data) {
            Err(_) => result.skipped.push(name),
            Ok(true) => result.still_encrypted.push(name),
            Ok(false) => {
                if crypt_zeroed(&data) {
                    result.all_zero_crypt.push(name);
                }
            }
        }
    }
    Ok(result)
}

/// `macho.SliceHasCryptid`: whether any slice has an encryption command with `cryptid`
/// != 0.
fn slice_has_cryptid(data: &[u8]) -> Result<bool> {
    let magic = le32(data, 0).map_err(|_| crate::error::format_err("too short"))?;
    match magic {
        FAT_MAGIC | FAT_CIGAM | FAT_MAGIC_64 | FAT_CIGAM_64 => check_fat(data),
        MAGIC_LE | MAGIC_64_LE => check_thin(data, false),
        MAGIC_BE | MAGIC_64_BE => check_thin(data, true),
        _ => Err(crate::error::format_err("not mach-o")),
    }
}

/// A slice of a thin or fat Mach-O (`macho.Slice`): its file offset and size.
struct Slice {
    offset: usize,
    size: usize,
}

/// `macho.fatSlices` / `macho.FatSlices`: the slices of a fat Mach-O, or the whole file
/// for a thin one.
fn slices(data: &[u8]) -> Result<Vec<Slice>> {
    let magic = le32(data, 0)?;
    let is64 = match magic {
        MAGIC_LE | MAGIC_64_LE | MAGIC_BE | MAGIC_64_BE => {
            return Ok(vec![Slice {
                offset: 0,
                size: data.len(),
            }]);
        }
        FAT_MAGIC | FAT_CIGAM => false,
        FAT_MAGIC_64 | FAT_CIGAM_64 => true,
        _ => return Err(crate::error::format_err("not Mach-O")),
    };
    let nfat = be32(data, 4)? as usize;
    if nfat == 0 || nfat > 32 {
        return Err(crate::error::format_err(format!(
            "implausible nfat_arch={nfat}"
        )));
    }
    let arch_size = if is64 { 32 } else { 20 };
    let mut out = Vec::with_capacity(nfat);
    for i in 0..nfat {
        let off = 8 + i * arch_size;
        let (offset, size) = if is64 {
            (be64(data, off + 8)?, be64(data, off + 16)?)
        } else {
            (
                u64::from(be32(data, off + 8)?),
                u64::from(be32(data, off + 12)?),
            )
        };
        let (Ok(offset), Ok(size)) = (usize::try_from(offset), usize::try_from(size)) else {
            return Err(crate::error::format_err("fat slice out of range"));
        };
        if offset.saturating_add(size) > data.len() {
            return Err(crate::error::format_err("fat slice out of range"));
        }
        out.push(Slice { offset, size });
    }
    Ok(out)
}

/// `macho.checkFat`: whether any slice has `cryptid` != 0.
fn check_fat(data: &[u8]) -> Result<bool> {
    for slice in slices(data)? {
        if slice.size < 4 {
            continue;
        }
        let bytes = &data[slice.offset..slice.offset + slice.size];
        let magic = le32(bytes, 0)?;
        if check_thin(bytes, magic == MAGIC_BE || magic == MAGIC_64_BE)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One encryption command: `(cryptoff, cryptsize, cryptid)`.
fn encryption_commands(data: &[u8], swap: bool) -> Result<Vec<(usize, usize, u32)>> {
    let read = |at: usize| if swap { be32(data, at) } else { le32(data, at) };
    if data.len() < 28 {
        return Err(crate::error::format_err("mach_header truncated"));
    }
    let magic = read(0)?;
    let is64 = magic == MAGIC_64_LE || magic == MAGIC_64_BE;
    let ncmds = read(16)? as usize;
    let sizeofcmds = read(20)? as usize;
    let header_size = if is64 { 32 } else { 28 };
    if header_size + sizeofcmds > data.len() {
        return Err(crate::error::format_err("load commands truncated"));
    }
    if ncmds > 1 << 16 {
        return Err(crate::error::format_err(format!(
            "implausible ncmds={ncmds}"
        )));
    }
    let want = if is64 {
        LC_ENCRYPTION_INFO_64
    } else {
        LC_ENCRYPTION_INFO
    };
    let mut found = Vec::new();
    let mut pos = header_size;
    for _ in 0..ncmds {
        if pos + 8 > data.len() {
            return Err(crate::error::format_err("load cmd truncated"));
        }
        let cmd = read(pos)?;
        let size = read(pos + 4)? as usize;
        if size < 8 || size > data.len() - pos {
            return Err(crate::error::format_err(format!("bad cmdsize={size}")));
        }
        if cmd == want {
            if size < 20 {
                return Err(crate::error::format_err("LC_ENCRYPTION_INFO truncated"));
            }
            found.push((
                read(pos + 8)? as usize,
                read(pos + 12)? as usize,
                read(pos + 16)?,
            ));
        }
        pos += size;
    }
    Ok(found)
}

/// `macho.checkThin`: whether any encryption command has `cryptid` != 0.
fn check_thin(data: &[u8], swap: bool) -> Result<bool> {
    Ok(encryption_commands(data, swap)?
        .iter()
        .any(|&(_, _, cryptid)| cryptid != 0))
}

/// `verify.go`'s `cryptZeroed`: whether any slice's encrypted region is all zeros.
fn crypt_zeroed(data: &[u8]) -> bool {
    let Ok(slices) = slices(data) else {
        return false;
    };
    slices.iter().any(|slice| {
        let bytes = &data[slice.offset..slice.offset + slice.size];
        // `macho.ReadEncryptionInfo` reads the first little-endian encryption command.
        if !matches!(le32(bytes, 0), Ok(MAGIC_LE | MAGIC_64_LE)) {
            return false;
        }
        let Ok(commands) = encryption_commands(bytes, false) else {
            return false;
        };
        let Some(&(cryptoff, cryptsize, _)) = commands.first() else {
            return false;
        };
        cryptsize != 0
            && slice.offset + cryptoff + cryptsize <= data.len()
            && bytes
                .get(cryptoff..cryptoff + cryptsize)
                .is_some_and(|region| region.iter().all(|&byte| byte == 0))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thin Mach-O with one encryption command over page 1.
    fn thin(magic: u32, cryptid: u32, fill: u8, big_endian: bool) -> Vec<u8> {
        let is64 = magic == MAGIC_64_LE;
        let (header, cmd, cmd_size) = if is64 {
            (32, LC_ENCRYPTION_INFO_64, 24)
        } else {
            (28, LC_ENCRYPTION_INFO, 20)
        };
        let word = |value: u32| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        let mut out = Vec::new();
        for value in [magic, 0x0100_000C, 0, 2, 1, cmd_size] {
            out.extend_from_slice(&word(value));
        }
        out.resize(header, 0);
        for value in [cmd, cmd_size, 0x1000, 0x1000, cryptid] {
            out.extend_from_slice(&word(value));
        }
        out.resize(0x1000, 0);
        out.resize(0x2000, fill);
        out
    }

    fn fat(slices: &[Vec<u8>]) -> Vec<u8> {
        let mut out = FAT_MAGIC.to_be_bytes().to_vec();
        let count = u32::try_from(slices.len()).expect("few slices");
        out.extend_from_slice(&count.to_be_bytes());
        let mut offset = 0x1000u32;
        for slice in slices {
            let size = u32::try_from(slice.len()).expect("small slice");
            for value in [0x0100_000C, 0, offset, size, 12] {
                out.extend_from_slice(&value.to_be_bytes());
            }
            offset += size;
        }
        out.resize(0x1000, 0);
        for slice in slices {
            out.extend_from_slice(slice);
        }
        out
    }

    fn run(members: Vec<(&str, Vec<u8>)>, skip_appex: bool) -> Verification {
        verify_members(
            members
                .into_iter()
                .map(|(name, data)| Ok((name.to_owned(), data))),
            skip_appex,
        )
        .expect("verifies")
    }

    #[test]
    fn flags_encrypted_and_zero_filled_binaries() {
        let result = run(
            vec![
                ("Payload/A.app/A", thin(MAGIC_64_LE, 0, 0xAA, false)),
                ("Payload/A.app/B", thin(MAGIC_64_LE, 1, 0xAA, false)),
                ("Payload/A.app/C", thin(MAGIC_LE, 0, 0, false)),
                (
                    "Payload/A.app/D",
                    fat(&[
                        thin(MAGIC_64_LE, 0, 1, false),
                        thin(MAGIC_64_LE, 1, 1, false),
                    ]),
                ),
                ("Payload/A.app/E", thin(MAGIC_64_LE, 1, 0xAA, true)),
                ("Payload/A.app/F", b"\xcf\xfa\xed\xfe".to_vec()),
                ("Payload/A.app/Info.plist", b"<plist/>".to_vec()),
                ("Other/X", thin(MAGIC_64_LE, 1, 0xAA, false)),
            ],
            false,
        );
        assert_eq!(
            result,
            Verification {
                scanned: 6,
                still_encrypted: vec![
                    "Payload/A.app/B".into(),
                    "Payload/A.app/D".into(),
                    "Payload/A.app/E".into(),
                ],
                all_zero_crypt: vec!["Payload/A.app/C".into()],
                skipped: vec!["Payload/A.app/F".into()],
            }
        );
        assert!(!result.ok());
    }

    #[test]
    fn skip_appex_ignores_extensions() {
        let members = || {
            vec![
                ("Payload/A.app/A", thin(MAGIC_64_LE, 0, 0xAA, false)),
                (
                    "Payload/A.app/PlugIns/E.appex/E",
                    thin(MAGIC_64_LE, 1, 0xAA, false),
                ),
            ]
        };
        assert!(run(members(), true).ok());
        assert_eq!(run(members(), true).scanned, 1);
        assert!(!run(members(), false).ok());
    }

    #[test]
    fn zero_filled_fat_slice() {
        let data = fat(&[
            thin(MAGIC_64_LE, 0, 0xAA, false),
            thin(MAGIC_64_LE, 0, 0, false),
        ]);
        assert!(crypt_zeroed(&data));
        assert!(!crypt_zeroed(&thin(MAGIC_64_LE, 0, 0xAA, false)));
    }
}
