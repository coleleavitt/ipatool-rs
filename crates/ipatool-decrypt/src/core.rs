//! `FairPlay` page-key derivation and page decryption.
//!
//! A port of `core.py` from EthanArbuckle/fairplay-decrypt (GPL-3). Its behavior and
//! error texts are reproduced.

use aes::Aes128;
use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockCipherEncrypt, BlockModeDecrypt, KeyInit, KeyIvInit};
use cbc::Decryptor;
use md5::{Digest, Md5};
use std::fmt::Write as _;

use crate::error::{Result, format_err};

const ARM64: u32 = 0x0100_000C;
const PAGE_SIZE: usize = 4096;

type Array16 = cipher::Array<u8, cipher::consts::U16>;

const ROUND_SELECTORS: [[(usize, usize); 4]; 4] = [
    [(0, 1), (2, 3), (4, 5), (6, 7)],
    [(2, 1), (0, 3), (4, 7), (6, 5)],
    [(2, 0), (4, 1), (6, 3), (7, 5)],
    [(1, 2), (4, 0), (7, 3), (5, 6)],
];
const ROUND_CONSTANTS: [u32; 4] = [0x6854_7C2E, 0x7E05_E1DE, 0xD495_038B, 0x0AB2_4ADD];
const FINAL_SELECTORS: [[usize; 4]; 4] = [[0, 1, 2, 3], [0, 2, 4, 6], [0, 3, 5, 7], [4, 5, 6, 7]];
const FINAL_CONSTANTS: [u32; 4] = [0x5B71_98C7, 0xEB5D_E22C, 0x0F8A_A8FA, 0xF6E1_BB3F];

/// The encrypted region of one arm64 Mach-O slice (`core.py`'s `MachoInfo`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachoInfo {
    base: usize,
    cryptoff: usize,
    cryptsize: usize,
    cryptid_offset: usize,
    subtype: u32,
}

fn be32(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

fn le32(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

/// `core.py`'s `arm64_encryption_infos`.
///
/// # Errors
/// If the data is not a supported arm64 Mach-O or a slice is malformed.
pub fn arm64_encryption_infos(data: &[u8]) -> Result<Vec<MachoInfo>> {
    let magic = data.get(..4).unwrap_or_default();
    let arm: Vec<(usize, usize, u32)> = match magic {
        b"\xca\xfe\xba\xbe" => {
            let count = be32(data, 4)? as usize;
            if !(1..=16).contains(&count) || 8 + count * 20 > data.len() {
                return Err(format_err("invalid FAT32 architecture table"));
            }
            let mut arm = Vec::new();
            for i in 0..count {
                let off = 8 + i * 20;
                let cpu = be32(data, off)?;
                let sub = be32(data, off + 4)?;
                let slice_off = be32(data, off + 8)? as usize;
                let size = be32(data, off + 12)? as usize;
                if cpu == ARM64 {
                    arm.push((slice_off, size, sub));
                }
            }
            arm
        }
        b"\xca\xfe\xba\xbf" => {
            let count = be32(data, 4)? as usize;
            if !(1..=16).contains(&count) || 8 + count * 32 > data.len() {
                return Err(format_err("invalid FAT64 architecture table"));
            }
            let mut arm = Vec::new();
            for i in 0..count {
                let off = 8 + i * 32;
                let cpu = be32(data, off)?;
                let sub = be32(data, off + 4)?;
                let slice_off = usize::try_from(be64(data, off + 8)?)
                    .map_err(|_| format_err("slice offset too large"))?;
                let size = usize::try_from(be64(data, off + 16)?)
                    .map_err(|_| format_err("slice size too large"))?;
                if cpu == ARM64 {
                    arm.push((slice_off, size, sub));
                }
            }
            arm
        }
        b"\xcf\xfa\xed\xfe" => {
            let subtype = le32(data, 8)?;
            vec![(0, data.len(), subtype)]
        }
        _ => return Err(format_err("not a supported arm64 Mach-O")),
    };
    if arm.is_empty() {
        return Err(format_err("no arm64 slices found"));
    }
    let mut infos = Vec::new();
    for (base, size, subtype) in arm {
        if let Some(info) = arm64_slice_info(data, base, size, subtype)? {
            infos.push(info);
        }
    }
    Ok(infos)
}

fn be64(data: &[u8], at: usize) -> Result<u64> {
    let bytes = data
        .get(at..at + 8)
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u64::from_be_bytes(bytes.try_into().expect("eight bytes")))
}

fn arm64_slice_info(
    data: &[u8],
    base: usize,
    slice_size: usize,
    subtype: u32,
) -> Result<Option<MachoInfo>> {
    if base + slice_size > data.len() || base + 32 > data.len() {
        return Err(format_err("arm64 slice exceeds file"));
    }
    let magic = le32(data, base)?;
    let cpu = le32(data, base + 4)?;
    let ncmds = le32(data, base + 16)? as usize;
    let sizeofcmds = le32(data, base + 20)? as usize;
    if magic != 0xFEED_FACF || cpu != ARM64 || ncmds > 1000 {
        return Err(format_err("invalid arm64 Mach-O header"));
    }
    let mut pos = base + 32;
    let commands_end = pos + sizeofcmds;
    if commands_end > base + slice_size {
        return Err(format_err("load commands exceed arm64 slice"));
    }
    let mut found: Vec<(usize, usize, u32, usize)> = Vec::new();
    for _ in 0..ncmds {
        if pos + 8 > commands_end {
            return Err(format_err("truncated load command"));
        }
        let cmd = le32(data, pos)?;
        let size = le32(data, pos + 4)? as usize;
        if size < 8 || pos + size > commands_end {
            return Err(format_err("invalid load command"));
        }
        if cmd == 0x2C {
            if size < 24 {
                return Err(format_err("short encryption info command"));
            }
            let cryptoff = le32(data, pos + 8)? as usize;
            let cryptsize = le32(data, pos + 12)? as usize;
            let cryptid = le32(data, pos + 16)?;
            found.push((cryptoff, cryptsize, cryptid, pos + 16));
        }
        pos += size;
    }
    if pos != commands_end {
        return Err(format_err("load command sizes do not match Mach-O header"));
    }
    let Some(&(cryptoff, cryptsize, cryptid, cryptid_offset)) = found.first() else {
        return Ok(None);
    };
    if found.len() != 1 {
        return Err(format_err(format!(
            "expected one encryption command, found {}",
            found.len()
        )));
    }
    if cryptid == 0 {
        return Ok(None);
    }
    if cryptid != 1 || cryptsize == 0 || !cryptsize.is_multiple_of(PAGE_SIZE) {
        return Err(format_err(format!(
            "unsupported cryptid/size: {cryptid}, {cryptsize}"
        )));
    }
    if cryptoff + cryptsize > slice_size || cryptid_offset >= base + cryptoff {
        return Err(format_err("invalid encrypted region bounds"));
    }
    Ok(Some(MachoInfo {
        base,
        cryptoff,
        cryptsize,
        cryptid_offset,
        subtype,
    }))
}

/// One atom in a SINF tree: its four-character type and the content range.
struct Atom {
    kind: [u8; 4],
    start: usize,
    end: usize,
}

/// `core.py`'s `_children`: the atoms between `start` and `end`.
fn children(data: &[u8], start: usize, end: usize) -> Result<Vec<Atom>> {
    let mut atoms = Vec::new();
    let mut offset = start;
    while offset < end {
        if offset + 8 > end {
            return Err(format_err("truncated SINF atom"));
        }
        let size = be32(data, offset)? as usize;
        if size < 8 || offset + size > end {
            return Err(format_err(format!("invalid SINF atom size at {offset:#x}")));
        }
        let kind = data[offset + 4..offset + 8].try_into().expect("four bytes");
        atoms.push(Atom {
            kind,
            start: offset + 8,
            end: offset + size,
        });
        offset += size;
    }
    Ok(atoms)
}

/// `core.py`'s `_sinf_field`.
fn sinf_field(sinf: &[u8], wanted: [u8; 4]) -> Result<&[u8]> {
    let roots = children(sinf, 0, sinf.len())?;
    if roots.len() != 1 || &roots[0].kind != b"sinf" {
        return Err(format_err("expected outer sinf atom"));
    }
    let schi: Vec<Atom> = children(sinf, roots[0].start, roots[0].end)?
        .into_iter()
        .filter(|atom| &atom.kind == b"schi")
        .collect();
    if schi.len() != 1 {
        return Err(format_err("expected one schi container"));
    }
    let fields: Vec<Atom> = children(sinf, schi[0].start, schi[0].end)?
        .into_iter()
        .filter(|atom| atom.kind == wanted)
        .collect();
    if fields.len() != 1 {
        return Err(format_err(format!(
            "expected one {} field",
            escape_fourcc(wanted)
        )));
    }
    Ok(&sinf[fields[0].start..fields[0].end])
}

/// Python's `repr(bytes)` for a four-byte field name, e.g. `b'priv'`.
fn escape_fourcc(value: [u8; 4]) -> String {
    let mut out = String::from("b'");
    for byte in value {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            0x20..=0x7E => out.push(byte as char),
            _ => write!(out, "\\x{byte:02x}").expect("writing to a String"),
        }
    }
    out.push('\'');
    out
}

/// `core.py`'s `derive_work16`: the 16-byte page-key work value from the credential and
/// the SINF `priv` record.
///
/// # Panics
/// Never; the `expect`s are on fixed-size conversions that always succeed.
///
/// # Errors
/// If the SINF is malformed or the credential does not decode its `priv` record.
pub fn derive_work16(credential: &[u8], sinf: &[u8]) -> Result<[u8; 16]> {
    let name_field = sinf_field(sinf, *b"name")?;
    let name = name_field
        .split(|&byte| byte == 0)
        .next()
        .unwrap_or_default();
    let iviv = sinf_field(sinf, *b"iviv")?;
    let raw_priv = sinf_field(sinf, *b"priv")?;
    if raw_priv.len() != 440 && raw_priv.len() != 448 {
        return Err(format_err("unsupported SINF priv-record length"));
    }
    let raw_priv = if raw_priv.len() == 440 {
        if raw_priv[432..] != [0u8; 8] {
            return Err(format_err("invalid SINF priv-record trailer"));
        }
        &raw_priv[..432]
    } else {
        raw_priv
    };
    if credential.len() != 16 || iviv.len() != 16 {
        return Err(format_err("invalid SINF priv-record dimensions"));
    }
    let mut hasher = Md5::new();
    hasher.update(name);
    hasher.update(iviv);
    let iv: [u8; 16] = hasher.finalize().into();
    let mut key = [0u8; 16];
    for (dst, &byte) in key.iter_mut().zip(credential) {
        *dst = byte ^ b'0';
    }
    let mut clear = raw_priv.to_vec();
    let key_arr = Array16::from(key);
    let iv_arr = Array16::from(iv);
    let decryptor = Decryptor::<Aes128>::new(&key_arr, &iv_arr);
    decryptor
        .decrypt_padded::<NoPadding>(&mut clear)
        .map_err(|_| format_err("credential does not decode this SINF priv record"))?;
    if clear.get(..4) != Some(b"itun")
        || clear.get(20..24) != Some(b"key ")
        || clear.get(44..48) != Some(b"iviv")
    {
        return Err(format_err(
            "credential does not decode this SINF priv record",
        ));
    }
    Ok(clear[24..40].try_into().expect("sixteen bytes"))
}

/// `core.py`'s `arm64_supf_record_offset`.
///
/// # Errors
/// If the SUPF format is unsupported or the arm64 record is missing or incomplete.
pub fn arm64_supf_record_offset(
    supf: &[u8],
    required_pages: usize,
    wanted_subtype: u32,
) -> Result<usize> {
    if supf.len() < 12
        || !matches!(supf[0], 3 | 5)
        || !matches!(&supf[1..4], b"507" | b"510" | b"612")
    {
        return Err(format_err("unsupported SUPF format"));
    }
    let header_size = if supf[0] == 3 { 32 } else { 12 };
    let count = be32(supf, 8)? as usize;
    if !(1..=16).contains(&count) {
        return Err(format_err("invalid SUPF architecture count"));
    }
    let mut pos = 12;
    for _ in 0..count {
        if pos + 4 + header_size > supf.len() {
            return Err(format_err("truncated SUPF architecture header"));
        }
        let length = be32(supf, pos)? as usize;
        let cpu = be32(supf, pos + 4)?;
        let subtype = be32(supf, pos + 8)?;
        if length < header_size || pos + 4 + length > supf.len() {
            return Err(format_err("invalid SUPF architecture length"));
        }
        let pages = be32(supf, pos + header_size)? as usize;
        if cpu == ARM64 && subtype == wanted_subtype {
            if pages < required_pages || length < header_size + pages * 32 {
                return Err(format_err("SUPF arm64 page records are incomplete"));
            }
            return Ok(pos + 4 + header_size);
        }
        pos += length + 4;
    }
    Err(format_err("arm64 SUPF record missing"))
}

/// `core.py`'s `source_blocks`.
fn source_blocks(wrapped: [u8; 16], absolute_index: u32) -> ([u8; 16], [u8; 16], [u8; 16]) {
    let page_offset = (u64::from(absolute_index) << 12).to_be_bytes();
    let mut first = [0u8; 16];
    first[..8].copy_from_slice(&wrapped[8..16]);
    first[8..].copy_from_slice(&page_offset);
    let second = wrapped;
    let mut third = [0u8; 16];
    third[..8].copy_from_slice(&page_offset);
    third[8..].copy_from_slice(&wrapped[..8]);
    (first, second, third)
}

/// `core.py`'s `finish`: the nonlinear mixing over eight 16-bit words.
fn finish(state: [u32; 8]) -> [u32; 8] {
    let mut src = state;
    for selectors in &ROUND_SELECTORS {
        let mut dst = [0u32; 8];
        for (i, (&(a, b), &constant)) in selectors.iter().zip(&ROUND_CONSTANTS).enumerate() {
            let mixed = src[a] + src[b]; // carry bit kept
            let square = mixed.wrapping_mul(mixed);
            let x = square ^ (mixed >> 16) ^ constant;
            dst[i * 2] = x & 0xFFFF;
            dst[i * 2 + 1] = x >> 16;
        }
        src = dst;
    }
    let mut dst = [0u32; 8];
    for (i, (&quartet, &constant)) in FINAL_SELECTORS.iter().zip(&FINAL_CONSTANTS).enumerate() {
        let a = (src[quartet[1]] << 16) | src[quartet[0]];
        let b = (src[quartet[3]] << 16) | src[quartet[2]];
        let mixed = a.wrapping_add(b); // carry bit dropped
        let square = u64::from(mixed) * u64::from(mixed);
        let lo = u32::try_from(square & 0xFFFF_FFFF).expect("masked to 32 bits");
        let hi = u32::try_from(square >> 32).expect("high 32 bits of a 64-bit value");
        let x = lo ^ hi ^ constant;
        dst[i * 2] = x & 0xFFFF;
        dst[i * 2 + 1] = x >> 16;
    }
    let mut out = [0u32; 8];
    for i in 0..8 {
        out[i] = state[i] ^ dst[i];
    }
    out
}

/// `core.py`'s `block_transform`: AES-128-ECB encrypt of `source` under `work`.
fn block_transform(source: [u8; 16], work: [u8; 16]) -> [u8; 16] {
    let key = Array16::from(work);
    let cipher = Aes128::new(&key);
    let mut block = Array16::from(source);
    cipher.encrypt_block(&mut block);
    block.into()
}

fn unpack_be16(block: [u8; 16]) -> [u32; 8] {
    let mut out = [0u32; 8];
    for (i, word) in out.iter_mut().enumerate() {
        *word = u32::from(u16::from_be_bytes([block[i * 2], block[i * 2 + 1]]));
    }
    out
}

fn pack_be16(words: [u32; 8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, &word) in words.iter().enumerate() {
        let word = u16::try_from(word).expect("a 16-bit word");
        out[i * 2..i * 2 + 2].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn xor8(a: [u32; 8], b: [u32; 8]) -> [u32; 8] {
    let mut out = [0u32; 8];
    for i in 0..8 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// `core.py`'s `derive_page_key`.
fn derive_page_key(wrapped: [u8; 16], absolute_index: u32, work: [u8; 16]) -> [u8; 16] {
    let (first, second, third) = source_blocks(wrapped, absolute_index);
    let a = unpack_be16(block_transform(first, work));
    let b = unpack_be16(block_transform(second, work));
    let c = unpack_be16(block_transform(third, work));
    pack_be16(finish(xor8(finish(xor8(finish(a), b)), c)))
}

/// `core.py`'s `decrypt_binary`: decrypts every encrypted arm64 page and clears `cryptid`.
///
/// Returns the rewritten Mach-O and the total number of decrypted pages.
///
/// # Panics
/// Never; the `expect`s are on fixed-size conversions that always succeed.
///
/// # Errors
/// If the Mach-O, SINF or SUPF records are malformed.
pub fn decrypt_binary(
    source: &[u8],
    sinf: &[u8],
    supf: &[u8],
    credential: &[u8],
) -> Result<(Vec<u8>, usize)> {
    let infos = arm64_encryption_infos(source)?;
    let work = derive_work16(credential, sinf)?;
    let mut result = source.to_vec();
    let mut total_pages = 0;
    for info in infos {
        let page_count = info.cryptsize / PAGE_SIZE;
        let record_start = arm64_supf_record_offset(supf, page_count, info.subtype)?;
        total_pages += page_count;
        for local_page in 0..page_count {
            let record = supf
                .get(record_start + 32 * local_page..record_start + 32 * (local_page + 1))
                .filter(|slice| slice.len() == 32)
                .ok_or_else(|| format_err("truncated SUPF page record"))?;
            let absolute_index =
                u32::try_from((info.base + info.cryptoff) / PAGE_SIZE + local_page)
                    .map_err(|_| format_err("page index out of range"))?;
            let wrapped: [u8; 16] = record[..16].try_into().expect("sixteen bytes");
            let key = derive_page_key(wrapped, absolute_index, work);
            let start = info.base + info.cryptoff + PAGE_SIZE * local_page;
            let iv: &[u8; 16] = record[16..32].try_into().expect("sixteen bytes");
            let key_arr = Array16::from(key);
            let iv_arr = Array16::from(*iv);
            let decryptor = Decryptor::<Aes128>::new(&key_arr, &iv_arr);
            decryptor
                .decrypt_padded::<NoPadding>(&mut result[start..start + PAGE_SIZE])
                .map_err(|_| format_err("failed to decrypt page"))?;
        }
        result[info.cryptid_offset..info.cryptid_offset + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    Ok((result, total_pages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_arm64_macho() {
        let err = arm64_encryption_infos(b"\x7fELF\0\0\0\0").unwrap_err();
        assert_eq!(err.to_string(), "not a supported arm64 Mach-O");
    }

    #[test]
    fn rejects_empty_input() {
        let err = arm64_encryption_infos(&[]).unwrap_err();
        assert_eq!(err.to_string(), "not a supported arm64 Mach-O");
    }

    #[test]
    fn rejects_bad_supf_format() {
        let err = arm64_supf_record_offset(b"\x05999\0\0\0\0\0\0\0\x01", 1, 0).unwrap_err();
        assert_eq!(err.to_string(), "unsupported SUPF format");
    }

    #[test]
    fn rejects_short_supf() {
        let err = arm64_supf_record_offset(b"\x05510", 1, 0).unwrap_err();
        assert_eq!(err.to_string(), "unsupported SUPF format");
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

    #[test]
    fn escape_fourcc_matches_python_repr() {
        assert_eq!(escape_fourcc(*b"priv"), "b'priv'");
        assert_eq!(
            escape_fourcc([0x00, b'a', 0x7f, b'\\']),
            "b'\\x00a\\x7f\\\\'"
        );
    }
}
