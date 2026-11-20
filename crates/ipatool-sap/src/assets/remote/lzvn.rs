//! LZVN decoding, ported from the decoder of `go-compressions/lzfse` (`lzvn.go`) that Go
//! uses for decmpfs blocks. Those streams end when the output is full, without the
//! end-of-stream opcode Apple's LZFSE block decoder requires.

use crate::error::{Error, Result};

/// Decodes `src` into at most `maximum_size` bytes.
pub(super) fn decompress(src: &[u8], maximum_size: usize) -> Result<Vec<u8>> {
    let mut dst = vec![0u8; maximum_size];
    let count = decode(&mut dst, src)?;
    dst.truncate(count);
    Ok(dst)
}

fn decode(dst: &mut [u8], src: &[u8]) -> Result<usize> {
    let error = |message: &str| Error::msg(format!("lzvn: {message}"));
    let (mut dpos, mut spos, mut previous_distance) = (0usize, 0usize, 0usize);
    let (dst_end, src_end) = (dst.len(), src.len());
    let literal =
        |dst: &mut [u8], dpos: &mut usize, spos: &mut usize, length: usize, what: &str| {
            if *spos + length > src_end || *dpos + length > dst_end {
                return Err(error(what));
            }
            dst[*dpos..*dpos + length].copy_from_slice(&src[*spos..*spos + length]);
            *dpos += length;
            *spos += length;
            Ok(())
        };
    while spos < src_end {
        let opcode = src[spos];
        spos += 1;
        match opcode {
            0x06 => return Ok(dpos),
            0x0e | 0x16 => {}
            0xf0..=0xff => {
                let length = if opcode == 0xf0 {
                    let Some(&extra) = src.get(spos) else {
                        return Err(error("truncated lrg_m opcode"));
                    };
                    spos += 1;
                    usize::from(extra) + 16
                } else {
                    usize::from(opcode & 0x0f)
                };
                if previous_distance == 0 {
                    return Err(error("match without prior distance"));
                }
                copy_match(dst, dpos, previous_distance, length)?;
                dpos += length;
            }
            0xe0..=0xef => {
                let length = if opcode == 0xe0 {
                    let Some(&extra) = src.get(spos) else {
                        return Err(error("truncated lrg_l opcode"));
                    };
                    spos += 1;
                    usize::from(extra) + 16
                } else {
                    usize::from(opcode & 0x0f)
                };
                literal(dst, &mut dpos, &mut spos, length, "literal overflow")?;
            }
            0xa0..=0xbf => {
                if spos + 2 > src_end {
                    return Err(error("truncated med_d opcode"));
                }
                let (first, second) = (src[spos], src[spos + 1]);
                spos += 2;
                let length = usize::from((opcode >> 3) & 0x03);
                let matched = usize::from((opcode & 0x07) << 2 | first & 0x03) + 3;
                let distance = usize::from(first >> 2) | usize::from(second) << 6;
                if distance == 0 {
                    return Err(error("zero match distance"));
                }
                literal(dst, &mut dpos, &mut spos, length, "med_d literal overflow")?;
                copy_match(dst, dpos, distance, matched)?;
                dpos += matched;
                previous_distance = distance;
            }
            _ => {
                let length = usize::from(opcode >> 6);
                let matched = usize::from((opcode >> 3) & 0x07) + 3;
                let distance = match opcode & 0x07 {
                    6 => {
                        if previous_distance == 0 {
                            return Err(error("pre_d without prior distance"));
                        }
                        literal(dst, &mut dpos, &mut spos, length, "pre_d literal overflow")?;
                        previous_distance
                    }
                    7 => {
                        if spos + 2 > src_end {
                            return Err(error("truncated lrg_d opcode"));
                        }
                        let distance = usize::from(u16::from_le_bytes([src[spos], src[spos + 1]]));
                        spos += 2;
                        if distance == 0 {
                            return Err(error("zero match distance"));
                        }
                        literal(dst, &mut dpos, &mut spos, length, "lrg_d literal overflow")?;
                        distance
                    }
                    low => {
                        let Some(&extra) = src.get(spos) else {
                            return Err(error("truncated sml_d opcode"));
                        };
                        spos += 1;
                        let distance = usize::from(low) << 8 | usize::from(extra);
                        if distance == 0 {
                            return Err(error("zero match distance"));
                        }
                        literal(dst, &mut dpos, &mut spos, length, "sml_d literal overflow")?;
                        distance
                    }
                };
                copy_match(dst, dpos, distance, matched)?;
                dpos += matched;
                previous_distance = distance;
            }
        }
    }
    Ok(dpos)
}

/// Copies `length` bytes from `distance` back, byte by byte so overlapping runs repeat.
fn copy_match(dst: &mut [u8], dpos: usize, distance: usize, length: usize) -> Result<()> {
    if distance == 0 || distance > dpos {
        return Err(Error::msg("lzvn: invalid match distance"));
    }
    if dpos + length > dst.len() {
        return Err(Error::msg("lzvn: match overflow"));
    }
    for index in dpos..dpos + length {
        dst[index] = dst[index - distance];
    }
    Ok(())
}
