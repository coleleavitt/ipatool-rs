//! Port of Go `profilegen/validate.go`: ABI-relevant values compared against an
//! independently exported native profile. Heap addresses and unrelated dependency closures
//! may differ; object layouts, method entry points and Swift value layouts may not.

use std::collections::{HashMap, HashSet};

use ipatool_sap::assets::StoreCache;
use ipatool_store::profile::{Call, Class, Constant, Method, Profile};

use crate::Generated;
use crate::error::{Error, OrPanic, Result};
use crate::source::{STORE_IMAGES, inside, le32, le64, relative_to, to_usize};

/// Go's `profileMemory`: profile pages over the cache, with the metadata patches applied.
pub(crate) struct ProfileMemory<'p> {
    profile: &'p Profile,
    pages: &'p [u8],
    index: HashMap<u64, u64>,
    cache: Option<&'p StoreCache<'p>>,
}

impl<'p> ProfileMemory<'p> {
    pub(crate) fn new(
        profile: &'p Profile,
        pages: &'p [u8],
        cache: Option<&'p StoreCache<'p>>,
    ) -> Self {
        Self {
            profile,
            pages,
            index: profile
                .pages
                .iter()
                .map(|page| (page.address, page.offset))
                .collect(),
            cache,
        }
    }

    fn read(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        let mut data = vec![0u8; to_usize(size)];
        let mut done = 0u64;
        while done < size {
            let page = (address + done) & !4095;
            let offset = (address + done) & 4095;
            let count = (size - done).min(4096 - offset);
            let owned;
            let source: &[u8] = if let Some(&position) = self.index.get(&page) {
                if position > self.pages.len() as u64 || self.pages.len() as u64 - position < 4096 {
                    return Err(Error::panic("invalid validation page index"));
                }
                &self.pages[to_usize(position)..to_usize(position) + 4096]
            } else {
                let cache = self
                    .cache
                    .ok_or_else(|| Error::panic(format!("page {page:#x} is not in the profile")))?;
                owned = cache.page(page).or_panic()?;
                &owned
            };
            data[to_usize(done)..to_usize(done + count)]
                .copy_from_slice(&source[to_usize(offset)..to_usize(offset + count)]);
            done += count;
        }
        for patch in &self.profile.metadata_patches {
            let start = address.max(patch.address);
            let end = (address + size).min(patch.address + patch.data.len() as u64);
            if start < end {
                data[to_usize(start - address)..to_usize(end - address)].copy_from_slice(
                    &patch.data[to_usize(start - patch.address)..to_usize(end - patch.address)],
                );
            }
        }
        Ok(data)
    }

    fn u64(&self, address: u64) -> Result<u64> {
        Ok(le64(&self.read(address, 8)?))
    }
}

impl Generated {
    /// Go's `Result.Validate`.
    ///
    /// # Errors
    ///
    /// Names the first ABI-relevant value that differs from `reference`.
    pub fn validate(
        &self,
        cache: &StoreCache<'_>,
        reference: &Profile,
        pages: &[u8],
    ) -> std::result::Result<(), Error> {
        validate(self, cache, reference, pages)
            .map_err(|error| error.recover("validate Store profile"))
    }
}

fn validate(
    generated: &Generated,
    cache: &StoreCache<'_>,
    reference: &Profile,
    pages: &[u8],
) -> Result<()> {
    let profile = &generated.profile;
    if profile.uuid != reference.uuid || profile.plugin_uuid != reference.plugin_uuid {
        return Err(Error::failed("reference uses different Store binaries"));
    }
    let mut targets = Vec::new();
    for name in STORE_IMAGES {
        let image = cache
            .open_image(name)
            .map_err(|error| Error::failed(format!("validate Store profile: {error}")))?;
        targets.extend(
            image
                .segments
                .iter()
                .filter(|segment| segment.name != "__LINKEDIT")
                .map(|segment| (segment.address, segment.address + segment.memory_size)),
        );
    }
    targets.sort_by_key(|range| range.0);

    compare_classes(&profile.classes, &reference.classes)?;
    if profile.features != reference.features {
        return Err(Error::failed("feature defaults differ from native profile"));
    }
    for (key, value) in &reference.strings {
        let address = parse_key(key)?;
        match profile.strings.get(key) {
            None if inside(address, &targets) => {
                return Err(Error::failed(format!("missing Store string at {key}")));
            }
            Some(actual) if actual != value => {
                return Err(Error::failed(format!("constant string mismatch at {key}")));
            }
            _ => {}
        }
    }
    for (key, value) in &reference.constants {
        let address = parse_key(key)?;
        match profile.constants.get(key) {
            None if inside(address, &targets) => {
                return Err(Error::failed(format!("missing Store constant at {key}")));
            }
            Some(actual) if !equal_constant(actual, value) => {
                return Err(Error::failed(format!("constant object mismatch at {key}")));
            }
            _ => {}
        }
    }
    compare_selector_calls(&profile.calls, &reference.calls)?;

    let cache: &StoreCache<'_> = cache;
    let old = ProfileMemory::new(reference, pages, Some(cache));
    let current = ProfileMemory::new(profile, &generated.pages, Some(cache));
    compare_metadata(&old, &current, reference, profile)?;
    for (key, &address) in &reference.swift_types {
        if let Some(layout) = reference.swift_dictionaries.get(&address.to_string()) {
            let generated_type = profile.swift_types.get(key).copied().unwrap_or_default();
            match profile.swift_dictionaries.get(&generated_type.to_string()) {
                Some(other)
                    if other.key_stride == layout.key_stride
                        && other.value_stride == layout.value_stride => {}
                _ => {
                    return Err(Error::failed(format!(
                        "swift dictionary layout mismatch: {key}"
                    )));
                }
            }
        }
    }
    for (key, &address) in &reference.swift_witnesses {
        let generated_witness = profile
            .swift_witnesses
            .get(key)
            .copied()
            .unwrap_or_default();
        if !equal_witness(
            &old,
            &current,
            address,
            generated_witness,
            &mut HashSet::new(),
        )? {
            return Err(Error::failed(format!(
                "swift witness methods mismatch: {key}"
            )));
        }
    }
    if reference.symbols != profile.symbols || reference.bridges != profile.bridges {
        return Err(Error::failed("swift bridge identities differ"));
    }
    Ok(())
}

fn parse_key(key: &str) -> Result<u64> {
    key.parse()
        .map_err(|error| Error::failed(format!("validate Store profile: {error}")))
}

fn compare_classes(generated: &[Class], reference: &[Class]) -> Result<()> {
    let classes: HashMap<&str, &Class> = generated
        .iter()
        .map(|class| (class.name.as_str(), class))
        .collect();
    let key = |method: &Method| format!("{}:{}", method.class_method, method.selector);
    for expected in reference {
        let Some(expected_methods) = expected
            .methods
            .as_ref()
            .filter(|methods| !methods.is_empty())
        else {
            continue;
        };
        let actual = classes.get(expected.name.as_str()).filter(|actual| {
            actual.address == expected.address
                && actual.meta_address == expected.meta_address
                && actual.size == expected.size
                && actual.superclass == expected.superclass
                && actual.ivars == expected.ivars
        });
        let Some(actual) = actual else {
            return Err(Error::failed(format!(
                "class layout mismatch: {}",
                expected.name
            )));
        };
        let methods: HashMap<String, &Method> = actual
            .methods
            .iter()
            .flatten()
            .map(|method| (key(method), method))
            .collect();
        for method in expected_methods {
            let same = methods.get(&key(method)).is_some_and(|actual| {
                actual.selector == method.selector
                    && actual.address == method.address
                    && actual.class_method == method.class_method
                    && actual.types == method.types
            });
            if !same {
                return Err(Error::failed(format!(
                    "method mismatch: {} {}",
                    expected.name, method.selector
                )));
            }
        }
    }
    Ok(())
}

fn compare_selector_calls(generated: &[Call], reference: &[Call]) -> Result<()> {
    let calls: HashMap<u64, &Call> = generated.iter().map(|call| (call.address, call)).collect();
    for call in reference.iter().filter(|call| !call.selector.is_empty()) {
        let same = calls.get(&call.address).is_some_and(|actual| {
            actual.selector == call.selector && actual.selector_address == call.selector_address
        });
        if !same {
            return Err(Error::failed(format!(
                "selector bridge mismatch at {:#x}",
                call.address
            )));
        }
    }
    Ok(())
}

fn compare_metadata(
    old: &ProfileMemory<'_>,
    current: &ProfileMemory<'_>,
    reference: &Profile,
    generated: &Profile,
) -> Result<()> {
    for (expected, actual) in [
        (&reference.swift_types, &generated.swift_types),
        (&reference.swift_accessors, &generated.swift_accessors),
        (&reference.swift_singletons, &generated.swift_singletons),
        (&reference.swift_foreign, &generated.swift_foreign),
        (&reference.objc_metadata, &generated.objc_metadata),
    ] {
        for (key, &a) in expected {
            let b = actual.get(key).copied().unwrap_or_default();
            if b == 0 {
                return Err(Error::failed(format!("missing Swift metadata {key}")));
            }
            let kind = |value: u64| if value > 0x7ff { 0 } else { value };
            let (kind_a, kind_b) = (kind(old.u64(a)?), kind(current.u64(b)?));
            if kind_a != kind_b {
                return Err(Error::failed(format!(
                    "swift metadata kind mismatch: {key}"
                )));
            }
            let native = old.read(old.u64(a - 8)? + 64, 24)?;
            // The old exporter omitted initialized in-image value-witness tables. Every
            // available native layout is compared; incomplete uncaptured tables carry no
            // reference evidence.
            if le32(&native[16..]) & 0x40_0000 == 0 {
                let layout = current.read(current.u64(b - 8)? + 64, 24)?;
                if native != layout {
                    return Err(Error::failed(format!(
                        "swift value layout mismatch: {key} native={} generated={}",
                        crate::hex(&native),
                        crate::hex(&layout)
                    )));
                }
            }
            match kind_a {
                0 => {
                    if old.u64(a + 64)? != current.u64(b + 64)?
                        || old.read(a + 48, 8)? != current.read(b + 48, 8)?
                    {
                        return Err(Error::failed(format!("swift class layout mismatch: {key}")));
                    }
                }
                0x200 | 0x201 | 0x305 if old.u64(a + 8)? != current.u64(b + 8)? => {
                    return Err(Error::failed(format!(
                        "swift type identity mismatch: {key}"
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Go's `equalConstant`: missing dictionary keys differ from zero-valued entries.
pub(crate) fn equal_constant(a: &Constant, b: &Constant) -> bool {
    a.kind == b.kind
        && a.text == b.text
        && a.number == b.number
        && a.is_boolean == b.is_boolean
        && a.boolean == b.boolean
        && a.items == b.items
        && a.entries == b.entries
}

/// Go's `equalWitness`: witness tables match when their methods match, wherever the tables
/// themselves live.
pub(crate) fn equal_witness(
    a: &ProfileMemory<'_>,
    b: &ProfileMemory<'_>,
    left: u64,
    right: u64,
    seen: &mut HashSet<(u64, u64)>,
) -> Result<bool> {
    if left == 0 || right == 0 {
        return Ok(false);
    }
    if !seen.insert((left, right)) {
        return Ok(true);
    }
    let descriptor = a.u64(left)?;
    if descriptor != b.u64(right)? {
        return Ok(false);
    }
    let displacement = le32(&a.read(descriptor, 4)?);
    let mut protocol = relative_to(descriptor, displacement & !1);
    if displacement & 1 != 0 {
        protocol = a.u64(protocol)?;
    }
    let count = le32(&a.read(protocol + 16, 4)?);
    if count > 1024 {
        return Err(Error::panic("invalid witness requirement count"));
    }
    let signatures = le32(&a.read(protocol + 12, 4)?);
    if signatures > 1024 {
        return Err(Error::panic("invalid witness signature count"));
    }
    for index in 0..u64::from(count) {
        let (x, y) = (a.u64(left + 8 + index * 8)?, b.u64(right + 8 + index * 8)?);
        if x == y {
            continue;
        }
        let flags = le32(&a.read(protocol + 24 + u64::from(signatures) * 12 + index * 8, 4)?);
        if flags & 15 != 0 || !equal_witness(a, b, x, y, seen)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ipatool_store::profile::Page;

    use super::*;

    #[test]
    fn compares_witness_methods_independently_of_their_table_addresses() {
        let mut data = vec![0u8; 4096];
        let put = |data: &mut Vec<u8>, address: usize, value: u64| {
            data[address - 4096..address - 4088].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut data, 0x1100, 0x100); // The conformance points to the protocol at 0x1200.
        put(&mut data, 0x1210, 1); // One protocol requirement.
        put(&mut data, 0x1218, 1); // A method requirement.
        put(&mut data, 0x1300, 0x1100);
        put(&mut data, 0x1308, 0x5000);
        put(&mut data, 0x1400, 0x1100);
        put(&mut data, 0x1408, 0x5000);
        let profile = Profile {
            pages: vec![Page {
                address: 4096,
                offset: 0,
            }],
            ..Profile::default()
        };
        let mut other = data.clone();
        let a = ProfileMemory::new(&profile, &data, None);
        let b = ProfileMemory::new(&profile, &other, None);
        assert!(equal_witness(&a, &b, 0x1300, 0x1400, &mut HashSet::new()).unwrap());
        other[0x408..0x410].copy_from_slice(&0x5008u64.to_le_bytes());
        let b = ProfileMemory::new(&profile, &other, None);
        assert!(!equal_witness(&a, &b, 0x1300, 0x1400, &mut HashSet::new()).unwrap());
    }

    #[test]
    fn distinguishes_missing_dictionary_keys_from_zero_valued_entries() {
        let constant = |key: &str| Constant {
            kind: "NSDictionary".to_owned(),
            entries: BTreeMap::from([(key.to_owned(), 0)]),
            ..Constant::default()
        };
        assert!(!equal_constant(&constant("a"), &constant("b")));
        assert!(equal_constant(&constant("a"), &constant("a")));
    }
}
