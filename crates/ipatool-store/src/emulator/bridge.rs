//! Go `bridge.go`, imports: the C, Objective-C, `CoreFoundation`, dispatch and `IOKit` services
//! the pinned frameworks call. Messages are in `messages.rs`.
#![allow(clippy::match_same_arms, reason = "the arms follow Go's cases")]

use std::collections::{BTreeMap, HashMap};

use sha2::{Digest as _, Sha512};

use super::callbacks::{callback, callback_function, copy_block, copy_byref, copy_value};
use super::compression::compression_symbol;
use super::runtime::{
    HEAP_SIZE, REG_LR, REG_X1, Timer, conversion, next_pending, pending_callback, queue_is_within,
    queue_pending,
};
use super::swift::swift_symbol;
use super::value::{Date, parse_http_date};
use super::{Guest, Payload, Result, Uc, Value, fail};

pub(crate) use super::messages::{message, responds};

fn new_string(uc: &mut Uc, text: &str) -> Result<u64> {
    uc.new_object("NSString", Payload::String(text.to_owned()))
}

pub(crate) fn new_error(
    uc: &mut Uc,
    domain: &str,
    code: i64,
    info: BTreeMap<String, u64>,
) -> Result<u64> {
    let domain = new_string(uc, domain)?;
    let code = uc.new_object("NSNumber", Payload::Int(code))?;
    let info = uc.new_object("NSDictionary", Payload::Dict(info))?;
    let fields = BTreeMap::from([
        ("domain".to_owned(), domain),
        ("code".to_owned(), code),
        ("userInfo".to_owned(), info),
    ]);
    uc.new_object("NSError", Payload::Dict(fields))
}

pub(crate) use new_error as cocoa_error;

/// Reads a C `struct tm` (Go reads the first 36 bytes as `int32` fields).
fn read_tm(uc: &Uc, address: u64) -> Result<Date> {
    let data = uc.read_mem(address, 36)?;
    let field = |index: usize| {
        i64::from(i32::from_le_bytes(
            data[index * 4..index * 4 + 4]
                .try_into()
                .unwrap_or_default(),
        ))
    };
    Ok(Date::from_civil(
        field(5) + 1900,
        field(4) + 1,
        field(3),
        field(2),
        field(1),
        field(0),
    ))
}

fn tm_fields(date: Date) -> [u32; 9] {
    let civil = date.civil();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Go's uint32(int)"
    )]
    let year = (civil.year - 1900) as u32;
    [
        civil.second,
        civil.minute,
        civil.hour,
        civil.day,
        civil.month - 1,
        year,
        civil.weekday,
        civil.year_day - 1,
        0,
    ]
}

fn write_words(uc: &mut Uc, address: u64, words: &[u32], size: usize) -> Result<()> {
    let mut data = vec![0u8; size];
    for (index, word) in words.iter().enumerate() {
        data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    uc.write_mem(address, &data)
}

fn preference_key(uc: &Uc, application: u64, key: u64) -> Result<String> {
    Ok(format!("{}/{}", uc.string(application)?, uc.string(key)?))
}

fn preference(uc: &Uc, application: u64, key: u64) -> Result<u64> {
    let key = preference_key(uc, application, key)?;
    Ok(uc.sti().preferences.get(&key).copied().unwrap_or_default())
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|error| super::Error::msg(error.to_string()))?;
    Ok(bytes)
}

/// Go's `newUUIDString`: a random version-4 UUID in uppercase.
pub(crate) fn new_uuid_string() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex = |range: std::ops::Range<usize>| {
        bytes[range]
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .concat()
    };
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    ))
}

fn now_nanos() -> u64 {
    let now = Date::now();
    #[allow(clippy::cast_sign_loss, reason = "Go's uint64(UnixNano)")]
    let nanos = now
        .seconds
        .wrapping_mul(1_000_000_000)
        .wrapping_add(i64::from(now.nanos)) as u64;
    nanos
}

fn object_class(uc: &Uc, address: u64) -> Option<String> {
    uc.sti()
        .objects
        .get(&address)
        .map(|object| object.class.clone())
}

fn class_name(uc: &Uc, address: u64) -> String {
    uc.sti()
        .class_names
        .get(&address)
        .cloned()
        .unwrap_or_default()
}

/// Go's `symbol`: one imported C or runtime function.
#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
pub(crate) fn symbol(uc: &mut Uc, name: &str) -> Result<()> {
    if compression_symbol(uc, name)? {
        return Ok(());
    }
    match name {
        "AMSError" => {
            // AMSAccountAuthenticationPlugin uses this factory when Apple returns a server
            // dialog and native presentation is disabled. Preserve the error as an NSError so
            // the framework can finish the request and the caller can inspect the original
            // response instead of aborting the guest.
            let mut info = BTreeMap::new();
            for (index, key) in [
                (1, "NSLocalizedDescription"),
                (2, "NSLocalizedFailureReason"),
                (3, "NSUnderlyingError"),
            ] {
                let value = uc.arg(index)?;
                if value != 0 {
                    info.insert(key.to_owned(), value);
                }
            }
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let code = uc.arg(0)? as i64;
            let error = new_error(uc, "AMSErrorDomain", code, info)?;
            uc.set(error)?;
        }
        "xpc_get_type" => {
            let dictionary =
                object_class(uc, uc.arg(0)?).is_some_and(|class| class == "HostXPCDictionary");
            let value = if dictionary {
                uc.sti().symbol("__xpc_type_dictionary")
            } else {
                0
            };
            uc.set(value)?;
        }
        "CC_SHA512" => {
            let length = uc.arg(1)? & 0xffff_ffff;
            if length > HEAP_SIZE {
                fail!("SHA-512 input exceeds runtime budget");
            }
            let data = uc.read_mem(uc.arg(0)?, length)?;
            let digest = Sha512::digest(&data);
            let mut output = uc.arg(2)?;
            if output == 0 {
                output = uc.alloc(64)?;
            }
            uc.write_mem(output, &digest)?;
            uc.set(output)?;
        }
        "strftime" => {
            let (out, limit, format, tm) =
                (uc.arg(0)?, uc.arg(1)?, uc.cstring(uc.arg(2)?)?, uc.arg(3)?);
            if format != "%a, %d %b %Y %H:%M:%S GMT" {
                fail!("unsupported date output format {format}");
            }
            let value = read_tm(uc, tm)?.civil().http_date();
            if value.len() as u64 + 1 > limit {
                return uc.set(0);
            }
            let mut bytes = value.into_bytes();
            bytes.push(0);
            uc.write_mem(out, &bytes)?;
            uc.set(bytes.len() as u64 - 1)?;
        }
        "strptime" => {
            let (value, format, out) = (
                uc.cstring_bytes(uc.arg(0)?)?,
                uc.cstring(uc.arg(1)?)?,
                uc.arg(2)?,
            );
            if format != "%a, %d %b %Y %H:%M:%S GMT" && format != "%a, %d %b %Y %H:%M:%S %Z" {
                fail!("unsupported date format {format}");
            }
            let Some(date) = std::str::from_utf8(&value).ok().and_then(parse_http_date) else {
                return uc.set(0);
            };
            for (index, word) in tm_fields(date).iter().enumerate() {
                uc.write32(out + index as u64 * 4, *word)?;
            }
            let end = uc.arg(0)? + value.len() as u64;
            uc.set(end)?;
        }
        "timegm" => {
            #[allow(clippy::cast_sign_loss, reason = "Go's uint64(Unix())")]
            let seconds = read_tm(uc, uc.arg(0)?)?.seconds as u64;
            uc.set(seconds)?;
        }
        // The portable response exposes no CFNetwork peer-address metadata.
        "CFURLResponseCopyPeerAddress" => uc.set(0)?,
        // No native resolver state was created by the failed res_9_ninit.
        "res_9_ndestroy" => {}
        // CFNetwork DNS diagnostics are unavailable; the host transport owns DNS.
        "res_9_ninit" => uc.set(0xffff_ffff)?,
        // The isolated runtime does not opt into diagnostics submission.
        "CRIsAutoSubmitEnabled" => uc.set(0)?,
        "NSUserName" | "NSFullUserName" => {
            let value = new_string(uc, "ipatool")?;
            uc.set(value)?;
        }
        "NSTemporaryDirectory" => {
            let value = new_string(uc, "/ipatool/tmp/")?;
            uc.set(value)?;
        }
        "CPSharedResourcesDirectory" | "NSHomeDirectory" => {
            let value = new_string(uc, "/ipatool")?;
            uc.set(value)?;
        }
        "getuid" | "geteuid" => uc.set(501)?,
        "getgid" | "getegid" => uc.set(20)?,
        "NSSearchPathForDirectoriesInDomains" => {
            if uc.arg(0)? != 5 || uc.arg(1)? != 8 {
                fail!("unsupported virtual filesystem directory");
            }
            let path = new_string(uc, "/System/Library")?;
            let value = uc.new_object("NSArray", Payload::Array(vec![path]))?;
            uc.set(value)?;
        }
        "MGCopyAnswerWithError" | "MGCopyAnswer" => {
            let key = uc.string(uc.arg(0)?)?;
            let value = match key.as_str() {
                "BuildVersion" => "26A434",
                "ProductVersion" => "27.0.1",
                "DeviceClass" => "Mac",
                "ProductType" => "Mac14,2",
                _ => fail!("unsupported device property {key}"),
            };
            if name == "MGCopyAnswerWithError" && uc.arg(2)? != 0 {
                uc.write_mem(uc.arg(2)?, &[0; 4])?;
            }
            let value = new_string(uc, value)?;
            uc.set(value)?;
        }
        "time" => {
            #[allow(clippy::cast_sign_loss, reason = "Go's uint64(Unix())")]
            let now = Date::now().seconds as u64;
            if uc.arg(0)? != 0 {
                uc.write64(uc.arg(0)?, now)?;
            }
            uc.set(now)?;
        }
        "localtime" | "localtime_r" | "gmtime_r" => {
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let seconds = uc.read64(uc.arg(0)?)? as i64;
            let out = if name == "localtime" {
                uc.alloc(56)?
            } else {
                uc.arg(1)?
            };
            write_words(uc, out, &tm_fields(Date::unix(seconds, 0)), 56)?;
            uc.set(out)?;
        }
        "objc_getAssociatedObject" => {
            let (object, key) = (uc.arg(0)?, uc.arg(1)?);
            let value = uc
                .sti()
                .associations
                .get(&object)
                .and_then(|values| values.get(&key))
                .copied()
                .unwrap_or_default();
            uc.set(value)?;
        }
        "objc_setAssociatedObject" => {
            let (object, key, value, policy) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?, uc.arg(3)?);
            let save = move |uc: &mut Uc, value: u64| {
                uc.st()
                    .associations
                    .entry(object)
                    .or_default()
                    .insert(key, value);
                uc.set(0)
            };
            uc.st().associations.entry(object).or_default();
            if policy & 255 == 3 {
                copy_value(uc, value, Box::new(save))?;
            } else {
                save(uc, value)?;
            }
        }
        "CFPreferencesGetAppIntegerValue" => {
            let found = preference(uc, uc.arg(1)?, uc.arg(0)?)?;
            let (value, valid) = match uc.sti().objects.get(&found).map(|object| &object.value) {
                Some(Payload::Int(value)) => (*value, true),
                _ => (0, false),
            };
            if uc.arg(2)? != 0 {
                uc.write_mem(uc.arg(2)?, &[u8::from(valid)])?;
            }
            #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int64)")]
            uc.set(value as u64)?;
        }
        "CFPreferencesGetAppBooleanValue" => {
            let found = preference(uc, uc.arg(1)?, uc.arg(0)?)?;
            let (value, valid) = match uc.sti().objects.get(&found).map(|object| &object.value) {
                Some(Payload::Bool(value)) => (*value, true),
                Some(Payload::Int(value)) => (*value != 0, true),
                _ => (false, false),
            };
            if uc.arg(2)? != 0 {
                uc.write_mem(uc.arg(2)?, &[u8::from(valid)])?;
            }
            uc.set(u64::from(value))?;
        }
        "CFPreferencesAppSynchronize" | "CFPreferencesSynchronize" => uc.set(1)?,
        "CFPreferencesCopyAppValue" | "CFPreferencesCopyValue" => {
            let value = preference(uc, uc.arg(1)?, uc.arg(0)?)?;
            uc.set(value)?;
        }
        "CFPreferencesSetAppValue" | "CFPreferencesSetValue" => {
            let key = preference_key(uc, uc.arg(2)?, uc.arg(0)?)?;
            let value = uc.arg(1)?;
            uc.st().preferences.insert(key, value);
            uc.set(0)?;
        }
        "objc_opt_self"
        | "objc_unsafeClaimAutoreleasedReturnValue"
        | "objc_retain"
        | "objc_retainAutoreleasedReturnValue"
        | "objc_claimAutoreleasedReturnValue"
        | "objc_autoreleaseReturnValue"
        | "objc_autorelease"
        | "objc_retainAutoreleaseReturnValue"
        | "objc_retainAutorelease"
        | "objc_release"
        | "objc_autoreleasePoolPop" => {}
        "objc_autoreleasePoolPush" => uc.set(0)?,
        "objc_getProperty" => {
            let value = uc.read64(uc.arg(0)? + uc.arg(2)?)?;
            uc.set(value)?;
        }
        "objc_setProperty_nonatomic" | "objc_setProperty_atomic" => {
            uc.write64(uc.arg(0)? + uc.arg(3)?, uc.arg(2)?)?;
        }
        "objc_setProperty_nonatomic_copy" | "objc_setProperty_atomic_copy" => {
            let (destination, value) = (uc.arg(0)? + uc.arg(3)?, uc.arg(2)?);
            copy_value(
                uc,
                value,
                Box::new(move |uc, copied| {
                    uc.write64(destination, copied)?;
                    uc.set(0)
                }),
            )?;
        }
        "objc_storeStrong" | "objc_storeWeak" | "objc_initWeak" => {
            let value = uc.arg(1)?;
            uc.write64(uc.arg(0)?, value)?;
            uc.set(value)?;
        }
        "objc_copyWeak" => {
            let value = uc.read64(uc.arg(1)?)?;
            uc.write64(uc.arg(0)?, value)?;
        }
        "objc_moveWeak" => {
            let value = uc.read64(uc.arg(1)?)?;
            uc.write64(uc.arg(0)?, value)?;
            uc.write64(uc.arg(1)?, 0)?;
        }
        "objc_loadWeakRetained" | "objc_loadWeak" => {
            let value = uc.read64(uc.arg(0)?)?;
            uc.set(value)?;
        }
        "objc_destroyWeak" => uc.write64(uc.arg(0)?, 0)?,
        "objc_alloc" | "objc_allocWithZone" | "objc_alloc_init" | "objc_opt_new" => {
            let class = class_name(uc, uc.arg(0)?);
            if class.is_empty() {
                fail!("unknown allocated class {:#x}", uc.arg(0)?);
            }
            let object = uc.new_object(&class, Payload::Nil)?;
            uc.set(object)?;
            if name != "objc_alloc" && name != "objc_allocWithZone" {
                message(uc, object, "init", false)?;
            }
        }
        "objc_opt_class" => {
            let value = uc.arg(0)?;
            if uc.sti().class_names.contains_key(&value) {
                return Ok(());
            }
            let Some(class) = object_class(uc, value) else {
                fail!("unknown object class {value:#x}");
            };
            let Some(address) = uc.sti().class(&class).map(|class| class.address) else {
                fail!("{}", super::runtime::NIL_DEREFERENCE);
            };
            uc.set(address)?;
        }
        "objc_opt_isKindOfClass" => {
            let target = class_name(uc, uc.arg(1)?);
            let kind = match object_class(uc, uc.arg(0)?) {
                Some(class) => uc.kind_of(&class, &target),
                None => false,
            };
            uc.set(u64::from(kind))?;
        }
        "objc_opt_respondsToSelector" => {
            let selector = uc.cstring(uc.arg(1)?)?;
            let value = responds(uc, uc.arg(0)?, &selector);
            uc.set(u64::from(value))?;
        }
        "objc_msgSend" => {
            let selector = uc.cstring(uc.arg(1)?)?;
            message(uc, uc.arg(0)?, &selector, false)?;
        }
        "objc_msgSendSuper2" => {
            let pair = uc.arg(0)?;
            let receiver = uc.read64(pair)?;
            let class = class_name(uc, uc.read64(pair + 8)?);
            let selector = uc.cstring(uc.arg(1)?)?;
            let meta = uc.sti().class_names.contains_key(&receiver);
            if let Some(superclass) = uc.sti().class(&class).map(|class| class.superclass.clone()) {
                let entry = uc.lookup(&superclass, &selector, meta);
                if entry != 0 {
                    return uc.jump(entry, receiver, &selector);
                }
            }
            message(uc, receiver, &selector, true)?;
        }
        "CFBundleGetMainBundle" => {
            let value = bundle(uc)?;
            uc.set(value)?;
        }
        "CFBundleGetInfoDictionary" => {
            let value = bundle_info(uc)?;
            uc.set(value)?;
        }
        "CFBundleGetValueForInfoDictionaryKey" => {
            let info = bundle_info(uc)?;
            let key = uc.string(uc.arg(1)?)?;
            let value = uc.dict(info)?.get(&key).copied().unwrap_or_default();
            uc.set(value)?;
        }
        "CFBundleCopyExecutableURL" => {
            let value = uc.new_object("NSURL", Payload::String("file:///ipatool".to_owned()))?;
            uc.set(value)?;
        }
        "getprogname" => {
            let pointer = uc.alloc(8)?;
            uc.write_mem(pointer, b"ipatool\0")?;
            uc.set(pointer)?;
        }
        "getpid" | "_getpid" => uc.set(123)?,
        "CFGetTypeID" => {
            let Some(object) = uc.sti().objects.get(&uc.arg(0)?) else {
                fail!("CFGetTypeID of unknown object");
            };
            let id = match object.value {
                Payload::Bool(_) => 4,
                Payload::Data(_) => 1,
                Payload::String(_) => 2,
                _ => 3,
            };
            uc.set(id)?;
        }
        "CFBooleanGetTypeID" => uc.set(4)?,
        "CFBooleanGetValue" => {
            let value = match &uc.obj(uc.arg(0)?)?.value {
                Payload::Bool(value) => *value,
                other => return Err(conversion(other, "bool")),
            };
            uc.set(u64::from(value))?;
        }
        "CFDataGetTypeID" => uc.set(1)?,
        "CFStringGetTypeID" => uc.set(2)?,
        "IOMasterPort" => {
            uc.write32(uc.arg(1)?, 1)?;
            uc.set(0)?;
        }
        "IOServiceMatching" | "IOBSDNameMatching" => {
            let (key, index) = if name == "IOServiceMatching" {
                ("IOProviderClass", 0)
            } else {
                ("IOBSDName", 2)
            };
            let text = uc.cstring(uc.arg(index)?)?;
            let value = new_string(uc, &text)?;
            let dictionary = uc.new_object(
                "NSDictionary",
                Payload::Dict(BTreeMap::from([(key.to_owned(), value)])),
            )?;
            uc.set(dictionary)?;
        }
        // The virtual device exposes no platform expert or serial service.
        "IOServiceGetMatchingService" => uc.set(0)?,
        "IOServiceGetMatchingServices" => {
            uc.write32(uc.arg(2)?, 1)?;
            uc.set(0)?;
        }
        "IOIteratorNext" => uc.set(1)?,
        "IORegistryEntryGetParentEntry" => {
            uc.write32(uc.arg(2)?, 2)?;
            uc.set(0)?;
        }
        "IORegistryEntryCreateCFProperty" | "IORegistryEntrySearchCFProperty" => {
            if uc.arg(0)? == 0 {
                return uc.set(0);
            }
            let index = if name == "IORegistryEntrySearchCFProperty" {
                2
            } else {
                1
            };
            let key = uc.string(uc.arg(index)?)?;
            let value = match key.as_str() {
                "IOBuiltin" | "IOPrimaryInterface" => {
                    uc.new_object("NSNumber", Payload::Bool(true))?
                }
                "IOMACAddress" => {
                    let hardware = uc.sti().hardware_id.clone();
                    uc.new_object("NSData", Payload::Data(hardware))?
                }
                _ => fail!("unsupported IOKit property {key}"),
            };
            uc.set(value)?;
        }
        "IOObjectRelease" | "CFRelease" => uc.set(0)?,
        "CFDataGetLength" => {
            let length = uc.data(uc.arg(0)?)?.len() as u64;
            uc.set(length)?;
        }
        "CFDataGetBytePtr" => {
            let data = uc.data(uc.arg(0)?)?.clone();
            let pointer = uc.alloc(data.len() as u64)?;
            let _ = uc.write_mem(pointer, &data);
            uc.set(pointer)?;
        }
        "CFDataGetBytes" => {
            let data = uc.data(uc.arg(0)?)?;
            let (start, length, destination) = (uc.arg(1)?, uc.arg(2)?, uc.arg(3)?);
            let end = start
                .checked_add(length)
                .filter(|&end| end <= data.len() as u64);
            let Some(end) = end else {
                fail!(
                    "runtime error: slice bounds out of range [{start}:{}] with capacity {}",
                    start.wrapping_add(length),
                    data.len()
                );
            };
            let slice = data[usize::try_from(start).unwrap_or_default()
                ..usize::try_from(end).unwrap_or_default()]
                .to_vec();
            let _ = uc.write_mem(destination, &slice);
            uc.set(0)?;
        }
        "CFUUIDCreate" => {
            let mut data = random_bytes::<16>()?;
            data[6] = (data[6] & 15) | 64;
            data[8] = (data[8] & 63) | 128;
            let value = uc.new_object("HostUUID", Payload::Data(data.to_vec()))?;
            uc.set(value)?;
        }
        "CFUUIDGetUUIDBytes" => {
            let data = uc.data(uc.arg(0)?)?;
            if data.len() < 16 {
                fail!(
                    "runtime error: index out of range [15] with length {}",
                    data.len()
                );
            }
            let low = u64::from_le_bytes(data[..8].try_into().unwrap_or_default());
            let high = u64::from_le_bytes(data[8..16].try_into().unwrap_or_default());
            uc.set(low)?;
            uc.set_reg(REG_X1, high)?;
        }
        "NSClassFromString" => {
            let name = uc.string(uc.arg(0)?)?;
            let address = uc.class_address(&name);
            uc.set(address)?;
        }
        "class_getMethodImplementation" => {
            let class = class_name(uc, uc.arg(0)?);
            let selector = uc.cstring(uc.arg(1)?)?;
            let entry = uc.lookup(&class, &selector, false);
            uc.set(entry)?;
        }
        "sel_getName" => {}
        "objc_getClass" => {
            let name = uc.cstring(uc.arg(0)?)?;
            let address = uc.class_address(&name);
            uc.set(address)?;
        }
        "NSStringFromClass" => {
            let class = uc.arg(0)?;
            if class == 0 {
                uc.set(0)?;
            } else {
                let name = class_name(uc, class);
                let value = new_string(uc, &name)?;
                uc.set(value)?;
            }
        }
        "NSSelectorFromString" => {
            let name = uc.string(uc.arg(0)?)?;
            let selector = uc.selector(&name)?;
            uc.set(selector)?;
        }
        "NSStringFromSelector" => {
            let name = uc.cstring(uc.arg(0)?)?;
            let value = new_string(uc, &name)?;
            uc.set(value)?;
        }
        "SecTaskCreateFromSelf" => {
            let value = uc.new_object("HostSecurityTask", Payload::Nil)?;
            uc.set(value)?;
        }
        "SecTaskCopyValueForEntitlement" => {
            if uc.arg(2)? != 0 {
                uc.write64(uc.arg(2)?, 0)?;
            }
            uc.set(0)?;
        }
        _ => return dispatch_symbol(uc, name),
    }
    Ok(())
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
fn dispatch_symbol(uc: &mut Uc, name: &str) -> Result<()> {
    match name {
        "dispatch_time" => {
            let mut base = uc.arg(0)?;
            if base == 0 {
                base = now_nanos();
            }
            uc.set(base.wrapping_add(uc.arg(1)?))?;
        }
        "dispatch_after" => {
            let (deadline, block) = (uc.arg(0)?, uc.arg(2)?);
            copy_block(
                uc,
                block,
                Box::new(move |uc, copied| {
                    uc.st().timers.push(Timer {
                        at: deadline,
                        block: copied,
                    });
                    uc.set(0)
                }),
            )?;
        }
        "dispatch_assert_queue" => {
            let (current, expected) = (uc.sti().current_queue, uc.arg(0)?);
            if !queue_is_within(uc, current, expected)? {
                fail!("dispatch queue mismatch: running {current:#x} expected {expected:#x}");
            }
        }
        "dispatch_sync" | "dispatch_sync_f" => {
            let previous = uc.sti().current_queue;
            uc.st().current_queue = uc.arg(0)?;
            let after: super::After = Box::new(move |uc| {
                uc.st().current_queue = previous;
                uc.set(0)
            });
            if name == "dispatch_sync" {
                callback(uc, uc.arg(1)?, &[], after)?;
            } else {
                callback_function(uc, uc.arg(2)?, &[uc.arg(1)?], after)?;
            }
        }
        "dispatch_block_create" => copy_block(uc, uc.arg(1)?, Box::new(|uc, block| uc.set(block)))?,
        "dispatch_async" => {
            let queue = uc.arg(0)?;
            copy_block(
                uc,
                uc.arg(1)?,
                Box::new(move |uc, block| {
                    queue_pending(uc, block, queue);
                    uc.set(0)
                }),
            )?;
        }
        "dispatch_suspend" => {
            let queue = uc.obj_mut(uc.arg(0)?)?;
            *queue
                .properties
                .entry("suspendCount".to_owned())
                .or_default() += 1;
        }
        "dispatch_resume" => {
            let queue = uc.obj_mut(uc.arg(0)?)?;
            let count = queue
                .properties
                .entry("suspendCount".to_owned())
                .or_default();
            if *count == 0 {
                fail!("resuming active dispatch queue");
            }
            *count -= 1;
        }
        "dispatch_retain" | "dispatch_release" => {}
        "dispatch_semaphore_create" => {
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let value = uc.arg(0)? as i64;
            let semaphore = uc.new_object("HostSemaphore", Payload::Int(value))?;
            uc.set(semaphore)?;
        }
        "dispatch_semaphore_signal" => {
            let semaphore = uc.obj_mut(uc.arg(0)?)?;
            let Payload::Int(value) = &mut semaphore.value else {
                return Err(conversion(&semaphore.value, "int64"));
            };
            *value += 1;
            uc.set(0)?;
        }
        "dispatch_semaphore_wait" => semaphore_wait(uc)?,
        "Block_copy" | "objc_retainBlock" => {
            copy_block(uc, uc.arg(0)?, Box::new(|uc, value| uc.set(value)))?;
        }
        "Block_release" | "Block_object_dispose" => {}
        "Block_object_assign" => {
            let (destination, value, flags) = (uc.arg(0)?, uc.arg(1)?, uc.arg(2)?);
            let store: super::callbacks::AfterValue =
                Box::new(move |uc, copied| uc.write64(destination, copied));
            if flags & 0xf == 7 {
                copy_block(uc, value, store)?;
            } else if flags & 8 != 0 {
                copy_byref(uc, value, store)?;
            } else {
                uc.write64(destination, value)?;
            }
        }
        "dispatch_once" | "swift_once" | "dispatch_once_f" => {
            let predicate = uc.arg(0)?;
            if uc.read64(predicate)? != 0 {
                return uc.set(0);
            }
            uc.write64(predicate, u64::MAX)?;
            let after: super::After = Box::new(|uc| uc.set(0));
            match name {
                "dispatch_once" => pending_callback(uc, uc.arg(1)?, after)?,
                "swift_once" => callback_function(uc, uc.arg(1)?, &[uc.arg(2)?], after)?,
                _ => callback_function(uc, uc.arg(2)?, &[uc.arg(1)?], after)?,
            }
        }
        "dispatch_queue_attr_make_with_autorelease_frequency"
        | "dispatch_queue_attr_make_with_qos_class" => {}
        "dispatch_queue_create_with_target" => {
            let target = uc.arg(2)?;
            let queue = uc.new_object("HostQueue", Payload::Nil)?;
            uc.obj_mut(queue)?
                .properties
                .insert("targetQueue".to_owned(), target);
            uc.set(queue)?;
        }
        "dispatch_set_target_queue" => {
            let target = uc.arg(1)?;
            uc.obj_mut(uc.arg(0)?)?
                .properties
                .insert("targetQueue".to_owned(), target);
        }
        "dispatch_queue_create" | "dispatch_get_global_queue" | "dispatch_get_main_queue" => {
            let queue = uc.new_object("HostQueue", Payload::Nil)?;
            uc.set(queue)?;
        }
        "os_unfair_lock_assert_not_owner"
        | "objc_sync_enter"
        | "objc_sync_exit"
        | "os_unfair_lock_lock_with_options"
        | "os_unfair_lock_lock"
        | "os_unfair_lock_unlock"
        | "os_unfair_lock_assert_owner"
        | "os_unfair_recursive_lock_lock_with_options"
        | "os_unfair_recursive_lock_unlock" => uc.set(0)?,
        "os_feature_enabled_impl" => {
            let key = format!("{}/{}", uc.cstring(uc.arg(0)?)?, uc.cstring(uc.arg(1)?)?);
            let Some(&value) = uc.sti().profile().features.get(&key) else {
                fail!("unresolved native feature flag {key}");
            };
            uc.set(u64::from(value))?;
        }
        "os_log_create" => {
            let log = uc.new_object("HostOSLog", Payload::Nil)?;
            uc.set(log)?;
        }
        "os_signpost_enabled" | "os_log_type_enabled" | "os_variant_has_internal_content" => {
            uc.set(0)?;
        }
        "malloc_size" => {
            let size = uc
                .sti()
                .allocations
                .get(&uc.arg(0)?)
                .copied()
                .unwrap_or_default();
            uc.set(size)?;
        }
        "malloc" | "calloc" | "malloc_type_malloc" | "malloc_type_calloc" => {
            let mut size = uc.arg(0)?;
            if name == "calloc" || name == "malloc_type_calloc" {
                let count = uc.arg(1)?;
                if size != 0 && count > HEAP_SIZE / size {
                    fail!("guest calloc exceeds heap budget");
                }
                size = size.wrapping_mul(count);
            }
            let address = uc.alloc(size)?;
            uc.set(address)?;
        }
        "chkstk_darwin" | "free" => {}
        "platform_memmove" | "platform_memcpy" | "memcpy" | "memmove" | "memcpy_chk"
        | "memmove_chk" => {
            let length = uc.arg(2)?;
            if length == 0 {
                return Ok(());
            }
            if length > HEAP_SIZE {
                fail!("guest copy exceeds memory budget");
            }
            let data = uc.read_mem(uc.arg(1)?, length)?;
            uc.write_mem(uc.arg(0)?, &data)?;
        }
        "memset" | "memset_chk" => {
            let length = uc.arg(2)?;
            if length > HEAP_SIZE {
                fail!("guest fill exceeds memory budget");
            }
            #[allow(clippy::cast_possible_truncation, reason = "Go's byte(value)")]
            let data = vec![uc.arg(1)? as u8; usize::try_from(length).unwrap_or_default()];
            uc.write_mem(uc.arg(0)?, &data)?;
        }
        "strlen" => {
            let length = uc.cstring_bytes(uc.arg(0)?)?.len() as u64;
            uc.set(length)?;
        }
        _ => {
            if swift_symbol(uc, name)? {
                return Ok(());
            }
            let lr = uc.reg(REG_LR).unwrap_or_default();
            fail!("unsupported import {name} lr={lr:#x}");
        }
    }
    Ok(())
}

fn semaphore_value(uc: &Uc, address: u64) -> Result<i64> {
    match &uc.obj(address)?.value {
        Payload::Int(value) => Ok(*value),
        other => Err(conversion(other, "int64")),
    }
}

fn semaphore_take(uc: &mut Uc, address: u64) -> Result<()> {
    if let Payload::Int(value) = &mut uc.obj_mut(address)?.value {
        *value -= 1;
    }
    uc.set(0)
}

fn semaphore_wait(uc: &mut Uc) -> Result<()> {
    let semaphore = uc.arg(0)?;
    if semaphore_value(uc, semaphore)? > 0 {
        return semaphore_take(uc, semaphore);
    }
    if uc.sti().pending.is_empty() {
        fail!("guest waits on an unavailable host service");
    }
    let block = next_pending(uc);
    if block == 0 {
        fail!("all pending queues are suspended");
    }
    pending_callback(
        uc,
        block,
        Box::new(move |uc| {
            if semaphore_value(uc, semaphore)? == 0 {
                fail!("guest semaphore remains unsignaled");
            }
            semaphore_take(uc, semaphore)
        }),
    )
}

/// Go's `bundle`: a fresh `NSBundle` for the host process.
pub(crate) fn bundle(uc: &mut Uc) -> Result<u64> {
    let object = uc.new_object("NSBundle", Payload::Nil)?;
    let identifier = new_string(uc, "me.majd.ipatool")?;
    let path = new_string(uc, "/ipatool")?;
    uc.obj_mut(object)?.properties = HashMap::from([
        ("bundleIdentifier".to_owned(), identifier),
        ("bundlePath".to_owned(), path),
    ]);
    Ok(object)
}

/// Go's `bundleInfo`.
pub(crate) fn bundle_info(uc: &mut Uc) -> Result<u64> {
    let mut info = BTreeMap::new();
    for (key, value) in [
        ("CFBundleIdentifier", "me.majd.ipatool"),
        ("CFBundleVersion", "1.0"),
        ("CFBundleExecutable", "ipatool"),
        ("CFBundleName", "ipatool"),
    ] {
        info.insert(key.to_owned(), new_string(uc, value)?);
    }
    uc.new_object("NSDictionary", Payload::Dict(info))
}

/// Go's `foundationCopy`: immutable or mutable copies of Foundation collections.
pub(crate) fn foundation_copy(uc: &mut Uc, receiver: u64, mutable: bool) -> Result<u64> {
    let object = uc.obj(receiver)?;
    let class = object.class.clone();
    match class.as_str() {
        "NSArray" | "NSMutableArray" | "NSSet" | "NSMutableSet" => {
            let base = if class.contains("Set") {
                "Set"
            } else {
                "Array"
            };
            let class = if mutable {
                format!("NSMutable{base}")
            } else {
                format!("NS{base}")
            };
            let items = uc.array(receiver)?.clone();
            uc.new_object(&class, Payload::Array(items))
        }
        "NSDictionary" | "NSMutableDictionary" => {
            let class = if mutable {
                "NSMutableDictionary"
            } else {
                "NSDictionary"
            };
            let values = uc.dict(receiver)?.clone();
            uc.new_object(class, Payload::Dict(values))
        }
        "NSMutableString" => {
            let value = object.value.clone();
            uc.new_object("NSString", value)
        }
        _ => Ok(receiver),
    }
}

/// Go's `collectionKey`: strings by value, numbers and other objects by identity text.
pub(crate) fn collection_key(uc: &Uc, address: u64) -> String {
    match uc.sti().objects.get(&address).map(|object| &object.value) {
        Some(Payload::String(text)) => text.clone(),
        Some(Payload::Int(value)) => format!("#number:{value}"),
        _ => format!("#object:{address:x}"),
    }
}

/// Go's `foundationEqual`.
#[allow(clippy::many_single_char_names, reason = "Go's names")]
pub(crate) fn foundation_equal(uc: &Uc, a: u64, b: u64) -> bool {
    if a == b {
        return true;
    }
    let (Some(x), Some(y)) = (uc.sti().objects.get(&a), uc.sti().objects.get(&b)) else {
        return false;
    };
    match (&x.value, &y.value) {
        (Payload::Date(v), Payload::Date(w)) => v == w,
        (Payload::String(v), Payload::String(w)) => v == w,
        (Payload::Int(v), Payload::Int(w)) => v == w,
        (Payload::Int(v), Payload::Bool(w)) | (Payload::Bool(w), Payload::Int(v)) => {
            *v == i64::from(*w)
        }
        (Payload::Bool(v), Payload::Bool(w)) => v == w,
        (Payload::Data(v), Payload::Data(w)) => v == w,
        (Payload::Array(v), Payload::Array(w)) => {
            v.len() == w.len() && v.iter().zip(w).all(|(&p, &q)| foundation_equal(uc, p, q))
        }
        (Payload::Dict(v), Payload::Dict(w)) => {
            v.len() == w.len()
                && v.iter()
                    .all(|(key, &p)| w.get(key).is_some_and(|&q| foundation_equal(uc, p, q)))
        }
        _ => false,
    }
}

/// Go's `characterSetContains`. The decimal-digit set uses Unicode's decimal digits.
pub(crate) fn character_set_contains(set: &super::Object, c: char) -> Result<bool> {
    let Payload::String(chars) = &set.value else {
        return Err(conversion(&set.value, "string"));
    };
    let mut matched = if chars == "0123456789" {
        is_go_digit(c)
    } else {
        chars.contains(c)
    };
    if set.prop("inverted") != 0 {
        matched = !matched;
    }
    Ok(matched)
}

/// Go's `unicode.IsDigit` (category Nd).
fn is_go_digit(c: char) -> bool {
    c.is_ascii_digit() || unicode_nd(c)
}

/// Unicode 15 decimal digit (Nd) runs outside ASCII, each ten code points from its zero.
fn unicode_nd(c: char) -> bool {
    const ZEROS: &[u32] = &[
        0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
        0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0,
        0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0,
        0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0,
        0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0,
        0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0,
        0x1E950, 0x1FBF0,
    ];
    let code = u32::from(c);
    ZEROS.iter().any(|&zero| (zero..zero + 10).contains(&code))
}

/// Go's `importValue`: a host value as a new Foundation object.
pub(crate) fn import_value(uc: &mut Uc, value: &Value) -> Result<u64> {
    match value {
        Value::Nil => Ok(0),
        Value::String(text) => new_string(uc, text),
        Value::Data(bytes) => uc.new_object("NSData", Payload::Data(bytes.clone())),
        Value::Date(date) => uc.new_object("NSDate", Payload::Date(*date)),
        Value::Boolean(flag) => uc.new_object("NSNumber", Payload::Bool(*flag)),
        Value::Integer(number) => uc.new_object("NSNumber", Payload::Int(*number)),
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_precision_loss,
            reason = "Go's integral float64 rule"
        )]
        Value::Real(number) => {
            if number.fract() == 0.0 && *number >= i64::MIN as f64 && *number < i64::MAX as f64 {
                uc.new_object("NSNumber", Payload::Int(*number as i64))
            } else {
                uc.new_object("NSNumber", Payload::Float(*number))
            }
        }
        Value::Array(items) => {
            let items = items
                .iter()
                .map(|item| import_value(uc, item))
                .collect::<Result<Vec<_>>>()?;
            uc.new_object("NSArray", Payload::Array(items))
        }
        Value::Dictionary(map) => {
            let mut result = BTreeMap::new();
            for (key, item) in map {
                result.insert(key.clone(), import_value(uc, item)?);
            }
            uc.new_object("NSDictionary", Payload::Dict(result))
        }
        Value::Other(kind) => fail!("unsupported Foundation import {kind}"),
    }
}

/// Go's `parserError`: `NSCocoaErrorDomain` 3840 for a JSON or property-list parse failure.
#[allow(dead_code, reason = "network.rs uses it")]
pub(crate) fn parser_error(uc: &mut Uc, out: u64) -> Result<()> {
    if out == 0 {
        return Ok(());
    }
    let error = new_error(uc, "NSCocoaErrorDomain", 3840, BTreeMap::new())?;
    uc.write64(out, error)
}
