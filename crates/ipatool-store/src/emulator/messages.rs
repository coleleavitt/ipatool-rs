//! Go `bridge.go`, messages: Objective-C sends to Foundation objects the pinned frameworks
//! expect from the host, after native methods in the image.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::bridge::{
    bundle, bundle_info, character_set_contains, cocoa_error, collection_key, foundation_copy,
    foundation_equal, new_uuid_string,
};
use super::callbacks::{callback, callback_function, copy_block};
use super::runtime::{
    HEAP_BASE, REG_D0, REG_SP, REG_X1, REG_X2, REG_X8, STACK_BASE, STACK_SIZE, conversion,
    next_pending, pending_callback, queue_pending,
};
use super::value::Date;
use super::{Guest, Payload, Result, Uc, fail, method_key};
use crate::gourl;

fn new_string(uc: &mut Uc, text: impl Into<String>) -> Result<u64> {
    uc.new_object("NSString", Payload::String(text.into()))
}

fn set_new(uc: &mut Uc, class: &str, value: Payload) -> Result<()> {
    let object = uc.new_object(class, value)?;
    uc.set(object)
}

fn set_string(uc: &mut Uc, text: impl Into<String>) -> Result<()> {
    let object = new_string(uc, text)?;
    uc.set(object)
}

fn set_property(uc: &mut Uc, object: u64, key: &str, value: u64) -> Result<()> {
    uc.obj_mut(object)?.properties.insert(key.to_owned(), value);
    Ok(())
}

/// The property name a one-argument `setX:` selector assigns (Go's inline key rule).
pub(crate) fn setter_key(selector: &str) -> Option<String> {
    let key = selector.strip_prefix("set")?.strip_suffix(':')?;
    if selector.matches(':').count() != 1 {
        return None;
    }
    let bytes = key.as_bytes();
    if bytes.len() < 2 || !bytes[1].is_ascii_uppercase() {
        let mut chars = key.chars();
        return Some(match chars.next() {
            Some(first) => first.to_lowercase().chain(chars).collect(),
            None => String::new(),
        });
    }
    Some(key.to_owned())
}

fn d0(uc: &Uc) -> Result<f64> {
    Ok(f64::from_bits(uc.reg(REG_D0)?))
}

/// Go's `strconv.ParseInt(v, 10, 64)` result, ignoring the error as the bridge does.
pub(crate) fn go_parse_int(text: &str) -> i64 {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return 0;
    }
    text.parse().unwrap_or(if text.starts_with('-') {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// Go's `path.Clean`.
pub(crate) fn go_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_owned(),
        (false, false) => joined,
    }
}

/// Go's `path.Join` of two elements.
fn go_join(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (true, true) => String::new(),
        (true, false) => go_clean(second),
        (false, true) => go_clean(first),
        (false, false) => go_clean(&format!("{first}/{second}")),
    }
}

/// Go's `path.Ext`.
fn go_ext(path: &str) -> &str {
    for (index, byte) in path.bytes().enumerate().rev() {
        match byte {
            b'/' => return "",
            b'.' => return &path[index..],
            _ => {}
        }
    }
    ""
}

/// Go's `path.Base`.
fn go_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
}

/// Go's `strings.Split`, which splits into UTF-8 sequences for an empty separator.
fn go_split(text: &str, separator: &str) -> Vec<String> {
    if separator.is_empty() {
        return text.chars().map(String::from).collect();
    }
    text.split(separator).map(str::to_owned).collect()
}

fn utf16_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn compare(left: &str, right: &str) -> u64 {
    #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int)")]
    let value = match left.cmp(right) {
        std::cmp::Ordering::Less => -1i64,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    } as u64;
    value
}

/// Go's `message`: Objective-C dispatch for `receiver`.
#[allow(clippy::too_many_lines, reason = "Go's dispatch order")]
pub(crate) fn message(uc: &mut Uc, receiver: u64, selector: &str, super_: bool) -> Result<()> {
    if receiver == 0 {
        uc.set_reg(REG_X1, 0)?;
        for register in REG_D0..REG_D0 + 4 {
            uc.set_reg(register, 0)?;
        }
        return uc.set(0);
    }
    let meta = uc.sti().class_names.contains_key(&receiver);
    let class = if meta {
        uc.sti().class_names[&receiver].clone()
    } else if let Some(object) = uc.sti().objects.get(&receiver) {
        object.class.clone()
    } else {
        if selector == "copy" && (HEAP_BASE..STACK_BASE + STACK_SIZE).contains(&receiver) {
            return copy_block(uc, receiver, Box::new(|uc, copied| uc.set(copied)));
        }
        fail!("unknown receiver {receiver:#x} for {selector}");
    };
    if class == "HostUnavailableProxy" {
        let error = cocoa_error(uc, "NSCocoaErrorDomain", 4099, BTreeMap::new())?;
        let block = uc.obj(receiver)?.prop("errorBlock");
        return callback(uc, block, &[error], Box::new(|uc| uc.set(0)));
    }
    // Logging is a host service; it must not pull platform logging infrastructure in.
    if class == "AMSLogConfig"
        || (class == "AMSConcurrency" && selector == "logFaultFromSwiftConcurrencyOnInternalBuild")
    {
        return uc.set(0);
    }
    // Store tasks use keep-alives to request host process assertions. The guest runs
    // synchronously inside the CLI, so retaining a scoped token is enough.
    if class == "AMSKeepAlive" {
        match selector {
            "keepAliveWithName:style:" | "rbs_keepAliveWithName:style:" if meta => {
                let value = uc.new_object("AMSKeepAlive", Payload::Nil)?;
                let (name, style) = (uc.arg(2)?, uc.arg(3)?);
                set_property(uc, value, "name", name)?;
                set_property(uc, value, "style", style)?;
                return uc.set(value);
            }
            "invalidate" | "rbs_invalidate" => return uc.set(0),
            "name" | "style" => {
                let value = uc.obj(receiver)?.prop(selector);
                return uc.set(value);
            }
            _ => {}
        }
    }
    let entry = uc.lookup(&class, selector, meta);
    if !super_ && entry != 0 {
        return uc.jump(entry, receiver, selector);
    }
    if super::notifications::notification_message(uc, receiver, &class, selector, meta)? {
        return Ok(());
    }
    if super::accounts::account_message(uc, receiver, &class, selector, meta)? {
        return Ok(());
    }
    if super::network::network_message(uc, receiver, &class, selector, meta)? {
        return Ok(());
    }
    if !meta
        && matches!(
            class.as_str(),
            "NSURLSessionConfiguration" | "NSOperationQueue" | "NSCache"
        )
    {
        if let Some(key) = setter_key(selector) {
            let value = uc.arg(2)?;
            set_property(uc, receiver, &key, value)?;
            return uc.set(0);
        }
        if let Some(&value) = uc.obj(receiver)?.properties.get(selector) {
            return uc.set(value);
        }
    }
    if selector == "initWithObjectsAndKeys:"
        || (meta && selector == "dictionaryWithObjectsAndKeys:")
    {
        let mut values = BTreeMap::new();
        let mut value = uc.arg(2)?;
        let sp = uc.reg(REG_SP)?;
        let mut index = 0u64;
        while value != 0 {
            if index > 1024 {
                fail!("dictionary varargs exceeds budget");
            }
            let key = uc.read64(sp + index * 16)?;
            values.insert(collection_key(uc, key), value);
            value = uc.read64(sp + index * 16 + 8)?;
            index += 1;
        }
        if meta {
            return set_new(uc, &class, Payload::Dict(values));
        }
        uc.obj_mut(receiver)?.value = Payload::Dict(values);
        return uc.set(receiver);
    }
    if meta {
        return class_message(uc, receiver, &class, selector);
    }
    instance_message(uc, receiver, &class, selector)
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
fn class_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<()> {
    match selector {
        "instanceMethodForSelector:" => {
            let name = uc.cstring(uc.arg(2)?)?;
            let entry = uc.lookup(class, &name, false);
            uc.set(entry)
        }
        "alloc" | "allocWithZone:" => set_new(uc, class, Payload::Nil),
        "new" => {
            let object = uc.new_object(class, Payload::Nil)?;
            uc.set(object)?;
            message(uc, object, "init", false)
        }
        "isSubclassOfClass:" => {
            let target = uc
                .sti()
                .class_names
                .get(&uc.arg(2)?)
                .cloned()
                .unwrap_or_default();
            let value = uc.kind_of(class, &target);
            uc.set(u64::from(value))
        }
        "class" => uc.set(receiver),
        "setWithObjects:" | "arrayWithObjects:" => {
            let mut items = Vec::new();
            let first = uc.arg(2)?;
            if first != 0 {
                items.push(first);
                let sp = uc.reg(REG_SP)?;
                for index in 0..1024 {
                    let value = uc.read64(sp + index * 8)?;
                    if value == 0 {
                        break;
                    }
                    items.push(value);
                }
            }
            set_new(uc, class, Payload::Array(items))
        }
        "dictionaryWithDictionary:" => {
            let values = uc.dict(uc.arg(2)?)?.clone();
            set_new(uc, class, Payload::Dict(values))
        }
        "fileURLWithPath:" | "fileURLWithPath:isDirectory:" => {
            let path = uc.string(uc.arg(2)?)?;
            let url = gourl::Url {
                scheme: "file".to_owned(),
                path,
                ..gourl::Url::default()
            };
            set_new(uc, "NSURL", Payload::String(url.to_string()))
        }
        "URLWithString:" => {
            let text = uc.string(uc.arg(2)?)?;
            set_new(uc, "NSURL", Payload::String(text))
        }
        "setWithObject:" | "arrayWithObject:" => {
            let item = uc.arg(2)?;
            set_new(uc, class, Payload::Array(vec![item]))
        }
        "setWithArray:" => {
            let source = uc.arg(2)?;
            let items = if source == 0 {
                Vec::new()
            } else {
                uc.array(source)?.clone()
            };
            set_new(uc, class, Payload::Array(items))
        }
        "UUID" => set_new(uc, "NSUUID", Payload::String(new_uuid_string()?)),
        "distantPast" => set_new(uc, "NSDate", Payload::Date(Date::unix(-62_135_769_600, 0))),
        "distantFuture" => set_new(
            uc,
            "NSDate",
            Payload::Date(Date::from_civil(4001, 1, 1, 0, 0, 0)),
        ),
        "dateWithTimeIntervalSinceNow:" | "dateWithTimeIntervalSince1970:" => {
            let mut seconds = d0(uc)?;
            if !seconds.is_finite() {
                fail!("unsupported date interval");
            }
            if selector == "dateWithTimeIntervalSinceNow:" {
                seconds += Date::now().interval();
            }
            #[allow(
                clippy::cast_precision_loss,
                reason = "Go compares with float64 bounds"
            )]
            if seconds <= i64::MIN as f64 || seconds >= i64::MAX as f64 {
                fail!("date interval exceeds supported range");
            }
            set_new(uc, "NSDate", Payload::Date(Date::from_interval(seconds)))
        }
        "date" | "now" => set_new(uc, "NSDate", Payload::Date(Date::now())),
        "defaultSessionConfiguration" | "ephemeralSessionConfiguration" => {
            let object = uc.new_object("NSURLSessionConfiguration", Payload::Nil)?;
            let classes = uc.new_object("NSArray", Payload::Array(Vec::new()))?;
            uc.obj_mut(object)?.properties = HashMap::from([
                ("usesClassicLoadingMode".to_owned(), 1),
                ("protocolClasses".to_owned(), classes),
                ("HTTPShouldSetCookies".to_owned(), 1),
                ("HTTPMaximumConnectionsPerHost".to_owned(), 6),
            ]);
            uc.set(object)
        }
        "interfaceWithProtocol:" => set_new(uc, "NSXPCInterface", Payload::Nil),
        "currentThread" => set_new(uc, "NSThread", Payload::Nil),
        "isMainThread" => uc.set(1),
        "null" => {
            if uc.sti().null_object == 0 {
                let null = uc.new_object("NSNull", Payload::Nil)?;
                uc.st().null_object = null;
            }
            let null = uc.sti().null_object;
            uc.set(null)
        }
        "standardUserDefaults" => set_new(uc, "NSUserDefaults", Payload::Dict(BTreeMap::new())),
        "preferredLanguages" => {
            let language = new_string(uc, "en-US")?;
            set_new(uc, "NSArray", Payload::Array(vec![language]))
        }
        "decimalDigitCharacterSet" => set_new(
            uc,
            "NSCharacterSet",
            Payload::String("0123456789".to_owned()),
        ),
        "characterSetWithCharactersInString:" => {
            let text = uc.string(uc.arg(2)?)?;
            set_new(uc, "NSCharacterSet", Payload::String(text))
        }
        "currentLocale" | "systemLocale" => {
            set_new(uc, "NSLocale", Payload::String("en_US".to_owned()))
        }
        "defaultManager" => set_new(uc, "NSFileManager", Payload::Nil),
        "bundleWithPath:" => {
            let path = uc.arg(2)?;
            if uc.string(path)? != "/System/Library/Frameworks/WebKit.framework" {
                return uc.set(0);
            }
            let object = uc.new_object("NSBundle", Payload::Nil)?;
            let version = new_string(uc, "22625.1.29.11.28")?;
            let info = uc.new_object(
                "NSDictionary",
                Payload::Dict(BTreeMap::from([("CFBundleVersion".to_owned(), version)])),
            )?;
            uc.obj_mut(object)?.properties = HashMap::from([
                ("bundlePath".to_owned(), path),
                ("infoDictionary".to_owned(), info),
            ]);
            uc.set(object)
        }
        "bundleForClass:" => {
            let object = bundle(uc)?;
            let name = uc
                .sti()
                .class_names
                .get(&uc.arg(2)?)
                .cloned()
                .unwrap_or_default();
            if name.contains("AMS") {
                let identifier = new_string(uc, "com.apple.AppleMediaServices")?;
                set_property(uc, object, "bundleIdentifier", identifier)?;
            }
            uc.set(object)
        }
        "mainBundle" => {
            let object = bundle(uc)?;
            uc.set(object)
        }
        "processInfo" => {
            let object = uc.new_object("NSProcessInfo", Payload::Nil)?;
            let name = new_string(uc, "ipatool")?;
            let environment = uc.new_object("NSDictionary", Payload::Dict(BTreeMap::new()))?;
            uc.obj_mut(object)?.properties = HashMap::from([
                ("processName".to_owned(), name),
                ("environment".to_owned(), environment),
            ]);
            uc.set(object)
        }
        "dictionary" | "dictionaryWithCapacity:" => {
            set_new(uc, class, Payload::Dict(BTreeMap::new()))
        }
        "weakToStrongObjectsMapTable"
        | "weakToWeakObjectsMapTable"
        | "strongToWeakObjectsMapTable"
        | "strongToStrongObjectsMapTable" => {
            set_new(uc, "NSMapTable", Payload::Dict(BTreeMap::new()))
        }
        "set" | "setWithCapacity:" | "array" | "arrayWithCapacity:" => {
            set_new(uc, class, Payload::Array(Vec::new()))
        }
        "numberWithDouble:" | "numberWithFloat:" => {
            let bits = uc.reg(REG_D0)?;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the low 32 bits hold the float"
            )]
            let value = if selector == "numberWithFloat:" {
                f64::from(f32::from_bits(bits as u32))
            } else {
                f64::from_bits(bits)
            };
            set_new(uc, "NSNumber", Payload::Float(value))
        }
        "numberWithBool:" => {
            let value = uc.arg(2)? != 0;
            set_new(uc, "NSNumber", Payload::Bool(value))
        }
        "numberWithInteger:"
        | "numberWithInt:"
        | "numberWithUnsignedInteger:"
        | "numberWithLongLong:"
        | "numberWithUnsignedLongLong:" => {
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let value = uc.arg(2)? as i64;
            set_new(uc, "NSNumber", Payload::Int(value))
        }
        "dictionaryWithObjects:forKeys:count:" => {
            let (values, keys, count) = (uc.arg(2)?, uc.arg(3)?, uc.arg(4)?);
            if count > 65536 {
                fail!("guest dictionary exceeds runtime limit");
            }
            let mut map = BTreeMap::new();
            for index in 0..count {
                let key = uc.string(uc.read64(keys + index * 8)?)?;
                map.insert(key, uc.read64(values + index * 8)?);
            }
            set_new(uc, "NSDictionary", Payload::Dict(map))
        }
        "setWithObjects:count:" | "arrayWithObjects:count:" => {
            let (values, count) = (uc.arg(2)?, uc.arg(3)?);
            if count > 65536 {
                fail!("guest array exceeds runtime limit");
            }
            let items = (0..count)
                .map(|index| uc.read64(values + index * 8))
                .collect::<Result<Vec<_>>>()?;
            set_new(uc, class, Payload::Array(items))
        }
        "stringWithFormat:" => {
            let format = uc.string(uc.arg(2)?)?;
            let text = format_varargs(uc, &format)?;
            set_string(uc, text)
        }
        "stringWithUTF8String:" => {
            let text = uc.cstring(uc.arg(2)?)?;
            set_string(uc, text)
        }
        "errorWithDomain:code:userInfo:" => {
            let domain = uc.arg(2)?;
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let code = uc.new_object("NSNumber", Payload::Int(uc.arg(3)? as i64))?;
            let info = uc.arg(4)?;
            let fields = BTreeMap::from([
                ("domain".to_owned(), domain),
                ("code".to_owned(), code),
                ("userInfo".to_owned(), info),
            ]);
            set_new(uc, "NSError", Payload::Dict(fields))
        }
        _ => fail!(
            "unsupported class message {}",
            method_key(class, selector, true)
        ),
    }
}

fn nil_object_value(uc: &Uc, receiver: u64) -> Result<Payload> {
    Ok(uc.obj(receiver)?.value.clone())
}

fn string_value(uc: &Uc, receiver: u64) -> Result<String> {
    uc.string(receiver)
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
fn instance_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<()> {
    match selector {
        "initWithMachServiceName:options:" => {
            let name = uc.arg(2)?;
            set_property(uc, receiver, "serviceName", name)?;
            uc.set(receiver)
        }
        "_setQueue:" => {
            let queue = uc.arg(2)?;
            set_property(uc, receiver, "queue", queue)?;
            uc.set(0)
        }
        "setRemoteObjectInterface:" => {
            let interface = uc.arg(2)?;
            set_property(uc, receiver, "interface", interface)?;
            uc.set(0)
        }
        "setInterruptionHandler:" | "setInvalidationHandler:" => {
            let key = selector.to_owned();
            copy_block(
                uc,
                uc.arg(2)?,
                Box::new(move |uc, block| {
                    set_property(uc, receiver, &key, block)?;
                    uc.set(0)
                }),
            )
        }
        "remoteObjectProxyWithErrorHandler:" => copy_block(
            uc,
            uc.arg(2)?,
            Box::new(|uc, block| {
                let proxy = uc.new_object("HostUnavailableProxy", Payload::Nil)?;
                set_property(uc, proxy, "errorBlock", block)?;
                uc.set(proxy)
            }),
        ),
        "resume"
        | "invalidate"
        | "setClass:forSelector:argumentIndex:ofReply:"
        | "setClasses:forSelector:argumentIndex:ofReply:"
        | "setInterface:forSelector:argumentIndex:ofReply:" => {
            if class != "NSXPCConnection" && class != "NSXPCInterface" {
                fail!("unsupported message {class} {selector}");
            }
            uc.set(0)
        }
        "addOperationWithBlock:" => copy_block(
            uc,
            uc.arg(2)?,
            Box::new(move |uc, block| {
                queue_pending(uc, block, receiver);
                uc.set(0)
            }),
        ),
        "lock" | "unlock" | "signal" | "broadcast" => {
            if !matches!(class, "NSCondition" | "NSLock" | "NSRecursiveLock") {
                fail!("unsupported lock receiver {class}");
            }
            uc.set(0)
        }
        "wait" | "waitUntilDate:" => {
            if class != "NSCondition" || uc.sti().pending.is_empty() {
                fail!("guest condition has no runnable callback");
            }
            let block = next_pending(uc);
            if block == 0 {
                fail!("all pending queues are suspended");
            }
            pending_callback(uc, block, Box::new(|uc| uc.set(1)))
        }
        "createDirectoryAtURL:withIntermediateDirectories:attributes:error:"
        | "createDirectoryAtPath:withIntermediateDirectories:attributes:error:" => {
            let out = uc.arg(5)?;
            if out != 0 {
                let error = cocoa_error(uc, "NSCocoaErrorDomain", 513, BTreeMap::new())?;
                uc.write64(out, error)?;
            }
            uc.set(0)
        }
        "isWritableFileAtPath:" | "fileExistsAtPath:" | "fileExistsAtPath:isDirectory:" => {
            if selector == "fileExistsAtPath:isDirectory:" && uc.arg(3)? != 0 {
                let _ = uc.write_mem(uc.arg(3)?, &[0]);
            }
            uc.set(0)
        }
        "URLsForDirectory:inDomains:" => {
            if class != "NSFileManager" {
                fail!("invalid file-manager receiver");
            }
            let url = uc.new_object(
                "NSURL",
                Payload::String("file:///ipatool/cache/".to_owned()),
            )?;
            set_new(uc, "NSArray", Payload::Array(vec![url]))
        }
        "URLForDirectory:inDomain:appropriateForURL:create:error:" => set_new(
            uc,
            "NSURL",
            Payload::String("file:///ipatool/cache/".to_owned()),
        ),
        "threadDictionary" => {
            if uc.obj(receiver)?.prop(selector) == 0 {
                let dictionary =
                    uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::new()))?;
                set_property(uc, receiver, selector, dictionary)?;
            }
            let value = uc.obj(receiver)?.prop(selector);
            uc.set(value)
        }
        "boolForKey:" => {
            let key = uc.string(uc.arg(2)?)?;
            let value = uc.dict(receiver)?.get(&key).copied().unwrap_or_default();
            if value == 0 {
                uc.set(0)
            } else {
                message(uc, value, "boolValue", false)
            }
        }
        "operatingSystemVersion" => {
            let out = uc.reg(REG_X8)?;
            uc.write64(out, 27)?;
            uc.write64(out + 8, 0)?;
            uc.write64(out + 16, 1)
        }
        "operatingSystemVersionString" => set_string(uc, "Version 27.0.1 (Build 26A434)"),
        "initWithTimeIntervalSince1970:" => {
            if class != "NSDate" {
                fail!("unsupported date initializer");
            }
            let date = Date::from_interval(d0(uc)?);
            uc.obj_mut(receiver)?.value = Payload::Date(date);
            uc.set(receiver)
        }
        "initWithBytesNoCopy:length:encoding:freeWhenDone:" => {
            let (bytes, length, encoding) = (uc.arg(2)?, uc.arg(3)?, uc.arg(4)?);
            if !uc.kind_of(class, "NSString")
                || (encoding != 4 && encoding != 1)
                || length > 1 << 20
            {
                fail!("unsupported string buffer initialization");
            }
            let data = uc.read_mem(bytes, length)?;
            uc.obj_mut(receiver)?.value =
                Payload::String(String::from_utf8_lossy(&data).into_owned());
            uc.set(receiver)
        }
        "initWithBytesNoCopy:length:freeWhenDone:" => {
            let (bytes, length) = (uc.arg(2)?, uc.arg(3)?);
            if !uc.kind_of(class, "NSData") || length > 1 << 20 {
                fail!("unsupported data buffer initialization");
            }
            let data = uc.read_mem(bytes, length)?;
            uc.obj_mut(receiver)?.value = Payload::Data(data);
            uc.set(receiver)
        }
        "timeIntervalSinceDate:" => {
            let date = date_value(uc, receiver)?;
            let other = date_value(uc, uc.arg(2)?)?;
            uc.set_reg(REG_D0, date.seconds_since(other).to_bits())
        }
        "timeIntervalSinceNow" | "timeIntervalSince1970" | "timeIntervalSinceReferenceDate" => {
            let date = date_value(uc, receiver)?;
            let seconds = match selector {
                "timeIntervalSinceNow" => date.seconds_since(Date::now()),
                "timeIntervalSinceReferenceDate" => date.interval() - 978_307_200.0,
                _ => date.interval(),
            };
            uc.set_reg(REG_D0, seconds.to_bits())
        }
        "localizedStringForKey:value:table:" => {
            if class != "NSBundle" {
                fail!("unsupported localization receiver");
            }
            // No bundle resource files are mounted. Foundation's documented fallback is the
            // supplied value, then the key itself.
            let value = uc.arg(3)?;
            if value != 0 && !uc.string(value)?.is_empty() {
                uc.set(value)
            } else {
                let key = uc.arg(2)?;
                uc.set(key)
            }
        }
        "localeIdentifier" => set_string(uc, "en_US"),
        "languageCode" => set_string(uc, "en"),
        "infoDictionary" => {
            let value = uc.obj(receiver)?.prop("infoDictionary");
            let value = if value == 0 { bundle_info(uc)? } else { value };
            uc.set(value)
        }
        "executableURL" | "bundleURL" => {
            set_new(uc, "NSURL", Payload::String("file:///ipatool".to_owned()))
        }
        "bundleIdentifier" | "bundlePath" | "processName" | "environment" => {
            let value = uc.obj(receiver)?.prop(selector);
            uc.set(value)
        }
        "objectForInfoDictionaryKey:" => {
            let mut info = uc.obj(receiver)?.prop("infoDictionary");
            if info == 0 {
                info = bundle_info(uc)?;
            }
            let key = uc.string(uc.arg(2)?)?;
            let value = uc.dict(info)?.get(&key).copied().unwrap_or_default();
            uc.set(value)
        }
        "initWithCapacity:" => {
            let value = if class == "NSMutableString" {
                Payload::String(String::new())
            } else if class.contains("Array") {
                Payload::Array(Vec::new())
            } else {
                Payload::Dict(BTreeMap::new())
            };
            uc.obj_mut(receiver)?.value = value;
            uc.set(receiver)
        }
        "appendFormat:" => {
            let format = uc.string(uc.arg(2)?)?;
            let text = string_value(uc, receiver)? + &format_varargs(uc, &format)?;
            uc.obj_mut(receiver)?.value = Payload::String(text);
            uc.set(0)
        }
        "init" => {
            let object = uc.obj_mut(receiver)?;
            if matches!(object.value, Payload::Nil) {
                object.value = match class {
                    "NSString" | "NSMutableString" => Payload::String(String::new()),
                    "NSCache" | "NSDictionary" | "NSMutableDictionary" => {
                        Payload::Dict(BTreeMap::new())
                    }
                    "NSArray"
                    | "NSMutableArray"
                    | "NSSet"
                    | "NSMutableSet"
                    | "NSOrderedSet"
                    | "NSMutableOrderedSet" => Payload::Array(Vec::new()),
                    _ => Payload::Nil,
                };
            }
            uc.set(receiver)
        }
        "containsObject:" => {
            let target = uc.arg(2)?;
            let wanted = uc
                .sti()
                .objects
                .get(&target)
                .map(|object| object.value.sprint());
            let matched = uc.array(receiver)?.iter().any(|&item| {
                item == target
                    || wanted.as_ref().is_some_and(|wanted| {
                        uc.sti()
                            .objects
                            .get(&item)
                            .is_some_and(|object| &object.value.sprint() == wanted)
                    })
            });
            uc.set(u64::from(matched))
        }
        "UUIDString" => {
            let value = nil_object_value(uc, receiver)?;
            set_new(uc, "NSString", value)
        }
        "initWithFormat:" => {
            let format = uc.string(uc.arg(2)?)?;
            let text = format_varargs(uc, &format)?;
            uc.obj_mut(receiver)?.value = Payload::String(text);
            uc.set(receiver)
        }
        "initWithFormat:locale:arguments:" => {
            let format = uc.string(uc.arg(2)?)?;
            let text = format_at(uc, &format, uc.arg(4)?)?;
            uc.obj_mut(receiver)?.value = Payload::String(text);
            uc.set(receiver)
        }
        "initWithString:" => {
            let text = uc.string(uc.arg(2)?)?;
            uc.obj_mut(receiver)?.value = Payload::String(text);
            uc.set(receiver)
        }
        _ => collection_message(uc, receiver, class, selector),
    }
}

fn date_value(uc: &Uc, address: u64) -> Result<Date> {
    match &uc.obj(address)?.value {
        Payload::Date(date) => Ok(*date),
        other => Err(conversion(other, "time.Time")),
    }
}

fn sorted_keys(map: &BTreeMap<String, u64>) -> Vec<String> {
    map.keys().cloned().collect()
}

struct Sort {
    items: Vec<u64>,
    block: u64,
    i: usize,
    j: usize,
}

/// Go's comparator-driven insertion sort for `sortedArrayUsingComparator:`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "each callback owns the state"
)]
fn sort_next(uc: &mut Uc, state: Rc<RefCell<Sort>>) -> Result<()> {
    loop {
        let mut sort = state.borrow_mut();
        if sort.i >= sort.items.len() {
            let items = std::mem::take(&mut sort.items);
            drop(sort);
            return set_new(uc, "NSArray", Payload::Array(items));
        }
        if sort.j == 0 {
            sort.i += 1;
            sort.j = sort.i;
            continue;
        }
        let (left, right, block) = (sort.items[sort.j - 1], sort.items[sort.j], sort.block);
        drop(sort);
        let next = Rc::clone(&state);
        return callback(
            uc,
            block,
            &[left, right],
            Box::new(move |uc| {
                #[allow(clippy::cast_possible_wrap, reason = "NSComparisonResult is signed")]
                let ordered = uc.arg(0)? as i64;
                {
                    let mut sort = next.borrow_mut();
                    if ordered > 0 {
                        let j = sort.j;
                        sort.items.swap(j - 1, j);
                        sort.j -= 1;
                    } else {
                        sort.i += 1;
                        sort.j = sort.i;
                    }
                }
                sort_next(uc, next)
            }),
        );
    }
}

fn enumerate_next(
    uc: &mut Uc,
    receiver: u64,
    keys: Rc<Vec<String>>,
    index: usize,
    block: u64,
    stop: u64,
) -> Result<()> {
    if index == keys.len() || uc.read64(stop)? != 0 {
        return uc.set(0);
    }
    let key = new_string(uc, keys[index].clone())?;
    let value = uc
        .dict(receiver)?
        .get(&keys[index])
        .copied()
        .unwrap_or_default();
    callback(
        uc,
        block,
        &[key, value, stop],
        Box::new(move |uc| enumerate_next(uc, receiver, keys, index + 1, block, stop)),
    )
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
fn collection_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<()> {
    match selector {
        "sortedArrayUsingComparator:" => {
            let items = uc.array(receiver)?.clone();
            if items.len() > 4096 {
                fail!("sort exceeds runtime budget");
            }
            let state = Rc::new(RefCell::new(Sort {
                items,
                block: uc.arg(2)?,
                i: 1,
                j: 1,
            }));
            sort_next(uc, state)
        }
        "compare:" => {
            if let Payload::Date(date) = uc.obj(receiver)?.value {
                let other = date_value(uc, uc.arg(2)?)?;
                #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int64(Compare))")]
                let order = date.cmp(&other) as i64 as u64;
                return uc.set(order);
            }
            let value = compare(&string_value(uc, receiver)?, &uc.string(uc.arg(2)?)?);
            uc.set(value)
        }
        "allObjects" => {
            let items = uc.array(receiver)?.clone();
            set_new(uc, "NSArray", Payload::Array(items))
        }
        "firstObject" | "lastObject" => {
            let items = uc.array(receiver)?;
            let value = if selector == "firstObject" {
                items.first()
            } else {
                items.last()
            };
            let value = value.copied().unwrap_or_default();
            uc.set(value)
        }
        "objectAtIndex:" | "objectAtIndexedSubscript:" => {
            let index = uc.arg(2)?;
            let items = uc.array(receiver)?;
            let Some(&value) = usize::try_from(index)
                .ok()
                .and_then(|index| items.get(index))
            else {
                fail!(
                    "runtime error: index out of range [{index}] with length {}",
                    items.len()
                );
            };
            uc.set(value)
        }
        "removeObjectsInArray:" | "removeObject:" => {
            let target = uc.arg(2)?;
            if target == 0 {
                return uc.set(0);
            }
            let remove = if selector == "removeObjectsInArray:" {
                uc.array(target)?.clone()
            } else {
                vec![target]
            };
            let kept: Vec<u64> = uc
                .array(receiver)?
                .iter()
                .copied()
                .filter(|&item| {
                    !remove
                        .iter()
                        .any(|&other| foundation_equal(uc, item, other))
                })
                .collect();
            uc.obj_mut(receiver)?.value = Payload::Array(kept);
            uc.set(0)
        }
        "addObjectsFromArray:" => {
            let mut items = uc.array(receiver)?.clone();
            items.extend(uc.array(uc.arg(2)?)?);
            uc.obj_mut(receiver)?.value = Payload::Array(items);
            uc.set(0)
        }
        "removeObjectAtIndex:" => {
            let index = uc.arg(2)?;
            let mut items = uc.array(receiver)?.clone();
            if index >= items.len() as u64 {
                fail!("array removal index out of bounds");
            }
            items.remove(usize::try_from(index).unwrap_or_default());
            uc.obj_mut(receiver)?.value = Payload::Array(items);
            uc.set(0)
        }
        "addObject:" => {
            let mut items = uc.array(receiver)?.clone();
            items.push(uc.arg(2)?);
            uc.obj_mut(receiver)?.value = Payload::Array(items);
            uc.set(0)
        }
        "path" | "lastPathComponent" => {
            let Payload::String(mut value) = nil_object_value(uc, receiver)? else {
                return Err(conversion(&uc.obj(receiver)?.value, "string"));
            };
            if class == "NSURL" {
                value = gourl::parse(&value)
                    .map_err(|error| super::Error::msg(error.to_string()))?
                    .path;
            }
            if selector == "lastPathComponent" {
                value = go_base(&value);
            }
            set_string(uc, value)
        }
        "absoluteString" => match nil_object_value(uc, receiver)? {
            Payload::String(text) => set_string(uc, text),
            other => Err(conversion(&other, "string")),
        },
        "stringValue" => {
            let text = uc.obj(receiver)?.value.sprint();
            set_string(uc, text)
        }
        "description" => {
            if !uc.kind_of(class, "NSString") {
                fail!(
                    "unsupported instance message {}",
                    method_key(class, selector, false)
                );
            }
            uc.set(receiver)
        }
        "lowercaseString" => {
            let text = string_value(uc, receiver)?.to_lowercase();
            set_string(uc, text)
        }
        "uppercaseString" => {
            let text = string_value(uc, receiver)?.to_uppercase();
            set_string(uc, text)
        }
        "appendString:" => {
            let text = string_value(uc, receiver)? + &uc.string(uc.arg(2)?)?;
            uc.obj_mut(receiver)?.value = Payload::String(text);
            uc.set(0)
        }
        "stringByAppendingString:" => {
            let text = string_value(uc, receiver)? + &uc.string(uc.arg(2)?)?;
            set_string(uc, text)
        }
        "initWithDomain:code:userInfo:" => {
            let domain = uc.arg(2)?;
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            let code = uc.new_object("NSNumber", Payload::Int(uc.arg(3)? as i64))?;
            let info = uc.arg(4)?;
            uc.obj_mut(receiver)?.value = Payload::Dict(BTreeMap::from([
                ("domain".to_owned(), domain),
                ("code".to_owned(), code),
                ("userInfo".to_owned(), info),
            ]));
            uc.set(receiver)
        }
        "copy" => {
            let entry = uc.lookup(class, "copyWithZone:", false);
            if entry == 0 {
                let copied = foundation_copy(uc, receiver, false)?;
                uc.set(copied)
            } else {
                uc.set_reg(REG_X2, 0)?;
                uc.jump(entry, receiver, "copyWithZone:")
            }
        }
        "retain" | "autorelease" | "copyWithZone:" => uc.set(receiver),
        "mutableCopy" | "mutableCopyWithZone:" => match nil_object_value(uc, receiver)? {
            Payload::Dict(map) => set_new(uc, "NSMutableDictionary", Payload::Dict(map)),
            value => set_new(uc, class, value),
        },
        "release" => uc.set(0),
        "class" => {
            let Some(address) = uc.sti().class(class).map(|class| class.address) else {
                fail!("{}", super::runtime::NIL_DEREFERENCE);
            };
            uc.set(address)
        }
        "isKindOfClass:" => {
            let target = uc
                .sti()
                .class_names
                .get(&uc.arg(2)?)
                .cloned()
                .unwrap_or_default();
            let value = uc.kind_of(class, &target);
            uc.set(u64::from(value))
        }
        "isEqualToDate:"
        | "isEqual:"
        | "isEqualToString:"
        | "isEqualToDictionary:"
        | "isEqualToArray:" => {
            let value = foundation_equal(uc, receiver, uc.arg(2)?);
            uc.set(u64::from(value))
        }
        "length" | "count" => {
            let count = match &uc.obj(receiver)?.value {
                Payload::String(text) => text.len(),
                Payload::Data(bytes) => bytes.len(),
                Payload::Array(items) => items.len(),
                Payload::Dict(map) => map.len(),
                _ => fail!("count of {class}"),
            };
            uc.set(count as u64)
        }
        "valueForKey:"
        | "__swift_objectForKeyedSubscript:"
        | "objectForKey:"
        | "objectForKeyedSubscript:" => {
            let key = collection_key(uc, uc.arg(2)?);
            let Payload::Dict(map) = &uc.obj(receiver)?.value else {
                fail!("dictionary access on {class}");
            };
            let value = map.get(&key).copied().unwrap_or_default();
            uc.set(value)
        }
        "setObject:forKey:cost:"
        | "setValue:forKey:"
        | "setObject:forKey:"
        | "setObject:forKeyedSubscript:" => {
            let key = collection_key(uc, uc.arg(3)?);
            let value = uc.arg(2)?;
            let Payload::Dict(map) = &mut uc.obj_mut(receiver)?.value else {
                fail!("dictionary mutation on {class}");
            };
            if value == 0 {
                map.remove(&key);
            } else {
                map.insert(key, value);
            }
            uc.set(0)
        }
        "countByEnumeratingWithState:objects:count:" => enumerate_fast(uc, receiver),
        "enumerateKeysAndObjectsUsingBlock:" => {
            let keys = Rc::new(sorted_keys(uc.dict(receiver)?));
            let block = uc.arg(2)?;
            let stop = uc.alloc(8)?;
            enumerate_next(uc, receiver, keys, 0, block, stop)
        }
        "allValues" => {
            let items = uc.dict(receiver)?.values().copied().collect();
            set_new(uc, "NSArray", Payload::Array(items))
        }
        "allKeys" => {
            let keys = sorted_keys(uc.dict(receiver)?);
            let items = keys
                .into_iter()
                .map(|key| new_string(uc, key))
                .collect::<Result<Vec<_>>>()?;
            set_new(uc, "NSArray", Payload::Array(items))
        }
        _ => scalar_message(uc, receiver, class, selector),
    }
}

fn enumerate_fast(uc: &mut Uc, receiver: u64) -> Result<()> {
    let (state, buffer, limit) = (uc.arg(2)?, uc.arg(3)?, uc.arg(4)?);
    if limit > 100_000 {
        fail!("enumeration limit too large");
    }
    let items = match uc.obj(receiver)?.value.clone() {
        Payload::Array(items) => items,
        Payload::Dict(map) => sorted_keys(&map)
            .into_iter()
            .map(|key| new_string(uc, key))
            .collect::<Result<_>>()?,
        _ => fail!("invalid collection"),
    };
    let index = uc.read64(state)?;
    if index >= items.len() as u64 {
        return uc.set(0);
    }
    let count = limit.min(items.len() as u64 - index);
    for offset in 0..count {
        uc.write64(
            buffer + offset * 8,
            items[usize::try_from(index + offset).unwrap_or_default()],
        )?;
    }
    if uc.obj(receiver)?.prop("mutationCounter") == 0 {
        let counter = uc.alloc(8)?;
        set_property(uc, receiver, "mutationCounter", counter)?;
    }
    let counter = uc.obj(receiver)?.prop("mutationCounter");
    uc.write64(state, index + count)?;
    uc.write64(state + 8, buffer)?;
    uc.write64(state + 16, counter)?;
    uc.set(count)
}

fn char_set(uc: &Uc, address: u64) -> Result<super::Object> {
    Ok(uc.obj(address)?.clone())
}

#[allow(clippy::too_many_lines, reason = "one arm per Go case")]
fn scalar_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<()> {
    match selector {
        "stringByAppendingPathComponent:" => {
            let text = go_join(&string_value(uc, receiver)?, &uc.string(uc.arg(2)?)?);
            set_string(uc, text)
        }
        "stringByDeletingPathExtension" => {
            let text = string_value(uc, receiver)?;
            let trimmed = text.strip_suffix(go_ext(&text)).unwrap_or(&text).to_owned();
            set_string(uc, trimmed)
        }
        "componentsJoinedByString:" => {
            let items = uc.array(receiver)?.clone();
            let parts = items
                .iter()
                .map(|&item| uc.string(item))
                .collect::<Result<Vec<_>>>()?;
            let separator = uc.string(uc.arg(2)?)?;
            set_string(uc, parts.join(&separator))
        }
        "componentsSeparatedByString:" => {
            let parts = go_split(&string_value(uc, receiver)?, &uc.string(uc.arg(2)?)?);
            let items = parts
                .into_iter()
                .map(|part| new_string(uc, part))
                .collect::<Result<Vec<_>>>()?;
            set_new(uc, "NSArray", Payload::Array(items))
        }
        "doubleValue" | "floatValue" => {
            let value = match &uc.obj(receiver)?.value {
                Payload::Float(value) => *value,
                #[allow(clippy::cast_precision_loss, reason = "Go's float64(int64)")]
                Payload::Int(value) => *value as f64,
                Payload::Bool(value) => f64::from(u8::from(*value)),
                Payload::String(text) => text.parse().unwrap_or_default(),
                _ => fail!("unsupported numeric conversion"),
            };
            #[allow(clippy::cast_possible_truncation, reason = "Go's float32(value)")]
            let bits = if selector == "floatValue" {
                u64::from((value as f32).to_bits())
            } else {
                value.to_bits()
            };
            uc.set_reg(REG_D0, bits)
        }
        "integerValue"
        | "intValue"
        | "longLongValue"
        | "unsignedIntegerValue"
        | "unsignedLongLongValue"
        | "boolValue" => {
            let value = match &uc.obj(receiver)?.value {
                Payload::Float(value) if selector == "boolValue" => i64::from(*value != 0.0),
                #[allow(clippy::cast_possible_truncation, reason = "Go's int64(float64)")]
                Payload::Float(value) => *value as i64,
                Payload::Int(value) => *value,
                Payload::String(text) => go_parse_int(text),
                Payload::Bool(value) => i64::from(*value),
                _ => 0,
            };
            #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int64)")]
            uc.set(value as u64)
        }
        "localizedDescription" | "localizedFailureReason" | "localizedRecoverySuggestion" => {
            if !uc.kind_of(class, "NSError") {
                fail!("unsupported localized error receiver");
            }
            let fields = uc.dict(receiver)?.clone();
            let key = match selector {
                "localizedDescription" => "NSLocalizedDescription",
                "localizedFailureReason" => "NSLocalizedFailureReason",
                _ => "NSLocalizedRecoverySuggestion",
            };
            let field = |name: &str| fields.get(name).copied().unwrap_or_default();
            let mut value = 0;
            if uc.sti().objects.contains_key(&field("userInfo")) {
                value = uc
                    .dict(field("userInfo"))?
                    .get(key)
                    .copied()
                    .unwrap_or_default();
            }
            if value == 0 && selector == "localizedDescription" {
                let domain = uc.string(field("domain"))?;
                let code = match uc.sti().objects.get(&field("code")) {
                    Some(object) => match &object.value {
                        Payload::Int(code) => code.to_string(),
                        other => format!("%!d({})", other.sprint()),
                    },
                    None => fail!("{}", super::runtime::NIL_DEREFERENCE),
                };
                value = new_string(uc, format!("{domain} error {code}"))?;
            }
            uc.set(value)
        }
        "domain" | "userInfo" | "code" => {
            let mut value = uc
                .dict(receiver)?
                .get(selector)
                .copied()
                .unwrap_or_default();
            if selector == "code" {
                value = match &uc.obj(value)?.value {
                    #[allow(clippy::cast_sign_loss, reason = "Go's uint64(int64)")]
                    Payload::Int(code) => *code as u64,
                    other => return Err(conversion(other, "int64")),
                };
            }
            uc.set(value)
        }
        "cStringUsingEncoding:" | "UTF8String" => {
            let mut bytes = string_value(uc, receiver)?.into_bytes();
            let address = uc.alloc(bytes.len() as u64 + 1)?;
            bytes.push(0);
            let _ = uc.write_mem(address, &bytes);
            uc.set(address)
        }
        "dataUsingEncoding:" => {
            let text = string_value(uc, receiver)?;
            let data = match uc.arg(2)? {
                // NSUTF8StringEncoding.
                4 => text.into_bytes(),
                // NSUnicodeStringEncoding: native arm64 byte order with a byte-order mark.
                10 => std::iter::once(0xfeff_u16)
                    .chain(text.encode_utf16())
                    .flat_map(u16::to_le_bytes)
                    .collect(),
                encoding => fail!("unsupported string encoding {encoding}"),
            };
            set_new(uc, "NSData", Payload::Data(data))
        }
        "base64EncodedStringWithOptions:" => {
            if !uc.kind_of(class, "NSData") {
                fail!("base64 encoding on {class}");
            }
            let text = STANDARD.encode(uc.data(receiver)?);
            set_string(uc, text)
        }
        "removeAllObjects" => {
            let object = uc.obj_mut(receiver)?;
            object.value = match object.value {
                Payload::Array(_) => Payload::Array(Vec::new()),
                Payload::Dict(_) => Payload::Dict(BTreeMap::new()),
                _ => fail!("removeAllObjects on {class}"),
            };
            uc.set(0)
        }
        "removeObjectForKey:" => {
            let key = collection_key(uc, uc.arg(2)?);
            uc.dict_mut(receiver)?.remove(&key);
            uc.set(0)
        }
        "addEntriesFromDictionary:" => {
            let entries = uc.dict(uc.arg(2)?)?.clone();
            uc.dict_mut(receiver)?.extend(entries);
            uc.set(0)
        }
        "initWithArray:" => {
            let items = uc.array(uc.arg(2)?)?.clone();
            uc.obj_mut(receiver)?.value = Payload::Array(items);
            uc.set(receiver)
        }
        "initWithDictionary:" => {
            let values = uc.dict(uc.arg(2)?)?.clone();
            uc.obj_mut(receiver)?.value = Payload::Dict(values);
            uc.set(receiver)
        }
        "initWithDictionary:copyItems:" => {
            initialize_dictionary(uc, receiver, uc.arg(2)?, uc.arg(3)? != 0)
        }
        "rangeOfString:options:" | "rangeOfString:" => {
            let (mut value, mut needle) = (string_value(uc, receiver)?, uc.string(uc.arg(2)?)?);
            let options = if selector == "rangeOfString:options:" {
                uc.arg(3)?
            } else {
                0
            };
            if options & 1 != 0 {
                (value, needle) = (value.to_lowercase(), needle.to_lowercase());
            }
            let index = if options & 4 != 0 {
                value.rfind(&needle)
            } else {
                value.find(&needle)
            };
            let (start, length) = match index {
                Some(index) => (utf16_len(&value[..index]), utf16_len(&needle)),
                None => (0x7fff_ffff_ffff_ffff, 0),
            };
            uc.set(start)?;
            uc.set_reg(REG_X1, length)
        }
        "invertedSet" => {
            let object = uc.obj(receiver)?.clone();
            let inverted = 1 - object.prop("inverted");
            let set = uc.new_object("NSCharacterSet", object.value)?;
            set_property(uc, set, "inverted", inverted)?;
            uc.set(set)
        }
        "stringByTrimmingCharactersInSet:" => {
            let set = char_set(uc, uc.arg(2)?)?;
            let text = string_value(uc, receiver)?;
            let mut failure = None;
            let trimmed = text.trim_matches(|c| match character_set_contains(&set, c) {
                Ok(found) => found,
                Err(error) => {
                    failure.get_or_insert(error);
                    false
                }
            });
            if let Some(error) = failure {
                return Err(error);
            }
            let trimmed = trimmed.to_owned();
            set_string(uc, trimmed)
        }
        "rangeOfCharacterFromSet:" | "rangeOfCharacterFromSet:options:" => {
            let set = char_set(uc, uc.arg(2)?)?;
            let options = if selector == "rangeOfCharacterFromSet:options:" {
                uc.arg(3)?
            } else {
                0
            };
            let (mut start, mut length, mut offset) = (0x7fff_ffff_ffff_ffff_u64, 0, 0);
            for c in string_value(uc, receiver)?.chars() {
                let width = c.len_utf16() as u64;
                if character_set_contains(&set, c)? {
                    (start, length) = (offset, width);
                    if options & 4 == 0 {
                        break;
                    }
                }
                offset += width;
            }
            uc.set(start)?;
            uc.set_reg(REG_X1, length)
        }
        "substringFromIndex:" | "substringToIndex:" | "substringWithRange:" => {
            let value: Vec<u16> = string_value(uc, receiver)?.encode_utf16().collect();
            let (mut start, mut end) = (0, value.len() as u64);
            match selector {
                "substringFromIndex:" => start = uc.arg(2)?,
                "substringToIndex:" => end = uc.arg(2)?,
                _ => {
                    start = uc.arg(2)?;
                    end = start.wrapping_add(uc.arg(3)?);
                }
            }
            if end > value.len() as u64 || start > end {
                fail!("substring range exceeds string");
            }
            let slice = &value[usize::try_from(start).unwrap_or_default()
                ..usize::try_from(end).unwrap_or_default()];
            set_string(uc, String::from_utf16_lossy(slice))
        }
        "hasSuffix:" | "containsString:" | "hasPrefix:" => {
            let (value, other) = (string_value(uc, receiver)?, uc.string(uc.arg(2)?)?);
            let found = match selector {
                "hasSuffix:" => value.ends_with(&other),
                "hasPrefix:" => value.starts_with(&other),
                _ => value.contains(&other),
            };
            uc.set(u64::from(found))
        }
        "caseInsensitiveCompare:" => {
            let value = compare(
                &string_value(uc, receiver)?.to_lowercase(),
                &uc.string(uc.arg(2)?)?.to_lowercase(),
            );
            uc.set(value)
        }
        _ => fail!(
            "unsupported instance message {}",
            method_key(class, selector, false)
        ),
    }
}

/// Go's `initializeDictionary`: `copyItems` copies each immediate value, using its native
/// `copyWithZone:` when available. Nested collection contents remain shared, as in Foundation.
fn initialize_dictionary(uc: &mut Uc, receiver: u64, source: u64, copy_items: bool) -> Result<()> {
    let values = uc.dict(source)?.clone();
    let keys = Rc::new(sorted_keys(&values));
    uc.obj_mut(receiver)?.value = Payload::Dict(values);
    if !copy_items {
        return uc.set(receiver);
    }
    copy_items_from(uc, receiver, keys, 0)
}

#[allow(clippy::needless_pass_by_value, reason = "each callback owns the keys")]
fn copy_items_from(
    uc: &mut Uc,
    receiver: u64,
    keys: Rc<Vec<String>>,
    mut index: usize,
) -> Result<()> {
    while index < keys.len() {
        let key = keys[index].clone();
        index += 1;
        let value = uc.dict(receiver)?.get(&key).copied().unwrap_or_default();
        let class = uc.obj(value)?.class.clone();
        let entry = uc.lookup(&class, "copyWithZone:", false);
        if entry != 0 {
            let selector = uc.selector("copyWithZone:")?;
            let keys = Rc::clone(&keys);
            return callback_function(
                uc,
                entry,
                &[value, selector, 0],
                Box::new(move |uc| {
                    let copied = uc.arg(0)?;
                    uc.dict_mut(receiver)?.insert(key, copied);
                    copy_items_from(uc, receiver, keys, index)
                }),
            );
        }
        let copied = foundation_copy(uc, value, false)?;
        uc.dict_mut(receiver)?.insert(key, copied);
    }
    uc.set(receiver)
}

const NUMBERS: [&str; 6] = [
    "integerValue",
    "intValue",
    "longLongValue",
    "unsignedIntegerValue",
    "unsignedLongLongValue",
    "boolValue",
];

/// Go's `responds`.
pub(crate) fn responds(uc: &Uc, receiver: u64, selector: &str) -> bool {
    if receiver == 0 {
        return false;
    }
    let state = uc.sti();
    let (class, meta, object) = match state.class_names.get(&receiver) {
        Some(class) => (class.clone(), true, None),
        None => match state.objects.get(&receiver) {
            Some(object) => (object.class.clone(), false, Some(object)),
            None => return false,
        },
    };
    if uc.lookup(&class, selector, meta) != 0 {
        return true;
    }
    let Some(object) = object else {
        return matches!(selector, "class" | "alloc" | "new");
    };
    if matches!(
        selector,
        "class"
            | "retain"
            | "release"
            | "autorelease"
            | "copy"
            | "mutableCopy"
            | "isKindOfClass:"
            | "isEqual:"
            | "respondsToSelector:"
    ) {
        return true;
    }

    match &object.value {
        Payload::String(_) => {
            matches!(
                selector,
                "length" | "isEqualToString:" | "hasPrefix:" | "UTF8String"
            ) || NUMBERS.contains(&selector)
        }
        Payload::Int(_) => NUMBERS.contains(&selector),
        Payload::Dict(_) => matches!(
            selector,
            "count"
                | "objectForKey:"
                | "objectForKeyedSubscript:"
                | "allKeys"
                | "setObject:forKey:"
                | "setObject:forKeyedSubscript:"
        ),
        Payload::Array(_) | Payload::Data(_) => selector == "count" || selector == "length",
        _ => false,
    }
}

/// Go's `format`: a format string with variadic arguments on the stack.
fn format_varargs(uc: &Uc, format: &str) -> Result<String> {
    let sp = uc.reg(REG_SP)?;
    format_at(uc, format, sp)
}

/// Go's `formatAt`.
pub(crate) fn format_at(uc: &Uc, format: &str, sp: u64) -> Result<String> {
    let bytes = format.as_bytes();
    let mut offset = 0;
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            result.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            fail!("incomplete format");
        }
        if bytes[i] == b'%' {
            result.push(b'%');
            i += 1;
            continue;
        }
        let width_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let width = &format[width_start..i];
        while i < bytes.len() && (bytes[i] == b'l' || bytes[i] == b'z') {
            i += 1;
        }
        if i >= bytes.len() {
            fail!("invalid format");
        }
        let value = uc.read64(sp + offset)?;
        offset += 8;
        let text = match bytes[i] {
            b'@' => format_object(uc, value)?,
            #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
            b'd' | b'i' => (value as i64).to_string(),
            b'u' => value.to_string(),
            verb @ (b'x' | b'X') => go_hex(value, width, verb == b'X'),
            // Go's fmt rejects %p for an integer operand.
            b'p' => format!("%!p(uint64={value})"),
            b's' => uc.cstring(value)?,
            _ => fail!("unsupported format {format}"),
        };
        result.extend_from_slice(text.as_bytes());
        i += 1;
    }
    Ok(String::from_utf8_lossy(&result).into_owned())
}

fn format_object(uc: &Uc, value: u64) -> Result<String> {
    if value == 0 {
        return Ok("(null)".to_owned());
    }
    if let Some(object) = uc.sti().objects.get(&value) {
        return Ok(object.value.sprint());
    }
    if let Some(class) = uc
        .sti()
        .class_names
        .get(&value)
        .filter(|class| !class.is_empty())
    {
        return Ok(class.clone());
    }
    fail!("unknown format object {value:#x}")
}

/// Go's `%<width>x` / `%<width>X`: a leading zero in the width pads with zeros.
fn go_hex(value: u64, width: &str, upper: bool) -> String {
    let digits = if upper {
        format!("{value:X}")
    } else {
        format!("{value:x}")
    };
    let size: usize = width.parse().unwrap_or_default();
    if digits.len() >= size {
        return digits;
    }
    let pad = if width.starts_with('0') { "0" } else { " " };
    pad.repeat(size - digits.len()) + &digits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_setter_keys_like_go() {
        assert_eq!(setter_key("setTimeout:").as_deref(), Some("timeout"));
        assert_eq!(
            setter_key("setHTTPMaximumConnectionsPerHost:").as_deref(),
            Some("HTTPMaximumConnectionsPerHost")
        );
        assert_eq!(setter_key("setObject:forKey:"), None);
        assert_eq!(setter_key("timeout"), None);
    }

    #[test]
    fn ports_go_path_helpers() {
        assert_eq!(go_join("/ipatool", "cache"), "/ipatool/cache");
        assert_eq!(go_join("a/", "../b"), "b");
        assert_eq!(go_clean("/../a//b/./c/.."), "/a/b");
        assert_eq!(go_base("/a/b/"), "b");
        assert_eq!(go_base(""), ".");
        assert_eq!(go_base("///"), "/");
        assert_eq!(go_ext("a/b.tar.gz"), ".gz");
        assert_eq!(go_ext("a.b/c"), "");
        assert_eq!(go_split("abc", ""), ["a", "b", "c"]);
        assert_eq!(go_hex(255, "04", false), "00ff");
        assert_eq!(go_hex(255, "4", true), "  FF");
        assert_eq!(go_parse_int("99999999999999999999"), i64::MAX);
        assert_eq!(go_parse_int("12x"), 0);
        assert_eq!(go_parse_int("-7"), -7);
    }
}
