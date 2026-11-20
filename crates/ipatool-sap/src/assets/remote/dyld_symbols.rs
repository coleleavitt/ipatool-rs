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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A large sparse cache member that records the largest requested read.
    struct SparseSymbols {
        data: Vec<u8>,
        largest: AtomicUsize,
    }

    impl ReadAt for SparseSymbols {
        fn read_at(&self, output: &mut [u8], offset: u64) -> Result<()> {
            self.largest.fetch_max(output.len(), Ordering::Relaxed);
            output.fill(0);
            let start = usize::try_from(offset).unwrap();
            if start < self.data.len() {
                let count = output.len().min(self.data.len() - start);
                output[..count].copy_from_slice(&self.data[start..start + count]);
            }
            Ok(())
        }
    }

    fn nlist(name: u32, n_type: u8, value: u64) -> Vec<u8> {
        let mut entry = name.to_le_bytes().to_vec();
        entry.extend_from_slice(&[n_type, 0, 0, 0]);
        entry.extend_from_slice(&value.to_le_bytes());
        entry
    }

    fn symtab_command(symbol_offset: u32, count: u32, string_offset: u32, size: u32) -> Vec<u8> {
        [LC_SYMTAB, 24, symbol_offset, count, string_offset, size]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn reads_only_referenced_strings_from_a_gigabyte_sized_string_table() {
        let mut data = vec![0u8; 2048];
        data[1024..1024 + 39].copy_from_slice(b"\0_regular\0_Go.symbol\0_target.symbol\0\0\0\0");
        let mut table = nlist(1, 0x0f, 0x1_8000_1000);
        table.extend_from_slice(&nlist(10, 0x0b, 21));
        data[128..128 + table.len()].copy_from_slice(&table);
        let source = SparseSymbols {
            data,
            largest: AtomicUsize::new(0),
        };
        let result =
            read_image_symbols(&source, 2 << 30, &symtab_command(128, 2, 1024, 1 << 30)).unwrap();
        assert_eq!(result.symbols.len(), 2);
        assert_eq!(result.symbols[0].name, "_regular");
        assert_eq!(result.symbols[1].name, "Go.symbol");
        assert_eq!(result.symbols[1].indirect_name, "target.symbol");
        assert!(source.largest.load(Ordering::Relaxed) <= 4096);
    }

    #[test]
    fn rejects_symbol_data_extending_beyond_the_cache_member() {
        let source = SparseSymbols {
            data: vec![0u8; 256],
            largest: AtomicUsize::new(0),
        };
        let Err(error) = read_image_symbols(&source, 256, &symtab_command(128, 10, 256, 128))
        else {
            panic!("symbols beyond the member were accepted");
        };
        assert!(
            error.to_string().contains("invalid image symbol table"),
            "{error}"
        );
    }
}
