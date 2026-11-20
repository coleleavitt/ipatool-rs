//! Go's host-side codecs for bridge values: `howett.net/plist` v1.0.1 `Marshal` (XML and
//! binary) and `Unmarshal`, and `encoding/json` `Marshal` and `Unmarshal` into `any`.
//! Response bodies the CLI parses and request bodies Apple receives go through these, so
//! the output is byte-for-byte Go's.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::emulator::{Date, Value};

/// Seconds between the Unix and Apple (2001-01-01) epochs.
const APPLE_EPOCH: i64 = 978_307_200;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// Go's `plist: no root element to encode`.
    #[error("plist: no root element to encode")]
    NoRoot,
    #[error("plist: unsupported type {0}")]
    PlistType(&'static str),
    #[error("plist: nil array element")]
    PlistNilElement,
    /// Go's binary writer cannot find a NaN it stored: NaN map keys never match.
    #[error("failed to find value in object map during serialization")]
    MissingObject,
    #[error("json: unsupported value: {0}")]
    JsonValue(String),
    #[error("json: unsupported type: {0}")]
    JsonType(&'static str),
    #[error("{0}")]
    Decode(String),
}

/// Go's `strconv.FormatFloat(f, 'e' or 'f' or 'g', -1, 64)` from the shortest digits.
fn shortest(value: f64) -> (bool, String, i32) {
    let text = format!("{:e}", value.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    (
        value.is_sign_negative(),
        digits,
        exponent.parse().unwrap_or_default(),
    )
}

fn format_e(negative: bool, digits: &str, exponent: i32) -> String {
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    let _ = write!(out, "e{sign}{:02}", exponent.unsigned_abs());
    out
}

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)` for finite values.
pub(crate) fn format_g(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let (negative, digits, exponent) = shortest(value);
    if !(-4..6).contains(&exponent) {
        return format_e(negative, &digits, exponent);
    }
    format!("{value}")
}

fn plist_float(value: f64) -> String {
    if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        format_g(value)
    }
}

/// Go's `xml.EscapeText`.
fn escape_xml(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c if (c < ' ') || matches!(c, '\u{fffe}' | '\u{ffff}') => {
                out.push('\u{fffd}');
            }
            c => out.push(c),
        }
    }
}

/// Go's `time.Time.Format(time.RFC3339)` in UTC.
fn rfc3339(date: Date, nanos: bool) -> String {
    let civil = date.civil();
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second
    );
    if nanos && date.nanos != 0 {
        let fraction = format!("{:09}", date.nanos);
        let _ = write!(out, ".{}", fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

fn xml_element(out: &mut String, tag: &str, text: &str) {
    if text.is_empty() {
        let _ = write!(out, "<{tag}/>");
    } else {
        let _ = write!(out, "<{tag}>");
        escape_xml(out, text);
        let _ = write!(out, "</{tag}>");
    }
}

/// A dictionary without its nil values, which howett's marshal drops.
fn present(map: &BTreeMap<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
    map.iter().filter(|(_, value)| !matches!(value, Value::Nil))
}

fn check(value: &Value) -> Result<(), CodecError> {
    match value {
        Value::Other(name) => Err(CodecError::PlistType(name)),
        Value::Array(items) => items.iter().try_for_each(check),
        Value::Dictionary(map) => present(map).try_for_each(|(_, value)| check(value)),
        _ => Ok(()),
    }
}

fn write_xml(out: &mut String, value: &Value) {
    match value {
        Value::Nil | Value::Other(_) => {}
        Value::String(text) => xml_element(out, "string", text),
        Value::Integer(number) => xml_element(out, "integer", &number.to_string()),
        Value::Real(number) => xml_element(out, "real", &plist_float(*number)),
        Value::Boolean(true) => out.push_str("<true/>"),
        Value::Boolean(false) => out.push_str("<false/>"),
        Value::Data(bytes) => xml_element(out, "data", &STANDARD.encode(bytes)),
        Value::Date(date) => xml_element(out, "date", &rfc3339(*date, false)),
        Value::Array(items) => {
            out.push_str("<array>");
            for item in items {
                write_xml(out, item);
            }
            out.push_str("</array>");
        }
        Value::Dictionary(map) => {
            out.push_str("<dict>");
            for (key, value) in present(map) {
                xml_element(out, "key", key);
                write_xml(out, value);
            }
            out.push_str("</dict>");
        }
    }
}

/// Go's `plist.Marshal(value, plist.XMLFormat)`.
///
/// # Errors
/// Fails where Go's encoder fails: a nil root or a value with no property-list type. Nil
/// array elements are skipped, as in Go's XML writer.
pub fn plist_xml(value: &Value) -> Result<Vec<u8>, CodecError> {
    if matches!(value, Value::Nil) {
        return Err(CodecError::NoRoot);
    }
    check(value)?;
    let mut out = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" ",
        "\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
        "<plist version=\"1.0\">"
    ));
    write_xml(&mut out, value);
    out.push_str("</plist>");
    Ok(out.into_bytes())
}

/// howett's `cfValue` tree: nil dictionary values are dropped by its marshal.
enum Cf<'a> {
    String(&'a str),
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Data(&'a [u8]),
    Date(Date),
    Array(Vec<Cf<'a>>),
    Dictionary(Vec<(&'a str, Cf<'a>)>),
}

fn cf(value: &Value) -> Result<Cf<'_>, CodecError> {
    Ok(match value {
        Value::Nil => return Err(CodecError::PlistNilElement),
        Value::Other(name) => return Err(CodecError::PlistType(name)),
        Value::String(text) => Cf::String(text),
        Value::Integer(number) => Cf::Integer(*number),
        Value::Real(number) => Cf::Real(*number),
        Value::Boolean(flag) => Cf::Boolean(*flag),
        Value::Data(bytes) => Cf::Data(bytes),
        Value::Date(date) => Cf::Date(*date),
        Value::Array(items) => Cf::Array(items.iter().map(cf).collect::<Result<_, _>>()?),
        Value::Dictionary(map) => Cf::Dictionary(
            present(map)
                .map(|(key, value)| Ok((key.as_str(), cf(value)?)))
                .collect::<Result<_, CodecError>>()?,
        ),
    })
}

/// howett's object-map keys (`cfValue.hash()`): Go interface values compare by dynamic
/// type and value, so each kind has its own namespace; data are uniqued by CRC-32 and
/// containers by identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ObjectKey<'a> {
    String(&'a str),
    Integer(i64),
    Real(u64),
    Boolean(bool),
    Data(u32),
    Date(Date),
    Container(*const Cf<'a>),
}

enum Object<'a> {
    Value(&'a Cf<'a>),
    Key(&'a str),
}

fn object_key<'a>(value: &'a Cf<'a>) -> ObjectKey<'a> {
    match value {
        Cf::String(text) => ObjectKey::String(text),
        Cf::Integer(number) => ObjectKey::Integer(*number),
        // Go map keys: -0 equals +0.
        Cf::Real(number) => ObjectKey::Real(if *number == 0.0 { 0 } else { number.to_bits() }),
        Cf::Boolean(flag) => ObjectKey::Boolean(*flag),
        Cf::Data(bytes) => ObjectKey::Data(crc32fast::hash(bytes)),
        Cf::Date(date) => ObjectKey::Date(*date),
        Cf::Array(_) | Cf::Dictionary(_) => ObjectKey::Container(std::ptr::from_ref(value)),
    }
}

#[derive(Default)]
struct Flattened<'a> {
    objects: Vec<Object<'a>>,
    map: HashMap<ObjectKey<'a>, u64>,
}

impl<'a> Flattened<'a> {
    /// Go's `flattenPlistValue`: unique kinds are written once; booleans and containers are
    /// always appended, and the map then points at the last one.
    fn add(&mut self, key: ObjectKey<'a>, unique: bool, object: Object<'a>) -> bool {
        if unique && self.map.contains_key(&key) {
            return false;
        }
        self.map.insert(key, self.objects.len() as u64);
        self.objects.push(object);
        true
    }

    fn flatten(&mut self, value: &'a Cf<'a>) {
        let unique = !matches!(value, Cf::Boolean(_) | Cf::Array(_) | Cf::Dictionary(_));
        if !self.add(object_key(value), unique, Object::Value(value)) {
            return;
        }
        match value {
            Cf::Dictionary(entries) => {
                for (key, _) in entries {
                    self.add(ObjectKey::String(key), true, Object::Key(key));
                }
                for (_, value) in entries {
                    self.flatten(value);
                }
            }
            Cf::Array(items) => items.iter().for_each(|item| self.flatten(item)),
            _ => {}
        }
    }
}

fn has_nested_nan(value: &Cf<'_>) -> bool {
    let nan =
        |item: &Cf<'_>| matches!(item, Cf::Real(number) if number.is_nan()) || has_nested_nan(item);
    match value {
        Cf::Array(items) => items.iter().any(nan),
        Cf::Dictionary(entries) => entries.iter().any(|(_, item)| nan(item)),
        _ => false,
    }
}

fn minimum_int_size(n: u64) -> usize {
    match n {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    }
}

fn write_sized(out: &mut Vec<u8>, n: u64, size: usize) {
    out.extend_from_slice(&n.to_be_bytes()[8 - size..]);
}

/// Go's `writeIntTag`. Its `SInt128` branch needs an unsigned value above `i64::MAX`, which
/// bridge values and counts never are.
fn write_int_tag(out: &mut Vec<u8>, n: u64) {
    let (marker, size) = match minimum_int_size(n) {
        1 => (0x10, 1),
        2 => (0x11, 2),
        4 => (0x12, 4),
        _ => (0x13, 8),
    };
    out.push(marker);
    write_sized(out, n, size);
}

fn write_counted(out: &mut Vec<u8>, tag: u8, count: usize) {
    match u8::try_from(count) {
        Ok(small) if small < 0xf => out.push(tag | small),
        _ => {
            out.push(tag | 0xf);
            write_int_tag(out, count as u64);
        }
    }
}

fn write_string(out: &mut Vec<u8>, text: &str) {
    if text.is_ascii() {
        write_counted(out, 0x50, text.len());
        out.extend_from_slice(text.as_bytes());
    } else {
        let units: Vec<u16> = text.encode_utf16().collect();
        write_counted(out, 0x60, units.len());
        for unit in units {
            out.extend_from_slice(&unit.to_be_bytes());
        }
    }
}

/// Go's `plist.Marshal(value, plist.BinaryFormat)`.
///
/// # Errors
/// As [`plist_xml`]; an array holding nil also fails (Go's encoder panics there).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "bplist field widths and Go's float64 date arithmetic"
)]
pub fn plist_binary(value: &Value) -> Result<Vec<u8>, CodecError> {
    if matches!(value, Value::Nil) {
        return Err(CodecError::NoRoot);
    }
    let root = cf(value)?;
    if has_nested_nan(&root) {
        return Err(CodecError::MissingObject);
    }
    let mut flat = Flattened::default();
    flat.flatten(&root);
    let reference_size = minimum_int_size(flat.objects.len() as u64);
    let reference = |out: &mut Vec<u8>, key: ObjectKey<'_>| {
        write_sized(
            out,
            flat.map.get(&key).copied().unwrap_or_default(),
            reference_size,
        );
    };
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(flat.objects.len());
    for object in &flat.objects {
        offsets.push(out.len() as u64);
        let value = match object {
            Object::Key(key) => {
                write_string(&mut out, key);
                continue;
            }
            Object::Value(value) => value,
        };
        match value {
            Cf::Boolean(truth) => out.push(if *truth { 0x09 } else { 0x08 }),
            Cf::Integer(number) => write_int_tag(&mut out, *number as u64),
            Cf::Real(number) => {
                out.push(0x23);
                out.extend_from_slice(&number.to_be_bytes());
            }
            Cf::Date(date) => {
                out.push(0x33);
                let nanos = date
                    .seconds
                    .wrapping_mul(1_000_000_000)
                    .wrapping_add(i64::from(date.nanos));
                let seconds = nanos as f64 / 1e9 - APPLE_EPOCH as f64;
                out.extend_from_slice(&seconds.to_be_bytes());
            }
            Cf::Data(bytes) => {
                write_counted(&mut out, 0x40, bytes.len());
                out.extend_from_slice(bytes);
            }
            Cf::String(text) => write_string(&mut out, text),
            Cf::Array(items) => {
                write_counted(&mut out, 0xa0, items.len());
                for item in items {
                    reference(&mut out, object_key(item));
                }
            }
            Cf::Dictionary(entries) => {
                write_counted(&mut out, 0xd0, entries.len());
                for (key, _) in entries {
                    reference(&mut out, ObjectKey::String(key));
                }
                for (_, item) in entries {
                    reference(&mut out, object_key(item));
                }
            }
        }
    }
    let offset_size = minimum_int_size(out.len() as u64);
    let top = flat
        .map
        .get(&object_key(&root))
        .copied()
        .unwrap_or_default();
    let table = out.len() as u64;
    for offset in offsets {
        write_sized(&mut out, offset, offset_size);
    }
    // Go's `bplistTrailer`: five unused bytes and the sort version, the offset and
    // reference sizes, the object count, the top object and the offset table offset.
    out.extend_from_slice(&[0; 6]);
    out.push(offset_size as u8);
    out.push(reference_size as u8);
    out.extend_from_slice(&(flat.objects.len() as u64).to_be_bytes());
    out.extend_from_slice(&top.to_be_bytes());
    out.extend_from_slice(&table.to_be_bytes());
    Ok(out)
}

/// Go's `encoding/json` float formatting.
fn json_float(value: f64) -> Result<String, CodecError> {
    if !value.is_finite() {
        return Err(CodecError::JsonValue(crate::emulator::go_float(value)));
    }
    let abs = value.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let (negative, digits, exponent) = shortest(value);
        let mut text = format_e(negative, &digits, exponent);
        // Go cleans `e-09` up to `e-9`.
        if let Some(index) = text.find("e-0") {
            text.remove(index + 2);
        }
        return Ok(text);
    }
    Ok(format!("{value}"))
}

/// Go's `appendString` with HTML escaping.
fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' || matches!(c, '<' | '>' | '&' | '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json(out: &mut String, value: &Value) -> Result<(), CodecError> {
    match value {
        Value::Nil => out.push_str("null"),
        Value::Other(name) => return Err(CodecError::JsonType(name)),
        Value::String(text) => json_string(out, text),
        Value::Integer(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Real(number) => out.push_str(&json_float(*number)?),
        Value::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Data(bytes) => json_string(out, &STANDARD.encode(bytes)),
        Value::Date(date) => json_string(out, &rfc3339(*date, true)),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item)?;
            }
            out.push(']');
        }
        Value::Dictionary(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json_string(out, key);
                out.push(':');
                write_json(out, item)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Go's `json.Marshal`. Dates are written in UTC; Go writes the `time.Time` location,
/// which for values made by `time.Unix` is the machine's local zone.
///
/// # Errors
/// Fails on NaN or infinite numbers and values with no JSON type.
pub fn json(value: &Value) -> Result<Vec<u8>, CodecError> {
    let mut out = String::new();
    write_json(&mut out, value)?;
    Ok(out.into_bytes())
}

fn from_json(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(flag) => Value::Boolean(flag),
        // Go decodes every number into a float64.
        serde_json::Value::Number(number) => Value::Real(number.as_f64().unwrap_or_default()),
        serde_json::Value::String(text) => Value::String(text),
        serde_json::Value::Array(items) => Value::Array(items.into_iter().map(from_json).collect()),
        serde_json::Value::Object(map) => Value::Dictionary(
            map.into_iter()
                .map(|(key, value)| (key, from_json(value)))
                .collect(),
        ),
    }
}

/// Go's `json.Unmarshal` into `any`.
///
/// # Errors
/// Fails on input that is not one JSON value.
pub fn parse_json(data: &[u8]) -> Result<Value, CodecError> {
    serde_json::from_slice(data)
        .map(from_json)
        .map_err(|err| CodecError::Decode(err.to_string()))
}

#[allow(
    clippy::cast_possible_wrap,
    reason = "Go's uint64 to int64 import wraps"
)]
pub(crate) fn from_plist(value: plist::Value) -> Value {
    match value {
        plist::Value::Array(items) => Value::Array(items.into_iter().map(from_plist).collect()),
        plist::Value::Dictionary(map) => Value::Dictionary(
            map.into_iter()
                .map(|(key, value)| (key, from_plist(value)))
                .collect(),
        ),
        plist::Value::Boolean(flag) => Value::Boolean(flag),
        plist::Value::Data(bytes) => Value::Data(bytes),
        plist::Value::Date(date) => {
            let time = std::time::SystemTime::from(date);
            Value::Date(match time.duration_since(std::time::UNIX_EPOCH) {
                Ok(after) => Date::unix(
                    i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
                    i64::from(after.subsec_nanos()),
                ),
                Err(before) => Date::unix(
                    -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
                    -i64::from(before.duration().subsec_nanos()),
                ),
            })
        }
        plist::Value::Real(number) => Value::Real(number),
        plist::Value::Integer(number) => Value::Integer(
            number
                .as_signed()
                .or_else(|| number.as_unsigned().map(|n| n as i64))
                .unwrap_or_default(),
        ),
        plist::Value::String(text) => Value::String(text),
        plist::Value::Uid(_) => Value::Other("plist.UID"),
        _ => Value::Other("unknown"),
    }
}

/// Go's `plist.Unmarshal` into `any` for XML and binary property lists. Go also reads
/// the `OpenStep` and `GNUstep` text formats.
///
/// # Errors
/// Fails on input that is not a property list.
pub fn parse_plist(data: &[u8]) -> Result<Value, CodecError> {
    plist::Value::from_reader(std::io::Cursor::new(data))
        .map(from_plist)
        .map_err(|err| CodecError::Decode(err.to_string()))
}

#[cfg(test)]
mod tests;
