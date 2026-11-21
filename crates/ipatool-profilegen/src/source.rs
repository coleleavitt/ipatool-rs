//! Port of Go `profilegen/source.go`: the Store images and their dependency closure,
//! read from the shared cache a page at a time.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

use ipatool_sap::assets::{DependencyKind, StoreCache, StoreImage};
use ipatool_store::profile::Class;

use crate::error::{Error, OrPanic, Result, Wrap};
use crate::{FRAMEWORK_UUID, PLUGIN_UUID};

pub(crate) const AMS_IMAGE: &str =
    "/System/Library/PrivateFrameworks/AppleMediaServices.framework/Versions/A/AppleMediaServices";
pub(crate) const AUTH_IMAGE: &str = "/System/Library/Accounts/Authentication/AMSAccountAuthenticationPlugin.bundle/Contents/MacOS/AMSAccountAuthenticationPlugin";
pub(crate) const STORE_IMAGES: [&str; 2] = [AMS_IMAGE, AUTH_IMAGE];

const MAXIMUM_READ: u64 = 64 << 20;
const MAXIMUM_STRING: usize = 1 << 20;

pub(crate) struct Source<'c> {
    pub(crate) cache: &'c StoreCache<'c>,
    pages: RefCell<HashMap<u64, Vec<u8>>>,
    /// Keyed by install name, so iteration is Go's sorted `imageNames`.
    pub(crate) images: BTreeMap<String, StoreImage>,
    pub(crate) exports: HashMap<String, u64>,
    pub(crate) symbols: RefCell<HashMap<u64, String>>,
    pub(crate) symbols_loaded: RefCell<HashSet<String>>,
    pub(crate) code: Vec<(u64, u64)>,
    pub(crate) target: Vec<(u64, u64)>,
    pub(crate) selector_base: u64,
    pub(crate) classes: HashMap<u64, Class>,
}

impl<'c> Source<'c> {
    /// A source with no images loaded.
    pub(crate) fn empty(cache: &'c StoreCache<'c>) -> Self {
        Self {
            cache,
            pages: RefCell::default(),
            images: BTreeMap::new(),
            exports: HashMap::new(),
            symbols: RefCell::default(),
            symbols_loaded: RefCell::default(),
            code: Vec::new(),
            target: Vec::new(),
            selector_base: 0,
            classes: HashMap::new(),
        }
    }

    /// Go's `newSource`: the Store images and their dependency closure.
    pub(crate) fn new(cache: &'c StoreCache<'c>) -> Result<Self> {
        let mut source = Self::empty(cache);
        for name in STORE_IMAGES {
            source.load_image(name).wrap("load bootstrap metadata")?;
        }
        if source.images[AMS_IMAGE].uuid != FRAMEWORK_UUID
            || source.images[AUTH_IMAGE].uuid != PLUGIN_UUID
        {
            return Err(Error::failed(
                "store images do not match pinned macOS 26A434",
            ));
        }
        let objc = source
            .images
            .get("/usr/lib/libobjc.A.dylib")
            .ok_or_else(|| Error::failed("libobjc is not a Store dependency"))?;
        for section in &objc.sections {
            if section.name == "__objc_opt_ro" {
                source.selector_base = section.address + source.u64(section.address + 40)?;
            }
        }
        source.code.sort_by_key(|range| range.0);
        source.target.sort_by_key(|range| range.0);
        Ok(source)
    }

    fn load_image(&mut self, name: &str) -> Result<()> {
        if self.images.contains_key(name) {
            return Ok(());
        }
        let image = self
            .cache
            .open_image(name)
            .map_err(|error| Error::failed(format!("load {name}: {error}")))?;
        let exports = match self.cache.dyld_exports(&image) {
            Ok(exports) => exports,
            Err(error) if image.has_exports_trie() => {
                return Err(Error::failed(format!("load bootstrap metadata: {error}")));
            }
            Err(_) => Vec::new(),
        };
        for export in exports {
            if export.address != 0 && !export.is_reexport() {
                self.symbols.get_mut().insert(
                    export.address,
                    export
                        .name
                        .strip_prefix('_')
                        .unwrap_or(&export.name)
                        .to_owned(),
                );
                self.exports.insert(export.name, export.address);
            }
        }
        for section in &image.sections {
            if section.has_instructions() {
                self.code
                    .push((section.address, section.address + section.size));
            }
        }
        if STORE_IMAGES.contains(&name) {
            for segment in &image.segments {
                if segment.name != "__LINKEDIT" {
                    self.target
                        .push((segment.address, segment.address + segment.memory_size));
                }
            }
        }

        let mut weak = HashSet::new();
        let mut delayed = HashSet::new();
        for dependency in &image.dependencies {
            match dependency.kind {
                DependencyKind::Weak => {
                    weak.insert(dependency.name.clone());
                }
                DependencyKind::Load => {
                    if dependency.flags & 8 != 0 {
                        delayed.insert(dependency.name.clone());
                    }
                    if dependency.flags & 1 != 0 {
                        weak.insert(dependency.name.clone());
                    }
                }
                _ => {}
            }
        }
        let dependencies: Vec<String> = image
            .dependencies
            .iter()
            .map(|dependency| dependency.name.clone())
            .collect();
        self.images.insert(name.to_owned(), image);

        for dependency in dependencies {
            if delayed.contains(&dependency) {
                continue;
            }
            if weak.contains(&dependency) {
                let images = self
                    .cache
                    .images()
                    .map_err(|error| Error::failed(format!("load bootstrap metadata: {error}")))?;
                if !images.contains_key(&dependency) {
                    continue;
                }
            }
            self.load_image(&dependency)
                .wrap(format!("dependency of {name}"))?;
        }
        Ok(())
    }

    pub(crate) fn read(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        if size > MAXIMUM_READ {
            return Err(Error::panic("bootstrap read exceeds size limit"));
        }
        let mut data = vec![0u8; to_usize(size)];
        let mut pages = self.pages.borrow_mut();
        let mut offset = 0u64;
        while offset < size {
            let address = address.wrapping_add(offset);
            let base = address & !4095;
            let page = match pages.entry(base) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(self.cache.page(base).or_panic()?)
                }
            };
            let start = to_usize(address & 4095);
            let count = (size - offset).min(4096 - (address & 4095));
            let end = to_usize(offset + count);
            data[to_usize(offset)..end].copy_from_slice(&page[start..start + to_usize(count)]);
            offset += count;
        }
        Ok(data)
    }

    pub(crate) fn u64(&self, address: u64) -> Result<u64> {
        Ok(le64(&self.read(address, 8)?))
    }

    pub(crate) fn u32(&self, address: u64) -> Result<u32> {
        Ok(le32(&self.read(address, 4)?))
    }

    pub(crate) fn cstring(&self, mut address: u64) -> Result<String> {
        if address == 0 {
            return Ok(String::new());
        }
        let mut out = Vec::new();
        while out.len() < MAXIMUM_STRING {
            let count = 4096 - (address & 4095);
            let page = self.read(address, count)?;
            if let Some(end) = page.iter().position(|&byte| byte == 0) {
                out.extend_from_slice(&page[..end]);
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            out.extend_from_slice(&page);
            address += count;
        }
        Err(Error::panic("unterminated bootstrap string"))
    }

    pub(crate) fn relative(&self, address: u64) -> Result<u64> {
        Ok(relative_to(address, self.u32(address)?))
    }

    /// Go's `describe`: the nearest preceding symbol, for diagnostics.
    pub(crate) fn describe(&self, address: u64) -> String {
        let symbols = self.symbols.borrow();
        let (best, name) = symbols
            .iter()
            .filter(|(symbol, _)| **symbol <= address)
            .max_by_key(|(symbol, _)| **symbol)
            .map_or((0, ""), |(symbol, name)| (*symbol, name.as_str()));
        format!("{name}+{:#x}", address - best)
    }
}

/// Go's `inside`: `regions` are sorted by start.
pub(crate) fn inside(address: u64, regions: &[(u64, u64)]) -> bool {
    let index = regions.partition_point(|region| region.0 <= address);
    index > 0 && address < regions[index - 1].1
}

/// `address` plus a signed 32-bit displacement.
pub(crate) fn relative_to(address: u64, displacement: u32) -> u64 {
    address.wrapping_add_signed(i64::from(displacement.cast_signed()))
}

pub(crate) fn le64(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[..8]);
    u64::from_le_bytes(bytes)
}

pub(crate) fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}

/// Guest sizes are bounded far below `usize` on every supported host.
pub(crate) fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
