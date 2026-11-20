//! The parts of `blacktop/go-macho` v1.1.282 that `machimage` relies on, with its quirks:
//! symbol names are matched case-insensitively, the export trie is read only from
//! `LC_DYLD_INFO(_ONLY)`, and bind/rebase opcodes are decoded the same way.

use object::LittleEndian as LE;
use object::macho::{self, LoadCommandType};
use object::read::macho::{MachHeader, Section as _, Segment as _};

use crate::error::{Error, Result};

const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const POINTER_SIZE: u64 = 8;

/// One `nlist_64` entry, as Go's `macho.Symbol`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub indirect_name: String,
    pub n_type: u8,
    pub sect: u8,
    pub desc: u16,
    pub value: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Segment {
    pub name: String,
    pub address: u64,
    pub size: u64,
    pub file_off: u64,
    pub file_size: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Section {
    pub name: String,
    pub address: u64,
    pub size: u64,
    pub offset: u32,
    pub flags: u32,
}

impl Section {
    pub fn has_instructions(&self) -> bool {
        self.flags & (macho::S_ATTR_PURE_INSTRUCTIONS.0 | macho::S_ATTR_SOME_INSTRUCTIONS.0) != 0
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DyldInfo {
    rebase: (u32, u32),
    bind: (u32, u32),
    weak_bind: (u32, u32),
    lazy_bind: (u32, u32),
    export: (u32, u32),
}

/// Go's `types.Bind`, reduced to the fields `machimage` reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bind {
    pub name: String,
    pub typ: u8,
    pub segment: String,
    pub seg_offset: u64,
    pub addend: i64,
}

/// Go's `types.Rebase`, reduced to the fields `machimage` reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rebase {
    pub typ: u8,
    pub segment: String,
    pub offset: u64,
    pub value: u64,
}

pub(super) struct File {
    pub cpu: u32,
    pub segments: Vec<Segment>,
    pub sections: Vec<Section>,
    pub symtab: Option<Vec<Symbol>>,
    pub dyld_info: Option<DyldInfo>,
    pub chained_fixups: Option<(u32, u32)>,
}

impl File {
    pub fn parse(data: &[u8], with_symtab: bool) -> Result<Self> {
        let endian = LE;
        let header = macho::MachHeader64::<LE>::parse(data, 0).map_err(object_error)?;
        let mut commands = header
            .load_commands(endian, data, 0)
            .map_err(object_error)?;
        let mut file = File {
            cpu: header.cputype(endian).0,
            segments: Vec::new(),
            sections: Vec::new(),
            symtab: None,
            dyld_info: None,
            chained_fixups: None,
        };
        let mut dyld_info = None;
        let mut dyld_info_only = None;
        while let Some(command) = commands.next().map_err(object_error)? {
            if let Some((segment, section_data)) = command.segment_64().map_err(object_error)? {
                file.segments.push(Segment {
                    name: c_name(segment.name()),
                    address: segment.vmaddr(endian),
                    size: segment.vmsize(endian),
                    file_off: segment.fileoff(endian),
                    file_size: segment.filesize(endian),
                });
                for section in segment
                    .sections(endian, section_data)
                    .map_err(object_error)?
                {
                    file.sections.push(Section {
                        name: c_name(section.name()),
                        address: section.addr(endian),
                        size: section.size(endian),
                        offset: section.offset(endian),
                        flags: section.flags(endian).0,
                    });
                }
            } else if let Some(symtab) = command.symtab().map_err(object_error)? {
                if with_symtab {
                    file.symtab = Some(parse_symtab(
                        data,
                        symtab.symoff.get(endian),
                        symtab.nsyms.get(endian),
                        symtab.stroff.get(endian),
                        symtab.strsize.get(endian),
                    )?);
                }
            } else if let Some(info) = command.dyld_info().map_err(object_error)? {
                let info = DyldInfo {
                    rebase: (info.rebase_off.get(endian), info.rebase_size.get(endian)),
                    bind: (info.bind_off.get(endian), info.bind_size.get(endian)),
                    weak_bind: (
                        info.weak_bind_off.get(endian),
                        info.weak_bind_size.get(endian),
                    ),
                    lazy_bind: (
                        info.lazy_bind_off.get(endian),
                        info.lazy_bind_size.get(endian),
                    ),
                    export: (info.export_off.get(endian), info.export_size.get(endian)),
                };
                let slot = if command.cmd() == macho::LC_DYLD_INFO {
                    &mut dyld_info
                } else {
                    &mut dyld_info_only
                };
                slot.get_or_insert(info);
            } else if command.cmd() == LoadCommandType(macho::LC_DYLD_CHAINED_FIXUPS.0)
                && file.chained_fixups.is_none()
            {
                let linkedit = command
                    .data::<macho::LinkeditDataCommand<LE>>()
                    .map_err(object_error)?;
                file.chained_fixups =
                    Some((linkedit.dataoff.get(endian), linkedit.datasize.get(endian)));
            }
        }
        file.dyld_info = dyld_info.or(dyld_info_only);
        Ok(file)
    }

    pub fn base_address(&self) -> u64 {
        self.segment("__TEXT").map_or(0, |segment| segment.address)
    }

    pub fn segment(&self, name: &str) -> Option<&Segment> {
        self.segments.iter().find(|segment| segment.name == name)
    }
}

pub(super) fn cpu_name(cpu: u32) -> String {
    let name = match cpu {
        1 => "VAX",
        6 => "MC680x0",
        7 => "i386",
        0x0100_0007 => "Amd64",
        8 => "MIPS",
        10 => "MC98000",
        11 => "HPPA",
        12 => "ARM",
        CPU_TYPE_ARM64 => "AARCH64",
        0x0200_000c => "ARM64_32",
        13 => "MC88000",
        14 => "SPARC",
        15 => "i860",
        18 => "PowerPC",
        0x0100_0012 => "PowerPC 64",
        _ => return cpu.to_string(),
    };
    name.to_owned()
}

pub(super) const fn is_arm64(cpu: u32) -> bool {
    cpu == CPU_TYPE_ARM64
}

fn object_error(error: object::read::Error) -> Error {
    Error::msg(error.to_string())
}

fn c_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn cstring(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// Go strips the leading underscore it adds to Go symbol names (golang/go#33808).
fn go_symbol_name(name: String) -> String {
    if name.contains('.') && name.starts_with('_') {
        name[1..].to_owned()
    } else {
        name
    }
}

fn slice(data: &[u8], offset: u64, size: u64) -> Result<&[u8]> {
    usize::try_from(offset)
        .ok()
        .zip(usize::try_from(size).ok())
        .and_then(|(start, size)| data.get(start..start.checked_add(size)?))
        .ok_or_else(|| Error::msg("unexpected EOF"))
}

fn parse_symtab(
    data: &[u8],
    symoff: u32,
    nsyms: u32,
    stroff: u32,
    strsize: u32,
) -> Result<Vec<Symbol>> {
    const NLIST_SIZE: u64 = 16;
    let strtab = slice(data, stroff.into(), strsize.into())?;
    let entries = slice(data, symoff.into(), u64::from(nsyms) * NLIST_SIZE)
        .map_err(|_| Error::msg("failed to read Symtab magic: unexpected EOF"))?;
    let mut symbols = Vec::with_capacity(entries.len() / 16);
    for entry in entries.as_chunks::<16>().0 {
        let strx = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        let n_type = entry[4];
        let value = u64::from_le_bytes([
            entry[8], entry[9], entry[10], entry[11], entry[12], entry[13], entry[14], entry[15],
        ]);
        let name = strtab
            .get(strx as usize..)
            .map_or_else(String::new, |bytes| go_symbol_name(cstring(bytes)));
        let indirect_name = if n_type & macho::N_TYPE == macho::N_INDR.0 {
            usize::try_from(value)
                .ok()
                .and_then(|offset| strtab.get(offset..))
                .map_or_else(String::new, |bytes| go_symbol_name(cstring(bytes)))
        } else {
            String::new()
        };
        symbols.push(Symbol {
            name,
            indirect_name,
            n_type,
            sect: entry[5],
            desc: u16::from_le_bytes([entry[6], entry[7]]),
            value,
        });
    }
    Ok(symbols)
}

/// Go's `strings.EqualFold`.
pub(super) fn equal_fold(left: &str, right: &str) -> bool {
    let mut left = left.chars();
    let mut right = right.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a == b || a.to_lowercase().eq(b.to_lowercase()) => {}
            _ => return false,
        }
    }
}

/// A byte reader over opcode streams, with Go's `bytes.Reader` end-of-input errors.
struct Cursor<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn uleb(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte().ok_or_else(|| Error::msg("EOF"))?;
            result |= u64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift = shift.saturating_add(7);
        }
    }

    fn sleb(&mut self) -> Result<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte().ok_or_else(|| Error::msg("EOF"))?;
            result |= i64::from(byte & 0x7f).checked_shl(shift).unwrap_or(0);
            shift = shift.saturating_add(7);
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }

    // Go's readString appends each byte as a rune.
    fn string(&mut self) -> Result<String> {
        let mut text = String::new();
        loop {
            match self.byte() {
                None => return Err(Error::msg("EOF")),
                Some(0) => return Ok(text),
                Some(byte) => text.push(char::from(byte)),
            }
        }
    }

    fn bytes_until_nul(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        while let Some(byte) = self.byte() {
            if byte == 0 {
                break;
            }
            bytes.push(byte);
        }
        bytes
    }
}

const BIND_KIND: u8 = 0;
const LAZY_KIND: u8 = 2;

/// Go's `File.GetBindInfo`; `None` is `ErrMachODyldInfoNotFound`.
pub(super) fn binds(
    file: &File,
    read_pointer: &dyn Fn(u64) -> Result<u64>,
    data: &[u8],
) -> Option<Result<Vec<Bind>>> {
    let info = file.dyld_info?;
    let run = || {
        let mut binds = Vec::new();
        for ((offset, size), kind) in [
            (info.bind, BIND_KIND),
            (info.weak_bind, 1),
            (info.lazy_bind, LAZY_KIND),
        ] {
            if size == 0 {
                continue;
            }
            let stream = slice(data, offset.into(), size.into()).map_err(|_| {
                Error::msg(format!(
                    "failed to read {} info: unexpected EOF",
                    bind_label(kind)
                ))
            })?;
            binds.extend(parse_binds(file, read_pointer, stream, kind)?);
        }
        Ok(binds)
    };
    Some(run())
}

fn bind_label(kind: u8) -> &'static str {
    match kind {
        BIND_KIND => "bind",
        LAZY_KIND => "lazy bind",
        _ => "weak bind",
    }
}

fn segment_at(file: &File, index: u8) -> Result<&Segment> {
    file.segments
        .get(usize::from(index))
        .ok_or_else(|| Error::msg(format!("segment index {index} out of range")))
}

fn parse_binds(
    file: &File,
    read_pointer: &dyn Fn(u64) -> Result<u64>,
    stream: &[u8],
    kind: u8,
) -> Result<Vec<Bind>> {
    let mut reader = Cursor::new(stream);
    let mut binds = Vec::new();
    let mut threaded = ThreadedTable::default();
    let mut seg_offset = 0u64;
    let mut bind = Bind::default();

    while let Some(byte) = reader.byte() {
        let immediate = byte & 0x0f;
        match byte & 0xf0 {
            0x00 => {
                if kind != LAZY_KIND {
                    return Ok(binds);
                }
                bind = Bind::default();
            }
            0x10 | 0x30 => {}
            0x20 => {
                reader.uleb()?;
            }
            0x40 => reader
                .string()?
                .trim_matches('\0')
                .clone_into(&mut bind.name),
            0x50 => bind.typ = immediate,
            0x60 => bind.addend = reader.sleb()?,
            0x70 => {
                seg_offset = reader.uleb()?;
                let segment = segment_at(file, immediate)?;
                bind.segment.clone_from(&segment.name);
            }
            0x80 => seg_offset = seg_offset.wrapping_add(reader.uleb()?),
            0x90 if threaded.enabled => threaded.entries.push(bind.clone()),
            0x90 | 0xa0 | 0xb0 | 0xc0 => {
                let (count, skip) = match byte & 0xf0 {
                    0x90 => (1, 0),
                    0xa0 => (1, reader.uleb()?),
                    0xb0 => (1, u64::from(immediate) * POINTER_SIZE),
                    _ => (reader.uleb()?, reader.uleb()?),
                };
                for _ in 0..count {
                    bind.seg_offset = seg_offset;
                    binds.push(bind.clone());
                    seg_offset = seg_offset.wrapping_add(skip).wrapping_add(POINTER_SIZE);
                }
            }
            0xd0 if immediate == 0x00 => {
                threaded.size = reader.uleb()?;
                threaded.enabled = true;
            }
            0xd0 if immediate == 0x01 => {
                let segment_file_off = file
                    .segment(&bind.segment)
                    .map_or(0, |segment| segment.file_off);
                seg_offset =
                    threaded.apply(read_pointer, segment_file_off, seg_offset, &mut binds)?;
            }
            0xd0 => {
                return Err(Error::msg(format!(
                    "bad threaded bind subopcode {immediate:#x}"
                )));
            }
            opcode => return Err(Error::msg(format!("bad bind opcode {opcode:#x}"))),
        }
    }
    Ok(binds)
}

#[derive(Default)]
struct ThreadedTable {
    enabled: bool,
    entries: Vec<Bind>,
    size: u64,
}

impl ThreadedTable {
    /// Walks a `BIND_SUBOPCODE_THREADED_APPLY` chain; returns the offset after it.
    fn apply(
        &mut self,
        read_pointer: &dyn Fn(u64) -> Result<u64>,
        segment_file_off: u64,
        mut seg_offset: u64,
        binds: &mut Vec<Bind>,
    ) -> Result<u64> {
        loop {
            let mut pointer = read_pointer(segment_file_off.wrapping_add(seg_offset))
                .map_err(|error| Error::msg(format!("failed to read pointer: {error}")))?;
            if pointer & (1 << 62) != 0 {
                let ordinal = pointer & 0xffff;
                let entry = usize::try_from(ordinal)
                    .ok()
                    .filter(|_| ordinal <= self.size)
                    .and_then(|ordinal| self.entries.get_mut(ordinal))
                    .ok_or_else(|| Error::msg("bind ordinal is out of range"))?;
                entry.seg_offset = seg_offset;
                binds.push(entry.clone());
            }
            pointer &= !(1u64 << 62);
            let delta = (pointer & 0x3ff8_0000_0000_0000) >> 51;
            seg_offset = seg_offset.wrapping_add(delta * POINTER_SIZE);
            if delta == 0 {
                return Ok(seg_offset);
            }
        }
    }
}

/// Go's `File.GetRebaseInfo`; `None` is `ErrMachODyldInfoNotFound`.
pub(super) fn rebases(
    file: &File,
    read_pointer: &dyn Fn(u64) -> Result<u64>,
    data: &[u8],
) -> Option<Result<Vec<Rebase>>> {
    let (offset, size) = file.dyld_info?.rebase;
    if size == 0 {
        return Some(Ok(Vec::new()));
    }
    Some(
        slice(data, offset.into(), size.into())
            .map_err(|_| Error::msg("failed to read rebase info: unexpected EOF"))
            .and_then(|stream| parse_rebases(file, read_pointer, stream)),
    )
}

fn parse_rebases(
    file: &File,
    read_pointer: &dyn Fn(u64) -> Result<u64>,
    stream: &[u8],
) -> Result<Vec<Rebase>> {
    let mut reader = Cursor::new(stream);
    let mut rebases = Vec::new();
    let mut rebase = Rebase::default();
    let push = |rebase: &mut Rebase, rebases: &mut Vec<Rebase>| -> Result<()> {
        let segment_off = file
            .segment(&rebase.segment)
            .map_or(0, |segment| segment.file_off);
        rebase.value = read_pointer(segment_off.wrapping_add(rebase.offset))
            .map_err(|error| Error::msg(format!("failed to read pointer: {error}")))?;
        rebases.push(rebase.clone());
        Ok(())
    };

    while let Some(byte) = reader.byte() {
        let immediate = byte & 0x0f;
        match byte & 0xf0 {
            0x00 => return Ok(rebases),
            0x10 => rebase.typ = immediate,
            0x20 => {
                rebase
                    .segment
                    .clone_from(&segment_at(file, immediate)?.name);
                rebase.offset = reader.uleb()?;
            }
            0x30 => rebase.offset = rebase.offset.wrapping_add(reader.uleb()?),
            0x40 => {
                rebase.offset = rebase
                    .offset
                    .wrapping_add(u64::from(immediate) * POINTER_SIZE);
            }
            0x50 => {
                for _ in 0..immediate {
                    push(&mut rebase, &mut rebases)?;
                    rebase.offset = rebase.offset.wrapping_add(POINTER_SIZE);
                }
            }
            0x60 => {
                let count = reader.uleb()?;
                for _ in 0..count {
                    push(&mut rebase, &mut rebases)?;
                    rebase.offset = rebase.offset.wrapping_add(POINTER_SIZE);
                }
            }
            0x70 => {
                push(&mut rebase, &mut rebases)?;
                let skip = reader.uleb()?;
                rebase.offset = rebase.offset.wrapping_add(skip).wrapping_add(POINTER_SIZE);
            }
            0x80 => {
                let count = reader.uleb()?;
                let skip = reader.uleb()?;
                for _ in 0..count {
                    push(&mut rebase, &mut rebases)?;
                    rebase.offset = rebase.offset.wrapping_add(skip).wrapping_add(POINTER_SIZE);
                }
            }
            opcode => return Err(Error::msg(format!("bad rebase opcode {opcode:#x}"))),
        }
    }
    Ok(rebases)
}

/// Go's `File.FindSymbolAddress`.
pub(super) fn find_symbol_address(
    symtab: Option<&[Symbol]>,
    dyld_info: Option<DyldInfo>,
    base_address: u64,
    data: &[u8],
    name: &str,
) -> Result<u64> {
    let Some(symtab) = symtab else {
        return Err(Error::msg("missing symbol table in record at byte 0x0"));
    };
    if let Some(symbol) = symtab.iter().find(|symbol| equal_fold(&symbol.name, name)) {
        return Ok(symbol.value);
    }
    if let Some(info) = dyld_info
        && info.export.1 > 0
    {
        let exports = slice(data, info.export.0.into(), info.export.1.into())
            .map_err(|_| Error::msg("failed to read bind info: unexpected EOF"))
            .and_then(|trie| parse_trie_exports(trie, base_address))
            .map_err(|error| Error::msg(format!("failed to get exports: {error}")))?;
        if let Some((_, address)) = exports.iter().find(|(export, _)| equal_fold(export, name)) {
            return Ok(*address);
        }
    }
    Err(Error::msg("symbol not found in macho symtab"))
}

const EXPORT_KIND_MASK: u64 = 0x03;
const EXPORT_KIND_REGULAR: u64 = 0x00;
const EXPORT_KIND_THREAD_LOCAL: u64 = 0x01;
const EXPORT_REEXPORT: u64 = 0x08;
const EXPORT_STUB_AND_RESOLVER: u64 = 0x10;

/// Go's `trie.ParseTrieExports`: names and addresses, in trie order.
fn parse_trie_exports(trie: &[u8], load_address: u64) -> Result<Vec<(String, u64)>> {
    let mut nodes = Vec::new();
    parse_trie(trie, 0, &mut Vec::new(), &mut nodes)
        .map_err(|error| Error::msg(format!("could not parse trie: {error}")))?;
    nodes
        .into_iter()
        .map(|(offset, name)| {
            read_export(trie, offset, load_address)
                .map(|address| (String::from_utf8_lossy(&name).into_owned(), address))
                .map_err(|error| {
                    Error::msg(format!("could not read trie export metadata: {error}"))
                })
        })
        .collect()
}

fn read_export(trie: &[u8], offset: usize, load_address: u64) -> Result<u64> {
    let mut reader = Cursor {
        data: trie,
        position: offset,
    };
    let flags = reader.uleb().map_err(|error| {
        Error::msg(format!(
            "could not parse ULEB128 symbol flag value: {error}"
        ))
    })?;
    if flags == EXPORT_REEXPORT {
        reader.uleb().map_err(|error| {
            Error::msg(format!(
                "could not parse ULEB128 symbol other reexport value: {error}"
            ))
        })?;
        reader.bytes_until_nul();
    } else if flags == EXPORT_STUB_AND_RESOLVER {
        reader.uleb().map_err(|error| {
            Error::msg(format!(
                "could not parse ULEB128 symbol other stub-n-resolver value: {error}"
            ))
        })?;
    }
    let mut value = reader
        .uleb()
        .map_err(|error| Error::msg(format!("could not parse ULEB128 symbol value: {error}")))?;
    let kind = flags & EXPORT_KIND_MASK;
    if (kind == EXPORT_KIND_REGULAR || kind == EXPORT_KIND_THREAD_LOCAL) && flags != EXPORT_REEXPORT
    {
        value = value.wrapping_add(load_address);
    }
    Ok(value)
}

// Go skips `terminalSize + 1` bytes, which assumes a one-byte terminal size.
fn parse_trie(
    trie: &[u8],
    position: u64,
    prefix: &mut Vec<u8>,
    nodes: &mut Vec<(usize, Vec<u8>)>,
) -> Result<()> {
    let start = usize::try_from(position).unwrap_or(usize::MAX);
    let mut reader = Cursor {
        data: trie,
        position: start,
    };
    let terminal_size = reader.uleb().map_err(|error| {
        Error::msg(format!(
            "could not parse ULEB128 terminalSize value: {error}"
        ))
    })?;
    if terminal_size != 0 {
        nodes.push((reader.position, prefix.clone()));
    }
    reader.position =
        usize::try_from(position.wrapping_add(terminal_size).wrapping_add(1)).unwrap_or(usize::MAX);
    let children = reader
        .byte()
        .ok_or_else(|| Error::msg("could not read childrenRemaining value: EOF"))?;
    for _ in 0..children {
        let edge = reader.bytes_until_nul();
        let child = reader.uleb().map_err(|error| {
            Error::msg(format!(
                "could not parse ULEB128 childNodeOffset value: {error}"
            ))
        })?;
        let length = prefix.len();
        prefix.extend_from_slice(&edge);
        let result = parse_trie(trie, child, prefix, nodes);
        prefix.truncate(length);
        result.map_err(|error| {
            Error::msg(format!("could not parse trie (recursive call): {error}"))
        })?;
    }
    Ok(())
}
