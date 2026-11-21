//! Port of Go `profilegen/discover.go`: the Swift metadata requests the bootstrap runs.

use std::collections::{BTreeMap, HashMap, HashSet};

use ipatool_sap::assets::{ImageSection, StoreImage};
use ipatool_store::profile::Call;

use crate::collect::{adrp, resolve_call};
use crate::error::{Error, Result};
use crate::source::{STORE_IMAGES, Source, le32, relative_to, to_usize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypeRequest {
    pub(crate) address: u64,
    pub(crate) length: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MetadataAccessor {
    pub(crate) descriptor: u64,
    pub(crate) address: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GlobalRequest {
    pub(crate) name: String,
    pub(crate) address: u64,
}

#[derive(Debug, Default)]
pub(crate) struct SwiftRequests {
    pub(crate) types: Vec<TypeRequest>,
    pub(crate) accessors: Vec<String>,
    pub(crate) globals: Vec<GlobalRequest>,
    pub(crate) nominals: Vec<MetadataAccessor>,
    pub(crate) conformances: Vec<u64>,
}

/// Go's `mangledNameLength`: Swift's embedded relative and absolute symbolic references may
/// contain zero bytes.
pub(crate) fn mangled_name_length(data: &[u8]) -> Result<usize> {
    let mut index = 0;
    while index < data.len() {
        let byte = data[index];
        if byte == 0 {
            return Ok(index);
        }
        index += 1;
        let size = match byte {
            0x01..=0x17 => 4,
            0x18..=0x1f => 8,
            _ => 0,
        };
        if size > data.len() - index {
            return Err(Error::failed("truncated Swift symbolic reference"));
        }
        index += size;
    }
    Err(Error::failed("unterminated Swift mangled name"))
}

/// Go's `concreteTypeRecords`: relative-name/length records whose both fields match a real
/// typeref boundary.
pub(crate) fn concrete_type_records(
    base: u64,
    data: &[u8],
    names: &HashMap<u64, u64>,
) -> Vec<TypeRequest> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 8 <= data.len() {
        let address = relative_to(base + offset as u64, le32(&data[offset..]));
        let length = u64::from(le32(&data[offset + 4..]));
        if length != 0 && names.get(&address) == Some(&length) {
            result.push(TypeRequest { address, length });
        }
        offset += 4;
    }
    result
}

/// The per-image state of Go's `discoverSwiftRequests`.
struct Discovery {
    exported_conformances: HashSet<u64>,
    types: HashMap<u64, u64>,
    conformances: HashSet<u64>,
    nominals: Vec<MetadataAccessor>,
}

impl Source<'_> {
    /// Go's `discoverSwiftRequests`.
    pub(crate) fn discover_swift_requests(&self) -> Result<SwiftRequests> {
        let mut discovery = Discovery {
            exported_conformances: self
                .exports
                .iter()
                .filter(|(name, _)| name.ends_with("Mc") && name.starts_with("_$s"))
                .map(|(_, address)| *address)
                .collect(),
            types: HashMap::new(),
            conformances: HashSet::new(),
            nominals: Vec::new(),
        };
        for name in STORE_IMAGES {
            let image = &self.images[name];
            let names = self
                .type_names(image)
                .map_err(|error| error.wrap(format!("read {name} type references")))?;
            for section in &image.sections {
                self.scan_section(section, &names, &mut discovery)?;
            }
        }

        let (accessors, globals) = self.call_requests()?;
        let mut requests = SwiftRequests {
            types: discovery
                .types
                .into_iter()
                .map(|(address, length)| TypeRequest { address, length })
                .collect(),
            accessors,
            globals,
            nominals: discovery.nominals,
            conformances: discovery.conformances.into_iter().collect(),
        };
        requests.types.sort_by_key(|request| request.address);
        requests.nominals.sort_by_key(|request| request.descriptor);
        requests.conformances.sort_unstable();
        requests.accessors.sort();
        requests.globals.sort_by(|a, b| a.name.cmp(&b.name));

        if requests.types.is_empty() {
            return Err(Error::failed(
                "store images have no concrete Swift type records",
            ));
        }
        Ok(requests)
    }

    /// The start and length of every mangled name in the image's `__swift5_typeref`.
    fn type_names(&self, image: &StoreImage) -> Result<HashMap<u64, u64>> {
        let mut names = HashMap::new();
        for section in image
            .sections
            .iter()
            .filter(|section| section.name == "__swift5_typeref")
        {
            let data = self.read(section.address, section.size)?;
            let mut offset = 0;
            while offset < data.len() {
                if data[offset] == 0 || data[offset] == 0xff {
                    offset += 1;
                    continue;
                }
                let length = mangled_name_length(&data[offset..])?;
                names.insert(section.address + offset as u64, length as u64);
                offset += length + 1;
            }
        }
        Ok(names)
    }

    fn scan_section(
        &self,
        section: &ImageSection,
        names: &HashMap<u64, u64>,
        discovery: &mut Discovery,
    ) -> Result<()> {
        if section.has_instructions() {
            self.code_conformances(section, discovery)?;
        }
        match section.name.as_str() {
            "__const" if section.segment == "__TEXT" => {
                let data = self.read(section.address, section.size)?;
                for record in concrete_type_records(section.address, &data, names) {
                    discovery.types.insert(record.address, record.length);
                }
            }
            "__swift5_types" => {
                for offset in (0..section.size.saturating_sub(3)).step_by(4) {
                    let descriptor = self.relative(section.address + offset)?;
                    let flags = self.u32(descriptor)?;
                    if flags & 0x80 == 0
                        && (16..=18).contains(&(flags & 0x1f))
                        && self.u32(descriptor + 12)? != 0
                    {
                        discovery.nominals.push(MetadataAccessor {
                            descriptor,
                            address: self.relative(descriptor + 12)?,
                        });
                    }
                }
            }
            "__swift5_proto" => {
                for offset in (0..section.size.saturating_sub(3)).step_by(4) {
                    discovery
                        .conformances
                        .insert(self.relative(section.address + offset)?);
                }
            }
            "__got" | "__auth_got" => {
                for offset in (0..section.size.saturating_sub(7)).step_by(8) {
                    let address = self.u64(section.address + offset)?;
                    if discovery.exported_conformances.contains(&address) {
                        discovery.conformances.insert(address);
                    }
                }
            }
            // Other sections carry no Swift metadata requests.
            _ => {}
        }
        Ok(())
    }

    /// Exported conformances loaded through dyld-coalesced GOT slots.
    fn code_conformances(&self, section: &ImageSection, discovery: &mut Discovery) -> Result<()> {
        let data = self.read(section.address, section.size)?;
        let mut offset = 0;
        while offset + 8 <= data.len() {
            if let Some(slot) = referenced_pointer(
                le32(&data[offset..]),
                le32(&data[offset + 4..]),
                section.address + offset as u64,
            ) && self.cache.contains(slot, 8)
            {
                let address = self.u64(slot)?;
                if discovery.exported_conformances.contains(&address) {
                    discovery.conformances.insert(address);
                }
            }
            offset += 4;
        }
        Ok(())
    }

    /// Metadata accessors of non-generic types and global accessors the Store images call.
    fn call_requests(&self) -> Result<(Vec<String>, Vec<GlobalRequest>)> {
        let calls = self.store_calls()?;
        let mut accessors = HashSet::new();
        let mut globals = HashSet::new();
        for name in calls
            .values()
            .map(|call| &call.symbol)
            .filter(|name| name.starts_with("$s"))
        {
            if let Some(stem) = name.strip_suffix("Ma") {
                let descriptor = self.export(&format!("_{stem}Mn"));
                if descriptor != 0 && self.u32(descriptor)? & 0x80 == 0 {
                    accessors.insert(name.clone());
                }
            }
            if name.ends_with("vau") {
                globals.insert(name.clone());
            }
        }
        let globals = globals
            .into_iter()
            .map(|name| match self.export(&format!("_{name}")) {
                0 => Err(Error::failed(format!(
                    "missing Swift global accessor {name}"
                ))),
                address => Ok(GlobalRequest { name, address }),
            })
            .collect::<Result<_>>()?;
        Ok((accessors.into_iter().collect(), globals))
    }

    fn export(&self, name: &str) -> u64 {
        self.exports.get(name).copied().unwrap_or_default()
    }

    /// Go's `storeCalls`: imported branch targets, including shared-cache stubs absent from
    /// the image's own symbol and import tables.
    pub(crate) fn store_calls(&self) -> Result<BTreeMap<u64, Call>> {
        let mut calls = BTreeMap::new();
        for name in STORE_IMAGES {
            for section in &self.images[name].sections {
                if !section.has_instructions() {
                    continue;
                }
                let data = self.read(section.address, section.size)?;
                let mut offset = 0u64;
                while offset + 4 <= section.size {
                    let word = le32(&data[to_usize(offset)..]);
                    if let Some(target) = branch_target(word, section.address + offset) {
                        resolve_call(self, &mut calls, target)?;
                    }
                    offset += 4;
                }
            }
        }
        Ok(calls)
    }
}

/// The target of a `B`/`BL` at `pc`.
pub(crate) fn branch_target(word: u32, pc: u64) -> Option<u64> {
    if word & 0x7c00_0000 != 0x1400_0000 {
        return None;
    }
    let mut immediate = i64::from(word & 0x03ff_ffff);
    if immediate & (1 << 25) != 0 {
        immediate -= 1 << 26;
    }
    Some(pc.wrapping_add_signed(immediate * 4))
}

/// Go's `referencedPointer`: an `ADRP` + `LDR` pair loading a dyld-coalesced GOT slot.
pub(crate) fn referenced_pointer(page: u32, load: u32, pc: u64) -> Option<u64> {
    if page & 0x9f00_0000 != 0x9000_0000
        || load & 0xffc0_0000 != 0xf940_0000
        || page & 31 != (load >> 5) & 31
    {
        return None;
    }
    Some(adrp(page, pc) + u64::from(load >> 10 & 0xfff) * 8)
}
