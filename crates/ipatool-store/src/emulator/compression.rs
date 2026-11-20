//! Go `compression.go`: zlib's `inflate` for response bodies, decoded on the host.

use std::io::Read as _;

use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};

use super::{Guest, Result, Uc, fail};

const LIMIT: usize = 32 << 20;
/// zlib's `Z_DATA_ERROR` as the unsigned return register.
const Z_DATA_ERROR: u64 = !2;

/// Go's `inflateStream`.
#[derive(Debug, Default)]
pub(crate) struct InflateStream {
    bits: i64,
    input: Vec<u8>,
    output: Vec<u8>,
    position: usize,
    decoded: bool,
}

/// Go's gzip, zlib and flate readers: `None` when the stream is invalid.
fn decode(bits: i64, input: &[u8]) -> Result<Option<Vec<u8>>> {
    let empty = input.is_empty();
    let reader: Box<dyn std::io::Read + '_> = match bits {
        // Go's gzip.Reader reads concatenated members, like MultiGzDecoder.
        31 | 47 => Box::new(MultiGzDecoder::new(input)),
        15 => Box::new(ZlibDecoder::new(input)),
        -15 => Box::new(DeflateDecoder::new(input)),
        _ => fail!("unsupported inflate window size"),
    };
    // Go's readers fail on a missing header or deflate stream (io.EOF, io.ErrUnexpectedEOF).
    if empty {
        return Ok(None);
    }
    let mut output = Vec::new();
    if reader
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut output)
        .is_err()
    {
        return Ok(None);
    }
    if output.len() > LIMIT {
        fail!("decompressed response exceeds runtime limit");
    }
    Ok(Some(output))
}

pub(crate) fn compression_symbol(uc: &mut Uc, name: &str) -> Result<bool> {
    match name {
        "inflateInit2_" => {
            if uc.arg(3)? != 112 {
                fail!("unsupported zlib stream ABI");
            }
            let stream = uc.arg(0)?;
            #[allow(clippy::cast_possible_truncation, reason = "Go's int32(windowBits)")]
            let bits = i64::from((uc.arg(1)? as u32).cast_signed());
            uc.st().inflaters.insert(
                stream,
                InflateStream {
                    bits,
                    ..InflateStream::default()
                },
            );
            uc.write64(stream + 16, 0)?;
            uc.write64(stream + 40, 0)?;
            uc.set(0)?;
        }
        "inflateEnd" => {
            let stream = uc.arg(0)?;
            uc.st().inflaters.remove(&stream);
            uc.set(0)?;
        }
        "inflate" => inflate(uc)?,
        _ => return Ok(false),
    }
    Ok(true)
}

fn inflate(uc: &mut Uc) -> Result<()> {
    let address = uc.arg(0)?;
    if !uc.sti().inflaters.contains_key(&address) {
        fail!("uninitialized inflate stream");
    }
    let size = uc.read64(address + 8)? & 0xffff_ffff;
    if size > 32 << 20 {
        fail!("inflate input exceeds budget");
    }
    if size != 0 {
        let pointer = uc.read64(address)?;
        let data = uc.read_mem(pointer, size)?;
        if let Some(stream) = uc.st().inflaters.get_mut(&address) {
            stream.input.extend_from_slice(&data);
        }
        uc.write64(address, pointer + size)?;
        uc.write32(address + 8, 0)?;
        let total = uc.read64(address + 16)?;
        uc.write64(address + 16, total + size)?;
    }
    let Some(stream) = uc.st().inflaters.get_mut(&address) else {
        fail!("uninitialized inflate stream");
    };
    if !stream.decoded {
        let Some(output) = decode(stream.bits, &stream.input)? else {
            return uc.set(Z_DATA_ERROR);
        };
        stream.output = output;
        stream.decoded = true;
    }
    let available = (stream.output.len() - stream.position) as u64;
    let space = uc.read64(address + 32)? & 0xffff_ffff;
    let count = space.min(available);
    let out = uc.read64(address + 24)?;
    let Some(stream) = uc.st().inflaters.get_mut(&address) else {
        fail!("uninitialized inflate stream");
    };
    let start = stream.position;
    let end = start + usize::try_from(count).unwrap_or_default();
    stream.position = end;
    let chunk = stream.output[start..end].to_vec();
    let finished = stream.position == stream.output.len();
    if count != 0 {
        uc.write_mem(out, &chunk)?;
    }
    uc.write64(address + 24, out + count)?;
    #[allow(clippy::cast_possible_truncation, reason = "Go's uint32(size-count)")]
    uc.write32(address + 32, (space - count) as u32)?;
    let total = uc.read64(address + 40)?;
    uc.write64(address + 40, total + count)?;
    uc.set(u64::from(finished))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn decodes_gzip_zlib_and_raw_deflate() {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"hello").unwrap();
        let gzip = gzip.finish().unwrap();
        assert_eq!(decode(31, &gzip).unwrap().unwrap(), b"hello");
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        zlib.write_all(b"hello").unwrap();
        assert_eq!(
            decode(15, &zlib.finish().unwrap()).unwrap().unwrap(),
            b"hello"
        );
        let mut raw =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(b"hello").unwrap();
        assert_eq!(
            decode(-15, &raw.finish().unwrap()).unwrap().unwrap(),
            b"hello"
        );
        assert!(decode(31, b"not gzip").unwrap().is_none());
        assert_eq!(
            decode(9, b"").unwrap_err().to_string(),
            "unsupported inflate window size"
        );
    }
}
