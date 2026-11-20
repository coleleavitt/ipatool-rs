//! Go `metadata.go`: one immutable runtime template per metadata digest.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use sha2::{Digest as _, Sha256};

use super::runtime::{Object, State, method_key};
use super::value::Payload;
use super::{Error, Result};
use crate::profile::{Call, Class, Profile};

/// A template object value. Unlike [`Payload`] it is `Send`, so the template can be shared.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TemplateValue {
    Nil,
    String(String),
    Int(i64),
    Bool(bool),
    Dict(BTreeMap<String, u64>),
    Array(Vec<u64>),
}

impl TemplateValue {
    fn payload(&self) -> Payload {
        match self {
            Self::Nil => Payload::Nil,
            Self::String(text) => Payload::String(text.clone()),
            Self::Int(value) => Payload::Int(*value),
            Self::Bool(value) => Payload::Bool(*value),
            Self::Dict(map) => Payload::Dict(map.clone()),
            Self::Array(items) => Payload::Array(items.clone()),
        }
    }
}

/// Go's `runtimeMetadata`.
#[derive(Debug, Default)]
pub(crate) struct RuntimeMetadata {
    pub(crate) profile: Profile,
    pub(crate) classes: HashMap<String, Arc<Class>>,
    pub(crate) class_names: HashMap<u64, String>,
    pub(crate) methods: HashMap<String, u64>,
    pub(crate) calls: HashMap<u64, Call>,
    pub(crate) selectors: HashMap<u64, String>,
    pub(crate) selector_addresses: HashMap<String, u64>,
    pub(crate) objects: HashMap<u64, (String, TemplateValue)>,
    pub(crate) entries: HashMap<u64, String>,
}

type Cached = Option<([u8; 32], Arc<RuntimeMetadata>)>;

/// Go's `metadataCache`: keeps one template, bounded independently of the number of requests.
/// The content digest prevents stale metadata reuse when callers change profiles.
pub(crate) struct MetadataCache {
    prepared: Mutex<Cached>,
}

pub(crate) static CACHED_METADATA: MetadataCache = MetadataCache::new();

impl MetadataCache {
    pub(crate) const fn new() -> Self {
        Self {
            prepared: Mutex::new(None),
        }
    }

    pub(crate) fn load(&self, raw: &[u8]) -> Result<Arc<RuntimeMetadata>> {
        let digest: [u8; 32] = Sha256::digest(raw).into();
        let mut prepared = self.prepared.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached, metadata)) = prepared.as_ref()
            && *cached == digest
        {
            return Ok(Arc::clone(metadata));
        }
        let metadata = Arc::new(prepare_metadata(raw)?);
        *prepared = Some((digest, Arc::clone(&metadata)));
        Ok(metadata)
    }
}

fn parse_address(key: &str, what: &str) -> Result<u64> {
    key.parse().map_err(|error| {
        Error::msg(format!(
            "decode Store {what} address: strconv.ParseUint: parsing {key:?}: {}",
            go_parse_error(&error)
        ))
    })
}

fn go_parse_error(error: &std::num::ParseIntError) -> &'static str {
    match error.kind() {
        std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
            "value out of range"
        }
        _ => "invalid syntax",
    }
}

pub(crate) fn prepare_metadata(raw: &[u8]) -> Result<RuntimeMetadata> {
    let mut profile: Profile = serde_json::from_slice(raw)
        .map_err(|error| Error::msg(format!("decode Store metadata: {error}")))?;
    if profile.uuid != crate::profile::FRAMEWORK_UUID {
        return Err(Error::msg("unsupported Store profile UUID"));
    }
    // Pinned dyld lazy-import thunk, verified against its resolved import slot (0x1eaaee178).
    // This is a host diagnostics preference, not Store logic.
    profile.calls.push(Call {
        address: 0x1_a2d0_fb94,
        symbol: "CRIsAutoSubmitEnabled".to_owned(),
        ..Call::default()
    });
    let mut metadata = RuntimeMetadata {
        profile: Profile::default(),
        classes: HashMap::new(),
        class_names: HashMap::new(),
        methods: HashMap::new(),
        calls: HashMap::new(),
        selectors: HashMap::new(),
        selector_addresses: HashMap::new(),
        objects: HashMap::new(),
        entries: HashMap::new(),
    };
    for class in &profile.classes {
        metadata
            .classes
            .insert(class.name.clone(), Arc::new(class.clone()));
        metadata
            .class_names
            .insert(class.address, class.name.clone());
        if class.meta_address != 0 {
            metadata
                .class_names
                .insert(class.meta_address, class.name.clone());
        }
        for method in class.methods.iter().flatten() {
            let key = method_key(&class.name, &method.selector, method.class_method);
            metadata.methods.insert(key.clone(), method.address);
            metadata.entries.insert(method.address, key);
        }
    }
    for (key, value) in &profile.strings {
        let address = parse_address(key, "string")?;
        metadata.objects.insert(
            address,
            ("NSString".to_owned(), TemplateValue::String(value.clone())),
        );
    }
    for (key, value) in &profile.constants {
        let address = parse_address(key, "constant")?;
        let data = match value.kind.as_str() {
            "NSString" => TemplateValue::String(value.text.clone()),
            "NSNumber" if value.is_boolean => TemplateValue::Bool(value.boolean),
            "NSNumber" => TemplateValue::Int(value.number),
            "NSDictionary" => TemplateValue::Dict(value.entries.clone()),
            "NSArray" | "NSSet" => TemplateValue::Array(value.items.clone()),
            _ => TemplateValue::Nil,
        };
        metadata.objects.insert(address, (value.kind.clone(), data));
    }
    for call in &profile.calls {
        metadata.calls.insert(call.address, call.clone());
        if !call.selector.is_empty() {
            metadata
                .selectors
                .insert(call.selector_address, call.selector.clone());
            metadata
                .selector_addresses
                .insert(call.selector.clone(), call.selector_address);
        }
    }
    // These tables have been converted to address-indexed templates above. Retain only that
    // representation for the lifetime of the process.
    profile.strings.clear();
    profile.constants.clear();
    metadata.profile = profile;
    Ok(metadata)
}

impl RuntimeMetadata {
    /// Go's `newRuntime`: class definitions, call sites and entry names are shared; the maps
    /// for dynamic classes, methods, selectors and objects belong to each VM.
    pub(crate) fn new_state(self: &Arc<Self>) -> State {
        let mut state = State::new(Arc::clone(self));
        state.classes.clone_from(&self.classes);
        state.class_names.clone_from(&self.class_names);
        state.methods.clone_from(&self.methods);
        state.selectors.clone_from(&self.selectors);
        state
            .selector_addresses
            .clone_from(&self.selector_addresses);
        state.objects = self
            .objects
            .iter()
            .map(|(&address, (class, value))| (address, Object::new(class, value.payload())))
            .collect();
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Constant;

    fn raw() -> Vec<u8> {
        let profile = Profile {
            uuid: crate::profile::FRAMEWORK_UUID.to_owned(),
            strings: BTreeMap::from([("8192".to_owned(), "original".to_owned())]),
            constants: BTreeMap::from([
                (
                    "4096".to_owned(),
                    Constant {
                        kind: "NSDictionary".to_owned(),
                        entries: BTreeMap::from([("value".to_owned(), 8192)]),
                        ..Constant::default()
                    },
                ),
                (
                    "12288".to_owned(),
                    Constant {
                        kind: "NSArray".to_owned(),
                        items: vec![8192],
                        ..Constant::default()
                    },
                ),
            ]),
            classes: vec![Class {
                name: "NSObject".to_owned(),
                address: 16384,
                size: 16,
                ..Class::default()
            }],
            ..Profile::default()
        };
        serde_json::to_vec(&profile).unwrap()
    }

    #[test]
    fn reuses_metadata_by_content_and_rejects_changed_invalid_metadata() {
        let cache = MetadataCache::new();
        let raw = raw();
        let first = cache.load(&raw).unwrap();
        let second = cache.load(&raw.clone()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let error = cache.load(br#"{"UUID":"wrong"}"#).unwrap_err();
        assert!(
            error.to_string().contains("unsupported Store profile"),
            "{error}"
        );
        cache.load(b"invalid json").unwrap_err();
        let second = cache.load(&raw).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn does_not_share_mutable_constants_or_dynamic_registrations_between_vms() {
        let prepared = MetadataCache::new().load(&raw()).unwrap();
        let mut first = prepared.new_state();
        let Payload::Dict(map) = &mut first.objects.get_mut(&4096).unwrap().value else {
            panic!()
        };
        map.insert("value".to_owned(), 99);
        let Payload::Array(items) = &mut first.objects.get_mut(&12288).unwrap().value else {
            panic!()
        };
        items[0] = 99;
        let object = first.objects.get_mut(&8192).unwrap();
        object.value = Payload::String("changed".to_owned());
        object.properties.insert("account".to_owned(), 99);
        first
            .classes
            .insert("OnlyFirst".to_owned(), Arc::new(Class::default()));
        first.class_names.insert(99, "OnlyFirst".to_owned());
        first.methods.insert("dynamic".to_owned(), 99);
        first.selectors.insert(99, "dynamic".to_owned());
        first.selector_addresses.insert("dynamic".to_owned(), 99);
        for guest in [prepared.new_state(), prepared.new_state()] {
            assert!(
                matches!(&guest.objects[&4096].value, Payload::Dict(map) if map["value"] == 8192)
            );
            assert!(
                matches!(&guest.objects[&12288].value, Payload::Array(items) if items == &[8192])
            );
            assert!(
                matches!(&guest.objects[&8192].value, Payload::String(text) if text == "original")
            );
            assert!(guest.objects[&8192].properties.is_empty());
            assert!(!guest.classes.contains_key("OnlyFirst"));
            assert!(!guest.class_names.contains_key(&99));
            assert!(!guest.methods.contains_key("dynamic"));
            assert!(!guest.selectors.contains_key(&99));
            assert!(!guest.selector_addresses.contains_key("dynamic"));
        }
    }

    #[test]
    fn prepares_one_template_for_concurrent_independent_runtimes() {
        let cache = MetadataCache::new();
        let raw = raw();
        let prepared: Vec<Arc<RuntimeMetadata>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| cache.load(&raw).unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        for (index, value) in prepared.iter().enumerate() {
            assert!(Arc::ptr_eq(value, &prepared[0]));
            let mut guest = value.new_state();
            if let Payload::Dict(map) = &mut guest.objects.get_mut(&4096).unwrap().value {
                map.insert("value".to_owned(), index as u64);
            }
        }
        assert_eq!(
            prepared[0].objects[&4096].1,
            TemplateValue::Dict(BTreeMap::from([("value".to_owned(), 8192)]))
        );
    }
}
