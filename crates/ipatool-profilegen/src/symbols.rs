//! Port of Go `profilegen/symbols.go`: private symbols such as value witnesses, from the
//! shared string table, loaded per image on first use.

use crate::error::{Error, Result};
use crate::source::{Source, le32, le64};

impl Source<'_> {
    pub(crate) fn symbol(&self, address: u64) -> Result<String> {
        if let Some(name) = self.symbols.borrow().get(&address)
            && !name.is_empty()
        {
            return Ok(name.clone());
        }
        for (name, image) in &self.images {
            if !image.sections.iter().any(|section| {
                address >= section.address && address - section.address < section.size
            }) {
                continue;
            }
            if !self.symbols_loaded.borrow().contains(name) {
                self.load_symbols(image)?;
                self.symbols_loaded.borrow_mut().insert(name.clone());
            }
            return Ok(self
                .symbols
                .borrow()
                .get(&address)
                .cloned()
                .unwrap_or_default());
        }
        Ok(String::new())
    }

    fn load_symbols(&self, image: &ipatool_sap::assets::StoreImage) -> Result<()> {
        let header = self.read(image.base_address(), 32)?;
        let commands = self.read(image.base_address() + 32, u64::from(le32(&header[20..])))?;
        let Some(link) = image.segment("__LINKEDIT") else {
            return Ok(());
        };
        let mut offset = 0;
        while offset + 8 <= commands.len() {
            let size = le32(&commands[offset + 4..]) as usize;
            if size < 8 || size > commands.len() - offset {
                return Err(Error::panic("invalid symbol load command"));
            }
            let command = &commands[offset..offset + size];
            offset += size;
            if le32(command) != 2 {
                continue;
            }
            if size != 24 {
                return Err(Error::panic("invalid symbol table command"));
            }
            let (symbol_offset, count, string_offset, string_size) = (
                le32(&command[8..]),
                le32(&command[12..]),
                le32(&command[16..]),
                le32(&command[20..]),
            );
            if u64::from(symbol_offset) < link.offset
                || u64::from(string_offset) < link.offset
                || count > 4 << 20
            {
                return Err(Error::panic("invalid cache symbol table"));
            }
            let table = self.read(
                link.address + u64::from(symbol_offset) - link.offset,
                u64::from(count) * 16,
            )?;
            for entry in table.as_chunks::<16>().0 {
                let address = le64(&entry[8..]);
                let index = le32(entry);
                if address == 0
                    || entry[4] & 0xe0 != 0
                    || entry[4] & 0xe != 0xe
                    || index >= string_size
                    || self
                        .symbols
                        .borrow()
                        .get(&address)
                        .is_some_and(|name| !name.is_empty())
                {
                    continue;
                }
                let text = self.cstring(
                    link.address + u64::from(string_offset) - link.offset + u64::from(index),
                )?;
                if text.len() > 1 {
                    self.symbols
                        .borrow_mut()
                        .insert(address, text.strip_prefix('_').unwrap_or(&text).to_owned());
                }
            }
            return Ok(());
        }
        Err(Error::panic(format!("no symbol table in {}", image.uuid)))
    }
}
