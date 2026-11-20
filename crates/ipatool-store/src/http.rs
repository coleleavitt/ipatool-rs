//! The HTTP values the Store emulator and backend exchange: Go's `http.Header`,
//! `http.Request` and `http.Response` as far as the Go Store code uses them.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Cursor, Read};

use crate::gourl;

/// A boxed transport or body error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Go's `textproto.CanonicalMIMEHeaderKey`: `content-type` becomes `Content-Type`. Keys
/// with bytes outside the header token set are returned unchanged, as in Go.
#[must_use]
pub fn canonical_header_key(key: &str) -> String {
    let valid = |c: u8| {
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
    };
    if !key.bytes().all(valid) {
        return key.to_owned();
    }
    let mut upper = true;
    key.bytes()
        .map(|c| {
            let c = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == b'-';
            char::from(c)
        })
        .collect()
}

/// Go's `http.Header`: canonical keys with their values in arrival order. Keys are sorted,
/// which only fixes the order Go leaves random.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header(BTreeMap<String, Vec<String>>);

impl Header {
    /// An empty header set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Go's `Get`: the first value.
    #[must_use]
    pub fn get(&self, key: &str) -> &str {
        self.0
            .get(&canonical_header_key(key))
            .and_then(|values| values.first())
            .map_or("", String::as_str)
    }

    /// Go's `Values`.
    #[must_use]
    pub fn values(&self, key: &str) -> &[String] {
        self.0
            .get(&canonical_header_key(key))
            .map_or(&[], Vec::as_slice)
    }

    /// Go's `Set`.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.insert(canonical_header_key(key), vec![value.into()]);
    }

    /// Go's `Add`.
    pub fn add(&mut self, key: &str, value: impl Into<String>) {
        self.0
            .entry(canonical_header_key(key))
            .or_default()
            .push(value.into());
    }

    /// Go's `Del`.
    pub fn del(&mut self, key: &str) {
        self.0.remove(&canonical_header_key(key));
    }

    /// Every key with its values, as Go's `range` over the map.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.0
            .iter()
            .map(|(key, values)| (key.as_str(), values.as_slice()))
    }

    /// Whether no header is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A header set as Go's HTTP client reads it from the wire.
    #[must_use]
    pub fn from_wire(headers: &reqwest::header::HeaderMap) -> Self {
        let mut header = Self::new();
        for (name, value) in headers {
            header.add(
                name.as_str(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            );
        }
        header
    }
}

/// Go's `http.Request` as the Store transport sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Header,
    pub body: Vec<u8>,
}

/// Go's `http.Response`: the body is a reader, so a transfer can stream it.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Header,
    pub body: Box<dyn Read + Send>,
    /// Go's `ContentLength`: `-1` when unknown.
    pub content_length: i64,
    /// Go's `Response.Request.URL`, which `location` resolves against.
    pub request_url: Option<String>,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("content_length", &self.content_length)
            .field("request_url", &self.request_url)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// A response with an in-memory body.
    #[must_use]
    pub fn new(status: u16, headers: Header, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            content_length: i64::try_from(body.len()).unwrap_or(i64::MAX),
            body: Box::new(Cursor::new(body)),
            request_url: None,
        }
    }

    /// Replaces the body with `data`, as Go's `io.NopCloser(bytes.NewReader(data))`.
    pub fn set_body(&mut self, data: Vec<u8>) {
        self.body = Box::new(Cursor::new(data));
    }

    /// Reads the whole body.
    ///
    /// # Errors
    /// Fails when the body reader fails.
    pub fn read_body(&mut self) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        self.body.read_to_end(&mut data)?;
        Ok(data)
    }

    /// Go's `Response.Location`: the `Location` header resolved against the request URL.
    ///
    /// # Errors
    /// Fails when the header is missing or does not parse.
    pub(crate) fn location(&self) -> Result<gourl::Url, LocationError> {
        let value = self.headers.get("Location");
        if value.is_empty() {
            return Err(LocationError::Missing);
        }
        let reference = gourl::parse(value).map_err(LocationError::Parse)?;
        Ok(match self.request_url.as_deref().map(gourl::parse) {
            Some(Ok(base)) => base.resolve_reference(&reference),
            _ => reference,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LocationError {
    #[error("http: no Location header in response")]
    Missing,
    #[error(transparent)]
    Parse(gourl::UrlError),
}

/// Go's `func(*http.Request) (*http.Response, error)` Store transport.
pub type Transport = Box<dyn FnMut(HttpRequest) -> Result<HttpResponse, BoxError>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_like_go() {
        for (key, want) in [
            ("content-type", "Content-Type"),
            ("X-APPLE-STORE-FRONT", "X-Apple-Store-Front"),
            ("x-dsid", "X-Dsid"),
            ("icloud-dsid", "Icloud-Dsid"),
            ("a b", "a b"),
            ("", ""),
            ("-x-", "-X-"),
        ] {
            assert_eq!(canonical_header_key(key), want);
        }
    }

    #[test]
    fn location_resolves_against_the_request() {
        let mut headers = Header::new();
        headers.set("location", "/target?x=1");
        let mut response = HttpResponse::new(302, headers, Vec::new());
        assert_eq!(
            response.location().map(|u| u.to_string()).ok().as_deref(),
            Some("/target?x=1")
        );
        response.request_url = Some("https://buy.itunes.apple.com/a/b".to_owned());
        assert_eq!(
            response.location().map(|u| u.to_string()).ok().as_deref(),
            Some("https://buy.itunes.apple.com/target?x=1")
        );
        response.headers.del("Location");
        assert!(matches!(response.location(), Err(LocationError::Missing)));
    }
}
