//! `LC_DYLD_CHAINED_FIXUPS` decoding as `blacktop/go-macho` v1.1.282 does it: the starts
//! and imports tables, then every chain walked through the image's current bytes.

use crate::error::{Error, Result};

use super::macho::Segment;

pub(super) const DYLD_CHAINED_PTR_ARM64E: u16 = 1;
const DYLD_CHAINED_PTR_64: u16 = 2;
const DYLD_CHAINED_PTR_32: u16 = 3;
const DYLD_CHAINED_PTR_32_CACHE: u16 = 4;
const DYLD_CHAINED_PTR_32_FIRMWARE: u16 = 5;
const DYLD_CHAINED_PTR_64_OFFSET: u16 = 6;
const DYLD_CHAINED_PTR_ARM64E_KERNEL: u16 = 7;
const DYLD_CHAINED_PTR_64_KERNEL_CACHE: u16 = 8;
const DYLD_CHAINED_PTR_ARM64E_USERLAND: u16 = 9;
const DYLD_CHAINED_PTR_ARM64E_FIRMWARE: u16 = 10;
const DYLD_CHAINED_PTR_X86_64_KERNEL_CACHE: u16 = 11;
const DYLD_CHAINED_PTR_ARM64E_USERLAND24: u16 = 12;

const DYLD_CHAINED_PTR_START_NONE: u16 = 0xffff;
const DYLD_CHAINED_PTR_START_MULTI: u16 = 0x8000;
const DYLD_CHAINED_PTR_START_LAST: u16 = 0x8000;

const DC_IMPORT: u32 = 1;
const DC_IMPORT_ADDEND: u32 = 2;
const DC_IMPORT_ADDEND64: u32 = 3;

/// The go-macho fixup type a chain entry decodes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Arm64eRebase,
    Arm64eAuthRebase,
    Arm64eBind,
    Arm64eAuthBind,
    Arm64eBind24,
    Arm64eAuthBind24,
    Ptr64Rebase,
    Ptr64RebaseOffset,
    Ptr64Bind,
    KernelCacheRebase,
    Ptr32Rebase,
    Ptr32Bind,
    Ptr32CacheRebase,
    Ptr32FirmwareRebase,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Fixup {
    pub offset: u64,
    pub raw: u64,
    pub kind: Kind,
    pub format: u16,
}

#[derive(Clone, Debug)]
pub(super) struct Import {
    pub name: String,
    pub addend: i64,
}

#[derive(Clone, Debug)]
struct Starts {
    page_size: u16,
    pointer_format: u16,
    segment_offset: u64,
    pages: Vec<u16>,
}

#[derive(Clone, Debug)]
pub(super) struct ChainedFixups {
    pub pointer_format: u16,
    pub imports: Vec<Import>,
    pub fixups: Vec<Fixup>,
    starts: Vec<Option<Starts>>,
}

pub(super) const fn bits(value: u64, start: u32, width: u32) -> u64 {
    (value >> start) & ((1 << width) - 1)
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self
            .position
            .checked_add(N)
            .and_then(|end| self.data.get(self.position..end))
            .ok_or_else(|| {
                Error::msg(if self.position >= self.data.len() {
                    "EOF"
                } else {
                    "unexpected EOF"
                })
            })?;
        self.position += N;
        let mut out = [0; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64> {
        self.take().map(u64::from_le_bytes)
    }

    fn seek(&mut self, position: u64) {
        self.position = usize::try_from(position).unwrap_or(usize::MAX);
    }
}

const fn stride(format: u16) -> Option<u64> {
    match format {
        DYLD_CHAINED_PTR_ARM64E
        | DYLD_CHAINED_PTR_ARM64E_USERLAND
        | DYLD_CHAINED_PTR_ARM64E_USERLAND24 => Some(8),
        DYLD_CHAINED_PTR_ARM64E_KERNEL
        | DYLD_CHAINED_PTR_ARM64E_FIRMWARE
        | DYLD_CHAINED_PTR_32_FIRMWARE
        | DYLD_CHAINED_PTR_64
        | DYLD_CHAINED_PTR_64_OFFSET
        | DYLD_CHAINED_PTR_32
        | DYLD_CHAINED_PTR_32_CACHE
        | DYLD_CHAINED_PTR_64_KERNEL_CACHE => Some(4),
        DYLD_CHAINED_PTR_X86_64_KERNEL_CACHE => Some(1),
        _ => None,
    }
}

impl ChainedFixups {
    /// Go's `File.DyldChainedFixups`: the starts and imports tables, without walking chains.
    pub fn read(blob: &[u8], segments: &[Segment]) -> Result<Self> {
        let mut reader = Reader {
            data: blob,
            position: 0,
        };
        let parse_starts = |reader: &mut Reader| -> Result<(Vec<Option<Starts>>, u16, [u32; 7])> {
            let mut header = [0u32; 7];
            for field in &mut header {
                *field = reader.u32()?;
            }
            let starts_offset = header[1];
            reader.seek(starts_offset.into());
            let count = reader.u32()?;
            let mut offsets = Vec::new();
            for _ in 0..count {
                offsets.push(reader.u32()?);
            }
            let mut pointer_format = 0;
            let mut starts = Vec::with_capacity(offsets.len());
            for offset in offsets {
                if offset == 0 {
                    starts.push(None);
                    continue;
                }
                reader.seek(u64::from(starts_offset) + u64::from(offset));
                let _size = reader.u32()?;
                let page_size = reader.u16()?;
                let format = reader.u16()?;
                let segment_offset = reader.u64()?;
                let _max_valid_pointer = reader.u32()?;
                let page_count = reader.u16()?;
                let mut pages = Vec::with_capacity(page_count.into());
                for _ in 0..page_count {
                    pages.push(reader.u16()?);
                }
                if pointer_format == 0 {
                    pointer_format = format;
                }
                starts.push(Some(Starts {
                    page_size,
                    pointer_format: format,
                    segment_offset,
                    pages,
                }));
            }
            Ok((starts, pointer_format, header))
        };
        let (mut starts, pointer_format, header) = parse_starts(&mut reader).map_err(|error| {
            Error::msg(format!(
                "failed to parse dyld chained fixup starts: {error}"
            ))
        })?;
        for (start, segment) in starts.iter_mut().zip(segments) {
            if let Some(start) = start {
                start.segment_offset = segment.file_off;
            }
        }
        let imports = read_imports(blob, &header).map_err(|error| {
            Error::msg(format!(
                "failed to parse dyld chained fixup imports: {error}"
            ))
        })?;
        Ok(Self {
            pointer_format,
            imports,
            fixups: Vec::new(),
            starts,
        })
    }

    /// Go's `DyldChainedFixups.Parse`. `read_at` reads the image as it will be loaded.
    pub fn walk(&mut self, read_at: &dyn Fn(u64, &mut [u8]) -> usize) -> Result<()> {
        let mut fixups = Vec::new();
        for start in self.starts.iter().flatten() {
            for (page_index, &offset_in_page) in start.pages.iter().enumerate() {
                if offset_in_page == DYLD_CHAINED_PTR_START_NONE {
                    continue;
                }
                for chain in chain_starts(start, offset_in_page)? {
                    self.walk_chain(start, page_index, chain, read_at, &mut fixups)?;
                }
            }
        }
        self.fixups = fixups;
        Ok(())
    }

    fn walk_chain(
        &self,
        start: &Starts,
        page_index: usize,
        offset_in_page: u16,
        read_at: &dyn Fn(u64, &mut [u8]) -> usize,
        fixups: &mut Vec<Fixup>,
    ) -> Result<()> {
        let format = start.pointer_format;
        let step = stride(format)
            .ok_or_else(|| Error::msg(format!("unsupported pointer chain format: {format}")))?;
        let page_start = start
            .segment_offset
            .wrapping_add(page_index as u64 * u64::from(start.page_size));
        let mut next = 0u64;
        loop {
            let offset = page_start
                .wrapping_add(u64::from(offset_in_page))
                .wrapping_add(next);
            let narrow = matches!(
                format,
                DYLD_CHAINED_PTR_32 | DYLD_CHAINED_PTR_32_CACHE | DYLD_CHAINED_PTR_32_FIRMWARE
            );
            let mut buffer = [0u8; 8];
            let size = if narrow { 4 } else { 8 };
            let count = read_at(offset, &mut buffer[..size]);
            if count < size {
                return Err(Error::msg(if count == 0 {
                    "EOF"
                } else {
                    "unexpected EOF"
                }));
            }
            let raw = u64::from_le_bytes(buffer);
            let is_bind = bits(raw, 62, 1) != 0;
            let is_auth = bits(raw, 63, 1) != 0;
            let (kind, link) = match format {
                DYLD_CHAINED_PTR_32 => {
                    let kind = if bits(raw, 31, 1) != 0 {
                        Kind::Ptr32Bind
                    } else {
                        Kind::Ptr32Rebase
                    };
                    (kind, bits(raw, 26, 5))
                }
                DYLD_CHAINED_PTR_32_CACHE => (Kind::Ptr32CacheRebase, bits(raw, 26, 5)),
                DYLD_CHAINED_PTR_32_FIRMWARE => (Kind::Ptr32FirmwareRebase, bits(raw, 26, 5)),
                DYLD_CHAINED_PTR_64 | DYLD_CHAINED_PTR_64_OFFSET => {
                    let kind = if bits(raw, 63, 1) != 0 {
                        Kind::Ptr64Bind
                    } else if format == DYLD_CHAINED_PTR_64 {
                        Kind::Ptr64Rebase
                    } else {
                        Kind::Ptr64RebaseOffset
                    };
                    (kind, bits(raw, 51, 12))
                }
                DYLD_CHAINED_PTR_64_KERNEL_CACHE | DYLD_CHAINED_PTR_X86_64_KERNEL_CACHE => {
                    (Kind::KernelCacheRebase, bits(raw, 51, 12))
                }
                DYLD_CHAINED_PTR_ARM64E_USERLAND24 => {
                    let kind = match (is_bind, is_auth) {
                        (false, false) => Kind::Arm64eRebase,
                        (true, true) => Kind::Arm64eAuthBind24,
                        (false, true) => Kind::Arm64eAuthRebase,
                        (true, false) => Kind::Arm64eBind24,
                    };
                    if is_bind
                        && usize::try_from(bits(raw, 0, 24))
                            .map_or(true, |ordinal| ordinal >= self.imports.len())
                    {
                        return Err(Error::msg(format!(
                            "chained bind at {offset:#x} has an invalid import ordinal"
                        )));
                    }
                    (kind, bits(raw, 51, 11))
                }
                _ => {
                    let kind = match (is_bind, is_auth) {
                        (false, false) => Kind::Arm64eRebase,
                        (true, false) => Kind::Arm64eBind,
                        (false, true) => Kind::Arm64eAuthRebase,
                        (true, true) => Kind::Arm64eAuthBind,
                    };
                    (kind, bits(raw, 51, 11))
                }
            };
            fixups.push(Fixup {
                offset,
                raw,
                kind,
                format,
            });
            if link == 0 {
                return Ok(());
            }
            next = next.wrapping_add(link * step);
        }
    }

    /// The import name and ordinal of a bind, as go-macho's `Bind.Name` and `Bind.Ordinal`.
    pub fn bind_import(&self, fixup: &Fixup) -> (String, u64) {
        let ordinal = match fixup.kind {
            Kind::Arm64eBind | Kind::Arm64eAuthBind => bits(fixup.raw, 0, 16),
            Kind::Ptr32Bind => bits(fixup.raw, 0, 20),
            _ => bits(fixup.raw, 0, 24),
        };
        let name = usize::try_from(ordinal)
            .ok()
            .and_then(|index| self.imports.get(index))
            .map_or_else(String::new, |import| import.name.clone());
        (name, ordinal)
    }

    /// Go's `DyldChainedFixups.Rebase` for the fixup types `machimage` does not decode itself.
    pub fn rebase(&mut self, fixup: &Fixup, preferred_load_address: u64) -> Result<u64> {
        self.pointer_format = fixup.format;
        let raw = fixup.raw;
        let offset = fixup.offset;
        match fixup.format {
            DYLD_CHAINED_PTR_ARM64E
            | DYLD_CHAINED_PTR_ARM64E_USERLAND
            | DYLD_CHAINED_PTR_ARM64E_USERLAND24
            | DYLD_CHAINED_PTR_ARM64E_KERNEL
            | DYLD_CHAINED_PTR_ARM64E_FIRMWARE => {
                if bits(raw, 62, 1) != 0 {
                    return Err(Error::msg(format!(
                        "offset {offset:#x} encodes a bind pointer, not a rebase"
                    )));
                }
                if bits(raw, 63, 1) != 0 {
                    return Ok(bits(raw, 0, 32));
                }
                let target = bits(raw, 43, 8) << 56 | bits(raw, 0, 43);
                Ok(match fixup.format {
                    DYLD_CHAINED_PTR_ARM64E
                    | DYLD_CHAINED_PTR_ARM64E_USERLAND24
                    | DYLD_CHAINED_PTR_ARM64E_FIRMWARE => {
                        target.wrapping_sub(preferred_load_address)
                    }
                    _ => target,
                })
            }
            DYLD_CHAINED_PTR_64 | DYLD_CHAINED_PTR_64_OFFSET => {
                if bits(raw, 63, 1) != 0 {
                    return Err(Error::msg(format!(
                        "offset {offset:#x} encodes a bind pointer, not a rebase"
                    )));
                }
                Ok(
                    (bits(raw, 36, 8) << 56 | bits(raw, 0, 36))
                        .wrapping_sub(preferred_load_address),
                )
            }
            DYLD_CHAINED_PTR_64_KERNEL_CACHE
            | DYLD_CHAINED_PTR_X86_64_KERNEL_CACHE
            | DYLD_CHAINED_PTR_32_CACHE => Ok(bits(raw, 0, 30)),
            DYLD_CHAINED_PTR_32 => {
                if bits(raw, 31, 1) != 0 {
                    return Err(Error::msg(format!(
                        "offset {offset:#x} encodes a bind pointer, not a rebase"
                    )));
                }
                Ok(bits(raw, 0, 26).wrapping_sub(preferred_load_address))
            }
            DYLD_CHAINED_PTR_32_FIRMWARE => {
                Ok(bits(raw, 0, 26).wrapping_sub(preferred_load_address))
            }
            format => Err(Error::msg(format!(
                "pointer format {format} not supported for rebase lookups"
            ))),
        }
    }
}

/// The chain starts of one page: the page start, or the overflow list it points into.
fn chain_starts(start: &Starts, page_start: u16) -> Result<Vec<u16>> {
    if page_start & DYLD_CHAINED_PTR_START_MULTI == 0 {
        return Ok(vec![page_start]);
    }
    let mut chains = Vec::new();
    let mut overflow = usize::from(page_start & !DYLD_CHAINED_PTR_START_MULTI);
    loop {
        let entry = *start
            .pages
            .get(overflow)
            .ok_or_else(|| Error::msg("chain start index out of range"))?;
        chains.push(entry & !DYLD_CHAINED_PTR_START_LAST);
        if entry & DYLD_CHAINED_PTR_START_LAST != 0 {
            return Ok(chains);
        }
        overflow += 1;
    }
}

fn read_imports(blob: &[u8], header: &[u32; 7]) -> Result<Vec<Import>> {
    let [
        _,
        _,
        imports_offset,
        symbols_offset,
        imports_count,
        imports_format,
        _,
    ] = *header;
    if imports_count == 0 {
        return Ok(Vec::new());
    }
    let mut reader = Reader {
        data: blob,
        position: 0,
    };
    reader.seek(imports_offset.into());
    let mut entries = Vec::new();
    for _ in 0..imports_count {
        let entry = match imports_format {
            DC_IMPORT => {
                let value = reader.u32()?;
                (bits(value.into(), 9, 23), 0)
            }
            DC_IMPORT_ADDEND => {
                let value = reader.u32()?;
                let addend = reader.u32()?.cast_signed();
                (bits(value.into(), 9, 23), i64::from(addend))
            }
            DC_IMPORT_ADDEND64 => {
                let value = reader.u64()?;
                let addend = reader.u64()?.cast_signed();
                (bits(value, 32, 32), addend)
            }
            _ => return Ok(Vec::new()),
        };
        entries.push(entry);
    }
    let pool = blob.get(symbols_offset as usize..).unwrap_or_default();
    entries
        .into_iter()
        .map(|(name_offset, addend)| {
            let name = usize::try_from(name_offset)
                .ok()
                .and_then(|start| pool.get(start..))
                .and_then(|bytes| {
                    bytes
                        .iter()
                        .position(|&byte| byte == 0)
                        .map(|end| &bytes[..end])
                })
                .ok_or_else(|| {
                    Error::msg(format!(
                        "failed to read string at: {}: EOF",
                        u64::from(symbols_offset) + name_offset
                    ))
                })?;
            Ok(Import {
                name: String::from_utf8_lossy(name).into_owned(),
                addend,
            })
        })
        .collect()
}
