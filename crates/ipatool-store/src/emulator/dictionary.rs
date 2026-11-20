//! Go `dictionary.go`: a bounded implementation of Swift native dictionary storage. Metadata
//! and strides come from the matching runtime. References live for the VM lifetime, so
//! copying these observed pointer-only key/value layouts is bitwise safe.

use std::collections::BTreeMap;

use unicode_normalization::UnicodeNormalization as _;

use super::hashing::{hash_bytes, hash_compress, init_hasher};
use super::swift::{return_swift_string, swift_string};
use super::{Guest, Payload, Result, Uc, fail};

const BUCKETS: u64 = 1024;
const STORAGE_CAPACITY: u64 = BUCKETS * 3 / 4;

pub(crate) fn ensure_unique_dictionary(
    uc: &mut Uc,
    slot: u64,
    unique: bool,
    capacity: u64,
    metadata: u64,
) -> Result<()> {
    let old = uc.read64(slot)?;
    let old_capacity = uc.read64(old + 24)?;
    if unique && capacity <= old_capacity {
        return uc.set(0);
    }
    let Some(layout) = uc
        .sti()
        .profile()
        .swift_dictionaries
        .get(&metadata.to_string())
        .copied()
    else {
        fail!("unprepared dictionary metadata {metadata:#x}");
    };
    if !matches!(layout.key_stride, 16 | 32 | 40) || !matches!(layout.value_stride, 8 | 16 | 32) {
        fail!("unsupported dictionary copy layout");
    }
    if capacity > STORAGE_CAPACITY {
        fail!("dictionary exceeds runtime capacity");
    }
    let count = uc.read64(old + 16)?;
    if count != 0 && old_capacity != STORAGE_CAPACITY {
        fail!("dictionary resizing requires rehash");
    }
    let header_size = 64 + BUCKETS / 8;
    let size = header_size + BUCKETS * (layout.key_stride + layout.value_stride);
    let storage = uc.alloc(size)?;
    uc.write64(storage, layout.metadata)?;
    uc.write64(storage + 8, 3)?;
    uc.write64(storage + 16, count)?;
    uc.write64(storage + 24, STORAGE_CAPACITY)?;
    uc.write64(storage + 32, 10)?;
    let seed = uc.read64(old + 40)?;
    uc.write64(storage + 40, seed)?;
    let keys = storage + header_size;
    let values = keys + BUCKETS * layout.key_stride;
    uc.write64(storage + 48, keys)?;
    uc.write64(storage + 56, values)?;
    if count != 0 {
        let spans = [
            (storage + 64, old + 64, BUCKETS / 8),
            (keys, uc.read64(old + 48)?, BUCKETS * layout.key_stride),
            (values, uc.read64(old + 56)?, BUCKETS * layout.value_stride),
        ];
        for (destination, source, length) in spans {
            let data = uc.read_mem(source, length)?;
            uc.write_mem(destination, &data)?;
        }
    }
    uc.write64(slot, storage)?;
    uc.set(u64::from(old_capacity != STORAGE_CAPACITY))
}

pub(crate) fn bridge_dictionary_to_swift(
    uc: &mut Uc,
    object: u64,
    key_type: u64,
    value_type: u64,
) -> Result<u64> {
    let empty = uc.sti().symbol("_swiftEmptyDictionarySingleton");
    if object == 0 {
        return Ok(empty);
    }
    let values: BTreeMap<String, u64> = uc.dict(object)?.clone();
    if values.is_empty() {
        return Ok(empty);
    }
    let (mut type_ref, mut key_stride, mut value_stride) = (0x1_a2e1_d5d6_u64, 16, 32);
    let string = uc.sti().symbol("$sSSN");
    if value_type == string {
        if key_type != value_type {
            fail!("unsupported typed dictionary bridge");
        }
        (type_ref, value_stride) = (0x1_a2e1_d7c6, 16);
    }
    if key_type == uc.sti().symbol("$ss11AnyHashableVN") {
        (type_ref, key_stride) = (0x1_a2e1_db1a, 40);
    }
    let metadata = uc
        .sti()
        .profile()
        .swift_types
        .get(&type_ref.to_string())
        .copied()
        .unwrap_or_default();
    if metadata == 0 {
        fail!("String/Any dictionary metadata not prepared");
    }
    let slot = uc.alloc(8)?;
    uc.write64(slot, empty)?;
    ensure_unique_dictionary(uc, slot, false, values.len() as u64, metadata)?;
    let storage = uc.read64(slot)?;
    let (keys, items) = (uc.read64(storage + 48)?, uc.read64(storage + 56)?);
    let seed = uc.read64(storage + 40)?;
    let buckets = 1u64 << (uc.read64(storage + 32)? & 0xff);
    for (key, &value) in &values {
        let hasher = uc.alloc(72)?;
        init_hasher(uc, hasher, seed)?;
        let mut bytes = key.nfc().collect::<String>().into_bytes();
        bytes.push(255);
        hash_bytes(uc, hasher, &bytes)?;
        let tail = uc.read64(hasher)?;
        let hash = hash_compress(uc, hasher, tail, true)?;
        let mut index = hash & (buckets - 1);
        while uc.read64(storage + 64 + (index / 64) * 8)? & (1 << (index % 64)) != 0 {
            index = (index + 1) & (buckets - 1);
        }
        let bitmap = storage + 64 + (index / 64) * 8;
        let bits = uc.read64(bitmap)?;
        uc.write64(bitmap, bits | (1 << (index % 64)))?;
        return_swift_string(uc, key)?;
        let (first, second) = (uc.arg(0)?, uc.arg(1)?);
        uc.write64(keys + index * key_stride, first)?;
        uc.write64(keys + index * key_stride + 8, second)?;
        if key_stride == 40 {
            let (metadata, witness) = (
                uc.sti().bridge("anyHashableStringMetadata"),
                uc.sti().bridge("anyHashableStringWitness"),
            );
            uc.write64(keys + index * key_stride + 24, metadata)?;
            uc.write64(keys + index * key_stride + 32, witness)?;
        }
        if value_stride == 16 {
            let Payload::String(text) = uc.obj(value)?.value.clone() else {
                fail!("non-string value in typed dictionary bridge");
            };
            return_swift_string(uc, &text)?;
            let (first, second) = (uc.arg(0)?, uc.arg(1)?);
            uc.write64(items + index * 16, first)?;
            uc.write64(items + index * 16 + 8, second)?;
        } else {
            store_swift_any(uc, items + index * 32, value)?;
        }
    }
    uc.write64(storage + 16, values.len() as u64)?;
    Ok(storage)
}

pub(crate) fn store_swift_any(uc: &mut Uc, destination: u64, object: u64) -> Result<()> {
    let Some(found) = uc.sti().objects.get(&object) else {
        fail!("unknown object bridged to Swift Any");
    };
    let (class, value) = (found.class.clone(), found.value.clone());
    let metadata = match value {
        Payload::String(text) => {
            return_swift_string(uc, &text)?;
            let (first, second) = (uc.arg(0)?, uc.arg(1)?);
            uc.write64(destination, first)?;
            uc.write64(destination + 8, second)?;
            uc.sti().symbol("$sSSN")
        }
        Payload::Int(value) => {
            #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int64)")]
            uc.write64(destination, value as u64)?;
            uc.sti().symbol("$sSiN")
        }
        Payload::Bool(value) => {
            uc.write64(destination, u64::from(value))?;
            uc.sti().symbol("$sSbN")
        }
        Payload::Float(value) => {
            uc.write64(destination, value.to_bits())?;
            uc.sti().symbol("$sSdN")
        }
        _ => {
            let class = if class.contains("Dictionary") {
                "NSDictionary"
            } else if class.contains("Array") {
                "NSArray"
            } else if class.contains("Data") {
                "NSData"
            } else {
                &class
            };
            uc.write64(destination, object)?;
            uc.sti()
                .profile()
                .objc_metadata
                .get(class)
                .copied()
                .unwrap_or_default()
        }
    };
    if metadata == 0 {
        fail!("unprepared Swift Any metadata");
    }
    uc.write64(destination + 24, metadata)
}

pub(crate) fn bridge_dictionary_to_objc(
    uc: &mut Uc,
    storage: u64,
    key_type: u64,
    value_type: u64,
) -> Result<u64> {
    let mut result = BTreeMap::new();
    let count = uc.read64(storage + 16)?;
    if count == 0 {
        return uc.new_object("NSDictionary", Payload::Dict(result));
    }
    let string = uc.sti().symbol("$sSSN");
    if key_type != string || value_type != string {
        fail!("unsupported dictionary Objective-C bridge types {key_type:#x}/{value_type:#x}");
    }
    let scale = uc.read64(storage + 32)? & 255;
    if count > 768 || scale > 10 {
        fail!("dictionary bridge exceeds budget");
    }
    let (keys, values) = (uc.read64(storage + 48)?, uc.read64(storage + 56)?);
    let buckets = 1u64 << scale;
    for index in 0..buckets {
        if uc.read64(storage + 64 + (index / 64) * 8)? & (1 << (index % 64)) == 0 {
            continue;
        }
        let key = swift_string(
            uc,
            uc.read64(keys + index * 16)?,
            uc.read64(keys + index * 16 + 8)?,
        )?;
        let value = swift_string(
            uc,
            uc.read64(values + index * 16)?,
            uc.read64(values + index * 16 + 8)?,
        )?;
        let value = uc.new_object("NSString", Payload::String(value))?;
        result.insert(key, value);
    }
    if result.len() as u64 != count {
        fail!("Swift dictionary count mismatch");
    }
    uc.new_object("NSDictionary", Payload::Dict(result))
}
