//! Go's `net/http` cookies as the network bridge uses them: `ParseSetCookie` (through
//! `Response.Cookies`), `Request.AddCookie` and the `Cookie` fields `NSHTTPCookie` maps to.

use crate::emulator::Date;
use crate::http::Header;

/// Go's `http.Cookie`, restricted to the fields the bridge reads or writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cookie {
    pub name: String,
    pub value: String,
    pub quoted: bool,
    pub path: String,
    pub domain: String,
    /// `None` is Go's zero `time.Time`.
    pub expires: Option<Date>,
    pub secure: bool,
    pub http_only: bool,
}

fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_token_byte)
}

/// Go's `textproto.TrimString`.
fn trim(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

fn valid_value_byte(b: u8) -> bool {
    (0x20..0x7f).contains(&b) && b != b'"' && b != b';' && b != b'\\'
}

/// Go's `parseCookieValue`: the value, whether it was quoted, and whether it is valid.
fn parse_value(raw: &str, allow_double_quote: bool) -> (String, bool, bool) {
    let (raw, quoted) =
        if allow_double_quote && raw.len() > 1 && raw.starts_with('"') && raw.ends_with('"') {
            (&raw[1..raw.len() - 1], true)
        } else {
            (raw, false)
        };
    if raw.bytes().all(valid_value_byte) {
        (raw.to_owned(), quoted, true)
    } else {
        (String::new(), quoted, false)
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Go's `time.Parse` for `Mon, 02 Jan 2006 15:04:05 MST` (`separator` = ' ') and
/// `Mon, 02-Jan-2006 15:04:05 MST` (`separator` = '-').
fn parse_expires(value: &str, separator: u8) -> Option<Date> {
    struct Cursor<'a>(&'a [u8]);
    impl<'a> Cursor<'a> {
        fn take(&mut self, count: usize) -> Option<&'a [u8]> {
            let (head, tail) = self.0.split_at_checked(count)?;
            self.0 = tail;
            Some(head)
        }
        fn literal(&mut self, expected: &[u8]) -> Option<()> {
            (self.take(expected.len())? == expected).then_some(())
        }
    }
    let name = |bytes: &[u8], names: &[&str]| -> Option<usize> {
        let text = std::str::from_utf8(bytes).ok()?.to_ascii_lowercase();
        names.iter().position(|candidate| *candidate == text)
    };
    let digits = |bytes: &[u8]| -> Option<i64> {
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(bytes).ok()?.parse().ok()
    };
    let mut rest = Cursor(value.as_bytes());
    name(rest.take(3)?, &WEEKDAYS)?;
    rest.literal(b", ")?;
    let day = digits(rest.take(2)?)?;
    rest.literal(&[separator])?;
    let month = name(rest.take(3)?, &MONTHS)? + 1;
    rest.literal(&[separator])?;
    let year = digits(rest.take(4)?)?;
    rest.literal(b" ")?;
    // Go's `15` takes one or two digits.
    let hour_width = if rest.0.get(1).is_some_and(u8::is_ascii_digit) {
        2
    } else {
        1
    };
    let hour = digits(rest.take(hour_width)?)?;
    rest.literal(b":")?;
    let minute = digits(rest.take(2)?)?;
    rest.literal(b":")?;
    let second = digits(rest.take(2)?)?;
    rest.literal(b" ")?;
    valid_zone(std::str::from_utf8(rest.0).ok()?).then_some(())?;
    let month = i64::try_from(month).ok()?;
    if hour > 23 || minute > 59 || second > 59 || day < 1 || day > days_in(year, month) {
        return None;
    }
    Some(Date::from_civil(year, month, day, hour, minute, second))
}

fn days_in(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Go's `parseTimeZone` for a trailing `MST` element. Go keeps the wall clock in UTC for
/// every zone it parses this way (even `GMT+3`) unless the name is an abbreviation of the
/// local zone, which is not modelled.
fn valid_zone(zone: &str) -> bool {
    let signed_hours = |text: &str| -> bool {
        let Some(hours) = text.strip_prefix(['+', '-']) else {
            return false;
        };
        !hours.is_empty()
            && hours.bytes().all(|b| b.is_ascii_digit())
            && hours.parse::<u32>().is_ok_and(|hours| hours <= 23)
    };
    if matches!(zone, "ChST" | "MeST" | "UTC" | "GMT") {
        return true;
    }
    if let Some(offset) = zone.strip_prefix("GMT") {
        return signed_hours(offset);
    }
    if zone.starts_with(['+', '-']) {
        return signed_hours(zone);
    }
    let upper = zone.bytes().take_while(u8::is_ascii_uppercase).count();
    upper == zone.len()
        && match upper {
            3 => true,
            4 => zone.ends_with('T') || zone == "WITA",
            5 => zone.ends_with('T'),
            _ => false,
        }
}

/// Go's `http.ParseSetCookie`, keeping only the fields of [`Cookie`].
pub(crate) fn parse_set_cookie(line: &str) -> Option<Cookie> {
    let mut parts = trim(line).split(';');
    let first = trim(parts.next()?);
    let (name, value) = first.split_once('=')?;
    let name = trim(name);
    if !is_token(name) {
        return None;
    }
    let (value, quoted, ok) = parse_value(value, true);
    if !ok {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_owned(),
        value,
        quoted,
        ..Cookie::default()
    };
    for part in parts {
        let part = trim(part);
        if part.is_empty() {
            continue;
        }
        let (attribute, value) = part.split_once('=').unwrap_or((part, ""));
        if !attribute.is_ascii() {
            continue;
        }
        let (value, _, ok) = parse_value(value, false);
        if !ok {
            continue;
        }
        match attribute.to_ascii_lowercase().as_str() {
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "domain" => cookie.domain = value,
            "path" => cookie.path = value,
            "expires" => {
                cookie.expires =
                    parse_expires(&value, b' ').or_else(|| parse_expires(&value, b'-'));
            }
            _ => {}
        }
    }
    Some(cookie)
}

/// Go's `Response.Cookies`: every valid `Set-Cookie` line, none past Go's 3000 limit.
pub(crate) fn read_set_cookies(headers: &Header) -> Vec<Cookie> {
    let lines = headers.values("Set-Cookie");
    if lines.len() > 3000 {
        return Vec::new();
    }
    lines
        .iter()
        .filter_map(|line| parse_set_cookie(line))
        .collect()
}

/// Go's `sanitizeCookieValue`.
fn sanitize_value(value: &str, quoted: bool) -> String {
    let value: String = value
        .chars()
        .filter(|c| c.is_ascii() && valid_value_byte(*c as u8))
        .collect();
    if quoted || value.contains([' ', ',']) {
        format!("\"{value}\"")
    } else {
        value
    }
}

/// Go's `Request.AddCookie`.
pub(crate) fn add_cookie(headers: &mut Header, cookie: &Cookie) {
    let pair = format!(
        "{}={}",
        cookie.name.replace(['\n', '\r'], "-"),
        sanitize_value(&cookie.value, cookie.quoted)
    );
    let existing = headers.get("Cookie");
    let value = if existing.is_empty() {
        pair
    } else {
        format!("{existing}; {pair}")
    };
    headers.set("Cookie", value);
}

#[cfg(test)]
mod tests;
