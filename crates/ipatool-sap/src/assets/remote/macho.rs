//! The parts of `blacktop/go-macho` the Go extractor uses: load-command parsing, symbol
//! tables, and `File.Export` of an image that lives inside a shared cache or kernel
//! collection. Output bytes follow go-macho's serialization exactly.

use super::{le16, le32, le64};
use crate::error::{Context, Error, Result};

pub(super) const MAGIC64: u32 = 0xfeed_facf;
pub(super) const MH_FILESET: u32 = 0xc;
const MH_DYLIB_IN_CACHE: u32 = 0x8000_0000;
const CPU_ARM64: u32 = 0x0100_000c;
const CPU_ARM64_32: u32 = 0x0200_000c;
const HEADER_SIZE: usize = 32;

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_ID_DYLIB: u32 = 0xd;
const LC_UUID: u32 = 0x1b;
pub(super) const LC_LOAD_DYLIB: u32 = 0xc;
pub(super) const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
pub(super) const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
pub(super) const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
pub(super) const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_SEGMENT_SPLIT_INFO: u32 = 0x1e;
const LC_FUNCTION_STARTS: u32 = 0x26;
const LC_DATA_IN_CODE: u32 = 0x29;
const LC_DYLIB_CODE_SIGN_DRS: u32 = 0x2b;
const LC_LINKER_OPTIMIZATION_HINT: u32 = 0x2e;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
const LC_FILESET_ENTRY: u32 = 0x8000_0035;
const LC_FUNCTION_VARIANTS: u32 = 0x37;
const LC_FUNCTION_VARIANT_FIXUPS: u32 = 0x38;
const LC_LAZY_LOAD_DYLIB_INFO: u32 = 0x3a;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2c;
const DYLIB_USE_MARKER: u32 = 0x1a74_1800;
const DYLIB_COMMAND_SIZE: usize = 24;
const DYLIB_USE_COMMAND_SIZE: u32 = 28;
const INDIRECT_SYMBOL_LOCAL: u32 = 0x8000_0000;

pub(super) const N_EXT: u8 = 0x01;
pub(super) const N_ABS: u8 = 0x02;
const N_TYPE: u8 = 0x0e;
const N_UNDF: u8 = 0x00;
const N_INDR: u8 = 0x0a;

const EXPORT_SYMBOL_FLAGS_KIND_MASK: u64 = 0x03;
const EXPORT_SYMBOL_FLAGS_KIND_REGULAR: u64 = 0x00;
const EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL: u64 = 0x01;
const EXPORT_SYMBOL_FLAGS_REEXPORT: u64 = 0x08;
const EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER: u64 = 0x10;

/// Reads an image's data: by file offset in the reader that holds its `__LINKEDIT`
/// (go-macho `cr.ReadAt`), and by virtual address (`cr.ReadAtAddr`).
pub(super) trait Source {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()>;
    fn read_at_address(&self, data: &mut [u8], address: u64) -> Result<()>;

    fn read_vec(&self, size: u64, offset: u64) -> Result<Vec<u8>> {
        let mut data =
            vec![0u8; usize::try_from(size).map_err(|_| Error::msg("Mach-O data is too large"))?];
        self.read_at(&mut data, offset)?;
        Ok(data)
    }
}

/// A Mach-O image inside one in-memory buffer, addressed through its own segments.
pub(super) struct FileReader<'a> {
    data: &'a [u8],
    segments: Vec<(u64, u64, u64)>,
}

impl<'a> FileReader<'a> {
    pub(super) fn new(data: &'a [u8], file: &File) -> Self {
        Self {
            data,
            segments: file
                .segments()
                .map(|segment| (segment.address, segment.memory_size, segment.offset))
                .collect(),
        }
    }

    fn offset(&self, address: u64) -> Result<u64> {
        self.segments
            .iter()
            .find(|(start, size, _)| *start <= address && address < start + size)
            .map(|(start, _, offset)| address - start + offset)
            .ok_or_else(|| {
                Error::msg(format!(
                    "address {address:#x} not within any segment's adress range"
                ))
            })
    }
}

impl Source for FileReader<'_> {
    fn read_at(&self, data: &mut [u8], offset: u64) -> Result<()> {
        super::ReadAt::read_at(self.data, data, offset)
    }

    fn read_at_address(&self, data: &mut [u8], address: u64) -> Result<()> {
        let offset = self.offset(address)?;
        super::ReadAt::read_at(self.data, data, offset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Symbol {
    pub(super) name: String,
    pub(super) indirect_name: String,
    pub(super) n_type: u8,
    pub(super) section: u8,
    pub(super) desc: u16,
    pub(super) value: u64,
}

impl Symbol {
    pub(super) fn is_external(&self) -> bool {
        self.n_type & N_EXT != 0
    }

    pub(super) fn is_undefined(&self) -> bool {
        self.n_type & N_TYPE == N_UNDF
    }

    pub(super) fn is_indirect(&self) -> bool {
        self.n_type & N_TYPE == N_INDR
    }
}

/// go-macho strips the `_` the Go toolchain adds to dotted symbol names.
pub(super) fn normalize_symbol_name(name: &str) -> &str {
    if name.contains('.') && name.starts_with('_') {
        &name[1..]
    } else {
        name
    }
}

pub(super) fn cstring(data: &[u8]) -> String {
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

#[derive(Clone)]
struct Header {
    magic: u32,
    cpu: u32,
    sub_cpu: u32,
    file_type: u32,
    command_count: u32,
    commands_size: u32,
    flags: u32,
    reserved: u32,
}

#[derive(Clone)]
pub(super) struct Section {
    pub(super) name: String,
    pub(super) segment: String,
    pub(super) address: u64,
    pub(super) size: u64,
    offset: u32,
    align: u32,
    relocation_offset: u32,
    relocation_count: u32,
    pub(super) flags: u32,
    reserved: [u32; 3],
}

#[derive(Clone)]
pub(super) struct Segment {
    command: u32,
    length: u32,
    pub(super) name: String,
    pub(super) address: u64,
    pub(super) memory_size: u64,
    pub(super) offset: u64,
    file_size: u64,
    max_protection: u32,
    protection: u32,
    section_count: u32,
    flags: u32,
    first_section: usize,
}

#[derive(Clone, Default)]
pub(super) struct Symtab {
    length: u32,
    pub(super) symbol_offset: u32,
    pub(super) symbol_count: u32,
    pub(super) string_offset: u32,
    pub(super) string_size: u32,
    pub(super) symbols: Vec<Symbol>,
}

#[derive(Clone, Default)]
pub(super) struct Dysymtab {
    /// The 18 `dysymtab_command` fields after `cmd` and `cmdsize`.
    pub(super) fields: [u32; 18],
    length: u32,
    pub(super) indirect_symbols: Vec<u32>,
}

impl Symtab {
    pub(super) fn from_command(raw: &[u8]) -> Self {
        Self {
            length: le32(&raw[4..]),
            symbol_offset: le32(&raw[8..]),
            symbol_count: le32(&raw[12..]),
            string_offset: le32(&raw[16..]),
            string_size: le32(&raw[20..]),
            symbols: Vec::new(),
        }
    }
}

impl Dysymtab {
    const ILOCALSYM: usize = 0;
    const NLOCALSYM: usize = 1;
    const IEXTDEFSYM: usize = 2;
    const NEXTDEFSYM: usize = 3;
    pub(super) const IUNDEFSYM: usize = 4;
    pub(super) const NUNDEFSYM: usize = 5;
    pub(super) const INDIRECTSYMOFF: usize = 12;
    pub(super) const NINDIRECTSYMS: usize = 13;
    const EXTRELOFF: usize = 14;
    const NEXTREL: usize = 15;
    const LOCRELOFF: usize = 16;
    const NLOCREL: usize = 17;

    pub(super) fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() < 80 {
            return Err(Error::msg("failed to read LC_DYSYMTAB: unexpected EOF"));
        }
        let mut fields = [0u32; 18];
        for (index, field) in fields.iter_mut().enumerate() {
            *field = le32(&raw[8 + index * 4..]);
        }
        Ok(Self {
            fields,
            length: le32(&raw[4..]),
            indirect_symbols: Vec::new(),
        })
    }
}

#[derive(Clone)]
enum Load {
    Segment(Segment),
    Symtab(Symtab),
    Dysymtab(Dysymtab),
    /// `linkedit_data_command` kinds: command, length, data offset and size.
    LinkEdit {
        command: u32,
        length: u32,
        offset: u32,
        size: u32,
    },
    Dylib {
        raw: Vec<u8>,
        name: String,
        flags: Option<u32>,
    },
    FilesetEntry {
        raw: Vec<u8>,
        file_offset: u64,
        entry_id: String,
    },
    /// Commands go-macho writes back from their decoded fields unchanged.
    Raw(Vec<u8>),
}

impl Load {
    fn command(&self) -> u32 {
        match self {
            Self::Segment(segment) => segment.command,
            Self::Symtab(_) => LC_SYMTAB,
            Self::Dysymtab(_) => LC_DYSYMTAB,
            Self::LinkEdit { command, .. } => *command,
            Self::Dylib { raw, .. } | Self::FilesetEntry { raw, .. } | Self::Raw(raw) => le32(raw),
        }
    }

    fn size(&self) -> u32 {
        match self {
            Self::Segment(segment) => segment.length,
            Self::Symtab(symtab) => symtab.length,
            Self::Dysymtab(dysymtab) => dysymtab.length,
            Self::LinkEdit { length, .. } => *length,
            Self::Dylib { raw, .. } | Self::FilesetEntry { raw, .. } | Self::Raw(raw) => {
                le32(&raw[4..])
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct File {
    header: Header,
    pub(super) image_type: u32,
    loads: Vec<Load>,
    sections: Vec<Section>,
}

impl File {
    /// Parses the image whose header starts at `offset` of `data`, reading its symbol
    /// table from `data` like go-macho `NewFile` does.
    pub(super) fn parse(data: &[u8], offset: u64) -> Result<Self> {
        let start = usize::try_from(offset).map_err(|_| Error::msg("Mach-O offset overflows"))?;
        let image = data
            .get(start..)
            .ok_or_else(|| Error::msg("failed to parse magic: EOF"))?;
        let mut file = Self::parse_header_and_commands(image)?;
        let source: &[u8] = data;
        file.load_symbols(source)?;
        Ok(file)
    }

    /// Parses the header and load commands at the start of `data`; symbol tables keep
    /// their commands but no symbols.
    pub(super) fn parse_header_and_commands(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(Error::msg("failed to parse magic: EOF"));
        }
        let header = Header {
            magic: le32(data),
            cpu: le32(&data[4..]),
            sub_cpu: le32(&data[8..]),
            file_type: le32(&data[12..]),
            command_count: le32(&data[16..]),
            commands_size: le32(&data[20..]),
            flags: le32(&data[24..]),
            reserved: le32(&data[28..]),
        };
        if header.magic != MAGIC64 {
            return Err(Error::msg(format!(
                "invalid magic number {:#x}",
                header.magic
            )));
        }
        let commands = data
            .get(HEADER_SIZE..HEADER_SIZE + header.commands_size as usize)
            .ok_or_else(|| Error::msg("failed to read load commands: unexpected EOF"))?;
        let mut file = Self {
            image_type: header.file_type,
            header,
            loads: Vec::new(),
            sections: Vec::new(),
        };
        let mut rest = commands;
        for _ in 0..file.header.command_count {
            if rest.len() < 8 {
                return Err(Error::msg("command block too small"));
            }
            let size = le32(&rest[4..]) as usize;
            if size < 8 || size > rest.len() {
                return Err(Error::msg("invalid command block size"));
            }
            let (raw, remaining) = rest.split_at(size);
            rest = remaining;
            let load = file.parse_command(raw)?;
            file.loads.push(load);
        }
        Ok(file)
    }

    fn parse_command(&mut self, raw: &[u8]) -> Result<Load> {
        let command = le32(raw);
        let length = le32(&raw[4..]);
        let truncated = || {
            Error::msg(format!(
                "failed to read load command {command:#x}: unexpected EOF"
            ))
        };
        Ok(match command {
            LC_SEGMENT_64 => {
                if raw.len() < 72 {
                    return Err(truncated());
                }
                let segment = Segment {
                    command,
                    length,
                    name: cstring(&raw[8..24]),
                    address: le64(&raw[24..]),
                    memory_size: le64(&raw[32..]),
                    offset: le64(&raw[40..]),
                    file_size: le64(&raw[48..]),
                    max_protection: le32(&raw[56..]),
                    protection: le32(&raw[60..]),
                    section_count: le32(&raw[64..]),
                    flags: le32(&raw[68..]),
                    first_section: self.sections.len(),
                };
                for index in 0..segment.section_count as usize {
                    let header = raw
                        .get(72 + index * 80..72 + (index + 1) * 80)
                        .ok_or_else(|| Error::msg("failed to read Section64: unexpected EOF"))?;
                    self.sections.push(Section {
                        name: cstring(&header[..16]),
                        segment: cstring(&header[16..32]),
                        address: le64(&header[32..]),
                        size: le64(&header[40..]),
                        offset: le32(&header[48..]),
                        align: le32(&header[52..]),
                        relocation_offset: le32(&header[56..]),
                        relocation_count: le32(&header[60..]),
                        flags: le32(&header[64..]),
                        reserved: [
                            le32(&header[68..]),
                            le32(&header[72..]),
                            le32(&header[76..]),
                        ],
                    });
                }
                Load::Segment(segment)
            }
            LC_SYMTAB => {
                if raw.len() < 24 {
                    return Err(truncated());
                }
                Load::Symtab(Symtab::from_command(raw))
            }
            LC_DYSYMTAB => Load::Dysymtab(Dysymtab::parse(raw)?),
            LC_CODE_SIGNATURE
            | LC_SEGMENT_SPLIT_INFO
            | LC_FUNCTION_STARTS
            | LC_DATA_IN_CODE
            | LC_DYLIB_CODE_SIGN_DRS
            | LC_LINKER_OPTIMIZATION_HINT
            | LC_DYLD_EXPORTS_TRIE
            | LC_DYLD_CHAINED_FIXUPS
            | LC_FUNCTION_VARIANTS
            | LC_FUNCTION_VARIANT_FIXUPS
            | LC_LAZY_LOAD_DYLIB_INFO => {
                if raw.len() < 16 {
                    return Err(truncated());
                }
                Load::LinkEdit {
                    command,
                    length,
                    offset: le32(&raw[8..]),
                    size: le32(&raw[12..]),
                }
            }
            LC_ID_DYLIB | LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB
            | LC_LOAD_UPWARD_DYLIB | LC_LAZY_LOAD_DYLIB => {
                if raw.len() < DYLIB_COMMAND_SIZE {
                    return Err(truncated());
                }
                let name_offset = le32(&raw[8..]) as usize;
                if name_offset >= raw.len() {
                    return Err(Error::msg("invalid name in dynamic library command"));
                }
                let uses = command != LC_ID_DYLIB
                    && le32(&raw[8..]) == DYLIB_USE_COMMAND_SIZE
                    && le32(&raw[12..]) == DYLIB_USE_MARKER;
                Load::Dylib {
                    raw: raw.to_vec(),
                    name: cstring(&raw[name_offset..]),
                    flags: uses.then(|| le32(&raw[DYLIB_COMMAND_SIZE..])),
                }
            }
            LC_FILESET_ENTRY => {
                if raw.len() < 32 {
                    return Err(truncated());
                }
                let entry_offset = le32(&raw[24..]) as usize;
                if entry_offset >= raw.len() {
                    return Err(Error::msg("invalid entry ID in fileset entry command"));
                }
                Load::FilesetEntry {
                    raw: raw.to_vec(),
                    file_offset: le64(&raw[16..]),
                    entry_id: cstring(&raw[entry_offset..]),
                }
            }
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY | LC_ENCRYPTION_INFO | LC_ENCRYPTION_INFO_64 => {
                return Err(Error::msg(format!(
                    "load command {command:#x} is not supported by this exporter"
                )));
            }
            _ => Load::Raw(raw.to_vec()),
        })
    }

    /// Reads symbols and indirect symbols through `source` file offsets, like go-macho.
    fn load_symbols(&mut self, source: &[u8]) -> Result<()> {
        for load in &mut self.loads {
            match load {
                Load::Symtab(symtab) => {
                    let strings = slice(
                        source,
                        symtab.string_offset.into(),
                        symtab.string_size.into(),
                    )
                    .with_context(|| {
                        format!("failed to read data at Stroff={:#x}", symtab.string_offset)
                    })?;
                    let table = slice(
                        source,
                        symtab.symbol_offset.into(),
                        u64::from(symtab.symbol_count) * 16,
                    )
                    .with_context(|| {
                        format!("failed to read data at Symoff={:#x}", symtab.symbol_offset)
                    })?;
                    symtab.symbols = table
                        .as_chunks::<16>()
                        .0
                        .iter()
                        .map(|entry| {
                            decode_symbol(entry, |offset| {
                                usize::try_from(offset)
                                    .ok()
                                    .and_then(|offset| strings.get(offset..))
                                    .map(cstring)
                            })
                        })
                        .collect();
                }
                Load::Dysymtab(dysymtab) => {
                    let count = dysymtab.fields[Dysymtab::NINDIRECTSYMS];
                    if count > 0 {
                        let table = slice(
                            source,
                            dysymtab.fields[Dysymtab::INDIRECTSYMOFF].into(),
                            u64::from(count) * 4,
                        )
                        .context("failed to read indirect symbols")?;
                        dysymtab.indirect_symbols = super::le32_words(table);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.loads.iter().filter_map(|load| match load {
            Load::Segment(segment) => Some(segment),
            _ => None,
        })
    }

    pub(super) fn segment(&self, name: &str) -> Option<&Segment> {
        self.segments().find(|segment| segment.name == name)
    }

    /// Every section in load-command order, like go-macho's `File.Sections`.
    pub(super) fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The first `LC_UUID`, like go-macho's `File.UUID`.
    pub(super) fn uuid(&self) -> Option<[u8; 16]> {
        self.loads.iter().find_map(|load| match load {
            Load::Raw(raw) if le32(raw) == LC_UUID && raw.len() >= 24 => {
                let mut uuid = [0u8; 16];
                uuid.copy_from_slice(&raw[8..24]);
                Some(uuid)
            }
            _ => None,
        })
    }

    /// The dependent-library commands (everything but `LC_ID_DYLIB`) with their command,
    /// name and dylib-use flags (0 for classic commands).
    pub(super) fn dependencies(&self) -> impl Iterator<Item = (u32, &str, u32)> {
        self.loads.iter().filter_map(|load| match load {
            Load::Dylib { raw, name, flags } if le32(raw) != LC_ID_DYLIB => {
                Some((le32(raw), name.as_str(), flags.unwrap_or_default()))
            }
            _ => None,
        })
    }

    /// The `LC_DYLD_EXPORTS_TRIE` data offset and size.
    pub(super) fn exports_trie(&self) -> Option<(u32, u32)> {
        self.loads.iter().find_map(|load| match load {
            Load::LinkEdit {
                command: LC_DYLD_EXPORTS_TRIE,
                offset,
                size,
                ..
            } => Some((*offset, *size)),
            _ => None,
        })
    }

    fn segment_mut(&mut self, name: &str) -> Option<&mut Segment> {
        self.loads.iter_mut().find_map(|load| match load {
            Load::Segment(segment) if segment.name == name => Some(segment),
            _ => None,
        })
    }

    pub(super) fn fileset_entry_ids(&self) -> Vec<String> {
        self.loads
            .iter()
            .filter_map(|load| match load {
                Load::FilesetEntry { entry_id, .. } => Some(entry_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// go-macho `GetFileSetFileByName`: the first entry whose ID equals `name` ignoring
    /// case or ends with it.
    pub(super) fn fileset_member(&self, data: &[u8], name: &str) -> Result<Self> {
        let lower = name.to_lowercase();
        self.loads
            .iter()
            .find_map(|load| match load {
                Load::FilesetEntry {
                    entry_id,
                    file_offset,
                    ..
                } if entry_id.eq_ignore_ascii_case(name)
                    || entry_id.to_lowercase().ends_with(&lower) =>
                {
                    Some(*file_offset)
                }
                _ => None,
            })
            .ok_or_else(|| Error::msg(format!("fileset does NOT contain {name}")))
            .and_then(|offset| Self::parse(data, offset))
    }

    pub(super) fn symtab(&self) -> Option<&Symtab> {
        self.loads.iter().find_map(|load| match load {
            Load::Symtab(symtab) => Some(symtab),
            _ => None,
        })
    }

    /// Replaces the symbol commands, in command order, with tables read elsewhere.
    pub(super) fn set_symbol_tables(&mut self, symtab: Symtab, dysymtab: Option<Dysymtab>) {
        let (mut symtab, mut dysymtab) = (Some(symtab), dysymtab);
        for load in &mut self.loads {
            match load {
                Load::Symtab(existing) => {
                    if let Some(table) = symtab.take() {
                        *existing = table;
                    }
                }
                Load::Dysymtab(existing) => {
                    if let Some(table) = dysymtab.take() {
                        *existing = table;
                    }
                }
                _ => {}
            }
        }
    }

    fn page_size(&self) -> u64 {
        if self.header.cpu == CPU_ARM64 || self.header.cpu == CPU_ARM64_32 {
            0x4000
        } else {
            0x1000
        }
    }

    pub(super) fn base_address(&self) -> u64 {
        self.segment("__TEXT").map_or(0, |segment| segment.address)
    }

    fn text_segment_first_section_relative_offset(&self) -> Result<u64> {
        let Some(text) = self.segment("__TEXT") else {
            return Ok(0);
        };
        for index in 0..text.section_count as usize {
            let section = self
                .sections
                .get(text.first_section + index)
                .ok_or_else(|| {
                    Error::msg(format!(
                        "__TEXT section index {} out of range",
                        text.first_section + index
                    ))
                })?;
            if section.offset == 0 {
                continue;
            }
            if section.address < text.address {
                return Err(Error::msg(format!(
                    "__TEXT section {} address {:#x} precedes segment address {:#x}",
                    section.name, section.address, text.address
                )));
            }
            return Ok(section.address - text.address);
        }
        Ok(0)
    }

    /// go-macho `File.Export` for an image inside a shared cache or kernel collection;
    /// returns the file it would write.
    pub(super) fn export(&mut self, source: &dyn Source, locals: &[Symbol]) -> Result<Vec<u8>> {
        if self.header.flags & MH_DYLIB_IN_CACHE == 0 {
            return Err(Error::msg("only images inside a cache can be exported"));
        }
        let page_size = self.page_size();
        let mut segment_map = SegmentMap::default();
        let mut next_offset = 0u64;
        for segment in self.segments() {
            let size = page_align(segment.file_size, page_size);
            segment_map.0.push(SegmentMapInfo {
                name: segment.name.clone(),
                old_start: segment.offset,
                new_start: next_offset,
                new_end: next_offset + size,
                original_memory_size: segment.memory_size,
                original_file_size: segment.file_size,
            });
            next_offset += size;
        }
        // Go's sort.Sort is an insertion sort below 12 elements, so equal keys keep their order.
        segment_map.0.sort_by_key(|info| info.old_start);

        let text_start = self.text_segment_first_section_relative_offset()?;
        self.optimize_load_commands(&segment_map)
            .context("failed to optimize load commands")?;
        let linkedit = self
            .optimize_linkedit(source, locals)
            .context("failed to optimize linkedit")?;
        let linkedit_size = self
            .segment("__LINKEDIT")
            .map_or(0, |segment| segment.file_size);
        if let Some(info) = segment_map
            .0
            .iter_mut()
            .find(|info| info.name == "__LINKEDIT")
        {
            info.new_end = info.new_start + linkedit_size;
            info.original_file_size = linkedit_size;
        }

        self.header.flags &= 0x7fff_ffff;
        if let Some(index) = self
            .loads
            .iter()
            .position(|load| load.command() == LC_DYLD_CHAINED_FIXUPS)
        {
            let removed = self.loads.remove(index);
            self.header.command_count -= 1;
            self.header.commands_size -= removed.size();
        }

        let mut output = Vec::with_capacity(
            usize::try_from(segment_map.0.last().map_or(0, |info| info.new_end)).unwrap_or(0),
        );
        self.write_header(&mut output);
        self.write_load_commands(&mut output)
            .context("failed to write load commands")?;
        let end_of_loads = output.len() as u64;

        for segment in self.segments() {
            let info = segment_map
                .lookup(&segment.name)
                .cloned()
                .unwrap_or_default();
            if segment.file_size > 0 {
                pad_to(&mut output, info.new_start)?;
                let read = || -> Result<Vec<u8>> {
                    let mut data = vec![0u8; to_usize(info.original_file_size)?];
                    source
                        .read_at_address(&mut data, segment.address)
                        .with_context(|| format!("failed to read segment {} data", segment.name))?;
                    Ok(data)
                };
                match segment.name.as_str() {
                    "__TEXT" => {
                        let data = read()?;
                        let data_start = text_start.max(end_of_loads);
                        if data_start > data.len() as u64 {
                            return Err(Error::msg(format!(
                                "__TEXT data start {data_start:#x} exceeds segment data length {:#x}",
                                data.len()
                            )));
                        }
                        pad_to(&mut output, end_of_loads + (data_start - end_of_loads))?;
                        output.extend_from_slice(&data[to_usize(data_start)?..]);
                    }
                    "__LINKEDIT" => output.extend_from_slice(&linkedit),
                    _ => output.extend_from_slice(&read()?),
                }
            }
            pad_to(&mut output, info.new_end)?;
        }
        Ok(output)
    }

    fn optimize_load_commands(&mut self, segment_map: &SegmentMap) -> Result<()> {
        let sections = &mut self.sections;
        for load in &mut self.loads {
            match load {
                Load::Segment(segment) => {
                    let info = segment_map.lookup(&segment.name).ok_or_else(|| {
                        Error::msg(format!(
                            "failed to remap offset in segment {}: failed to remap offset {:#x}",
                            segment.name, segment.offset
                        ))
                    })?;
                    let size = info.new_end - info.new_start;
                    segment.offset = info.new_start + segment.offset.wrapping_sub(info.old_start);
                    segment.file_size = size;
                    segment.memory_size = if info.original_memory_size > info.original_file_size {
                        size + (info.original_memory_size - info.original_file_size)
                    } else {
                        size
                    };
                    for index in 0..segment.section_count as usize {
                        let section = sections
                            .get_mut(segment.first_section + index)
                            .ok_or_else(|| Error::msg("segment section index is out of range"))?;
                        if section.offset != 0 {
                            section.offset = go_u32(
                                segment.offset + section.address.wrapping_sub(segment.address),
                            );
                        }
                    }
                }
                Load::LinkEdit {
                    command, offset, ..
                } if matches!(
                    *command,
                    LC_DYLIB_CODE_SIGN_DRS | LC_LINKER_OPTIMIZATION_HINT
                ) =>
                {
                    *offset = u32::try_from(
                        segment_map
                            .remap((*offset).into())
                            .with_context(|| format!("failed to remap offset in {command:#x}"))?,
                    )
                    .unwrap_or(u32::MAX);
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn optimize_linkedit(&mut self, source: &dyn Source, locals: &[Symbol]) -> Result<Vec<u8>> {
        let linkedit_offset = self
            .segment("__LINKEDIT")
            .ok_or_else(|| Error::msg("unable to find __LINKEDIT segment"))?
            .offset;
        if !self
            .loads
            .iter()
            .any(|load| matches!(load, Load::Dysymtab(_)))
        {
            return Err(Error::msg("binary has no LC_DYSYMTAB"));
        }
        let mut linkedit = Vec::new();
        let base_address = self.base_address();

        let mut exports = Vec::new();
        for kind in [
            LC_DYLD_EXPORTS_TRIE,
            LC_DATA_IN_CODE,
            LC_FUNCTION_STARTS,
            LC_FUNCTION_VARIANTS,
            LC_FUNCTION_VARIANT_FIXUPS,
        ] {
            let Some(Load::LinkEdit { offset, size, .. }) =
                self.loads.iter_mut().find(|load| load.command() == kind)
            else {
                continue;
            };
            let data = source
                .read_vec((*size).into(), (*offset).into())
                .with_context(|| format!("failed to read {kind:#x} data"))?;
            if kind == LC_DYLD_EXPORTS_TRIE {
                exports = parse_trie_exports(&data, base_address)
                    .context("failed to get LC_DYLD_EXPORTS_TRIE exports")?;
            }
            *offset = go_u32(linkedit_offset).wrapping_add(go_u32(linkedit.len() as u64));
            linkedit.extend_from_slice(&data);
            pointer_align(&mut linkedit);
        }

        let new_symbol_table_offset = linkedit.len() as u64;
        let mut local_symbols = Vec::new();
        let mut defined_symbols = Vec::new();
        let mut undefined_symbols = Vec::new();
        let classify = |symbol: &Symbol,
                        name: String,
                        local: &mut Vec<_>,
                        defined: &mut Vec<_>,
                        undefined: &mut Vec<_>| {
            let entry = (symbol.clone(), name);
            if !symbol.is_external() {
                local.push(entry);
            } else if symbol.is_undefined() {
                undefined.push(entry);
            } else {
                defined.push(entry);
            }
        };
        for symbol in locals {
            classify(
                symbol,
                symbol.name.clone(),
                &mut local_symbols,
                &mut defined_symbols,
                &mut undefined_symbols,
            );
        }
        if let Some(symtab) = self.symtab() {
            for symbol in &symtab.symbols {
                if symbol.name == "<redacted>" {
                    continue;
                }
                classify(
                    symbol,
                    symbol.name.clone(),
                    &mut local_symbols,
                    &mut defined_symbols,
                    &mut undefined_symbols,
                );
            }
        }
        for export in &exports {
            if !export.is_regular() || export.is_reexport() {
                defined_symbols.push((
                    Symbol {
                        name: export.name.clone(),
                        indirect_name: String::new(),
                        n_type: N_INDR | N_EXT,
                        section: 0,
                        desc: 0,
                        value: export.address,
                    },
                    format!("{}\0{}", export.name, export.reexport),
                ));
            }
        }
        let counts = [
            go_u32(local_symbols.len() as u64),
            go_u32(defined_symbols.len() as u64),
            go_u32(undefined_symbols.len() as u64),
        ];
        let mut names = vec![0u8];
        for (symbol, name) in local_symbols
            .iter()
            .chain(&defined_symbols)
            .chain(&undefined_symbols)
        {
            linkedit.extend_from_slice(&go_u32(names.len() as u64).to_le_bytes());
            linkedit.push(symbol.n_type);
            linkedit.push(symbol.section);
            linkedit.extend_from_slice(&symbol.desc.to_le_bytes());
            linkedit.extend_from_slice(&symbol.value.to_le_bytes());
            names.extend_from_slice(name.as_bytes());
            names.push(0);
        }
        pointer_align(&mut linkedit);

        let new_indirect_table_offset = linkedit.len() as u64;
        let local_count = go_u32(locals.len() as u64);
        let indirect_symbols = self.dysymtab_mut().map_or_else(Vec::new, |dysymtab| {
            if local_count > 0 {
                for symbol in &mut dysymtab.indirect_symbols {
                    if *symbol & INDIRECT_SYMBOL_LOCAL == 0 {
                        *symbol = symbol.wrapping_add(local_count);
                    }
                }
            }
            dysymtab.indirect_symbols.clone()
        });
        for symbol in &indirect_symbols {
            linkedit.extend_from_slice(&symbol.to_le_bytes());
        }
        pointer_align(&mut linkedit);

        let new_string_pool_offset = linkedit.len() as u64;
        while names.len() % 8 != 0 {
            names.push(0);
        }
        linkedit.extend_from_slice(&names);

        let symbol_offset = go_u32(linkedit_offset + new_symbol_table_offset);
        let mut has_symtab = false;
        for load in &mut self.loads {
            if let Load::Symtab(symtab) = load {
                symtab.symbol_count = counts.iter().sum();
                symtab.symbol_offset = symbol_offset;
                symtab.string_offset = go_u32(linkedit_offset + new_string_pool_offset);
                symtab.string_size = go_u32(names.len() as u64);
                has_symtab = true;
            }
        }
        if has_symtab
            && let Some(Load::LinkEdit {
                offset, size: 0, ..
            }) = self
                .loads
                .iter_mut()
                .find(|load| load.command() == LC_DATA_IN_CODE)
        {
            *offset = symbol_offset;
        }
        if let Some(dysymtab) = self.dysymtab_mut() {
            let fields = &mut dysymtab.fields;
            fields[Dysymtab::ILOCALSYM] = 0;
            fields[Dysymtab::NLOCALSYM] = counts[0];
            fields[Dysymtab::IEXTDEFSYM] = counts[0];
            fields[Dysymtab::NEXTDEFSYM] = counts[1];
            fields[Dysymtab::IUNDEFSYM] = counts[0] + counts[1];
            fields[Dysymtab::NUNDEFSYM] = counts[2];
            fields[Dysymtab::EXTRELOFF] = 0;
            fields[Dysymtab::NEXTREL] = 0;
            fields[Dysymtab::LOCRELOFF] = 0;
            fields[Dysymtab::NLOCREL] = 0;
            fields[Dysymtab::NINDIRECTSYMS] = go_u32(indirect_symbols.len() as u64);
            fields[Dysymtab::INDIRECTSYMOFF] = go_u32(linkedit_offset + new_indirect_table_offset);
        }
        let page_size = self.page_size();
        if let Some(segment) = self.segment_mut("__LINKEDIT") {
            segment.file_size = linkedit.len() as u64;
            segment.memory_size = page_align(segment.file_size, page_size);
        }
        Ok(linkedit)
    }

    fn dysymtab_mut(&mut self) -> Option<&mut Dysymtab> {
        self.loads.iter_mut().find_map(|load| match load {
            Load::Dysymtab(dysymtab) => Some(dysymtab),
            _ => None,
        })
    }

    fn write_header(&self, output: &mut Vec<u8>) {
        let header = &self.header;
        for field in [
            header.magic,
            header.cpu,
            header.sub_cpu,
            header.file_type,
            header.command_count,
            header.commands_size,
            header.flags,
            header.reserved,
        ] {
            output.extend_from_slice(&field.to_le_bytes());
        }
    }

    fn write_load_commands(&self, output: &mut Vec<u8>) -> Result<()> {
        for load in &self.loads {
            match load {
                Load::Segment(segment) => {
                    put32(output, segment.command);
                    put32(output, segment.length);
                    put_name(output, &segment.name);
                    for value in [
                        segment.address,
                        segment.memory_size,
                        segment.offset,
                        segment.file_size,
                    ] {
                        output.extend_from_slice(&value.to_le_bytes());
                    }
                    for value in [
                        segment.max_protection,
                        segment.protection,
                        segment.section_count,
                        segment.flags,
                    ] {
                        put32(output, value);
                    }
                    for index in 0..segment.section_count as usize {
                        let own = &self.sections[segment.first_section + index];
                        // go-macho writes each section found again by segment and name.
                        let section = self
                            .sections
                            .iter()
                            .find(|section| {
                                section.segment == own.segment && section.name == own.name
                            })
                            .unwrap_or(own);
                        put_name(output, &section.name);
                        put_name(output, &section.segment);
                        output.extend_from_slice(&section.address.to_le_bytes());
                        output.extend_from_slice(&section.size.to_le_bytes());
                        for value in [
                            section.offset,
                            section.align,
                            section.relocation_offset,
                            section.relocation_count,
                            section.flags,
                            section.reserved[0],
                            section.reserved[1],
                            section.reserved[2],
                        ] {
                            put32(output, value);
                        }
                    }
                }
                Load::Symtab(symtab) => {
                    for value in [
                        LC_SYMTAB,
                        symtab.length,
                        symtab.symbol_offset,
                        symtab.symbol_count,
                        symtab.string_offset,
                        symtab.string_size,
                    ] {
                        put32(output, value);
                    }
                }
                Load::Dysymtab(dysymtab) => {
                    put32(output, LC_DYSYMTAB);
                    put32(output, dysymtab.length);
                    for value in dysymtab.fields {
                        put32(output, value);
                    }
                }
                Load::LinkEdit {
                    command,
                    length,
                    offset,
                    size,
                } => {
                    for value in [*command, *length, *offset, *size] {
                        put32(output, value);
                    }
                }
                Load::Dylib { raw, name, flags } => {
                    let start = output.len();
                    output.extend_from_slice(&raw[..DYLIB_COMMAND_SIZE]);
                    if let Some(flags) = flags {
                        put32(output, *flags);
                    }
                    output.extend_from_slice(name.as_bytes());
                    output.push(0);
                    let length = le32(&raw[4..]) as usize;
                    let written = output.len() - start;
                    if length < written {
                        return Err(Error::msg(format!(
                            "{:#x} cmdsize {length} is smaller than the {written} bytes written",
                            le32(raw)
                        )));
                    }
                    output.resize(start + length, 0);
                }
                Load::FilesetEntry { raw, .. } | Load::Raw(raw) => output.extend_from_slice(raw),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct SegmentMapInfo {
    name: String,
    old_start: u64,
    new_start: u64,
    new_end: u64,
    original_memory_size: u64,
    original_file_size: u64,
}

#[derive(Default)]
struct SegmentMap(Vec<SegmentMapInfo>);

impl SegmentMap {
    fn lookup(&self, name: &str) -> Option<&SegmentMapInfo> {
        self.0.iter().find(|info| info.name == name)
    }

    fn remap(&self, offset: u64) -> Result<u64> {
        self.0
            .iter()
            .find(|info| {
                info.old_start <= offset && offset <= info.old_start + info.original_file_size
            })
            .map(|info| info.new_start + (offset - info.old_start))
            .ok_or_else(|| Error::msg(format!("failed to remap offset {offset:#x}")))
    }
}

pub(super) fn decode_symbol(entry: &[u8], name_at: impl Fn(u64) -> Option<String>) -> Symbol {
    let n_type = entry[4];
    let name = name_at(u64::from(le32(entry))).unwrap_or_default();
    let value = le64(&entry[8..]);
    let indirect_name = if n_type & N_TYPE == N_INDR {
        name_at(value).unwrap_or_default()
    } else {
        String::new()
    };
    Symbol {
        name: normalize_symbol_name(&name).to_owned(),
        indirect_name: normalize_symbol_name(&indirect_name).to_owned(),
        n_type,
        section: entry[5],
        desc: le16(&entry[6..]),
        value,
    }
}

pub(super) struct TrieExport {
    pub(super) name: String,
    reexport: String,
    pub(super) flags: u64,
    pub(super) address: u64,
}

impl TrieExport {
    fn is_regular(&self) -> bool {
        self.flags & EXPORT_SYMBOL_FLAGS_KIND_MASK == EXPORT_SYMBOL_FLAGS_KIND_REGULAR
    }

    pub(super) fn is_reexport(&self) -> bool {
        self.flags == EXPORT_SYMBOL_FLAGS_REEXPORT
    }
}

/// go-macho `trie.ParseTrieExports`: every terminal node in depth-first order.
pub(super) fn parse_trie_exports(data: &[u8], load_address: u64) -> Result<Vec<TrieExport>> {
    let mut nodes = Vec::new();
    parse_trie(data, 0, &mut Vec::new(), &mut nodes, 0).context("could not parse trie")?;
    nodes
        .into_iter()
        .map(|(offset, name)| {
            read_export(data, offset, name, load_address)
                .context("could not read trie export metadata")
        })
        .collect()
}

fn parse_trie(
    data: &[u8],
    position: u64,
    prefix: &mut Vec<u8>,
    nodes: &mut Vec<(u64, String)>,
    depth: usize,
) -> Result<()> {
    if depth > 4096 {
        return Err(Error::msg("trie is too deep"));
    }
    let mut cursor = position;
    let terminal_size =
        read_uleb128(data, &mut cursor).context("could not parse ULEB128 terminalSize value")?;
    if terminal_size != 0 {
        nodes.push((cursor, String::from_utf8_lossy(prefix).into_owned()));
    }
    let mut cursor = position.wrapping_add(terminal_size).wrapping_add(1);
    let children = *usize::try_from(cursor)
        .ok()
        .and_then(|index| data.get(index))
        .ok_or_else(|| Error::msg("could not read childrenRemaining value: EOF"))?;
    cursor += 1;
    for _ in 0..children {
        let length = prefix.len();
        while let Some(&byte) = usize::try_from(cursor)
            .ok()
            .and_then(|index| data.get(index))
        {
            cursor += 1;
            if byte == 0 {
                break;
            }
            prefix.push(byte);
        }
        let child = read_uleb128(data, &mut cursor)
            .context("could not parse ULEB128 childNodeOffset value")?;
        parse_trie(data, child, prefix, nodes, depth + 1)
            .context("could not parse trie (recursive call)")?;
        prefix.truncate(length);
    }
    Ok(())
}

fn read_export(
    data: &[u8],
    mut cursor: u64,
    name: String,
    load_address: u64,
) -> Result<TrieExport> {
    let flags =
        read_uleb128(data, &mut cursor).context("could not parse ULEB128 symbol flag value")?;
    let mut reexport = Vec::new();
    if flags == EXPORT_SYMBOL_FLAGS_REEXPORT {
        read_uleb128(data, &mut cursor)
            .context("could not parse ULEB128 symbol other reexport value")?;
        while let Some(&byte) = usize::try_from(cursor)
            .ok()
            .and_then(|index| data.get(index))
        {
            cursor += 1;
            if byte == 0 {
                break;
            }
            reexport.push(byte);
        }
    } else if flags == EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER {
        read_uleb128(data, &mut cursor)
            .context("could not parse ULEB128 symbol other stub-n-resolver value")?;
    }
    let mut address =
        read_uleb128(data, &mut cursor).context("could not parse ULEB128 symbol value")?;
    let kind = flags & EXPORT_SYMBOL_FLAGS_KIND_MASK;
    if (kind == EXPORT_SYMBOL_FLAGS_KIND_REGULAR || kind == EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL)
        && flags != EXPORT_SYMBOL_FLAGS_REEXPORT
    {
        address = address.wrapping_add(load_address);
    }
    Ok(TrieExport {
        name,
        reexport: String::from_utf8_lossy(&reexport).into_owned(),
        flags,
        address,
    })
}

fn read_uleb128(data: &[u8], cursor: &mut u64) -> Result<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *usize::try_from(*cursor)
            .ok()
            .and_then(|index| data.get(index))
            .ok_or_else(|| Error::msg("EOF"))?;
        *cursor += 1;
        if shift < 64 {
            result |= u64::from(byte & 0x7f) << shift;
        }
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

fn slice(data: &[u8], offset: u64, size: u64) -> Result<&[u8]> {
    usize::try_from(offset)
        .ok()
        .zip(usize::try_from(size).ok())
        .and_then(|(offset, size)| data.get(offset..offset.checked_add(size)?))
        .ok_or_else(|| Error::msg("unexpected EOF"))
}

fn page_align(offset: u64, align: u64) -> u64 {
    offset.next_multiple_of(align)
}

fn pointer_align(data: &mut Vec<u8>) {
    while data.len() % 8 != 0 {
        data.push(0);
    }
}

fn pad_to(output: &mut Vec<u8>, length: u64) -> Result<()> {
    let length = to_usize(length)?;
    if output.len() < length {
        output.resize(length, 0);
    }
    Ok(())
}

fn put32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_name(output: &mut Vec<u8>, name: &str) {
    let mut field = [0u8; 16];
    let bytes = name.as_bytes();
    let count = bytes.len().min(16);
    field[..count].copy_from_slice(&bytes[..count]);
    output.extend_from_slice(&field);
}

/// Go's `uint32(value)`: the low 32 bits.
fn go_u32(value: u64) -> u32 {
    let [a, b, c, d, ..] = value.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

fn to_usize(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::msg("Mach-O size overflows"))
}
