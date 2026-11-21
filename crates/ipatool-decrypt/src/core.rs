//! `FairPlay` page-key derivation and page decryption of one Mach-O.
//!
//! A port of ipadecrypt's `internal/fairplay/fairplay.go` (MIT, itself a Go port of
//! `core.py` from EthanArbuckle/fairplay-decrypt, GPL-3). Its slice choice, formats and
//! error texts are reproduced.

use aes::Aes128;
use aes::cipher::block_padding::NoPadding;
use aes::cipher::{BlockCipherEncrypt, BlockModeDecrypt, KeyInit, KeyIvInit};
use cbc::Decryptor;
use md5::{Digest, Md5};

use crate::error::{Result, format_err, go_quote};

pub(crate) const CPU_TYPE_ARM: u32 = 0x0000_000C;
pub(crate) const CPU_TYPE_ARM64: u32 = 0x0100_000C;
pub(crate) const CPU_TYPE_ARM64_32: u32 = 0x0200_000C;
const CPU_SUBTYPE_ARMV7: u32 = 9;
const CPU_SUBTYPE_ARM64E: u32 = 2;
/// Masks the capability bits out of a cpusubtype (`CPU_SUBTYPE_MASK` is their complement).
const CPU_SUBTYPE_MASK: u32 = 0x00FF_FFFF;
const PAGE_SIZE: usize = 4096;

const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2C;
const MACHO_MAGIC_LE: u32 = 0xFEED_FACE;
const MACHO_MAGIC_64_LE: u32 = 0xFEED_FACF;

/// The 16-byte length of a `FairPlay` account credential.
pub const CREDENTIAL_SIZE: usize = 16;

/// The SUPF format tags (bytes 1..4) ipadecrypt accepts; all share one layout. `405` is a
/// fork addition: ipadecrypt 7c82f86 rejects it, but Google Home 1.25.1215 (2017) ships it
/// and decrypts with it to the same bytes as with its `.supp` (see `PLAN.md`).
const SUPF_FORMATS: [&[u8; 3]; 8] = [
    b"319", b"322", b"405", b"502", b"507", b"510", b"611", b"612",
];

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

/// The slice a Mach-O is thinned to and its encrypted region (`fairplay.go`'s
/// `machoInfo`). Offsets are file offsets except `cryptoff`, which is slice-relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachoInfo {
    /// The slice's file offset (0 for a thin Mach-O).
    pub base: usize,
    /// The slice's size.
    pub size: usize,
    /// The slice's cputype.
    pub cpu: u32,
    /// The slice's cpusubtype, capability bits included.
    pub subtype: u32,
    /// The encrypted region's offset within the slice.
    pub cryptoff: usize,
    /// The encrypted region's size, a multiple of 4096.
    pub cryptsize: usize,
    /// The file offset of the `cryptid` word.
    pub cryptid_offset: usize,
}

pub(crate) fn be32(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at.saturating_add(4))
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

pub(crate) fn be64(data: &[u8], at: usize) -> Result<u64> {
    let bytes = data
        .get(at..at.saturating_add(8))
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u64::from_be_bytes(bytes.try_into().expect("eight bytes")))
}

pub(crate) fn le32(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at.saturating_add(4))
        .ok_or_else(|| format_err("unexpected end of data"))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

fn to_usize(value: u64) -> usize {
    // A value past usize::MAX cannot fit in memory either; the bounds checks reject it.
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[derive(Clone, Copy)]
struct ArmSlice {
    base: usize,
    size: usize,
    cpu: u32,
    subtype: u32,
}

/// The ARM slices of a thin or fat Mach-O, grouped in thinning order: plain arm64,
/// arm64e, `arm64_32`, 32-bit ARM.
fn arm_slices(data: &[u8]) -> Result<[Vec<ArmSlice>; 4]> {
    let mut groups: [Vec<ArmSlice>; 4] = Default::default();
    let mut add = |cpu: u32, subtype: u32, base: usize, size: usize| {
        let slice = ArmSlice {
            base,
            size,
            cpu,
            subtype,
        };
        let group = match cpu {
            CPU_TYPE_ARM64 if subtype & CPU_SUBTYPE_MASK == CPU_SUBTYPE_ARM64E => 1,
            CPU_TYPE_ARM64 => 0,
            CPU_TYPE_ARM64_32 => 2,
            CPU_TYPE_ARM => 3,
            _ => return,
        };
        groups[group].push(slice);
    };
    match data.get(..4).unwrap_or_default() {
        b"\xca\xfe\xba\xbe" => {
            let count = be32(data, 4)? as usize;
            if !(1..=16).contains(&count) || 8 + count * 20 > data.len() {
                return Err(format_err("invalid FAT32 architecture table"));
            }
            for i in 0..count {
                let off = 8 + i * 20;
                add(
                    be32(data, off)?,
                    be32(data, off + 4)?,
                    be32(data, off + 8)? as usize,
                    be32(data, off + 12)? as usize,
                );
            }
        }
        b"\xca\xfe\xba\xbf" => {
            let count = be32(data, 4)? as usize;
            if !(1..=16).contains(&count) || 8 + count * 32 > data.len() {
                return Err(format_err("invalid FAT64 architecture table"));
            }
            for i in 0..count {
                let off = 8 + i * 32;
                add(
                    be32(data, off)?,
                    be32(data, off + 4)?,
                    to_usize(be64(data, off + 8)?),
                    to_usize(be64(data, off + 16)?),
                );
            }
        }
        [0xce | 0xcf, 0xfa, 0xed, 0xfe] if data.len() >= 28 => {
            add(le32(data, 4)?, le32(data, 8)?, 0, data.len());
        }
        _ => return Err(format_err("not a supported ARM Mach-O")),
    }
    Ok(groups)
}

/// `fairplay.go`'s `armEncryptionInfo`: the encrypted region of the slice the output is
/// thinned to: the plain arm64 slice, else arm64e, else `arm64_32`, else 32-bit ARM (armv7
/// when there are several). `None` when that slice has no active encryption command.
///
/// # Errors
/// If the data is not an ARM Mach-O, the slice choice is ambiguous, or the chosen slice
/// is malformed.
pub fn arm_encryption_info(data: &[u8]) -> Result<Option<MachoInfo>> {
    let groups = arm_slices(data)?;
    let mut slices: Vec<ArmSlice> = groups
        .into_iter()
        .find(|group| !group.is_empty())
        .unwrap_or_default();
    // Fat 32-bit apps pair armv7 with armv7s or armv6; armv7 runs on every device that
    // runs either.
    if slices.len() > 1 && slices[0].cpu == CPU_TYPE_ARM {
        slices.retain(|slice| slice.subtype & CPU_SUBTYPE_MASK == CPU_SUBTYPE_ARMV7);
    }
    let [chosen] = slices[..] else {
        return Err(format_err(format!(
            "expected one ARM slice to decrypt, found {}",
            slices.len()
        )));
    };
    slice_encryption_info(data, chosen)
}

fn slice_encryption_info(data: &[u8], chosen: ArmSlice) -> Result<Option<MachoInfo>> {
    let ArmSlice {
        base,
        size: slice_size,
        ..
    } = chosen;
    // arm64_32 uses the 32-bit header and load command, like 32-bit ARM.
    let (header_size, enc_cmd, want_magic) = if chosen.cpu == CPU_TYPE_ARM64 {
        (32, LC_ENCRYPTION_INFO_64, MACHO_MAGIC_64_LE)
    } else {
        (28, LC_ENCRYPTION_INFO, MACHO_MAGIC_LE)
    };
    if base.saturating_add(slice_size) > data.len() || base.saturating_add(header_size) > data.len()
    {
        return Err(format_err("Mach-O slice exceeds file"));
    }
    let magic = le32(data, base)?;
    let cpu = le32(data, base + 4)?;
    let ncmds = le32(data, base + 16)? as usize;
    let sizeofcmds = le32(data, base + 20)? as usize;
    if magic != want_magic || cpu != chosen.cpu || ncmds > 1000 {
        return Err(format_err("invalid Mach-O slice header"));
    }
    let mut pos = base + header_size;
    let commands_end = pos + sizeofcmds;
    if commands_end > base + slice_size {
        return Err(format_err("load commands exceed Mach-O slice"));
    }
    let (mut cryptoff, mut cryptsize, mut cryptid, mut cryptid_offset) = (0, 0, 0, 0);
    let mut found = 0;
    for _ in 0..ncmds {
        if pos + 8 > commands_end {
            return Err(format_err("truncated load command"));
        }
        let cmd = le32(data, pos)?;
        let size = le32(data, pos + 4)? as usize;
        if size < 8 || pos + size > commands_end {
            return Err(format_err("invalid load command"));
        }
        if cmd == enc_cmd {
            if size < 20 {
                return Err(format_err("short encryption info command"));
            }
            cryptoff = le32(data, pos + 8)? as usize;
            cryptsize = le32(data, pos + 12)? as usize;
            cryptid = le32(data, pos + 16)?;
            cryptid_offset = pos + 16;
            found += 1;
        }
        pos += size;
    }
    if pos != commands_end {
        return Err(format_err("load command sizes do not match Mach-O header"));
    }
    if found == 0 || (found == 1 && cryptid == 0) {
        return Ok(None);
    }
    if found != 1 {
        return Err(format_err(format!(
            "expected one encryption command, found {found}"
        )));
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
        size: slice_size,
        cpu: chosen.cpu,
        subtype: chosen.subtype,
        cryptoff,
        cryptsize,
        cryptid_offset,
    }))
}

/// One atom in a SINF tree: its four-character type and the content range.
struct Atom {
    kind: [u8; 4],
    start: usize,
    end: usize,
}

/// `fairplay.go`'s `atomChildren`: the atoms between `start` and `end`.
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

/// `fairplay.go`'s `sinfField`: the one `wanted` field in the SINF's `schi` container.
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
            go_quote(std::str::from_utf8(&wanted).unwrap_or_default())
        )));
    }
    Ok(&sinf[fields[0].start..fields[0].end])
}

/// `fairplay.go`'s `deriveWork16`: the 16-byte page-key work value from the credential
/// and the SINF `priv` record.
///
/// # Panics
/// Never; the `expect` is on a fixed-size conversion that always succeeds.
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
    let raw_priv = match raw_priv.len() {
        448 => raw_priv,
        440 => {
            if raw_priv[432..] != [0u8; 8] {
                return Err(format_err("invalid SINF priv-record trailer"));
            }
            &raw_priv[..432]
        }
        _ => return Err(format_err("unsupported SINF priv-record length")),
    };
    if credential.len() != CREDENTIAL_SIZE || iviv.len() != 16 {
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
    Decryptor::<Aes128>::new(&Array16::from(key), &Array16::from(iv))
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

/// `fairplay.go`'s `isArcadeSinf`: whether `sinf` is an Apple Arcade license, which only
/// the account's Arcade credential decodes (its `key ` field starts with 4).
#[must_use]
pub fn is_arcade_sinf(sinf: &[u8]) -> bool {
    sinf_field(sinf, *b"key ").is_ok_and(|key| key.len() == 4 && key[0] == 4)
}

/// `fairplay.go`'s `CredentialDecodes`: whether `credential` unwraps the SINF's `priv`
/// record, i.e. belongs to that license's account and kind.
#[must_use]
pub fn credential_decodes(credential: &[u8], sinf: &[u8]) -> bool {
    derive_work16(credential, sinf).is_ok()
}

/// `fairplay.go`'s `pclassPageKeyOffset`: the offset of the 32-byte record for
/// `absolute_page` in a SUPP (`.supp` v01 / `.v4.supp` v04). One table indexes every 4 KiB
/// page of the file from offset 0; the v01 header carries a 20-byte code-signature SHA-1
/// that v04 omits.
///
/// # Errors
/// If the SUPP format is unsupported, its table is malformed, or the page is missing.
pub fn pclass_page_key_offset(supp: &[u8], absolute_page: usize) -> Result<usize> {
    if supp.len() < 8 || !matches!(supp[0], 1 | 4) {
        return Err(format_err("unsupported SUPP format"));
    }
    let pos = if supp[0] == 1 { 24 } else { 4 };
    if pos + 4 > supp.len() {
        return Err(format_err("truncated SUPP header"));
    }
    let count = be32(supp, pos)? as usize;
    if count == 0 || count > 1 << 20 || pos + 4 + count * 32 > supp.len() {
        return Err(format_err("invalid SUPP page count"));
    }
    if absolute_page >= count {
        return Err(format_err("SUPP page key missing"));
    }
    Ok(pos + 4 + 32 * absolute_page)
}

/// `fairplay.go`'s `armSupfRecordOffset`: the per-page record block of the slice with
/// `wanted_cpu` and `wanted_subtype` (capability bits ignored) in a SUPF.
///
/// # Errors
/// If the SUPF format is unsupported or the slice's records are missing or incomplete.
pub fn arm_supf_record_offset(
    supf: &[u8],
    required_pages: usize,
    wanted_cpu: u32,
    wanted_subtype: u32,
) -> Result<usize> {
    if supf.len() < 12
        || !matches!(supf[0], 3 | 5)
        || !SUPF_FORMATS.iter().any(|format| supf[1..4] == format[..])
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
        if cpu == wanted_cpu && subtype & CPU_SUBTYPE_MASK == wanted_subtype & CPU_SUBTYPE_MASK {
            if pages < required_pages || length < header_size + pages * 32 {
                return Err(format_err("SUPF page records are incomplete"));
            }
            return Ok(pos + 4 + header_size);
        }
        pos += length + 4;
    }
    Err(format_err(format!(
        "SUPF record missing for cputype={wanted_cpu:#x} cpusubtype={wanted_subtype:#x}"
    )))
}

/// `fairplay.go`'s `sourceBlocks`.
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

/// `fairplay.go`'s `finish`: the nonlinear mixing over eight 16-bit words.
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

/// `fairplay.go`'s `blockTransform`: AES-128-ECB encrypt of `source` under the work key.
fn block_transform(cipher: &Aes128, source: [u8; 16]) -> [u8; 16] {
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

/// `fairplay.go`'s `derivePageKey`.
fn derive_page_key(cipher: &Aes128, wrapped: [u8; 16], absolute_index: u32) -> [u8; 16] {
    let (first, second, third) = source_blocks(wrapped, absolute_index);
    let a = unpack_be16(block_transform(cipher, first));
    let b = unpack_be16(block_transform(cipher, second));
    let c = unpack_be16(block_transform(cipher, third));
    pack_be16(finish(xor8(finish(xor8(finish(a), b)), c)))
}

/// `fairplay.go`'s `decryptBinary`: decrypts every encrypted page of the slice
/// [`arm_encryption_info`] picks and clears `cryptid`. `key_file` is a SUPF
/// (`.supf`/`.v5.supf`) or a SUPP (`.supp`/`.v4.supp`). A fat binary is thinned to that
/// slice; the others are dropped, as on the device.
///
/// Returns the decrypted thin Mach-O and its page count, or `None` when the slice is not
/// encrypted.
///
/// # Panics
/// Never; the `expect`s are on fixed-size conversions that always succeed.
///
/// # Errors
/// If the Mach-O, SINF or key-file records are malformed or the credential does not
/// decode the SINF.
pub fn decrypt_binary(
    source: &[u8],
    sinf: &[u8],
    key_file: &[u8],
    credential: &[u8],
) -> Result<Option<(Vec<u8>, usize)>> {
    let Some(info) = arm_encryption_info(source)? else {
        return Ok(None);
    };
    let page_count = info.cryptsize / PAGE_SIZE;
    let first_page = (info.base + info.cryptoff) / PAGE_SIZE;
    let pclass = matches!(key_file.first(), Some(1 | 4));
    let mut record_start = 0;
    if !pclass {
        record_start = arm_supf_record_offset(key_file, page_count, info.cpu, info.subtype)?;
    }
    let work = derive_work16(credential, sinf)?;
    let key_cipher = Aes128::new(&Array16::from(work));

    let mut result = source[info.base..info.base + info.size].to_vec();
    for local_page in 0..page_count {
        let absolute_index = first_page + local_page;
        if pclass {
            record_start = pclass_page_key_offset(key_file, absolute_index)?;
        }
        let record = key_file
            .get(record_start..record_start + 32)
            .ok_or_else(|| format_err("truncated SUPF/SUPP page record"))?;
        record_start += 32;
        let index =
            u32::try_from(absolute_index).map_err(|_| format_err("page index out of range"))?;
        let wrapped: [u8; 16] = record[..16].try_into().expect("sixteen bytes");
        let iv: [u8; 16] = record[16..].try_into().expect("sixteen bytes");
        let key = derive_page_key(&key_cipher, wrapped, index);
        let start = info.cryptoff + PAGE_SIZE * local_page;
        Decryptor::<Aes128>::new(&Array16::from(key), &Array16::from(iv))
            .decrypt_padded::<NoPadding>(&mut result[start..start + PAGE_SIZE])
            .map_err(|_| format_err("failed to decrypt page"))?;
    }
    let cryptid = info.cryptid_offset - info.base;
    result[cryptid..cryptid + 4].copy_from_slice(&0u32.to_le_bytes());
    Ok(Some((result, page_count)))
}

#[cfg(test)]
mod tests;
