//! Go's `net/url` (go1.26.5) parse, escape and resolve rules. The Store emulator checks
//! and rebuilds URLs exactly as the Go emulator does, and the `url` crate (WHATWG)
//! normalizes differently: it resolves dot segments, drops default ports and treats `\`
//! as `/`.

#![allow(dead_code, reason = "the network bridge uses the rest of Go's API")]

use std::fmt::{self, Write as _};
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Path,
    PathSegment,
    Host,
    Zone,
    UserPassword,
    QueryComponent,
    Fragment,
}

/// Go's `url.Error` and the errors it wraps, with Go's text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum UrlError {
    #[error("parse {}: {reason}", go_quote(url.as_bytes()))]
    Parse { url: String, reason: String },
    /// `URL.JoinPath` returns `setPath`'s escape error unwrapped.
    #[error("{0}")]
    Escape(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Userinfo {
    pub username: String,
    pub password: Option<String>,
}

impl fmt::Display for Userinfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&escape(&self.username, Encoding::UserPassword))?;
        if let Some(password) = &self.password {
            write!(f, ":{}", escape(password, Encoding::UserPassword))?;
        }
        Ok(())
    }
}

/// Go's `url.URL`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Url {
    pub scheme: String,
    pub opaque: String,
    pub user: Option<Userinfo>,
    pub host: String,
    pub path: String,
    pub fragment: String,
    pub raw_query: String,
    pub raw_path: String,
    pub raw_fragment: String,
    pub force_query: bool,
    pub omit_host: bool,
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn unhex(c: u8) -> u8 {
    9 * (c >> 6) + (c & 15)
}

pub(crate) fn should_escape(c: u8, mode: Encoding) -> bool {
    if c.is_ascii_alphanumeric() {
        return false;
    }
    if matches!(mode, Encoding::Host | Encoding::Zone)
        && matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'['
                | b']'
                | b'<'
                | b'>'
                | b'"'
        )
    {
        return false;
    }
    match c {
        b'-' | b'_' | b'.' | b'~' => return false,
        b'$' | b'&' | b'+' | b',' | b'/' | b':' | b';' | b'=' | b'?' | b'@' => match mode {
            Encoding::Path => return c == b'?',
            Encoding::PathSegment => return matches!(c, b'/' | b';' | b',' | b'?'),
            Encoding::UserPassword => return matches!(c, b'@' | b'/' | b'?' | b':'),
            Encoding::QueryComponent => return true,
            Encoding::Fragment => return false,
            Encoding::Host | Encoding::Zone => {}
        },
        _ => {}
    }
    !(mode == Encoding::Fragment && matches!(c, b'!' | b'(' | b')' | b'*'))
}

/// Go's `strconv.Quote`.
pub(crate) fn go_quote(text: &[u8]) -> String {
    let mut out = String::from("\"");
    for chunk in text.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{7}' => out.push_str("\\a"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{b}' => out.push_str("\\v"),
                c if c < ' ' || c == '\u{7f}' => {
                    let _ = write!(out, "\\x{:02x}", u32::from(c));
                }
                c if c.is_control() => {
                    let _ = write!(out, "\\u{:04x}", u32::from(c));
                }
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out.push('"');
    out
}

fn quote(text: &[u8]) -> String {
    go_quote(text)
}

/// Go's `unescape`; the error text is Go's `EscapeError` or `InvalidHostError`.
pub(crate) fn unescape(s: &str, mode: Encoding) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut count = 0;
    let mut has_plus = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                count += 1;
                if i + 2 >= bytes.len() || !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
                    let end = (i + 3).min(bytes.len());
                    return Err(format!("invalid URL escape {}", quote(&bytes[i..end])));
                }
                let escaped = &bytes[i..i + 3];
                if mode == Encoding::Host && unhex(bytes[i + 1]) < 8 && escaped != b"%25" {
                    return Err(format!("invalid URL escape {}", quote(escaped)));
                }
                if mode == Encoding::Zone {
                    let value = (unhex(bytes[i + 1]) << 4) | unhex(bytes[i + 2]);
                    if escaped != b"%25" && value != b' ' && should_escape(value, Encoding::Host) {
                        return Err(format!("invalid URL escape {}", quote(escaped)));
                    }
                }
                i += 3;
            }
            b'+' => {
                has_plus = mode == Encoding::QueryComponent;
                i += 1;
            }
            c => {
                if matches!(mode, Encoding::Host | Encoding::Zone)
                    && c < 0x80
                    && should_escape(c, mode)
                {
                    return Err(format!(
                        "invalid character {} in host name",
                        quote(&bytes[i..=i])
                    ));
                }
                i += 1;
            }
        }
    }
    if count == 0 && !has_plus {
        return Ok(s.to_owned());
    }
    let plus = if mode == Encoding::QueryComponent {
        b' '
    } else {
        b'+'
    };
    let mut out = Vec::with_capacity(bytes.len() - 2 * count);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                out.push((unhex(bytes[i + 1]) << 4) | unhex(bytes[i + 2]));
                i += 2;
            }
            b'+' => out.push(plus),
            c => out.push(c),
        }
        i += 1;
    }
    // Go strings hold arbitrary bytes; decoded invalid UTF-8 is kept lossily, which only
    // matters for display, never for the host and path checks.
    Ok(String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned()))
}

pub(crate) fn escape(s: &str, mode: Encoding) -> String {
    const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c == b' ' && mode == Encoding::QueryComponent {
            out.push('+');
        } else if should_escape(c, mode) {
            out.push('%');
            out.push(char::from(UPPER_HEX[usize::from(c >> 4)]));
            out.push(char::from(UPPER_HEX[usize::from(c & 15)]));
        } else {
            out.push(char::from(c));
        }
    }
    out
}

/// Go's `url.QueryEscape`.
pub(crate) fn query_escape(s: &str) -> String {
    escape(s, Encoding::QueryComponent)
}

/// Go's `url.QueryUnescape`.
pub(crate) fn query_unescape(s: &str) -> Result<String, String> {
    unescape(s, Encoding::QueryComponent)
}

/// Go's `url.PathUnescape`.
pub(crate) fn path_unescape(s: &str) -> Result<String, String> {
    unescape(s, Encoding::PathSegment)
}

fn get_scheme(raw: &str) -> Result<(&str, &str), String> {
    for (i, c) in raw.bytes().enumerate() {
        match c {
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' => {
                if i == 0 {
                    return Ok(("", raw));
                }
            }
            b':' => {
                if i == 0 {
                    return Err("missing protocol scheme".to_owned());
                }
                return Ok((&raw[..i], &raw[i + 1..]));
            }
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

fn valid_optional_port(port: &str) -> bool {
    port.is_empty() || (port.starts_with(':') && port[1..].bytes().all(|b| b.is_ascii_digit()))
}

fn valid_userinfo(s: &str) -> bool {
    s.chars().all(|r| {
        r.is_ascii_alphanumeric()
            || matches!(
                r,
                '-' | '.'
                    | '_'
                    | ':'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
                    | '@'
            )
    })
}

fn parse_ip_literal(hostname: &str) -> Result<(), String> {
    if hostname.parse::<Ipv4Addr>().is_ok() {
        return Err("invalid IP-literal".to_owned());
    }
    let address = hostname
        .split_once('%')
        .map_or(hostname, |(address, _)| address);
    // netip.ParseAddr rejects an empty zone.
    if hostname.ends_with('%') || address.parse::<Ipv6Addr>().is_err() {
        return Err(format!(
            "invalid host: ParseAddr({hostname:?}): unable to parse IP"
        ));
    }
    Ok(())
}

fn parse_host(scheme: &str, host: &str) -> Result<String, String> {
    match host.rfind('[') {
        Some(open) if open > 0 => return Err("invalid IP-literal".to_owned()),
        Some(_) => {
            let Some(close) = host.rfind(']') else {
                return Err("missing ']' in host".to_owned());
            };
            let colon_port = &host[close + 1..];
            if !valid_optional_port(colon_port) {
                return Err(format!("invalid port {colon_port:?} after host"));
            }
            let colon_port = unescape(colon_port, Encoding::Host)?;
            let hostname = &host[1..close];
            let unescaped = if let Some(zone) = hostname.find("%25") {
                unescape(&hostname[..zone], Encoding::Host)?
                    + &unescape(&hostname[zone..], Encoding::Zone)?
            } else {
                unescape(hostname, Encoding::Host)?
            };
            parse_ip_literal(&unescaped)?;
            return Ok(format!("[{unescaped}]{colon_port}"));
        }
        None => {}
    }
    if let Some(first) = host.find(':') {
        let last = host.rfind(':').unwrap_or(first);
        // Go 1.26 enforces strict colons for http and https only.
        let colon = if last != first && scheme != "http" && scheme != "https" {
            last
        } else {
            first
        };
        let colon_port = &host[colon..];
        if !valid_optional_port(colon_port) {
            return Err(format!("invalid port {colon_port:?} after host"));
        }
    }
    unescape(host, Encoding::Host)
}

fn parse_authority(scheme: &str, authority: &str) -> Result<(Option<Userinfo>, String), String> {
    let at = authority.rfind('@');
    let host = parse_host(scheme, at.map_or(authority, |at| &authority[at + 1..]))?;
    let Some(at) = at else {
        return Ok((None, host));
    };
    let userinfo = &authority[..at];
    if !valid_userinfo(userinfo) {
        return Err("net/url: invalid userinfo".to_owned());
    }
    let user = match userinfo.split_once(':') {
        None => Userinfo {
            username: unescape(userinfo, Encoding::UserPassword)?,
            password: None,
        },
        Some((username, password)) => Userinfo {
            username: unescape(username, Encoding::UserPassword)?,
            password: Some(unescape(password, Encoding::UserPassword)?),
        },
    };
    Ok((Some(user), host))
}

fn parse_inner(raw: &str) -> Result<Url, String> {
    if raw.bytes().any(|b| b < b' ' || b == 0x7f) {
        return Err("net/url: invalid control character in URL".to_owned());
    }
    let mut url = Url::default();
    if raw == "*" {
        "*".clone_into(&mut url.path);
        return Ok(url);
    }
    let (scheme, rest) = get_scheme(raw)?;
    url.scheme = scheme.to_ascii_lowercase();
    let mut rest = if rest.ends_with('?') && rest.matches('?').count() == 1 {
        url.force_query = true;
        &rest[..rest.len() - 1]
    } else if let Some((before, query)) = rest.split_once('?') {
        query.clone_into(&mut url.raw_query);
        before
    } else {
        rest
    };
    if !rest.starts_with('/') {
        if !url.scheme.is_empty() {
            rest.clone_into(&mut url.opaque);
            return Ok(url);
        }
        let segment = rest.split('/').next().unwrap_or_default();
        if segment.contains(':') {
            return Err("first path segment in URL cannot contain colon".to_owned());
        }
    }
    if (!url.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
        let mut authority = &rest[2..];
        rest = "";
        if let Some(slash) = authority.find('/') {
            rest = &authority[slash..];
            authority = &authority[..slash];
        }
        (url.user, url.host) = parse_authority(&url.scheme, authority)?;
    } else if !url.scheme.is_empty() && rest.starts_with('/') {
        url.omit_host = true;
    }
    url.set_path(rest)?;
    Ok(url)
}

/// Go's `url.Parse`.
pub(crate) fn parse(raw: &str) -> Result<Url, UrlError> {
    let (before, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    let mut url = parse_inner(before).map_err(|reason| UrlError::Parse {
        url: before.to_owned(),
        reason,
    })?;
    if fragment.is_empty() {
        return Ok(url);
    }
    url.set_fragment(fragment)
        .map_err(|reason| UrlError::Parse {
            url: raw.to_owned(),
            reason,
        })?;
    Ok(url)
}

fn valid_encoded(s: &str, mode: Encoding) -> bool {
    s.bytes().all(|c| {
        matches!(
            c,
            b'!' | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'['
                | b']'
                | b'%'
        ) || !should_escape(c, mode)
    })
}

/// Go's `resolvePath`.
fn resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_owned()
    } else if !reference.starts_with('/') {
        let cut = base.rfind('/').map_or(0, |i| i + 1);
        format!("{}{reference}", &base[..cut])
    } else {
        reference.to_owned()
    };
    if full.is_empty() {
        return String::new();
    }
    let mut dst = String::from("/");
    let mut first = true;
    let mut elem = "";
    for part in full.split('/') {
        elem = part;
        if elem == "." {
            first = false;
            continue;
        }
        if elem == ".." {
            let rest = dst[1..].to_owned();
            dst = String::from("/");
            match rest.rfind('/') {
                None => first = true,
                Some(index) => dst.push_str(&rest[..index]),
            }
        } else {
            if !first {
                dst.push('/');
            }
            dst.push_str(elem);
            first = false;
        }
    }
    if elem == "." || elem == ".." {
        dst.push('/');
    }
    if dst.len() > 1 && dst.as_bytes()[1] == b'/' {
        dst.remove(0);
    }
    dst
}

/// Go's `path.Clean`.
fn clean(path: &str) -> String {
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

/// Go's `path.Join`.
fn join(elements: &[&str]) -> String {
    let parts: Vec<&str> = elements.iter().copied().filter(|e| !e.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    clean(&parts.join("/"))
}

impl Url {
    fn set_path(&mut self, p: &str) -> Result<(), String> {
        self.path = unescape(p, Encoding::Path)?;
        self.raw_path = if escape(&self.path, Encoding::Path) == p {
            String::new()
        } else {
            p.to_owned()
        };
        Ok(())
    }

    fn set_fragment(&mut self, f: &str) -> Result<(), String> {
        self.fragment = unescape(f, Encoding::Fragment)?;
        self.raw_fragment = if escape(&self.fragment, Encoding::Fragment) == f {
            String::new()
        } else {
            f.to_owned()
        };
        Ok(())
    }

    /// Go's `EscapedPath`.
    pub(crate) fn escaped_path(&self) -> String {
        if !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Encoding::Path)
            && unescape(&self.raw_path, Encoding::Path).is_ok_and(|p| p == self.path)
        {
            return self.raw_path.clone();
        }
        if self.path == "*" {
            return "*".to_owned();
        }
        escape(&self.path, Encoding::Path)
    }

    fn escaped_fragment(&self) -> String {
        if !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Encoding::Fragment)
            && unescape(&self.raw_fragment, Encoding::Fragment).is_ok_and(|f| f == self.fragment)
        {
            return self.raw_fragment.clone();
        }
        escape(&self.fragment, Encoding::Fragment)
    }

    fn split_host_port(&self) -> (&str, &str) {
        let mut host = self.host.as_str();
        let mut port = "";
        if let Some(colon) = host.rfind(':')
            && valid_optional_port(&host[colon..])
        {
            port = &host[colon + 1..];
            host = &host[..colon];
        }
        if host.starts_with('[') && host.ends_with(']') && host.len() >= 2 {
            host = &host[1..host.len() - 1];
        }
        (host, port)
    }

    /// Go's `Hostname`.
    pub(crate) fn hostname(&self) -> &str {
        self.split_host_port().0
    }

    /// Go's `Port`.
    pub(crate) fn port(&self) -> &str {
        self.split_host_port().1
    }

    /// Go's `ResolveReference`.
    pub(crate) fn resolve_reference(&self, reference: &Url) -> Url {
        let mut url = reference.clone();
        if reference.scheme.is_empty() {
            url.scheme.clone_from(&self.scheme);
        }
        if !reference.scheme.is_empty() || !reference.host.is_empty() || reference.user.is_some() {
            // The resolved path is validly escaped, as in Go.
            let _ = url.set_path(&resolve_path(&reference.escaped_path(), ""));
            return url;
        }
        if !reference.opaque.is_empty() {
            url.user = None;
            url.host = String::new();
            url.path = String::new();
            return url;
        }
        if reference.path.is_empty() && !reference.force_query && reference.raw_query.is_empty() {
            url.raw_query.clone_from(&self.raw_query);
            if reference.fragment.is_empty() {
                url.fragment.clone_from(&self.fragment);
                url.raw_fragment.clone_from(&self.raw_fragment);
            }
        }
        if reference.path.is_empty() && !self.opaque.is_empty() {
            url.opaque.clone_from(&self.opaque);
            url.user = None;
            url.host = String::new();
            url.path = String::new();
            return url;
        }
        url.host.clone_from(&self.host);
        url.user.clone_from(&self.user);
        let _ = url.set_path(&resolve_path(
            &self.escaped_path(),
            &reference.escaped_path(),
        ));
        url
    }

    /// Go's `URL.JoinPath` error path, used by `url.JoinPath`.
    fn join_path(&self, elements: &[&str]) -> Result<Url, String> {
        let escaped = self.escaped_path();
        let relative = !escaped.starts_with('/');
        let first = if relative {
            format!("/{escaped}")
        } else {
            escaped
        };
        let mut all: Vec<&str> = vec![&first];
        all.extend_from_slice(elements);
        let mut p = join(&all);
        if relative {
            p.remove(0);
        }
        if all.last().is_some_and(|last| last.ends_with('/')) && !p.ends_with('/') {
            p.push('/');
        }
        let mut url = self.clone();
        url.set_path(&p)?;
        Ok(url)
    }
}

/// Go's `url.JoinPath`.
pub(crate) fn join_path(base: &str, elements: &[&str]) -> Result<String, UrlError> {
    let url = parse(base)?;
    url.join_path(elements)
        .map(|url| url.to_string())
        .map_err(UrlError::Escape)
}

impl fmt::Display for Url {
    /// Go's `URL.String`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = String::new();
        if !self.scheme.is_empty() {
            buf.push_str(&self.scheme);
            buf.push(':');
        }
        if self.opaque.is_empty() {
            let no_authority = self.host.is_empty() && self.user.is_none();
            if !(no_authority && (self.scheme.is_empty() || self.omit_host)) {
                if !self.host.is_empty() || !self.path.is_empty() || self.user.is_some() {
                    buf.push_str("//");
                }
                if let Some(user) = &self.user {
                    buf.push_str(&user.to_string());
                    buf.push('@');
                }
                buf.push_str(&escape(&self.host, Encoding::Host));
            }
            let path = self.escaped_path();
            if !path.is_empty() && !path.starts_with('/') && !self.host.is_empty() {
                buf.push('/');
            }
            if buf.is_empty() && path.split('/').next().is_some_and(|s| s.contains(':')) {
                buf.push_str("./");
            }
            buf.push_str(&path);
        } else {
            buf.push_str(&self.opaque);
        }
        if self.force_query || !self.raw_query.is_empty() {
            buf.push('?');
            buf.push_str(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            buf.push('#');
            buf.push_str(&self.escaped_fragment());
        }
        f.write_str(&buf)
    }
}

#[cfg(test)]
mod tests;
