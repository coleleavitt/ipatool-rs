//! Go `swift.go`: Swift runtime ABI services. The runtime owns all allocations until it is
//! dropped, including Swift references. These are ABI services, not replacements for AMS
//! request or response methods.
#![allow(clippy::match_same_arms, reason = "the arms follow Go's cases")]

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization as _;

use super::async_tasks::{async_symbol, task_local_value, value_size};
use super::bridge::foundation_copy;
use super::callbacks::{callback_function, copy_block};
use super::dictionary::{
    bridge_dictionary_to_objc, bridge_dictionary_to_swift, ensure_unique_dictionary,
};
use super::hashing::{hash_bytes, hash_compress, hash_word, init_hasher};
use super::runtime::{REG_X1, REG_X8, REG_X20, queue_pending};
use super::{Guest, Object, Payload, Result, Uc, fail};
use crate::gourl;
use crate::profile::witness_key;

fn nfc(text: &str) -> String {
    text.nfc().collect()
}

fn copy_memory(uc: &mut Uc, destination: u64, source: u64, size: u64) -> Result<()> {
    let data = uc.read_mem(source, size)?;
    uc.write_mem(destination, &data)
}

/// Go's `http.CanonicalHeaderKey`.
pub(crate) fn canonical_header_key(key: &str) -> String {
    const TOKEN: &[u8] = b"!#$%&'*+-.^_`|~";
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || TOKEN.contains(&byte))
    {
        return key.to_owned();
    }
    let mut upper = true;
    key.bytes()
        .map(|byte| {
            let byte = if upper {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            };
            upper = byte == b'-';
            char::from(byte)
        })
        .collect()
}

fn profile_key(map: &std::collections::BTreeMap<String, u64>, key: u64) -> u64 {
    map.get(&key.to_string()).copied().unwrap_or_default()
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
pub(crate) fn swift_symbol(uc: &mut Uc, name: &str) -> Result<bool> {
    if async_symbol(uc, name)? {
        return Ok(true);
    }
    let symbol = |uc: &Uc, name: &str| uc.sti().symbol(name);
    match name {
        "$ss27_stringCompareWithSmolCheck__9expectingSbs11_StringGutsV_ADs01_G16ComparisonResultOtF" =>
        {
            let left = nfc(&swift_string(uc, uc.arg(0)?, uc.arg(1)?)?);
            let right = nfc(&swift_string(uc, uc.arg(2)?, uc.arg(3)?)?);
            match uc.arg(4)? {
                0 => uc.set(u64::from(left == right))?,
                1 => uc.set(u64::from(left < right))?,
                _ => fail!("unsupported Swift string comparison"),
            }
        }
        "$ss18_DictionaryStorageC6resize8original8capacity4moveAByxq_Gs05__RawaB0C_SiSbtFZ" => {
            let (storage_type, capacity) = (uc.arg(3)?, uc.arg(1)?);
            let metadata = uc
                .sti()
                .profile()
                .swift_dictionaries
                .iter()
                .find(|(_, layout)| layout.metadata == storage_type)
                .and_then(|(key, _)| key.parse::<u64>().ok())
                .unwrap_or_default();
            if metadata == 0 {
                fail!("unprepared dictionary storage {storage_type:#x}");
            }
            let slot = uc.alloc(8)?;
            let empty = symbol(uc, "_swiftEmptyDictionarySingleton");
            uc.write64(slot, empty)?;
            ensure_unique_dictionary(uc, slot, false, capacity, metadata)?;
            let storage = uc.read64(slot)?;
            uc.set(storage)?;
        }
        "swift_dynamicCast" => dynamic_cast(uc)?,
        "$ss17CodingUserInfoKeyVwCP" | "$sSSwCP" | "$sSSwcp" | "$sSSwca" => {
            // The pinned runtime shares this witness with String: two words, followed by a
            // retain that the scoped allocation lifetime subsumes.
            copy_memory(uc, uc.arg(0)?, uc.arg(1)?, 16)?;
        }
        // References remain alive until this runtime is dropped.
        "$ss11AnyHashableVwxx" | "$sSSwxx" => {}
        "$ss11AnyHashableVwcp" | "$ss11AnyHashableVwca" => {
            // This bridge constructs only inline String-backed boxes.
            if uc.read64(uc.arg(1)? + 24)? != uc.sti().bridge("anyHashableStringMetadata") {
                fail!("unsupported AnyHashable copy layout");
            }
            copy_memory(uc, uc.arg(0)?, uc.arg(1)?, 40)?;
        }
        "$sSD10FoundationE36_unconditionallyBridgeFromObjectiveCySDyxq_GSo12NSDictionaryCSgFZ" => {
            let (object, key_type, value_type) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?);
            let string = symbol(uc, "$sSSN");
            if (key_type != string && key_type != symbol(uc, "$ss11AnyHashableVN"))
                || (value_type != symbol(uc, "$sypN").wrapping_add(8) && value_type != string)
            {
                fail!("unsupported dictionary bridge metadata {key_type:#x}/{value_type:#x}");
            }
            let storage = bridge_dictionary_to_swift(uc, object, key_type, value_type)?;
            uc.set(storage)?;
        }
        "$sSo13os_log_type_ta0A0E5errorABvgZ" => uc.set(16)?,
        "$sSo13os_log_type_ta0A0E5faultABvgZ" => uc.set(17)?,
        "$sSo13os_log_type_ta0A0E4infoABvgZ" => uc.set(1)?,
        "$sSo13os_log_type_ta0A0E5debugABvgZ" => uc.set(2)?,
        "$sSo13os_log_type_ta0A0E7defaultABvgZ" => uc.set(0)?,
        "$sSS6appendyySSF" => {
            let this = uc.reg(REG_X20)?;
            let value = swift_string(uc, uc.read64(this)?, uc.read64(this + 8)?)?
                + &swift_string(uc, uc.arg(0)?, uc.arg(1)?)?;
            return_swift_string(uc, &value)?;
            let (first, second) = (uc.arg(0)?, uc.arg(1)?);
            uc.write64(this, first)?;
            uc.write64(this + 8, second)?;
        }
        "$ss9_typeName_9qualifiedSSypXp_SbtF" => {
            let metadata = uc.arg(0)?;
            let mut name = uc
                .sti()
                .class_names
                .get(&metadata)
                .cloned()
                .unwrap_or_default();
            if name.is_empty() && uc.read64(metadata)? == 0x305 {
                let class = uc.read64(metadata + 8)?;
                name = uc
                    .sti()
                    .class_names
                    .get(&class)
                    .cloned()
                    .unwrap_or_default();
            }
            if name.is_empty() {
                fail!("unprepared Swift type name {metadata:#x}");
            }
            return_swift_string(uc, &name)?;
        }
        "$ss9TaskLocalC12wrappedValueAByxGx_tcfc" => {
            let this = uc.reg(REG_X20)?;
            let metadata = uc.read64(this)?;
            let value_type = uc.read64(metadata + 80)?;
            let size = value_size(uc, value_type)?;
            let allocated = uc.sti().allocations.get(&this).copied().unwrap_or_default();
            if size > allocated.wrapping_sub(16) {
                fail!("TaskLocal value exceeds allocation");
            }
            copy_memory(uc, this + 16, uc.arg(0)?, size)?;
            let mut object = Object::new("HostSwiftTaskLocal", Payload::Nil);
            object.properties = HashMap::from([
                ("valueSize".to_owned(), size),
                ("defaultValue".to_owned(), this + 16),
            ]);
            uc.st().objects.insert(this, object);
            uc.set(this)?;
        }
        "$ss9TaskLocalC3getxyF" | "$ss9TaskLocalC12wrappedValuexvg" => {
            let this = uc.reg(REG_X20)?;
            let Some(object) = uc.sti().objects.get(&this) else {
                fail!("unknown TaskLocal");
            };
            let (default, size) = (object.prop("defaultValue"), object.prop("valueSize"));
            let out = uc.reg(REG_X8)?;
            let mut source = task_local_value(uc, this);
            if source == 0 {
                source = default;
            }
            copy_memory(uc, out, source, size)?;
        }
        "$sSo9OS_os_logC0B0E9subsystem8categoryABSS_SStcfC" => {
            let log = uc.new_object("HostOSLog", Payload::Nil)?;
            uc.set(log)?;
        }
        "$s10Foundation10URLRequestV8setValue_18forHTTPHeaderFieldySSSg_SStF" => {
            let copied = mutable_swift_request(uc)?;
            if uc.obj(copied)?.prop("allHTTPHeaderFields") == 0 {
                let fields = uc.new_object(
                    "NSMutableDictionary",
                    Payload::Dict(std::collections::BTreeMap::new()),
                )?;
                uc.obj_mut(copied)?
                    .properties
                    .insert("allHTTPHeaderFields".to_owned(), fields);
            }
            let fields = uc.obj(copied)?.prop("allHTTPHeaderFields");
            let key = canonical_header_key(&swift_string(uc, uc.arg(2)?, uc.arg(3)?)?);
            if uc.arg(1)? == 0 {
                uc.dict_mut(fields)?.remove(&key);
            } else {
                let value = swift_string(uc, uc.arg(0)?, uc.arg(1)?)?;
                let value = uc.new_object("NSString", Payload::String(value))?;
                uc.dict_mut(fields)?.insert(key, value);
            }
        }
        "swift_initWithCopy_strong"
        | "swift_initWithTake_strong"
        | "swift_assignWithCopy_strong"
        | "swift_assignWithTake_strong" => {
            let value = uc.read64(uc.arg(1)?)?;
            uc.write64(uc.arg(0)?, value)?;
        }
        "$s10Foundation3URLV14absoluteStringSSvg" => {
            let this = uc.reg(REG_X20)?;
            let value = uc.string(uc.read64(this)?)?;
            return_swift_string(uc, &value)?;
        }
        "$s10Foundation10URLRequestV3urlAA3URLVSgvg" => {
            let this = uc.reg(REG_X20)?;
            let Some(object) = uc.sti().objects.get(&uc.read64(this)?) else {
                fail!("invalid Swift URLRequest value");
            };
            let url = object.prop("URL");
            let out = uc.reg(REG_X8)?;
            uc.write64(out, url)?;
            uc.write64(out + 8, 0)?;
        }
        "swift_getSingletonMetadata" => {
            let value = profile_key(&uc.sti().profile().swift_singletons, uc.arg(1)?);
            if value == 0 {
                fail!("unprepared singleton metadata {:#x}", uc.arg(1)?);
            }
            uc.set(value)?;
            uc.set_reg(REG_X1, 0)?;
        }
        "$s10Foundation22_convertErrorToNSErrorySo0E0Cs0C0_pF" => {
            let mut value = uc.arg(0)?;
            let expected = profile_key(&uc.sti().profile().swift_types, 0x1_a2e1_c7fa);
            for _ in 0..16 {
                let Some(object) = uc.sti().objects.get(&value) else {
                    fail!("unknown Swift error value");
                };
                if uc.kind_of(&object.class, "NSError") {
                    uc.set(value)?;
                    return Ok(true);
                }
                if object.class != "HostSwiftError" || object.prop("metadata") != expected {
                    fail!(
                        "unsupported Swift error bridge {} metadata {:#x}",
                        object.class,
                        object.prop("metadata")
                    );
                }
                value = uc.read64(object.prop("value"))?;
            }
            fail!("recursive Swift error box");
        }
        "$ss38_bridgeAnythingNonVerbatimToObjectiveCyyXlxnlF" => {
            let (source, metadata) = (uc.arg(0)?, uc.arg(1)?);
            let object = if metadata == symbol(uc, "$sSSN") {
                let text = swift_string(uc, uc.read64(source)?, uc.read64(source + 8)?)?;
                uc.new_object("NSString", Payload::String(text))?
            } else if metadata == symbol(uc, "$sSiN") {
                #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
                let value = uc.read64(source)? as i64;
                uc.new_object("NSNumber", Payload::Int(value))?
            } else {
                fail!("unsupported nonverbatim bridge metadata {metadata:#x}");
            };
            uc.set(object)?;
        }
        "$sSD10FoundationE19_bridgeToObjectiveCSo12NSDictionaryCyF" => {
            let value = bridge_dictionary_to_objc(uc, uc.arg(0)?, uc.arg(1)?, uc.arg(2)?)?;
            uc.set(value)?;
        }
        "$sSS4hash4intoys6HasherVz_tF" => {
            let mut bytes = nfc(&swift_string(uc, uc.arg(1)?, uc.arg(2)?)?).into_bytes();
            bytes.push(255);
            hash_bytes(uc, uc.arg(0)?, &bytes)?;
        }
        "swift_allocError" => {
            let (metadata, witness, source) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?);
            let size = value_size(uc, metadata)?;
            if size > 1 << 20 {
                fail!("Swift error exceeds budget");
            }
            let value = uc.alloc(size)?;
            let boxed = uc.new_object("HostSwiftError", Payload::Nil)?;
            uc.obj_mut(boxed)?.properties = HashMap::from([
                ("metadata".to_owned(), metadata),
                ("witness".to_owned(), witness),
                ("value".to_owned(), value),
            ]);
            if source != 0 {
                copy_memory(uc, value, source, size)?;
            }
            uc.set(boxed)?;
            uc.set_reg(REG_X1, value)?;
        }
        "swift_getErrorValue" => {
            let boxed = match uc.sti().objects.get(&uc.arg(0)?) {
                Some(object) if object.class == "HostSwiftError" => object.clone(),
                _ => fail!("unsupported unboxed Swift error"),
            };
            let out = uc.arg(2)?;
            uc.write64(out, boxed.prop("value"))?;
            uc.write64(out + 8, boxed.prop("metadata"))?;
            uc.write64(out + 16, boxed.prop("witness"))?;
        }
        "swift_errorRetain" | "swift_errorRelease" | "swift_willThrow" => {}
        "$s10Foundation10URLRequestV36_unconditionallyBridgeFromObjectiveCyACSo12NSURLRequestCSgFZ" =>
        {
            let value = uc.arg(0)?;
            let valid = uc
                .sti()
                .objects
                .get(&value)
                .is_some_and(|object| uc.kind_of(&object.class, "NSURLRequest"));
            if !valid {
                fail!("invalid Swift request bridge input");
            }
            let out = uc.reg(REG_X8)?;
            uc.write64(out, value)?;
        }
        "$s10Foundation10URLRequestV19_bridgeToObjectiveCSo12NSURLRequestCyF"
        | "$s10Foundation3URLV19_bridgeToObjectiveCSo5NSURLCyF" => {
            let this = uc.reg(REG_X20)?;
            let value = uc.read64(this)?;
            uc.set(value)?;
        }
        "$sScPwst" => {
            if uc.arg(2)? != 1 || uc.arg(1)? > 1 {
                fail!("unsupported task priority optional tag");
            }
            let size = value_size(uc, uc.arg(3)?)?;
            if size != 1 {
                fail!("unexpected priority size");
            }
            #[allow(clippy::cast_possible_truncation, reason = "the tag is 0 or 1")]
            let tag = uc.arg(1)? as u8;
            uc.write_mem(uc.arg(0)? + size, &[tag])?;
        }
        "$sScPwet" => {
            let size = value_size(uc, uc.arg(2)?)?;
            let data = uc.read_mem(uc.arg(0)? + size, 1)?;
            uc.set(u64::from(data[0]))?;
        }
        "$s10Foundation3URLV06isFileB0Sbvg" => {
            let this = uc.reg(REG_X20)?;
            let text = uc.string(uc.read64(this)?)?;
            let parsed =
                gourl::parse(&text).map_err(|error| super::Error::msg(error.to_string()))?;
            uc.set(u64::from(parsed.scheme == "file"))?;
        }
        "$sSqwta"
        | "$sSqwca"
        | "$sSqwcp"
        | "$sSqwtk"
        | "swift_cvw_initWithCopy"
        | "swift_cvw_assignWithCopy"
        | "swift_cvw_initWithTake"
        | "swift_cvw_assignWithTake" => {
            let size = value_size(uc, uc.arg(2)?)?;
            if size > 1024 {
                fail!("Swift value copy exceeds budget");
            }
            copy_memory(uc, uc.arg(0)?, uc.arg(1)?, size)?;
        }
        "$s10Foundation3URLVwst" => {
            if uc.arg(2)? != 1 || uc.arg(1)? > 1 {
                fail!("unsupported URL optional tag");
            }
            if uc.arg(1)? == 1 {
                uc.write64(uc.arg(0)?, 0)?;
                uc.write64(uc.arg(0)? + 8, 0)?;
            }
        }
        "$s10Foundation3URLVwet" => {
            if uc.arg(1)? != 1 {
                fail!("unsupported URL optional tag");
            }
            let value = uc.read64(uc.arg(0)?)?;
            uc.set(u64::from(value == 0))?;
        }
        "$s10Foundation3URLV9appending10components13directoryHintACxd_AC09DirectoryF0OtSyRzlF"
        | "$s10Foundation3URLV6append10components13directoryHintyxd_AC09DirectoryF0OtSyRzlF" => {
            let mut this = uc.reg(REG_X20)?;
            let array = uc.arg(0)?;
            let count = uc.read64(array + 16)?;
            if count > 32 {
                fail!("URL component count exceeds budget");
            }
            let mut value = uc.string(uc.read64(this)?)?;
            for index in 0..count {
                let item = array + 32 + index * 16;
                let component = swift_string(uc, uc.read64(item)?, uc.read64(item + 8)?)?;
                value = gourl::join_path(&value, &[&component])
                    .map_err(|error| super::Error::msg(error.to_string()))?;
            }
            if name.contains("9appending") {
                this = uc.reg(REG_X8)?;
            }
            let url = uc.new_object("NSURL", Payload::String(value))?;
            uc.write64(this, url)?;
            uc.write64(this + 8, 0)?;
        }
        "$s10Foundation3URLV36_unconditionallyBridgeFromObjectiveCyACSo5NSURLCSgFZ" => {
            let value = uc.arg(0)?;
            if uc
                .sti()
                .objects
                .get(&value)
                .is_none_or(|object| object.class != "NSURL")
            {
                fail!("invalid Swift URL bridge input");
            }
            let out = uc.reg(REG_X8)?;
            uc.write64(out, value)?;
            uc.write64(out + 8, 0)?;
        }
        "$s10ObjectiveC8SelectorV2eeoiySbAC_ACtFZ" => {
            let equal = uc.arg(0)? == uc.arg(1)?;
            uc.set(u64::from(equal))?;
        }
        "$ss17_NativeDictionaryV12ensureUnique02isD08capacityS2b_SitF" => {
            let this = uc.reg(REG_X20)?;
            ensure_unique_dictionary(uc, this, uc.arg(0)? != 0, uc.arg(1)?, uc.arg(2)?)?;
        }
        "$ss6HasherV5_seedABSi_tcfC" => {
            let out = uc.reg(REG_X8)?;
            init_hasher(uc, out, uc.arg(0)?)?;
        }
        "$ss6HasherV8_combineyySuF" => {
            let this = uc.reg(REG_X20)?;
            hash_word(uc, this, uc.arg(0)?)?;
        }
        "$ss6HasherV9_finalizeSiyF" => {
            let this = uc.reg(REG_X20)?;
            let tail = uc.read64(this)?;
            let hash = hash_compress(uc, this, tail, true)?;
            uc.set(hash)?;
        }
        "$s10ObjectiveC8SelectorV4hash4intoys6HasherVz_tF" => {
            hash_word(uc, uc.arg(0)?, uc.arg(1)?)?;
        }
        "$sSo17OS_dispatch_queueC8DispatchE5async5group3qos5flags7executeySo0a1_b1_F0CSg_AC0D3QoSVAC0D13WorkItemFlagsVyyXBtF" =>
        {
            if uc.arg(0)? != 0 {
                fail!("Swift dispatch group unsupported");
            }
            let queue = uc.reg(REG_X20)?;
            copy_block(
                uc,
                uc.arg(3)?,
                Box::new(move |uc, block| {
                    queue_pending(uc, block, queue);
                    uc.set(0)
                }),
            )?;
        }
        "$sSo17OS_dispatch_queueC8DispatchE5label3qos10attributes20autoreleaseFrequency6targetABSS_AC0D3QoSVAbCE10AttributesVAbCE011AutoreleaseI0OABSgtcfC"
        | "$sSo24OS_dispatch_queue_serialC8DispatchE5label3qos10attributes20autoreleaseFrequency6targetABSS_AC0E3QoSVAbCE10AttributesVSo0a1_b1_C0CACE011AutoreleaseJ0OANSgtcfC" =>
        {
            let target = uc.arg(5)?;
            let label = swift_string(uc, uc.arg(0)?, uc.arg(1)?)?;
            let queue = uc.new_object("HostQueue", Payload::String(label))?;
            uc.obj_mut(queue)?
                .properties
                .insert("targetQueue".to_owned(), target);
            uc.set(queue)?;
        }
        "$ss10SetAlgebraPyxqd__ncSTRd__7ElementQyd__ACRtzlufCTj" => {
            let accessors = &uc.sti().profile().swift_accessors;
            let accessor = |key: &str| accessors.get(key).copied().unwrap_or_default();
            let kind = uc.arg(3)?;
            let known = kind
                == accessor("$sSo24OS_dispatch_queue_serialC8DispatchE10AttributesVMa")
                || kind == accessor("$sSo17OS_dispatch_queueC8DispatchE10AttributesVMa")
                || kind == accessor("$s8Dispatch0A13WorkItemFlagsVMa");
            if !known || uc.read64(uc.read64(uc.arg(0)?)? + 16)? != 0 {
                fail!("unsupported SetAlgebra initializer");
            }
            let out = uc.reg(REG_X8)?;
            uc.write64(out, 0)?;
        }
        "$s8Dispatch0A3QoSV11unspecifiedACvgZ" => {
            let out = uc.reg(REG_X8)?;
            uc.write64(out, 5)?;
            uc.write64(out + 8, 0)?;
        }
        // Conservatively copy; host handles can retain aliases for the VM lifetime.
        "swift_isUniquelyReferenced_nonNull_native"
        | "swift_isUniquelyReferenced_nonNull_bridgeObject" => uc.set(0)?,
        "swift_retain_n"
        | "swift_bridgeObjectRetain_n"
        | "swift_retain"
        | "swift_bridgeObjectRetain"
        | "swift_unknownObjectRetain" => {}
        "swift_destroy_strong"
        | "$ss15CollectionOfOneV8IteratorVwxx"
        | "swift_pod_destroy"
        | "swift_arrayDestroy"
        | "swift_deallocPartialClassInstance"
        | "swift_cvw_destroy"
        | "swift_noop_void_return"
        | "swift_release_n"
        | "swift_bridgeObjectRelease_n"
        | "swift_release"
        | "swift_bridgeObjectRelease"
        | "swift_unknownObjectRelease"
        | "swift_deallocObject" => {}
        "swift_arrayInitWithCopy"
        | "swift_arrayInitWithTakeFrontToBack"
        | "swift_arrayInitWithTakeBackToFront" => array_init(uc, name)?,
        "swift_allocObject" => {
            let (metadata, size) = (uc.arg(0)?, uc.arg(1)?);
            if size < 16 {
                fail!("invalid Swift object size");
            }
            let object = uc.alloc(size)?;
            uc.write64(object, metadata)?;
            // Inline refcount: one strong reference, live object.
            uc.write64(object + 8, 3)?;
            uc.set(object)?;
        }
        "swift_getForeignTypeMetadata" => {
            let candidate = uc.arg(1)?;
            let value = profile_key(&uc.sti().profile().swift_foreign, candidate);
            if value != 0 {
                uc.set(value)?;
                uc.set_reg(REG_X1, 0)?;
                return Ok(true);
            }
            // AMSErrorCode: the pinned native runtime returns this complete, static enum
            // metadata unchanged. Do not generalize to foreign classes with initializers.
            if candidate != 0x1_eaa5_2f08 || uc.read64(candidate)? != 0x201 {
                fail!("unsupported foreign metadata {candidate:#x}");
            }
            uc.set(candidate)?;
            uc.set_reg(REG_X1, 0)?;
        }
        "swift_getWitnessTable" => {
            let (descriptor, metadata) = (uc.arg(0)?, uc.arg(1)?);
            let value = uc
                .sti()
                .profile()
                .swift_witnesses
                .get(&witness_key(descriptor, metadata))
                .copied()
                .unwrap_or_default();
            if value == 0 {
                fail!("unprepared witness descriptor {descriptor:#x} for metadata {metadata:#x}");
            }
            uc.set(value)?;
        }
        "$sSD17dictionaryLiteralSDyxq_Gx_q_td_tcfC" => {
            if uc.read64(uc.arg(0)? + 16)? != 0 {
                fail!("nonempty Swift dictionary literal unsupported");
            }
            let empty = symbol(uc, "_swiftEmptyDictionarySingleton");
            uc.set(empty)?;
        }
        "swift_defaultActor_initialize" => {
            // Serial host scheduler: a freshly allocated actor has an empty mailbox. Enqueue
            // and async entry points remain explicit unsupported services.
            let actor = uc.arg(0)?;
            uc.st()
                .objects
                .insert(actor, Object::new("HostSwiftActor", Payload::Nil));
        }
        "swift_getObjectType" => {
            let value = uc.read64(uc.arg(0)?)?;
            uc.set(value)?;
        }
        "swift_getObjCClassFromMetadata" => {
            let metadata = uc.arg(0)?;
            if uc
                .sti()
                .class_names
                .get(&metadata)
                .is_some_and(|name| !name.is_empty())
            {
                return Ok(true);
            }
            if uc.read64(metadata)? == 0x305 {
                let class = uc.read64(metadata + 8)?;
                uc.set(class)?;
                return Ok(true);
            }
            fail!("unknown Objective-C class metadata");
        }
        // Objective-C compatible class metadata uses the class address itself.
        "swift_getObjCClassMetadata" => {}
        "swift_getTypeByMangledNameInContext2" => {
            let value = profile_key(&uc.sti().profile().swift_types, uc.arg(0)?);
            if value == 0 || uc.arg(2)? != 0 || uc.arg(3)? != 0 {
                fail!("unprepared Swift type {:#x}/{}", uc.arg(0)?, uc.arg(1)?);
            }
            uc.set(value)?;
        }
        "$sSS10FoundationE36_unconditionallyBridgeFromObjectiveCySSSo8NSStringCSgFZ" => {
            let value = uc.string(uc.arg(0)?)?;
            return_swift_string(uc, &value)?;
        }
        "$sSS10FoundationE19_bridgeToObjectiveCSo8NSStringCyF" => {
            let value = swift_string(uc, uc.arg(0)?, uc.arg(1)?)?;
            let object = uc.new_object("NSString", Payload::String(value))?;
            uc.set(object)?;
        }
        _ => return swift_pattern(uc, name),
    }
    Ok(true)
}

fn dynamic_cast(uc: &mut Uc) -> Result<()> {
    let (destination, source, mut source_type, target_type) =
        (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?, uc.arg(3)?);
    let symbol = |uc: &Uc, name: &str| uc.sti().symbol(name);
    let string = symbol(uc, "$sSSN");
    if source_type == symbol(uc, "$ss11AnyHashableVN") && target_type == string {
        if uc.read64(source + 24)? != uc.sti().bridge("anyHashableStringMetadata") {
            fail!("unsupported AnyHashable cast box");
        }
        source_type = target_type;
    } else if source_type == symbol(uc, "$sypN").wrapping_add(8) {
        source_type = uc.read64(source + 24)?;
    }
    if source_type == target_type {
        let size = value_size(uc, source_type)?;
        if size > 24 {
            fail!("unsupported Swift cast value size");
        }
        copy_memory(uc, destination, source, size)?;
        return uc.set(1);
    }
    let scalar = [
        symbol(uc, "$sSiN"),
        symbol(uc, "$sSbN"),
        symbol(uc, "$sSdN"),
    ];
    if target_type == string && scalar.contains(&source_type) && uc.arg(4)? & 1 == 0 {
        return uc.set(0);
    }
    fail!(
        "unsupported Swift cast metadata {source_type:#x} -> {target_type:#x} flags={}",
        uc.arg(4)?
    )
}

fn array_init(uc: &mut Uc, name: &str) -> Result<()> {
    let (destination, source, count, metadata) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?, uc.arg(3)?);
    if count == 0 {
        return uc.set(destination);
    }
    let witnesses = uc.read64(metadata.wrapping_sub(8))?;
    let stride = uc.read64(witnesses + 72)?;
    if count > 65536 || stride > 1 << 20 {
        fail!("Swift array copy exceeds budget");
    }
    let offset = if name == "swift_arrayInitWithCopy" {
        16
    } else {
        32
    };
    let function = uc.read64(witnesses + offset)?;
    let step = ArrayInit {
        destination,
        source,
        count,
        metadata,
        stride,
        function,
        back_to_front: name == "swift_arrayInitWithTakeBackToFront",
    };
    step.run(uc, 0)
}

#[derive(Clone, Copy)]
struct ArrayInit {
    destination: u64,
    source: u64,
    count: u64,
    metadata: u64,
    stride: u64,
    function: u64,
    back_to_front: bool,
}

impl ArrayInit {
    fn run(self, uc: &mut Uc, index: u64) -> Result<()> {
        if index == self.count {
            return uc.set(self.destination);
        }
        let item = if self.back_to_front {
            self.count - index - 1
        } else {
            index
        };
        callback_function(
            uc,
            self.function,
            &[
                self.destination + item * self.stride,
                self.source + item * self.stride,
                self.metadata,
            ],
            Box::new(move |uc| self.run(uc, index + 1)),
        )
    }
}

/// Go's `swift_memcpy%d_%d` scan: the size, or `None` when the name does not match.
fn memcpy_size(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("swift_memcpy")?;
    let (size, alignment) = rest.split_once('_')?;
    let digits = |text: &str| {
        let end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        text[..end].parse::<u64>().ok()
    };
    digits(alignment)?;
    if size.is_empty() || !size.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    size.parse().ok()
}

fn swift_pattern(uc: &mut Uc, name: &str) -> Result<bool> {
    if (name.contains("NativeBoxIyLm8ELm8ELm8E")
        && name.contains("initializeBufferWithCopyOfBuffer"))
        || (name.contains("BridgeObjectBox")
            && (name.contains("initializeWithCopy") || name.contains("assignWithCopy")))
    {
        let value = uc.read64(uc.arg(1)?)?;
        uc.write64(uc.arg(0)?, value)?;
        return Ok(true);
    }
    if name.contains("BridgeObjectBox") && name.contains("destroy") {
        return Ok(true);
    }
    if name.contains("tuple_initializeWithCopy") || name.contains("tuple_assignWithCopy") {
        let size = value_size(uc, uc.arg(2)?)?;
        if size > 1024 {
            fail!("tuple copy exceeds budget");
        }
        copy_memory(uc, uc.arg(0)?, uc.arg(1)?, size)?;
        return Ok(true);
    }
    if name.starts_with("swift_memcpy") {
        let Some(size) = memcpy_size(name).filter(|&size| size <= 1024) else {
            fail!("unsupported Swift copy size");
        };
        copy_memory(uc, uc.arg(0)?, uc.arg(1)?, size)?;
        return Ok(true);
    }
    if name.contains("SwiftRetainableBox") && name.contains("7destroy") {
        return Ok(true);
    }
    if name.contains("SwiftRetainableBox") && name.contains("initializeBufferWithCopyOfBuffer") {
        let value = uc.read64(uc.arg(1)?)?;
        uc.write64(uc.arg(0)?, value)?;
        return Ok(true);
    }
    let global = uc
        .sti()
        .profile()
        .swift_globals
        .get(name)
        .copied()
        .unwrap_or_default();
    if global != 0 {
        uc.set(global)?;
        return Ok(true);
    }
    if (name.starts_with("$sSo17OS_dispatch_queueC8DispatchE")
        || name.starts_with("$s10Foundation3URLV13DirectoryHintO"))
        && name.ends_with("Owui")
    {
        if value_size(uc, uc.arg(2)?)? != 1 {
            fail!("unexpected dispatch enum size");
        }
        #[allow(clippy::cast_possible_truncation, reason = "Go's byte(tag)")]
        let tag = uc.arg(1)? as u8;
        uc.write_mem(uc.arg(0)?, &[tag])?;
        return Ok(true);
    }
    let accessor = uc
        .sti()
        .profile()
        .swift_accessors
        .get(name)
        .copied()
        .unwrap_or_default();
    if accessor != 0 {
        uc.set(accessor)?;
        uc.set_reg(REG_X1, 0)?;
        return Ok(true);
    }
    Ok(false)
}

/// Go's `returnSwiftString`: a small string in two registers, or immortal native UTF-8.
pub(crate) fn return_swift_string(uc: &mut Uc, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() <= 15 {
        let mut words = [0u8; 16];
        words[..bytes.len()].copy_from_slice(bytes);
        #[allow(clippy::cast_possible_truncation, reason = "at most 15")]
        let mut tag = 0xa0 | bytes.len() as u8;
        if value.is_ascii() {
            tag |= 0x40;
        }
        words[15] = tag;
        let (low, high) = words.split_at(8);
        uc.set(u64::from_le_bytes(low.try_into().unwrap_or_default()))?;
        return uc.set_reg(
            REG_X1,
            u64::from_le_bytes(high.try_into().unwrap_or_default()),
        );
    }
    // Immortal native UTF-8 uses an address biased by the 32-byte storage header.
    let pointer = uc.alloc(bytes.len() as u64 + 33)?;
    let mut data = bytes.to_vec();
    data.push(0);
    uc.write_mem(pointer + 32, &data)?;
    let mut flags = bytes.len() as u64 | 0x3000_0000_0000_0000;
    if value.is_ascii() {
        flags |= 0xc000_0000_0000_0000;
    }
    uc.set(flags)?;
    uc.set_reg(REG_X1, pointer | 0x8000_0000_0000_0000)
}

/// Go's `swiftString`.
pub(crate) fn swift_string(uc: &Uc, first: u64, second: u64) -> Result<String> {
    if second & (1 << 61) != 0 {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&first.to_le_bytes());
        bytes[8..].copy_from_slice(&second.to_le_bytes());
        let length = usize::try_from((second >> 56) & 15).unwrap_or_default();
        return Ok(String::from_utf8_lossy(&bytes[..length]).into_owned());
    }
    let pointer = second & 0x0fff_ffff_ffff_ffff;
    if second & (1 << 60) != 0 {
        return uc.string(pointer);
    }
    if second & (1 << 63) == 0 {
        fail!("unsupported mortal Swift string");
    }
    let length = first & 0x0000_ffff_ffff_ffff;
    if length > 1 << 20 {
        fail!("Swift string exceeds runtime limit");
    }
    let data = uc.read_mem(pointer + 32, length)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// Go's `mutableSwiftRequest`: copies the request a Swift `URLRequest` value holds.
fn mutable_swift_request(uc: &mut Uc) -> Result<u64> {
    let this = uc.reg(REG_X20)?;
    let source = uc.read64(this)?;
    let Some(object) = uc.sti().objects.get(&source).cloned() else {
        fail!("invalid Swift request storage");
    };
    let copied = uc.new_object(&object.class, object.value.clone())?;
    if let Some(size) = uc.sti().class(&object.class).map(|class| class.size) {
        copy_memory(uc, copied, source, size)?;
    }
    uc.obj_mut(copied)?
        .properties
        .extend(object.properties.clone());
    let fields = object.prop("allHTTPHeaderFields");
    if fields != 0 {
        let fields = foundation_copy(uc, fields, true)?;
        uc.obj_mut(copied)?
            .properties
            .insert("allHTTPHeaderFields".to_owned(), fields);
    }
    uc.write64(this, copied)?;
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_header_keys_like_go() {
        assert_eq!(
            canonical_header_key("x-apple-store-front"),
            "X-Apple-Store-Front"
        );
        assert_eq!(canonical_header_key("CONTENT-type"), "Content-Type");
        assert_eq!(canonical_header_key("bad key"), "bad key");
    }

    #[test]
    fn scans_memcpy_names_like_go() {
        assert_eq!(memcpy_size("swift_memcpy16_8"), Some(16));
        assert_eq!(memcpy_size("swift_memcpy8_8"), Some(8));
        assert_eq!(memcpy_size("swift_memcpy_8"), None);
    }
}
