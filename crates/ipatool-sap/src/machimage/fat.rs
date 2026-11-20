//! Universal (fat) binary slicing. Port of Go `machimage/range.go`, `reader.go` and `ARM64Slice`.

use std::io::Read;

use crate::error::{Context, Error, Result};

const MAGIC_FAT: u32 = 0xcafe_babe;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64E: u32 = 2;
const CPU_SUBTYPE_MASK: u32 = 0x00ff_ffff;
const HEADER_SIZE: u64 = 8;
const ARCH_SIZE: u64 = 20;

fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

fn is_arm64e(arch: &[u8]) -> bool {
    be32(arch) == CPU_TYPE_ARM64 && be32(&arch[4..]) & CPU_SUBTYPE_MASK == CPU_SUBTYPE_ARM64E
}

fn overlap_error() -> Error {
    Error::msg("arm64e slice exceeds input size or overlaps universal binary header")
}

/// Returns a thin image unchanged or the arm64e slice of a universal binary.
pub fn arm64_slice(input: &[u8]) -> Result<&[u8]> {
    if input.len() < 4 || be32(input) != MAGIC_FAT {
        return Ok(input);
    }
    if input.len() < 8 {
        return Err(Error::msg("truncated universal binary header"));
    }
    let len = input.len() as u64;
    let count = u64::from(be32(&input[4..]));
    if count == 0 || count > (len - HEADER_SIZE) / ARCH_SIZE {
        return Err(Error::msg("invalid universal binary architecture count"));
    }
    for index in 0..count {
        #[allow(clippy::cast_possible_truncation)]
        let header = &input[(HEADER_SIZE + index * ARCH_SIZE) as usize..];
        if !is_arm64e(header) {
            continue;
        }
        let offset = u64::from(be32(&header[8..]));
        let size = u64::from(be32(&header[12..]));
        let end = offset.checked_add(size);
        if size == 0 || offset < HEADER_SIZE + count * ARCH_SIZE || end.is_none_or(|end| end > len)
        {
            return Err(overlap_error());
        }
        #[allow(clippy::cast_possible_truncation)]
        return Ok(&input[offset as usize..end.unwrap_or_default() as usize]);
    }
    Err(Error::msg("universal binary has no arm64e slice"))
}

/// Locates a thin image or the arm64e slice reading only headers through `read_at`,
/// which fills the whole buffer from the given offset.
pub fn arm64_range(
    read_at: &mut dyn FnMut(&mut [u8], u64) -> Result<()>,
    size: u64,
) -> Result<(u64, u64)> {
    if size < 4 {
        return Ok((0, size));
    }
    let mut header = [0u8; 8];
    read_at(&mut header[..4], 0).context("read image magic")?;
    if be32(&header) != MAGIC_FAT {
        return Ok((0, size));
    }
    if size < 8 {
        return Err(Error::msg("truncated universal binary header"));
    }
    read_at(&mut header[4..], 4).context("read universal binary header")?;
    let count = u64::from(be32(&header[4..]));
    if count == 0 || count > (size - 8) / ARCH_SIZE {
        return Err(Error::msg("invalid universal binary architecture count"));
    }
    let mut arch = [0u8; 20];
    for index in 0..count {
        read_at(&mut arch, 8 + index * ARCH_SIZE).context("read universal binary architecture")?;
        if !is_arm64e(&arch) {
            continue;
        }
        let offset = u64::from(be32(&arch[8..]));
        let length = u64::from(be32(&arch[12..]));
        if length == 0 || offset < 8 + count * ARCH_SIZE || offset > size || length > size - offset
        {
            return Err(overlap_error());
        }
        return Ok((offset, length));
    }
    Err(Error::msg("universal binary has no arm64e slice"))
}

/// Consumes exactly `size` bytes and keeps only the arm64e image, so a caller can hash
/// the whole input with a tee reader.
pub fn read_arm64(input: &mut dyn Read, size: usize) -> Result<Vec<u8>> {
    let mut header = [0u8; 8];
    let prefix = size.min(4);
    input
        .read_exact(&mut header[..prefix])
        .context("read image magic")?;
    if prefix < 4 || be32(&header) != MAGIC_FAT {
        let mut data = vec![0u8; size];
        data[..prefix].copy_from_slice(&header[..prefix]);
        input
            .read_exact(&mut data[prefix..])
            .context("read thin image")?;
        return Ok(data);
    }
    if size < 8 {
        return Err(Error::msg("truncated universal binary header"));
    }
    input
        .read_exact(&mut header[4..])
        .context("read universal binary header")?;
    let size = size as u64;
    let count = u64::from(be32(&header[4..]));
    if count == 0 || count > (size - 8) / ARCH_SIZE {
        return Err(Error::msg("invalid universal binary architecture count"));
    }
    let (mut offset, mut length) = (0, 0);
    let mut arch = [0u8; 20];
    for _ in 0..count {
        input
            .read_exact(&mut arch)
            .context("read universal binary architecture")?;
        if length == 0 && is_arm64e(&arch) {
            offset = u64::from(be32(&arch[8..]));
            length = u64::from(be32(&arch[12..]));
            if length == 0 || offset < 8 + count * ARCH_SIZE || offset + length > size {
                return Err(overlap_error());
            }
        }
    }
    if length == 0 {
        return Err(Error::msg("universal binary has no arm64e slice"));
    }
    discard(input, offset - (8 + count * ARCH_SIZE)).context("read universal binary prefix")?;
    #[allow(clippy::cast_possible_truncation)]
    let mut data = vec![0u8; length as usize];
    input.read_exact(&mut data).context("read arm64e image")?;
    discard(input, size - offset - length).context("read universal binary suffix")?;
    Ok(data)
}

fn discard(input: &mut dyn Read, count: u64) -> Result<()> {
    let copied = std::io::copy(&mut input.take(count), &mut std::io::sink())
        .context("consume image bytes")?;
    if copied == count {
        Ok(())
    } else {
        Err(Error::msg("consume image bytes: unexpected EOF"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn universal(slices: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let header = 8 + 20 * slices.len();
        let mut out = Vec::new();
        out.extend(MAGIC_FAT.to_be_bytes());
        out.extend(u32::try_from(slices.len()).unwrap().to_be_bytes());
        let mut offset = header;
        for (cpu, sub, data) in slices {
            for v in [
                *cpu,
                *sub,
                u32::try_from(offset).unwrap(),
                u32::try_from(data.len()).unwrap(),
                0,
            ] {
                out.extend(v.to_be_bytes());
            }
            offset += data.len();
        }
        for (_, _, data) in slices {
            out.extend(*data);
        }
        out
    }

    #[test]
    fn selects_arm64e_slice_three_ways() {
        let fat = universal(&[
            (0x0100_0007, 3, b"x86"),
            (CPU_TYPE_ARM64, 0x8000_0002, b"arm64e!"),
        ]);
        assert_eq!(arm64_slice(&fat).unwrap(), b"arm64e!");
        let mut read_at = |buf: &mut [u8], off: u64| {
            #[allow(clippy::cast_possible_truncation)]
            buf.copy_from_slice(&fat[off as usize..off as usize + buf.len()]);
            Ok(())
        };
        assert_eq!(
            arm64_range(&mut read_at, fat.len() as u64).unwrap(),
            (51, 7)
        );
        let mut reader = &fat[..];
        assert_eq!(read_arm64(&mut reader, fat.len()).unwrap(), b"arm64e!");
        assert_eq!(reader.len(), 0);
    }

    #[test]
    fn thin_and_missing_slice() {
        assert_eq!(
            arm64_slice(b"\xcf\xfa\xed\xfethin").unwrap(),
            b"\xcf\xfa\xed\xfethin"
        );
        let fat = universal(&[(CPU_TYPE_ARM64, 0, b"arm64")]);
        assert_eq!(
            arm64_slice(&fat).unwrap_err().to_string(),
            "universal binary has no arm64e slice"
        );
    }
}
