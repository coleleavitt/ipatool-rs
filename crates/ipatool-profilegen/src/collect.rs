//! Port of Go `profilegen/collect.go`: the metadata, calls and page snapshot the Store
//! runtime profile carries.

use std::collections::{BTreeMap, HashMap, HashSet};

use ipatool_store::profile::{Call, Class, Constant, MetadataPatch, Page, Profile};

use crate::classes::reachable_classes;
use crate::discover::branch_target;
use crate::error::{Error, Result};
use crate::objc::class_name;
use crate::references::data_references;
use crate::source::{AMS_IMAGE, AUTH_IMAGE, STORE_IMAGES, Source, inside, le32, le64, to_usize};
use crate::vm::{BOOT_HEAP, Guest, Vm};

const MAXIMUM_CONSTANT_DEPTH: usize = 16;

/// Go's `adrp`: the page an `ADRP` at `pc` addresses.
pub(crate) fn adrp(word: u32, pc: u64) -> u64 {
    let mut immediate = i64::from((word >> 5 & 0x7ffff) << 2 | word >> 29 & 3);
    if immediate & (1 << 20) != 0 {
        immediate -= 1 << 21;
    }
    (pc & !4095).wrapping_add_signed(immediate << 12)
}

/// Go's `collector.resolve`: a branch target that is an imported stub, a selector stub or a
/// named function outside the Store images.
pub(crate) fn resolve_call(
    source: &Source<'_>,
    calls: &mut BTreeMap<u64, Call>,
    address: u64,
) -> Result<()> {
    if inside(address, &source.target) || calls.contains_key(&address) {
        return Ok(());
    }
    let code = source.read(address, 16)?;
    let (first, second, third) = (le32(&code), le32(&code[4..]), le32(&code[8..]));
    if first & 0x9f00_0000 == 0x9000_0000 {
        let mut pointer = adrp(first, address);
        pointer = match second & 0xffc0_0000 {
            0x9100_0000 => pointer + u64::from(second >> 10 & 0xfff),
            0xf940_0000 => source.u64(pointer + u64::from(second >> 10 & 0xfff) * 8)?,
            _ => 0,
        };
        if pointer != 0 && first & 31 == 1 {
            calls.insert(
                address,
                Call {
                    address,
                    selector: source.cstring(pointer)?,
                    selector_address: pointer,
                    ..Call::default()
                },
            );
            return Ok(());
        }
        if pointer != 0 && first & 31 == 17 && third & 0xffc0_0000 == 0xf940_0000 {
            let target = source.u64(pointer)?;
            let mut name = source.symbol(target)?;
            if name.is_empty() {
                name = format!("unresolved_{target:x}");
            }
            calls.insert(address, symbol_call(address, &name));
            return Ok(());
        }
    }
    let name = source.symbol(address)?;
    if !name.is_empty() {
        calls.insert(address, symbol_call(address, &name));
    }
    Ok(())
}

fn symbol_call(address: u64, name: &str) -> Call {
    Call {
        address,
        symbol: canonical_symbol(name).to_owned(),
        ..Call::default()
    }
}

pub(crate) fn canonical_symbol(name: &str) -> &str {
    match name {
        "swift_release_preservemost" => "swift_release",
        "swift_retain_preservemost" => "swift_retain",
        "dispatch_channel_async" => "dispatch_async",
        "dispatch_queue_create_with_target$V2" => "dispatch_queue_create_with_target",
        "exp2l" => "exp2",
        "_platform_bzero" => "__bzero",
        "objc_release_x0" => "objc_release",
        "objc_retain_x0" => "objc_retain",
        "powl" => "pow",
        "dispatch_assert_queue$V2" => "dispatch_assert_queue",
        "dispatch_assert_queue_not$V2" => "dispatch_assert_queue_not",
        _ => name,
    }
}

struct Collector<'v, 'u, 'c> {
    vm: &'v mut Vm<'u, 'c>,
    profile: &'v mut Profile,
    pages: HashSet<u64>,
    metadata_pages: HashSet<u64>,
    calls: BTreeMap<u64, Call>,
    /// Class addresses to names; `class_names` adds the metaclasses.
    classes: HashMap<u64, String>,
    references: HashSet<u64>,
    class_names: HashMap<u64, String>,
    needed_classes: HashSet<String>,
}

/// Go's `collect`: fills `profile` and returns the snapshot pages it indexes.
pub(crate) fn collect(
    vm: &mut Vm<'_, '_>,
    profile: &mut Profile,
    classes: &[Class],
) -> Result<Vec<u8>> {
    let source = &vm.get_data().source;
    profile.uuid.clone_from(&source.images[AMS_IMAGE].uuid);
    profile
        .plugin_uuid
        .clone_from(&source.images[AUTH_IMAGE].uuid);
    profile.strings = BTreeMap::new();
    profile.constants = BTreeMap::new();
    let mut collector = Collector {
        vm,
        profile,
        pages: HashSet::new(),
        metadata_pages: HashSet::new(),
        calls: BTreeMap::new(),
        classes: HashMap::new(),
        references: HashSet::new(),
        class_names: HashMap::new(),
        needed_classes: HashSet::new(),
    };
    for class in classes {
        collector.classes.insert(class.address, class.name.clone());
        collector
            .class_names
            .insert(class.address, class.name.clone());
        collector
            .class_names
            .insert(class.meta_address, class.name.clone());
    }
    collector.scan_images()?;
    collector.add_exported_calls();
    collector.add_metadata()?;
    collector.patch_metadata()?;
    let pages = collector.snapshot()?;

    for name in ["_kCFBooleanTrue", "_kCFBooleanFalse"] {
        if let Some(&address) = collector.source().exports.get(name)
            && address != 0
        {
            let value = collector.source().u64(address)?;
            collector.constant(value, 0)?;
        }
    }
    let calls = std::mem::take(&mut collector.calls);
    collector.profile.calls = calls.into_values().collect();
    let mut needed = std::mem::take(&mut collector.needed_classes);
    needed.extend(collector.profile.objc_metadata.keys().cloned());
    if !collector.profile.strings.is_empty() {
        needed.insert("NSString".to_owned());
    }
    needed.extend(
        collector
            .profile
            .constants
            .values()
            .map(|constant| constant.kind.clone()),
    );
    collector.profile.classes = reachable_classes(classes, &mut needed);
    Ok(pages)
}

impl<'c> Collector<'_, '_, 'c> {
    fn source(&self) -> &Source<'c> {
        &self.vm.get_data().source
    }

    fn in_cache(&self, address: u64) -> bool {
        self.source().cache.contains(address, 32)
    }

    fn scan_images(&mut self) -> Result<()> {
        let sections: Vec<(String, u64, u64, bool)> = STORE_IMAGES
            .iter()
            .flat_map(|name| &self.source().images[*name].sections)
            .map(|section| {
                (
                    section.name.clone(),
                    section.address,
                    section.size,
                    section.has_instructions(),
                )
            })
            .collect();
        for (name, address, size, instructions) in sections {
            if instructions {
                self.scan_code(address, size)?;
            } else if name != "__bss" && name != "__common" {
                self.scan_data(&name, address, size)?;
            }
        }
        Ok(())
    }

    fn scan_code(&mut self, address: u64, size: u64) -> Result<()> {
        let code = self.source().read(address, size)?;
        let mut references = Vec::new();
        data_references(&code, address, &mut |reference| references.push(reference));
        for reference in references {
            self.reference(reference)?;
        }
        let mut offset = 0u64;
        while offset + 4 <= size {
            if let Some(target) = branch_target(le32(&code[to_usize(offset)..]), address + offset) {
                resolve_call(&self.vm.get_data().source, &mut self.calls, target)?;
            }
            offset += 4;
        }
        Ok(())
    }

    fn scan_data(&mut self, name: &str, address: u64, size: u64) -> Result<()> {
        let data = self.source().read(address, size)?;
        if name == "__cstring" || name == "__objc_classname" {
            for text in data.split(|&byte| byte == 0) {
                self.needed_classes
                    .insert(class_name(&String::from_utf8_lossy(text)));
            }
        }
        let mut offset = 0u64;
        while offset + 8 <= size {
            if name == "__cfstring" && offset % 32 == 0 {
                self.record_string(address + offset)?;
            }
            let value = le64(&data[to_usize(offset)..]);
            if matches!(
                name,
                "__got"
                    | "__auth_got"
                    | "__objc_classrefs"
                    | "__objc_superrefs"
                    | "__objc_selrefs"
                    | "__la_symbol_ptr"
                    | "__nl_symbol_ptr"
            ) {
                self.reference(value)?;
            }
            if self.classes.get(&value).is_some_and(|class| {
                matches!(
                    class.as_str(),
                    "NSConstantDictionary" | "NSConstantArray" | "NSConstantIntegerNumber"
                )
            }) {
                self.constant(address + offset, 0)?;
            }
            offset += 8;
        }
        Ok(())
    }

    fn add_exported_calls(&mut self) {
        let source = &self.vm.get_data().source;
        let mut added = Vec::new();
        for call in self.calls.values() {
            if let Some(&address) = source.exports.get(&format!("_{}", call.symbol))
                && address != 0
                && !inside(address, &source.target)
            {
                added.push(symbol_call(address, &call.symbol));
            }
        }
        for name in ["___chkstk_darwin", "gmtime_r", "localtime_r"] {
            if let Some(&address) = source.exports.get(&format!("_{name}"))
                && address != 0
            {
                added.push(symbol_call(address, name));
            }
        }
        for call in added {
            self.calls.insert(call.address, call);
        }
    }

    fn add_metadata(&mut self) -> Result<()> {
        let profile = &self.profile;
        let indirect: Vec<u64> = [
            &profile.swift_types,
            &profile.swift_accessors,
            &profile.swift_singletons,
            &profile.swift_foreign,
            &profile.objc_metadata,
        ]
        .into_iter()
        .flat_map(BTreeMap::values)
        .copied()
        .collect();
        let bridges: Vec<u64> = profile.bridges.values().copied().collect();
        let string_metadata = profile
            .bridges
            .get("anyHashableStringMetadata")
            .copied()
            .unwrap_or_default();
        let witnesses: Vec<u64> = profile.swift_witnesses.values().copied().collect();
        let globals: Vec<u64> = profile.swift_globals.values().copied().collect();
        let dictionaries: Vec<u64> = profile
            .swift_dictionaries
            .values()
            .map(|layout| layout.metadata)
            .collect();
        let symbols: Vec<(String, u64)> = profile
            .symbols
            .iter()
            .map(|(name, address)| (name.clone(), *address))
            .collect();

        for address in indirect {
            self.metadata(address)?;
            let witnesses = self.vm.u64(address.wrapping_sub(8))?;
            self.metadata(witnesses)?;
        }
        for address in bridges {
            self.metadata(address)?;
        }
        let witness_table = self.vm.u64(string_metadata.wrapping_sub(8))?;
        self.metadata(witness_table)?;
        for address in witnesses {
            self.metadata(address)?;
        }
        for address in globals {
            self.metadata(address)?;
            let value = self.vm.u64(address)?;
            self.metadata(value)?;
        }
        for address in dictionaries {
            self.metadata(address)?;
        }
        for (name, address) in symbols {
            self.reference(address)?;
            if name.ends_with('N') {
                let offset = if name == "$sypN" { 0 } else { 8 };
                let value = self.vm.u64(address.wrapping_sub(offset))?;
                self.metadata(value)?;
            }
        }
        Ok(())
    }

    /// Initialized metadata inside the executable images, including Swift field offsets.
    /// The image loader supplies executable pages separately.
    fn patch_metadata(&mut self) -> Result<()> {
        let mut pages: Vec<u64> = self.vm.get_data().mapped.iter().copied().collect();
        pages.sort_unstable();
        let mut patches = Vec::new();
        for page in pages {
            let source = self.source();
            if !inside(page, &source.target) || inside(page, &source.code) {
                continue;
            }
            let original = source.read(page, 4096)?;
            let current = self.vm.read(page, 4096)?;
            for offset in (0..4096).step_by(8) {
                if current[offset..offset + 8] != original[offset..offset + 8] {
                    patches.push(MetadataPatch {
                        address: page + offset as u64,
                        data: current[offset..offset + 8].to_vec(),
                    });
                }
            }
        }
        self.profile.metadata_patches = patches;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<Vec<u8>> {
        let mut pages = Vec::new();
        let mut done = HashSet::new();
        for pass in 0..2 {
            let mut addresses: Vec<u64> = self.pages.iter().copied().collect();
            addresses.sort_unstable();
            for address in addresses {
                if !done.insert(address) {
                    continue;
                }
                let data = self.snapshot_page(address)?;
                let metadata = self.metadata_pages.contains(&address);
                for chunk in data.as_chunks::<8>().0 {
                    self.snapshot_pointer(u64::from_le_bytes(*chunk), metadata, pass == 0)?;
                }
                self.profile.pages.push(Page {
                    address,
                    offset: pages.len() as u64,
                });
                pages.extend_from_slice(&data);
            }
        }
        Ok(pages)
    }

    /// One pointer slot of a snapshot page.
    fn snapshot_pointer(&mut self, pointer: u64, metadata: bool, first_pass: bool) -> Result<()> {
        if let Some(name) = self.class_names.get(&pointer) {
            self.needed_classes.insert(name.clone());
        }
        if metadata && self.in_cache(pointer) {
            let source = &self.vm.get_data().source;
            if !inside(pointer, &source.target) && inside(pointer, &source.code) {
                let name = source.symbol(pointer)?;
                if !name.is_empty() {
                    self.calls.insert(pointer, symbol_call(pointer, &name));
                }
            }
        }
        // Swift metadata contains dynamically indexed tables. Their pointer dependencies
        // stay; ordinary data pages are rooted at the exact operands, not at every
        // neighboring pointer.
        if first_pass && metadata {
            self.add_page(pointer);
        }
        Ok(())
    }

    /// Go's `snapshotPage`: untouched pages come straight from the binary; mapping thousands
    /// of read-only pages into Unicorn would make export needlessly quadratic.
    fn snapshot_page(&mut self, address: u64) -> Result<Vec<u8>> {
        let state = self.vm.get_data();
        if !state.mapped.contains(&address)
            && !(BOOT_HEAP..BOOT_HEAP + state.cursor).contains(&address)
        {
            return state.source.read(address, 4096);
        }
        let mut data = self.vm.read(address, 4096)?;
        let source = self.source();
        if source.cache.contains(address, 4096) {
            for offset in (0..4096u64).step_by(4) {
                if inside(address + offset, &source.code) {
                    let start = to_usize(offset);
                    data[start..start + 4].copy_from_slice(&source.read(address + offset, 4)?);
                }
            }
        }
        Ok(data)
    }

    /// Go's `addPage`: objects the host bridge represents are exported as layouts and values;
    /// their runtime internals would pull in unrelated registries.
    fn add_page(&mut self, address: u64) {
        let key = address.to_string();
        if self.class_names.contains_key(&address)
            || self.profile.strings.contains_key(&key)
            || self.profile.constants.contains_key(&key)
        {
            return;
        }
        self.raw_page(address);
    }

    /// Go's `rawPage`: storage native Swift metadata needs, even at a class address.
    fn raw_page(&mut self, address: u64) {
        let state = self.vm.get_data();
        if !inside(address, &state.source.target)
            && !inside(address, &state.source.code)
            && state.source.cache.contains(address, 32)
        {
            self.pages.insert(address & !4095);
        }
        if (BOOT_HEAP..BOOT_HEAP + state.cursor).contains(&address) {
            self.pages.insert(address & !4095);
        }
    }

    /// Go's `reference`: an address operand or pointer slot. Object graphs start here, never
    /// at incidental pointers elsewhere in a copied page.
    fn reference(&mut self, mut address: u64) -> Result<()> {
        while !self.references.contains(&address)
            && self.in_cache(address)
            && !inside(address, &self.source().code)
        {
            self.references.insert(address);
            if let Some(name) = self.class_names.get(&address) {
                self.needed_classes.insert(name.clone());
                return Ok(());
            }
            if self.record_string(address)? {
                return Ok(());
            }
            self.constant(address, 0)?;
            if self.profile.constants.contains_key(&address.to_string()) {
                return Ok(());
            }
            self.add_page(address);
            // An imported global can add several indirections before the object; follow
            // that chain rather than scanning neighboring pointer slots.
            address = self.source().u64(address)?;
        }
        self.add_page(address);
        Ok(())
    }

    fn metadata(&mut self, address: u64) -> Result<()> {
        if address != 0 {
            self.raw_page(address);
            self.reference(address)?;
            self.metadata_pages.insert(address & !4095);
        }
        Ok(())
    }

    fn constant(&mut self, address: u64, depth: usize) -> Result<()> {
        if address == 0 || depth > MAXIMUM_CONSTANT_DEPTH || !self.in_cache(address) {
            return Ok(());
        }
        let key = address.to_string();
        if self.profile.constants.contains_key(&key) {
            return Ok(());
        }
        let class = self
            .classes
            .get(&self.source().u64(address)?)
            .cloned()
            .unwrap_or_default();
        let mut item = Constant::default();
        match class.as_str() {
            "__NSCFConstantString" | "NSConstantString" => {
                "NSString".clone_into(&mut item.kind);
                item.text = self.string_value(address)?.unwrap_or_default();
            }
            "NSConstantIntegerNumber" => {
                "NSNumber".clone_into(&mut item.kind);
                item.number = self.source().u64(address + 16)?.cast_signed();
            }
            "__NSCFBoolean" => {
                "NSNumber".clone_into(&mut item.kind);
                item.is_boolean = true;
                let true_address = self
                    .source()
                    .exports
                    .get("_kCFBooleanTrue")
                    .copied()
                    .unwrap_or_default();
                item.boolean = address == self.source().u64(true_address)?;
                if item.boolean {
                    item.number = 1;
                }
            }
            "__NSDictionary0" => "NSDictionary".clone_into(&mut item.kind),
            "__NSArray0" => "NSArray".clone_into(&mut item.kind),
            "NSConstantDictionary" => {
                "NSDictionary".clone_into(&mut item.kind);
                self.profile.constants.insert(key.clone(), item.clone());
                let source = self.source();
                let (count, keys, values) = (
                    source.u64(address + 16)?,
                    source.u64(address + 24)?,
                    source.u64(address + 32)?,
                );
                if count > 1 << 20 {
                    return Err(Error::panic("constant dictionary too large"));
                }
                for index in 0..count {
                    let source = self.source();
                    let (entry, value) = (
                        source.u64(keys + index * 8)?,
                        source.u64(values + index * 8)?,
                    );
                    self.constant(value, depth + 1)?;
                    if let Some(text) = self.string_value(entry)? {
                        item.entries.insert(text, value);
                    }
                }
            }
            "NSConstantArray" => {
                "NSArray".clone_into(&mut item.kind);
                self.profile.constants.insert(key.clone(), item.clone());
                let (count, values) = (
                    self.source().u64(address + 8)?,
                    self.source().u64(address + 16)?,
                );
                if count > 1 << 20 {
                    return Err(Error::panic("constant array too large"));
                }
                for index in 0..count {
                    let value = self.source().u64(values + index * 8)?;
                    self.constant(value, depth + 1)?;
                    item.items.push(value);
                }
            }
            _ => return Ok(()),
        }
        self.profile.constants.insert(key, item);
        Ok(())
    }

    fn string_value(&self, address: u64) -> Result<Option<String>> {
        if address == 0 || !self.in_cache(address) {
            return Ok(None);
        }
        let source = self.source();
        let class = self
            .classes
            .get(&source.u64(address)?)
            .map_or("", String::as_str);
        if class != "__NSCFConstantString" && class != "NSConstantString" {
            return Ok(None);
        }
        let (pointer, length) = (source.u64(address + 16)?, source.u64(address + 24)?);
        if length > 1 << 20 {
            return Err(Error::panic("constant string too large"));
        }
        if source.u32(address + 8)? & 0x10 != 0 {
            let data = source.read(pointer, length * 2)?;
            let units: Vec<u16> = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes(*unit))
                .collect();
            return Ok(Some(String::from_utf16_lossy(&units)));
        }
        Ok(Some(
            String::from_utf8_lossy(&source.read(pointer, length)?).into_owned(),
        ))
    }

    fn record_string(&mut self, address: u64) -> Result<bool> {
        let key = address.to_string();
        if self.profile.strings.contains_key(&key) {
            return Ok(true);
        }
        if let Some(text) = self.string_value(address)? {
            self.profile.strings.insert(key, text);
            return Ok(true);
        }
        Ok(false)
    }
}
