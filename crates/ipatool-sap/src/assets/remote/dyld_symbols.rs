//! Port of Go `assets/dyld_symbols.go`: an image's symbol tables without the cache-wide
//! string table. Export needs only this image's nlists and the strings they reference.

use super::ReadAt;
use super::dyld::{Cache, DYLD_MAXIMUM_LOAD_COMMANDS_SIZE, Image, read_cstring, valid_range};
use super::le32;
use super::macho::{self, Dysymtab, Symtab};
use crate::error::{Context, Error, Result};

const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const MAXIMUM_SYMBOLS: u32 = 4 << 20;

impl Cache<'_> {
    pub(super) fn image_symbols(&self, image: &Image, file: &mut macho::File) -> Result<()> {
        let member = &self.members[image.member];
        let mut header = [0u8; 32];
        member
            .file
            .read_at(&mut header, image.offset)
            .context("read image header")?;
        let size = u64::from(le32(&header[20..]));
        if size > DYLD_MAXIMUM_LOAD_COMMANDS_SIZE {
            return Err(Error::msg("invalid image load command size"));
        }
        let mut commands = vec![0u8; usize::try_from(size).unwrap_or(0)];
        member
            .file
            .read_at(&mut commands, image.offset + 32)
            .context("read image commands")?;
        let linkedit = &self.members[self.linkedit_member(image)?];

        let mut symtab = None;
        let mut dysymtab = None;
        let mut offset = 0;
        while offset < commands.len() {
            if commands.len() - offset < 8 {
                return Err(Error::msg("truncated image command"));
            }
            let length = le32(&commands[offset + 4..]) as usize;
            if length < 8 || length > commands.len() - offset {
                return Err(Error::msg("invalid image command size"));
            }
            let raw = &commands[offset..offset + length];
            offset += length;
            match le32(raw) {
                LC_SYMTAB => {
                    symtab = Some(read_image_symbols(
                        linkedit.file.as_ref(),
                        linkedit.size,
                        raw,
                    )?);
                }
                LC_DYSYMTAB => {
                    let mut table = Dysymtab::parse(raw)
                        .map_err(|_| Error::msg("decode dynamic symbols: unexpected EOF"))?;
                    let undefined_end = u64::from(table.fields[Dysymtab::IUNDEFSYM])
                        + u64::from(table.fields[Dysymtab::NUNDEFSYM]);
                    let count = table.fields[Dysymtab::NINDIRECTSYMS];
                    let Some(symbols) = symtab.as_ref() else {
                        return Err(Error::msg("invalid dynamic symbol table"));
                    };
                    if undefined_end > symbols.symbols.len() as u64 || count > MAXIMUM_SYMBOLS {
                        return Err(Error::msg("invalid dynamic symbol table"));
                    }
                    let indirect_offset = u64::from(table.fields[Dysymtab::INDIRECTSYMOFF]);
                    if !valid_range(linkedit.size, indirect_offset, u64::from(count) * 4) {
                        return Err(Error::msg("indirect symbols exceed cache member"));
                    }
                    let mut encoded = vec![0u8; count as usize * 4];
                    linkedit
                        .file
                        .read_at(&mut encoded, indirect_offset)
                        .context("read indirect symbols")?;
                    table.indirect_symbols = super::le32_words(&encoded);
                    dysymtab = Some(table);
                }
                _ => {}
            }
        }
        if let Some(symtab) = symtab {
            file.set_symbol_tables(symtab, dysymtab);
        }
        Ok(())
    }
}

/// Decodes just the referenced names, keeping go-macho's symbol name normalization.
pub(super) fn read_image_symbols(file: &dyn ReadAt, size: u64, raw: &[u8]) -> Result<Symtab> {
    if raw.len() < 24 {
        return Err(Error::msg("decode symbols: unexpected EOF"));
    }
    let mut table = Symtab::from_command(raw);
    if table.symbol_count > MAXIMUM_SYMBOLS
        || !valid_range(
            size,
            table.symbol_offset.into(),
            u64::from(table.symbol_count) * 16,
        )
        || !valid_range(size, table.string_offset.into(), table.string_size.into())
    {
        return Err(Error::msg("invalid image symbol table"));
    }
    let mut encoded = vec![0u8; table.symbol_count as usize * 16];
    file.read_at(&mut encoded, table.symbol_offset.into())
        .context("read image symbols")?;
    let strings_end = u64::from(table.string_offset) + u64::from(table.string_size);
    let read_name = |offset: u64| -> Result<String> {
        if offset >= u64::from(table.string_size) {
            return Ok(String::new());
        }
        let name = read_cstring(file, strings_end, u64::from(table.string_offset) + offset)?;
        Ok(macho::normalize_symbol_name(&name).to_owned())
    };
    let mut symbols = Vec::with_capacity(table.symbol_count as usize);
    for entry in encoded.as_chunks::<16>().0 {
        let name = read_name(u64::from(le32(entry)))?;
        let mut symbol = macho::decode_symbol(entry, |_| None);
        symbol.name = name;
        if symbol.is_indirect() {
            symbol.indirect_name = read_name(symbol.value)?;
        }
        symbols.push(symbol);
    }
    table.symbols = symbols;
    Ok(table)
}
