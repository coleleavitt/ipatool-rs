//! LZBITMAP decoding, ported from `go-macos-pkg/pkg/lzbitmap` (itself a translation of
//! Corellium's libzbitmap). A stream is `ZBM\x09` followed by chunks of 24-bit
//! little-endian lengths, ending with a chunk that decompresses to zero bytes.

use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"ZBM\x09";
const MAX_CHUNK: usize = 0x8000;
const CHUNK_HEADER_SIZE: usize = 6;
const COMPRESSED_CHUNK_HEADER_SIZE: usize = 15;
const BITMAP_COUNT: usize = 12;
const BITMAP_BASE: u8 = 3;
const BITMAP_BYTE_COUNT: usize = 17;
const MAX_PERIOD_BYTES: u8 = 2;
const REPEAT_BASE: usize = 4;

#[derive(Clone, Copy, Default)]
struct Bitmap {
    bits: u8,
    period_bytes: u8,
}

struct Decoder<'a> {
    src: &'a [u8],
    out: Vec<u8>,
    pos: usize,
    end: usize,
    decompressed_length: usize,
    written: usize,
    prewritten: usize,
    period: usize,
    data: usize,
    meta1: usize,
    meta2: usize,
    meta3: usize,
    high_nibble: bool,
    bitmaps: [Bitmap; BITMAP_COUNT],
}

fn format_error(message: impl std::fmt::Display) -> Error {
    Error::msg(format!("lzbitmap: invalid stream: {message}"))
}

fn u24(data: &[u8]) -> usize {
    usize::from(data[0]) | usize::from(data[1]) << 8 | usize::from(data[2]) << 16
}

pub(super) fn decompress(src: &[u8]) -> Result<Vec<u8>> {
    if src.len() < MAGIC.len() || &src[..MAGIC.len()] != MAGIC {
        return Err(format_error("not \"ZBM\\t\""));
    }
    let mut decoder = Decoder {
        src,
        out: Vec::new(),
        pos: MAGIC.len(),
        end: 0,
        decompressed_length: 0,
        written: 0,
        prewritten: 0,
        period: 0,
        data: 0,
        meta1: 0,
        meta2: 0,
        meta3: 0,
        high_nibble: false,
        bitmaps: [Bitmap::default(); BITMAP_COUNT],
    };
    loop {
        decoder.chunk()?;
        decoder.pos = decoder.end;
        decoder.prewritten += decoder.decompressed_length;
        if decoder.decompressed_length == 0 {
            return Ok(decoder.out);
        }
    }
}

impl Decoder<'_> {
    fn chunk(&mut self) -> Result<()> {
        if self.pos + CHUNK_HEADER_SIZE > self.src.len() {
            return Err(format_error("truncated chunk header"));
        }
        let length = u24(&self.src[self.pos..]);
        if length < CHUNK_HEADER_SIZE || self.pos + length > self.src.len() {
            return Err(format_error(format!(
                "chunk length {length} outside the stream"
            )));
        }
        self.end = self.pos + length;
        self.decompressed_length = u24(&self.src[self.pos + 3..]);
        if self.decompressed_length > MAX_CHUNK {
            return Err(format_error(format!(
                "chunk decompresses to {}, over the {MAX_CHUNK} limit",
                self.decompressed_length
            )));
        }
        if length == self.decompressed_length + CHUNK_HEADER_SIZE {
            self.out
                .extend_from_slice(&self.src[self.pos + CHUNK_HEADER_SIZE..self.end]);
            self.written = self.decompressed_length;
            return Ok(());
        }
        self.compressed_chunk(length)
    }

    fn compressed_chunk(&mut self, length: usize) -> Result<()> {
        if length < COMPRESSED_CHUNK_HEADER_SIZE {
            return Err(format_error(format!(
                "compressed chunk of {length} bytes has no header"
            )));
        }
        self.written = 0;
        self.period = 8;
        self.data = self.pos + COMPRESSED_CHUNK_HEADER_SIZE;
        let header = &self.src[self.pos..];
        let (offset1, offset2, offset3) =
            (u24(&header[6..]), u24(&header[9..]), u24(&header[12..]));
        if offset1 >= length || offset2 >= length || offset3 >= length {
            return Err(format_error("metadata offset outside the chunk"));
        }
        self.meta1 = self.pos + offset1;
        self.meta2 = self.pos + offset2;
        self.meta3 = self.pos + offset3;
        self.high_nibble = false;
        self.read_bitmaps(length)?;
        while self.written < self.decompressed_length {
            self.one_bitmap()?;
        }
        Ok(())
    }

    fn read_bitmaps(&mut self, length: usize) -> Result<()> {
        if length < BITMAP_BYTE_COUNT {
            return Err(format_error("chunk too short to hold its bitmaps"));
        }
        let mut at = self.end - BITMAP_BYTE_COUNT;
        let mut bit = 0;
        let mut next = || -> Result<u8> {
            if at >= self.end {
                return Err(format_error("bitmaps run past the chunk"));
            }
            let value = (self.src[at] >> bit) & 1;
            bit += 1;
            if bit == 8 {
                bit = 0;
                at += 1;
            }
            Ok(value)
        };
        let mut bitmaps = [Bitmap::default(); BITMAP_COUNT];
        for (index, bitmap) in bitmaps.iter_mut().enumerate() {
            for shift in 0..8 {
                bitmap.bits |= next()? << shift;
            }
            for shift in 0..2 {
                bitmap.period_bytes |= next()? << shift;
            }
            if bitmap.period_bytes > MAX_PERIOD_BYTES {
                return Err(format_error(format!(
                    "bitmap {index} wants {} period bytes",
                    bitmap.period_bytes
                )));
            }
        }
        self.bitmaps = bitmaps;
        Ok(())
    }

    fn read_nibble(&mut self) -> Result<u8> {
        if self.meta3 >= self.end {
            return Err(format_error("bitmap numbers run past the chunk"));
        }
        let byte = self.src[self.meta3];
        if self.high_nibble {
            self.high_nibble = false;
            self.meta3 += 1;
            Ok(byte >> 4)
        } else {
            self.high_nibble = true;
            Ok(byte & 0xf)
        }
    }

    fn rewind_nibble(&mut self) {
        if self.high_nibble {
            self.high_nibble = false;
        } else {
            self.high_nibble = true;
            self.meta3 -= 1;
        }
    }

    fn one_bitmap(&mut self) -> Result<()> {
        let number = self.read_nibble()?;
        let repeat = self.repetition_count()?;
        for _ in 0..repeat {
            self.apply_number(number)?;
        }
        Ok(())
    }

    /// The trailing bitmaps of a chunk never carry a count, or it could not be told apart
    /// from a bitmap number.
    fn repetition_count(&mut self) -> Result<usize> {
        if self.decompressed_length - self.written <= 8 {
            return Ok(1);
        }
        let mut nibble = self.read_nibble()?;
        if nibble != 0xf {
            self.rewind_nibble();
            return Ok(1);
        }
        let mut total = REPEAT_BASE;
        while nibble == 0xf {
            nibble = self.read_nibble()?;
            total += usize::from(nibble);
            if total > MAX_CHUNK {
                return Err(format_error(format!("repetition count over {MAX_CHUNK}")));
            }
        }
        Ok(total)
    }

    fn apply_number(&mut self, number: u8) -> Result<()> {
        if number == 0xf {
            return Err(format_error("0xf is a repetition marker, not a bitmap"));
        }
        if number > MAX_PERIOD_BYTES {
            return self.apply(self.bitmaps[usize::from(number - BITMAP_BASE)]);
        }
        if self.meta2 >= self.end {
            return Err(format_error("inline bitmaps run past the chunk"));
        }
        let bitmap = Bitmap {
            bits: self.src[self.meta2],
            period_bytes: number,
        };
        self.meta2 += 1;
        self.apply(bitmap)
    }

    fn apply(&mut self, bitmap: Bitmap) -> Result<()> {
        if bitmap.period_bytes > 0 {
            self.period = 0;
            for index in 0..usize::from(bitmap.period_bytes) {
                if self.meta1 >= self.end {
                    return Err(format_error("periods run past the chunk"));
                }
                self.period |= usize::from(self.src[self.meta1]) << (index * 8);
                self.meta1 += 1;
            }
        }
        if self.period == 0 {
            return Err(format_error("zero repetition period"));
        }
        for bit in 0..8 {
            if self.written == self.decompressed_length {
                break;
            }
            if bitmap.bits & (1 << bit) != 0 {
                if self.data >= self.end {
                    return Err(format_error("literals run past the chunk"));
                }
                self.out.push(self.src[self.data]);
                self.data += 1;
            } else {
                if self.prewritten + self.written < self.period {
                    return Err(format_error(format!(
                        "period {} reaches before the output",
                        self.period
                    )));
                }
                self.out.push(self.out[self.out.len() - self.period]);
            }
            self.written += 1;
        }
        Ok(())
    }
}
