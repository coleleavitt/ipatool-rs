//! Port of Go `internal/store/emulator/network.go`: the Foundation networking classes
//! (`NSURLSession`, `NSURLRequest`, `NSHTTPCookie`, `NSURLComponents`, `NSURL`, the JSON and
//! property-list serializers) and the transport that runs a data task over the caller's
//! HTTP client.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

use super::bridge::{foundation_copy, import_value, message, parser_error};
use super::callbacks::delegate_call;
use super::client::allowed_url;
use super::codec;
use super::runtime::{REG_X2, REG_X3, conversion, export, host_block, queue_pending};
use super::{Date, Error, Guest, Object, Payload, Result, Uc, Value, fail};
use crate::cookie::{self, Cookie};
use crate::gourl;
use crate::http::{BoxError, Header, HttpRequest, HttpResponse, Transport, canonical_header_key};

/// Go's 32 MiB limit on a buffered Store response and on one `appendBytes:length:`.
const BODY_LIMIT: u64 = 32 << 20;
/// Go writes the timeout interval to unicorn register 40 (`D0`).
const REG_D0: i32 = 40;
/// Seconds between the Unix and Apple (2001-01-01) epochs.
const APPLE_EPOCH: i64 = 978_307_200;

/// The network fields of Go's `Runtime`.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools, reason = "Go's Runtime flags")]
pub(crate) struct NetworkState {
    pub(crate) last_response: Option<HttpResponse>,
    pub(crate) transport_error: Option<BoxError>,
    pub(crate) stream_response: bool,
    pub(crate) no_redirects: bool,
    pub(crate) auth_request: bool,
    pub(crate) terms_action_adapter_installed: bool,
    pub(crate) transport: Option<Transport>,
    pub(crate) response_headers: HashMap<u64, Header>,
}

fn host<T: Clone + 'static>(payload: &Payload, want: &str) -> Result<T> {
    if let Payload::Host(value) = payload
        && let Some(value) = value.borrow().downcast_ref::<T>()
    {
        return Ok(value.clone());
    }
    Err(conversion(payload, want))
}

fn new_host<T: 'static>(uc: &mut Uc, class: &str, value: T) -> Result<u64> {
    uc.new_object(
        class,
        Payload::Host(Rc::new(std::cell::RefCell::new(value))),
    )
}

fn new_string(uc: &mut Uc, text: impl Into<String>) -> Result<u64> {
    uc.new_object("NSString", Payload::String(text.into()))
}

/// Runs `edit` on the `*url.URL` an `NSURLComponents` object holds.
fn with_components<R>(
    uc: &Uc,
    receiver: u64,
    edit: impl FnOnce(&mut gourl::Url) -> R,
) -> Result<R> {
    let payload = &uc.obj(receiver)?.value;
    if let Payload::Host(value) = payload
        && let Some(url) = value.borrow_mut().downcast_mut::<gourl::Url>()
    {
        return Ok(edit(url));
    }
    Err(conversion(payload, "*url.URL"))
}

fn export_value(uc: &Uc, address: u64) -> Result<Value> {
    export(uc, address, &mut HashSet::new(), 0)
}

/// Go's `strings.EqualFold(fmt.Sprint(value), "TRUE")` for a cookie property.
fn property_flag(uc: &Uc, address: u64) -> Result<bool> {
    if address == 0 {
        return Ok(false);
    }
    Ok(uc.obj(address)?.value.sprint().eq_ignore_ascii_case("TRUE"))
}

/// The `*http.Cookie` Go builds from an `NSHTTPCookie` properties dictionary.
fn cookie_from_properties(uc: &Uc, properties: u64) -> Result<Cookie> {
    let fields = uc.dict(properties)?.clone();
    let field = |name: &str| fields.get(name).copied().unwrap_or_default();
    let expires = match field("Expires") {
        0 => None,
        address => match uc.obj(address)?.value {
            Payload::Date(date) => Some(date),
            _ => None,
        },
    };
    Ok(Cookie {
        name: uc.string(field("Name"))?,
        value: uc.string(field("Value"))?,
        domain: uc.string(field("Domain"))?,
        path: uc.string(field("Path"))?,
        secure: property_flag(uc, field("Secure"))?,
        http_only: property_flag(uc, field("HttpOnly"))?,
        expires,
        quoted: false,
    })
}

fn now_unix() -> i64 {
    Date::now().seconds
}

/// Go's `networkMessage`: whether the selector was handled.
#[allow(clippy::too_many_lines, reason = "one dispatch table, as in Go")]
pub(crate) fn network_message(
    uc: &mut Uc,
    receiver: u64,
    class: &str,
    selector: &str,
    meta: bool,
) -> Result<bool> {
    if !meta
        && class == "NSURLSessionConfiguration"
        && matches!(
            selector,
            "_allowsTCPFastOpen" | "_allowsTLSSessionTickets" | "URLCache"
        )
    {
        // These optional CFNetwork facilities are absent from the transport.
        uc.set(0)?;
        return Ok(true);
    }
    if !meta && class == "NSHTTPURLResponse" && selector == "_CFURLResponse" {
        uc.set(receiver)?;
        return Ok(true);
    }
    if !meta && class == "NSURLSessionDataTask" && selector == "_timingData" {
        // The portable transport has no CFNetwork timing-data object.
        uc.set(0)?;
        return Ok(true);
    }
    if !meta && class == "AKAppleIDSession" && selector == "handleResponse:forRequest:shouldRetry:"
    {
        // Pinned AuthKit _canHandleAnisetteURLResponse: accepts exactly these challenge
        // status codes. All other responses leave shouldRetry false.
        let response = uc.arg(2)?;
        let status = uc.obj(response)?.prop("statusCode");
        if matches!(status, 431 | 433 | 434 | 441 | 442 | 443 | 444) {
            fail!("AuthKit anisette challenge is not supported");
        }
        let out = uc.arg(4)?;
        if out != 0 {
            uc.write_mem(out, &[0])?;
        }
        uc.set(0)?;
        return Ok(true);
    }
    if meta {
        return class_message(uc, class, selector);
    }
    instance_message(uc, receiver, class, selector)
}

#[allow(clippy::too_many_lines, reason = "one dispatch table, as in Go")]
fn class_message(uc: &mut Uc, class: &str, selector: &str) -> Result<bool> {
    match (class, selector) {
        ("NSURLComponents", "componentsWithURL:resolvingAgainstBaseURL:") => {
            let text = uc.string(uc.arg(2)?)?;
            match gourl::parse(&text) {
                Err(_) => uc.set(0)?,
                Ok(url) => {
                    let components = new_host(uc, "NSURLComponents", url)?;
                    uc.set(components)?;
                }
            }
        }
        ("NSURLCache", "sharedURLCache") => uc.set(0)?,
        ("NSJSONSerialization", "isValidJSONObject:") => {
            let valid = valid_json_object(uc, uc.arg(2)?, true, 0);
            uc.set(u64::from(valid))?;
        }
        ("NSURLQueryItem", "queryItemWithName:value:") => {
            let (name, value) = (uc.arg(2)?, uc.arg(3)?);
            let item = uc.new_object(class, Payload::Nil)?;
            uc.obj_mut(item)?.properties =
                HashMap::from([("name".to_owned(), name), ("value".to_owned(), value)]);
            uc.set(item)?;
        }
        ("NSHTTPCookie", "cookieWithProperties:") => {
            let cookie = cookie_from_properties(uc, uc.arg(2)?)?;
            let object = new_host(uc, "NSHTTPCookie", cookie)?;
            uc.set(object)?;
        }
        ("NSHTTPCookie", "requestHeaderFieldsWithCookies:") => {
            let mut headers = Header::new();
            for item in uc.array(uc.arg(2)?)?.clone() {
                let cookie: Cookie = host(&uc.obj(item)?.value, "*http.Cookie")?;
                cookie::add_cookie(&mut headers, &cookie);
            }
            let mut fields = std::collections::BTreeMap::new();
            let value = headers.get("Cookie").to_owned();
            if !value.is_empty() {
                fields.insert("Cookie".to_owned(), new_string(uc, value)?);
            }
            let dictionary = uc.new_object("NSDictionary", Payload::Dict(fields))?;
            uc.set(dictionary)?;
        }
        ("NSHTTPCookie", "cookiesWithResponseHeaderFields:forURL:") => {
            let fields_address = uc.arg(2)?;
            let mut headers = Header::new();
            for (key, value) in uc.dict(fields_address)?.clone() {
                headers.set(&key, uc.string(value)?);
            }
            if let Some(raw) = uc.st().network.response_headers.get(&fields_address) {
                headers = raw.clone();
            }
            let origin = gourl::parse(&uc.string(uc.arg(3)?)?).ok();
            let mut items = Vec::new();
            for mut cookie in cookie::read_set_cookies(&headers) {
                if cookie.domain.is_empty() {
                    let Some(origin) = &origin else {
                        fail!("{}", super::runtime::NIL_DEREFERENCE);
                    };
                    origin.hostname().clone_into(&mut cookie.domain);
                } else if !cookie.domain.starts_with('.') {
                    cookie.domain.insert(0, '.');
                }
                if cookie.path.is_empty() {
                    "/".clone_into(&mut cookie.path);
                }
                items.push(new_host(uc, "NSHTTPCookie", cookie)?);
            }
            let array = uc.new_object("NSArray", Payload::Array(items))?;
            uc.set(array)?;
        }
        ("NSURLSession", "sessionWithConfiguration:delegate:delegateQueue:") => {
            let properties = HashMap::from([
                ("configuration".to_owned(), uc.arg(2)?),
                ("delegate".to_owned(), uc.arg(3)?),
                ("delegateQueue".to_owned(), uc.arg(4)?),
            ]);
            let session = uc.new_object("NSURLSession", Payload::Nil)?;
            uc.obj_mut(session)?.properties = properties;
            uc.set(session)?;
        }
        ("NSURLRequest" | "NSMutableURLRequest", "requestWithURL:") => {
            let url = uc.arg(2)?;
            let request = uc.new_object(class, Payload::Nil)?;
            let method = new_string(uc, "GET")?;
            let headers =
                uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::default()))?;
            uc.obj_mut(request)?.properties = HashMap::from([
                ("URL".to_owned(), url),
                ("HTTPMethod".to_owned(), method),
                ("allHTTPHeaderFields".to_owned(), headers),
            ]);
            uc.set(request)?;
        }
        ("NSJSONSerialization", "JSONObjectWithData:options:error:") => {
            let data = uc.data(uc.arg(2)?)?.clone();
            match codec::parse_json(&data) {
                Err(_) => {
                    parser_error(uc, uc.arg(4)?)?;
                    uc.set(0)?;
                }
                Ok(value) => {
                    let object = import_value(uc, &value)?;
                    uc.set(object)?;
                }
            }
        }
        ("NSPropertyListSerialization", "propertyList:isValidForFormat:") => {
            let valid =
                export_value(uc, uc.arg(2)?).is_ok_and(|value| codec::plist_xml(&value).is_ok());
            uc.set(u64::from(valid))?;
        }
        ("NSPropertyListSerialization", "propertyListWithData:options:format:error:") => {
            let data = uc.data(uc.arg(2)?)?.clone();
            match codec::parse_plist(&data) {
                Err(_) => {
                    parser_error(uc, uc.arg(5)?)?;
                    uc.set(0)?;
                }
                Ok(value) => {
                    let object = import_value(uc, &value)?;
                    uc.set(object)?;
                }
            }
        }
        ("NSPropertyListSerialization", "dataWithPropertyList:format:options:error:") => {
            let value = export_value(uc, uc.arg(2)?)?;
            let data = if uc.arg(3)? == 200 {
                codec::plist_binary(&value)
            } else {
                codec::plist_xml(&value)
            }
            .map_err(|err| Error::msg(err.to_string()))?;
            let object = uc.new_object("NSData", Payload::Data(data))?;
            uc.set(object)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn copy_headers(uc: &mut Uc, receiver: u64) -> Result<()> {
    let headers = uc.obj(receiver)?.prop("allHTTPHeaderFields");
    if headers != 0 {
        let copied = foundation_copy(uc, headers, true)?;
        uc.obj_mut(receiver)?
            .properties
            .insert("allHTTPHeaderFields".to_owned(), copied);
    }
    Ok(())
}

/// The `NSURLRequest` family; `None` when the selector is not one of these.
fn request_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<Option<()>> {
    match selector {
        "HTTPBody" | "HTTPBodyStream" => {
            // Bodyless requests have nil bodies. The framework inspects these optional
            // properties when handling redirects, even if no setter ran.
            let value = uc.obj(receiver)?.prop(selector);
            uc.set(value)?;
        }
        "copy" | "copyWithZone:" | "mutableCopy" | "mutableCopyWithZone:" => {
            if matches!(selector, "copy" | "mutableCopy") {
                let zone = format!("{selector}WithZone:");
                let entry = uc.lookup(class, &zone, false);
                if entry != 0 {
                    uc.set_reg(REG_X2, 0)?;
                    uc.jump(entry, receiver, &zone)?;
                    return Ok(Some(()));
                }
            }
            let object = uc.obj(receiver)?.clone();
            let copied = uc.new_object(class, object.value)?;
            uc.obj_mut(copied)?.properties.extend(object.properties);
            copy_headers(uc, copied)?;
            uc.set(copied)?;
        }
        "_CFURLRequest" => uc.set(receiver)?,
        "_initWithCFURLRequest:" => {
            let source = uc.obj(uc.arg(2)?)?.properties.clone();
            uc.obj_mut(receiver)?.properties = source;
            copy_headers(uc, receiver)?;
            uc.set(receiver)?;
        }
        "setValue:forHTTPHeaderField:" | "addValue:forHTTPHeaderField:" => {
            if uc.obj(receiver)?.prop("allHTTPHeaderFields") == 0 {
                let headers =
                    uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::default()))?;
                uc.obj_mut(receiver)?
                    .properties
                    .insert("allHTTPHeaderFields".to_owned(), headers);
            }
            let headers = uc.obj(receiver)?.prop("allHTTPHeaderFields");
            let key = canonical_header_key(&uc.string(uc.arg(3)?)?);
            let value = uc.arg(2)?;
            let fields = uc.dict_mut(headers)?;
            if value == 0 {
                fields.remove(&key);
            } else {
                fields.insert(key, value);
            }
            uc.set(0)?;
        }
        "valueForHTTPHeaderField:" => {
            let headers = uc.obj(receiver)?.prop("allHTTPHeaderFields");
            let key = canonical_header_key(&uc.string(uc.arg(2)?)?);
            let value = uc.dict(headers)?.get(&key).copied().unwrap_or_default();
            uc.set(value)?;
        }
        _ => return Ok(None),
    }
    Ok(Some(()))
}

#[allow(clippy::too_many_lines, reason = "one dispatch table, as in Go")]
fn instance_message(uc: &mut Uc, receiver: u64, class: &str, selector: &str) -> Result<bool> {
    if selector == "initWithIndexesInRange:" && class == "NSIndexSet" {
        let range = [uc.arg(2)?, uc.arg(3)?];
        uc.obj_mut(receiver)?.value = Payload::IndexSet(range);
        uc.set(receiver)?;
        return Ok(true);
    }
    if selector == "containsIndex:" && class == "NSIndexSet" {
        let payload = &uc.obj(receiver)?.value;
        let Payload::IndexSet([location, length]) = *payload else {
            return Err(conversion(payload, "[2]uint64"));
        };
        let index = uc.arg(2)?;
        uc.set(u64::from(index >= location && index - location < length))?;
        return Ok(true);
    }
    if selector == "initWithURL:" && uc.kind_of(class, "NSURLRequest") {
        uc.set_reg(REG_X3, 0)?;
        uc.set_reg(REG_D0, 60f64.to_bits())?;
        message(
            uc,
            receiver,
            "initWithURL:cachePolicy:timeoutInterval:",
            false,
        )?;
        return Ok(true);
    }
    if selector == "initWithURL:cachePolicy:timeoutInterval:" && uc.kind_of(class, "NSURLRequest") {
        let (url, policy) = (uc.arg(2)?, uc.arg(3)?);
        let method = new_string(uc, "GET")?;
        let headers = uc.new_object("NSMutableDictionary", Payload::Dict(BTreeMap::default()))?;
        uc.obj_mut(receiver)?.properties = HashMap::from([
            ("URL".to_owned(), url),
            ("cachePolicy".to_owned(), policy),
            ("HTTPMethod".to_owned(), method),
            ("allHTTPHeaderFields".to_owned(), headers),
        ]);
        uc.set(receiver)?;
        return Ok(true);
    }
    if class == "NSHTTPURLResponse" && selector == "valueForHTTPHeaderField:" {
        let headers = uc.obj(receiver)?.prop("allHeaderFields");
        let name = uc.string(uc.arg(2)?)?;
        let value = uc
            .dict(headers)?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&name))
            .map_or(0, |(_, value)| *value);
        uc.set(value)?;
        return Ok(true);
    }
    if uc.kind_of(class, "NSURLRequest")
        && request_message(uc, receiver, class, selector)?.is_some()
    {
        return Ok(true);
    }
    if class == "NSURLSession" && selector == "dataTaskWithRequest:" {
        let request = uc.arg(2)?;
        let task = uc.new_object("NSURLSessionDataTask", Payload::Nil)?;
        uc.obj_mut(task)?.properties = HashMap::from([
            ("currentRequest".to_owned(), request),
            ("originalRequest".to_owned(), request),
            ("session".to_owned(), receiver),
            ("taskIdentifier".to_owned(), task),
            ("state".to_owned(), 1),
        ]);
        uc.set(task)?;
        return Ok(true);
    }
    if class == "NSURLSessionDataTask" && selector == "resume" {
        if uc.obj(receiver)?.prop("state") != 1 {
            fail!("task resumed twice");
        }
        uc.obj_mut(receiver)?
            .properties
            .insert("state".to_owned(), 0);
        let block = host_block(uc, Rc::new(move |uc: &mut Uc| perform_http(uc, receiver)))?;
        uc.st().pending.push(block);
        uc.set(0)?;
        return Ok(true);
    }
    if (class == "NSURLSession"
        && matches!(selector, "invalidateAndCancel" | "finishTasksAndInvalidate"))
        || (class == "NSURLSessionDataTask" && selector == "cancel")
    {
        uc.set(0)?;
        return Ok(true);
    }
    if matches!(
        class,
        "NSURLSession" | "NSURLSessionDataTask" | "NSHTTPURLResponse" | "NSURLResponse"
    ) || uc.kind_of(class, "NSURLRequest")
    {
        if selector.starts_with("set")
            && selector.ends_with(':')
            && selector.matches(':').count() == 1
        {
            let key = &selector[3..selector.len() - 1];
            let key = if key.len() < 2 || !key.as_bytes()[1].is_ascii_uppercase() {
                let mut lowered = key[..1].to_ascii_lowercase();
                lowered.push_str(&key[1..]);
                lowered
            } else {
                key.to_owned()
            };
            let value = uc.arg(2)?;
            uc.obj_mut(receiver)?.properties.insert(key, value);
            uc.set(0)?;
            return Ok(true);
        }
        if let Some(value) = uc.obj(receiver)?.properties.get(selector).copied() {
            uc.set(value)?;
            return Ok(true);
        }
    }
    if class == "NSHTTPCookie" && cookie_message(uc, receiver, selector)? {
        return Ok(true);
    }
    if class == "NSURLQueryItem" && matches!(selector, "name" | "value") {
        let value = uc.obj(receiver)?.prop(selector);
        uc.set(value)?;
        return Ok(true);
    }
    if class == "NSURLComponents" {
        return components_message(uc, receiver, selector);
    }
    if class == "NSURL" {
        return url_message(uc, receiver, selector);
    }
    if matches!(class, "NSData" | "NSMutableData") {
        return data_message(uc, receiver, selector);
    }
    Ok(false)
}

fn cookie_message(uc: &mut Uc, receiver: u64, selector: &str) -> Result<bool> {
    if selector == "initWithProperties:" {
        let cookie = cookie_from_properties(uc, uc.arg(2)?)?;
        uc.obj_mut(receiver)?.value = Payload::Host(Rc::new(std::cell::RefCell::new(cookie)));
        uc.set(receiver)?;
        return Ok(true);
    }
    let cookie: Cookie = host(&uc.obj(receiver)?.value, "*http.Cookie")?;
    let result = match selector {
        "properties" => {
            let mut fields = std::collections::BTreeMap::from([
                ("Name".to_owned(), Value::String(cookie.name)),
                ("Value".to_owned(), Value::String(cookie.value)),
                ("Domain".to_owned(), Value::String(cookie.domain)),
                ("Path".to_owned(), Value::String(cookie.path)),
                (
                    "Created".to_owned(),
                    Value::Integer(now_unix() - APPLE_EPOCH),
                ),
            ]);
            if cookie.secure {
                fields.insert("Secure".to_owned(), Value::String("TRUE".to_owned()));
            }
            if cookie.http_only {
                fields.insert("HttpOnly".to_owned(), Value::String("TRUE".to_owned()));
            }
            match cookie.expires {
                None => fields.insert("Discard".to_owned(), Value::String("TRUE".to_owned())),
                Some(date) => fields.insert("Expires".to_owned(), Value::Date(date)),
            };
            import_value(uc, &Value::Dictionary(fields))?
        }
        "name" => new_string(uc, cookie.name)?,
        "domain" => new_string(uc, cookie.domain)?,
        "path" => new_string(uc, cookie.path)?,
        "value" => new_string(uc, cookie.value)?,
        "expiresDate" => match cookie.expires {
            None => 0,
            Some(date) => uc.new_object("NSDate", Payload::Date(date))?,
        },
        "isSessionOnly" => u64::from(cookie.expires.is_none()),
        "version" => 0,
        "isSecure" => u64::from(cookie.secure),
        "isHTTPOnly" => u64::from(cookie.http_only),
        _ => return Ok(false),
    };
    uc.set(result)?;
    Ok(true)
}

/// Go's `encodeComponentQuery`: `NSURLComponents.setQuery:` percent-encoding.
pub(crate) fn encode_component_query(query: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(query.len());
    for &c in query.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&c) {
            encoded.push(char::from(c));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(c >> 4)]));
            encoded.push(char::from(HEX[usize::from(c & 15)]));
        }
    }
    encoded
}

/// Go's query-item escaping: `url.QueryEscape` with `+` written as `%20`.
fn escape_query_item(text: &str) -> String {
    gourl::query_escape(text).replace('+', "%20")
}

#[allow(clippy::too_many_lines, reason = "one dispatch table, as in Go")]
fn components_message(uc: &mut Uc, receiver: u64, selector: &str) -> Result<bool> {
    if selector == "initWithURL:resolvingAgainstBaseURL:" {
        let url =
            gourl::parse(&uc.string(uc.arg(2)?)?).map_err(|err| Error::msg(err.to_string()))?;
        uc.obj_mut(receiver)?.value = Payload::Host(Rc::new(std::cell::RefCell::new(url)));
        uc.set(receiver)?;
        return Ok(true);
    }
    let url: gourl::Url = host(&uc.obj(receiver)?.value, "*url.URL")?;
    match selector {
        "setQuery:" | "setPercentEncodedQuery:" => {
            let query = uc.arg(2)?;
            let raw = if query == 0 {
                String::new()
            } else {
                let text = uc.string(query)?;
                if selector == "setQuery:" {
                    encode_component_query(&text)
                } else {
                    text
                }
            };
            with_components(uc, receiver, |url| {
                url.force_query = query != 0;
                url.raw_query = raw;
            })?;
            uc.set(0)?;
        }
        "query" | "percentEncodedQuery" => {
            if url.raw_query.is_empty() && !url.force_query {
                uc.set(0)?;
            } else {
                let value = if selector == "query" {
                    gourl::path_unescape(&url.raw_query)
                        .map_err(|_| Error::msg("invalid URL query encoding"))?
                } else {
                    url.raw_query
                };
                let text = new_string(uc, value)?;
                uc.set(text)?;
            }
        }
        "setFragment:" => {
            let fragment = uc.string(uc.arg(2)?)?;
            with_components(uc, receiver, |url| {
                url.fragment = fragment;
                url.raw_fragment.clear();
            })?;
            uc.set(0)?;
        }
        "URL" => {
            let text = url.to_string();
            let object = uc.new_object("NSURL", Payload::String(text))?;
            uc.set(object)?;
        }
        "queryItems" | "percentEncodedQueryItems" => {
            let mut items = Vec::new();
            if !url.raw_query.is_empty() {
                for part in url.raw_query.split('&') {
                    let (key, value) = part
                        .split_once('=')
                        .map_or((part, None), |(k, v)| (k, Some(v)));
                    let decode = |text: &str| {
                        if selector == "queryItems" {
                            // Go ignores the error and keeps the empty result.
                            gourl::query_unescape(&text.replace('+', "%2B")).unwrap_or_default()
                        } else {
                            text.to_owned()
                        }
                    };
                    let name = new_string(uc, decode(key))?;
                    let item = uc.new_object("NSURLQueryItem", Payload::Nil)?;
                    uc.obj_mut(item)?.properties.insert("name".to_owned(), name);
                    if let Some(value) = value {
                        let value = new_string(uc, decode(value))?;
                        uc.obj_mut(item)?
                            .properties
                            .insert("value".to_owned(), value);
                    }
                    items.push(item);
                }
            }
            let array = uc.new_object("NSArray", Payload::Array(items))?;
            uc.set(array)?;
        }
        "setQueryItems:" | "setPercentEncodedQueryItems:" => {
            let mut parts = Vec::new();
            let list = uc.arg(2)?;
            if list != 0 {
                for item in uc.array(list)?.clone() {
                    let properties = &uc.obj(item)?.properties;
                    let (name, value) = (
                        properties.get("name").copied().unwrap_or_default(),
                        properties.get("value").copied().unwrap_or_default(),
                    );
                    let (name, value_text) = (uc.string(name)?, uc.string(value)?);
                    let part = if selector == "setPercentEncodedQueryItems:" {
                        if value == 0 {
                            name
                        } else {
                            format!("{name}={value_text}")
                        }
                    } else if value == 0 {
                        escape_query_item(&name)
                    } else {
                        format!(
                            "{}={}",
                            escape_query_item(&name),
                            escape_query_item(&value_text)
                        )
                    };
                    parts.push(part);
                }
            }
            with_components(uc, receiver, |url| url.raw_query = parts.join("&"))?;
            uc.set(0)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn url_message(uc: &mut Uc, receiver: u64, selector: &str) -> Result<bool> {
    let payload = &uc.obj(receiver)?.value;
    let Payload::String(text) = payload else {
        return Err(conversion(payload, "string"));
    };
    let url = gourl::parse(text).map_err(|err| Error::msg(err.to_string()))?;
    let result = match selector {
        "URLByAppendingPathComponent:" | "URLByAppendingPathComponent:isDirectory:" => {
            let component = uc.string(uc.arg(2)?)?;
            let joined = gourl::join_path(&url.to_string(), &[&component])
                .map_err(|err| Error::msg(err.to_string()))?;
            uc.new_object("NSURL", Payload::String(joined))?
        }
        "fileSystemRepresentation" => {
            let mut value = url.path.into_bytes();
            value.push(0);
            let pointer = uc.alloc(value.len() as u64)?;
            // Go ignores this write error.
            let _ = uc.write_mem(pointer, &value);
            pointer
        }
        "isFileURL" => u64::from(url.scheme == "file"),
        "fragment" if url.fragment.is_empty() => 0,
        "fragment" => new_string(uc, url.fragment)?,
        "host" => new_string(uc, url.hostname())?,
        "scheme" => new_string(uc, url.scheme)?,
        "query" => new_string(uc, url.raw_query)?,
        _ => return Ok(false),
    };
    uc.set(result)?;
    Ok(true)
}

fn data_message(uc: &mut Uc, receiver: u64, selector: &str) -> Result<bool> {
    match selector {
        "bytes" => {
            let data = match &uc.obj(receiver)?.value {
                Payload::Data(data) => data.clone(),
                _ => Vec::new(),
            };
            let pointer = uc.alloc(data.len() as u64)?;
            // Go ignores this write error.
            let _ = uc.write_mem(pointer, &data);
            uc.set(pointer)?;
        }
        "appendBytes:length:" => {
            let size = uc.arg(3)?;
            if size > BODY_LIMIT {
                fail!("data append exceeds budget");
            }
            let other = uc.read_mem(uc.arg(2)?, size)?;
            append_data(uc, receiver, &other)?;
            uc.set(0)?;
        }
        "appendData:" => {
            let other = uc.data(uc.arg(2)?)?.clone();
            append_data(uc, receiver, &other)?;
            uc.set(0)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Go's `data, _ := object.Value.([]byte); object.Value = append(data, other...)`.
fn append_data(uc: &mut Uc, receiver: u64, other: &[u8]) -> Result<()> {
    let object = uc.obj_mut(receiver)?;
    let mut data = match std::mem::take(&mut object.value) {
        Payload::Data(data) => data,
        _ => Vec::new(),
    };
    data.extend_from_slice(other);
    object.value = Payload::Data(data);
    Ok(())
}

/// Go's `validJSONObject`.
fn valid_json_object(uc: &Uc, address: u64, top: bool, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let Ok(object) = uc.obj(address) else {
        return false;
    };
    match &object.value {
        Payload::Dict(map) => map
            .values()
            .all(|child| valid_json_object(uc, *child, false, depth + 1)),
        Payload::Array(items) => {
            object.class.contains("Array")
                && items
                    .iter()
                    .all(|child| valid_json_object(uc, *child, false, depth + 1))
        }
        Payload::String(_) | Payload::Bool(_) | Payload::Int(_) => !top,
        Payload::Float(value) => !top && value.is_finite(),
        _ => !top && object.class == "NSNull",
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Go's `http.NewRequest` method check: an HTTP token, empty meaning `GET`.
pub(crate) fn valid_method(method: &str) -> bool {
    method
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

/// Go's `performHTTP`: sends a resumed data task and feeds the response to the session
/// delegate, following redirects through the framework's redirection callback.
#[allow(clippy::too_many_lines, reason = "one transport step, as in Go")]
pub(crate) fn perform_http(uc: &mut Uc, task: u64) -> Result<()> {
    let request_address = uc.obj(task)?.prop("currentRequest");
    let request: Object = uc.obj(request_address)?.clone();
    let endpoint = uc.string(request.prop("URL"))?;
    let network = &uc.sti().network;
    let authenticated = network.auth_request || !network.stream_response;
    let parsed = gourl::parse(&endpoint)
        .ok()
        .filter(|url| allowed_url(url, authenticated));
    let Some(origin) = parsed else {
        fail!("unsupported transport URL");
    };
    let body = match request.prop("HTTPBody") {
        0 => Vec::new(),
        address => uc.data(address)?.clone(),
    };
    let mut method = uc.string(request.prop("HTTPMethod"))?;
    if method.is_empty() {
        "GET".clone_into(&mut method);
    }
    if !valid_method(&method) {
        fail!("cannot construct transport request");
    }
    let mut headers = Header::new();
    if let Some(fields) = request
        .properties
        .get("allHTTPHeaderFields")
        .copied()
        .filter(|a| *a != 0)
    {
        for (key, value) in uc.dict(fields)?.clone() {
            headers.set(&key, uc.string(value)?);
        }
    }
    let outgoing = HttpRequest {
        method: method.clone(),
        url: endpoint,
        headers,
        body,
    };
    let Some(transport) = uc.st().network.transport.as_mut() else {
        fail!("store transport is not configured");
    };
    let mut response = match transport(outgoing) {
        Ok(response) => response,
        Err(err) => {
            uc.st().network.transport_error = Some(err);
            fail!("Store HTTP transport failed");
        }
    };
    let status = response.status;
    let redirect = is_redirect(status);
    let response_headers = response.headers.clone();
    let content_length = response.content_length;
    let location = response.location();
    let mut data = Vec::new();
    if uc.sti().network.stream_response && !redirect {
        uc.st().network.last_response = Some(response);
    } else {
        let read = (&mut response.body)
            .take(BODY_LIMIT + 1)
            .read_to_end(&mut data);
        drop(std::mem::replace(
            &mut response.body,
            Box::new(std::io::empty()),
        ));
        if let Err(err) = read {
            uc.st().network.transport_error = Some(Box::new(err));
            fail!("Store HTTP response read failed");
        }
        if data.len() as u64 > BODY_LIMIT {
            fail!("Store API response exceeds size limit");
        }
        response.set_body(data.clone());
        uc.st().network.last_response = Some(response);
    }

    let mut fields = std::collections::BTreeMap::new();
    for (key, values) in response_headers.iter() {
        fields.insert(key.to_owned(), new_string(uc, values.join(", "))?);
    }
    let header_object = uc.new_object("NSDictionary", Payload::Dict(fields))?;
    uc.st()
        .network
        .response_headers
        .insert(header_object, response_headers.clone());
    let mime = response_headers
        .get("Content-Type")
        .split(';')
        .next()
        .unwrap_or_default()
        .to_owned();
    let mime = new_string(uc, mime)?;
    let result = uc.new_object("NSHTTPURLResponse", Payload::Nil)?;
    #[allow(clippy::cast_sign_loss, reason = "Go's uint64(ContentLength)")]
    let expected = content_length as u64;
    uc.obj_mut(result)?.properties = HashMap::from([
        ("statusCode".to_owned(), u64::from(status)),
        ("allHeaderFields".to_owned(), header_object),
        ("URL".to_owned(), request.prop("URL")),
        ("MIMEType".to_owned(), mime),
        ("expectedContentLength".to_owned(), expected),
    ]);
    uc.obj_mut(task)?
        .properties
        .insert("response".to_owned(), result);
    let session = uc.obj(task)?.prop("session");
    let delegate = uc.obj(session)?.prop("delegate");

    let data = Rc::new(data);
    let completion = host_block(
        uc,
        Rc::new(move |uc: &mut Uc| {
            if uc.arg(1)? != 1 {
                fail!("framework rejected response");
            }
            let data = Rc::clone(&data);
            let receive = host_block(
                uc,
                Rc::new(move |uc: &mut Uc| {
                    let chunk = uc.new_object("NSData", Payload::Data((*data).clone()))?;
                    delegate_call(
                        uc,
                        delegate,
                        "URLSession:dataTask:didReceiveData:",
                        vec![session, task, chunk],
                        Box::new(move |uc: &mut Uc| {
                            let complete = host_block(
                                uc,
                                Rc::new(move |uc: &mut Uc| {
                                    uc.obj_mut(task)?.properties.insert("state".to_owned(), 3);
                                    delegate_call(
                                        uc,
                                        delegate,
                                        "URLSession:task:didCompleteWithError:",
                                        vec![session, task, 0],
                                        Box::new(|uc: &mut Uc| uc.set(0)),
                                    )
                                }),
                            )?;
                            uc.st().pending.push(complete);
                            uc.set(0)
                        }),
                    )
                }),
            )?;
            uc.st().pending.push(receive);
            uc.set(0)
        }),
    )?;
    let deliver = move |uc: &mut Uc| -> Result<()> {
        delegate_call(
            uc,
            delegate,
            "URLSession:dataTask:didReceiveResponse:completionHandler:",
            vec![session, task, result, completion],
            Box::new(|uc: &mut Uc| uc.set(0)),
        )
    };

    if !redirect || uc.sti().network.no_redirects {
        return deliver(uc);
    }
    let Ok(destination) = location else {
        return deliver(uc);
    };
    if uc.obj(task)?.prop("redirectCount") >= 4 {
        fail!("redirect limit exceeded");
    }
    let network = &uc.sti().network;
    let auth = network.auth_request;
    let authenticated = auth || !network.stream_response;
    let port = destination.port();
    if !allowed_url(&destination, authenticated)
        || (auth
            && (status == 303
                || (!port.is_empty() && port != "443")
                || !destination.raw_path.is_empty()
                || !destination.hostname().ends_with("buy.itunes.apple.com")
                || destination.path.trim_end_matches('/')
                    != "/WebObjects/MZFinance.woa/wa/authenticate"))
    {
        fail!("invalid Store redirect destination");
    }
    let proposed = uc.new_object("NSMutableURLRequest", Payload::Nil)?;
    let destination_url = uc.new_object("NSURL", Payload::String(destination.to_string()))?;
    let mut properties = request.properties.clone();
    properties.insert("URL".to_owned(), destination_url);
    let mut fields = match request.prop("allHTTPHeaderFields") {
        0 => std::collections::BTreeMap::new(),
        address => uc.dict(address)?.clone(),
    };
    if destination.hostname() != origin.hostname() {
        for key in [
            "Authorization",
            "Cookie",
            "X-Token",
            "X-Dsid",
            "Icloud-Dsid",
        ] {
            fields.remove(key);
        }
    }
    if status == 303 || (matches!(status, 301 | 302) && method == "POST") {
        let get = new_string(uc, "GET")?;
        properties.insert("HTTPMethod".to_owned(), get);
        properties.insert("HTTPBody".to_owned(), 0);
        fields.remove("Content-Length");
        fields.remove("Content-Type");
    }
    let fields = uc.new_object("NSMutableDictionary", Payload::Dict(fields))?;
    properties.insert("allHTTPHeaderFields".to_owned(), fields);
    uc.obj_mut(proposed)?.properties.extend(properties);
    let deliver = Rc::new(deliver);
    let redirect_completion = host_block(
        uc,
        Rc::new(move |uc: &mut Uc| {
            let next = uc.arg(1)?;
            if next == 0 {
                return deliver(uc);
            }
            if uc.obj(next).is_err() {
                fail!("invalid redirected request");
            }
            let object = uc.obj_mut(task)?;
            object.properties.insert("currentRequest".to_owned(), next);
            *object
                .properties
                .entry("redirectCount".to_owned())
                .or_default() += 1;
            let again = host_block(uc, Rc::new(move |uc: &mut Uc| perform_http(uc, task)))?;
            queue_pending(uc, again, 0);
            uc.set(0)
        }),
    )?;
    delegate_call(
        uc,
        delegate,
        "URLSession:task:willPerformHTTPRedirection:newRequest:completionHandler:",
        vec![session, task, result, proposed, redirect_completion],
        Box::new(|uc: &mut Uc| uc.set(0)),
    )
}
