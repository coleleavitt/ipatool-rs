//! Apple's XML property-list envelopes (`pkg/http/client.go`): responses can wrap
//! the plist in `<Document>`, embed it in HTML, or send a bare `<dict>` or `<key>` list.

use std::sync::LazyLock;

use regex::bytes::Regex;
use serde::de::DeserializeOwned;

static DOCUMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<Document\b[^>]*>(.*)</Document>").expect("valid regex"));
static PLIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<plist\b[^>]*>.*?</plist>").expect("valid regex"));
static DICT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<dict\b[^>]*>.*</dict>").expect("valid regex"));
static HTML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]*>").expect("valid regex"));

/// Go's `normalizeXMLPlistBody`: the innermost `<dict>` Apple sent, or the trimmed body.
pub(crate) fn normalize(body: &[u8]) -> Vec<u8> {
    let mut normalized = body.trim_ascii();
    if normalized.is_empty() {
        return Vec::new();
    }
    if let Some(inner) = DOCUMENT
        .captures(normalized)
        .and_then(|captures| captures.get(1))
        .map(|inner| inner.as_bytes().trim_ascii())
        .filter(|inner| !inner.is_empty())
    {
        normalized = inner;
    }
    if let Some(plist) = PLIST.find(normalized) {
        normalized = plist.as_bytes().trim_ascii();
    }
    if let Some(dict) = DICT.find(normalized) {
        return dict.as_bytes().trim_ascii().to_vec();
    }
    if normalized.windows(5).any(|window| window == b"<key>") {
        return [b"<dict>", normalized, b"</dict>"].concat();
    }
    normalized.to_vec()
}

/// Go's `looksLikePropertyList`: keeps HTML error pages away from the plist parser.
pub(crate) fn looks_like_plist(body: &[u8]) -> bool {
    let trimmed = body.trim_ascii();
    if trimmed.starts_with(b"bplist") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    [&b"<?xml"[..], b"<plist", b"<dict", b"<key"]
        .iter()
        .any(|marker| lower.windows(marker.len()).any(|window| window == *marker))
}

/// Go's `bodySnippet`: tag-free, whitespace-collapsed, at most 200 bytes plus an ellipsis.
const SNIPPET_MAX_LEN: usize = 200;

pub(crate) fn snippet(body: &[u8]) -> String {
    let text = HTML_TAG.replace_all(body, &b" "[..]);
    let mut snippet = String::from_utf8_lossy(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if snippet.len() > SNIPPET_MAX_LEN {
        let mut end = SNIPPET_MAX_LEN;
        while !snippet.is_char_boundary(end) {
            end -= 1;
        }
        snippet.truncate(end);
        snippet.push('…');
    }
    snippet
}

/// Decodes a normalized body. The `plist` crate wants a `<plist>` root; Go's parser does not.
pub(crate) fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, plist::Error> {
    let normalized = normalize(body);
    if normalized.starts_with(b"bplist") || normalized.starts_with(b"<plist") {
        return plist::from_bytes(&normalized);
    }
    plist::from_bytes(&[b"<plist version=\"1.0\">", &normalized[..], b"</plist>"].concat())
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Foo {
        foo: String,
    }

    #[test]
    fn decodes_apple_envelopes() {
        for body in [
            "<plist><dict><key>foo</key><string>bar</string></dict></plist>",
            "<Document><dict><key>foo</key><string>bar</string></dict></Document>",
            "<Document><Protocol><plist><dict><key>foo</key><string>bar</string></dict></plist></Protocol></Document>",
            "<key>foo</key><string>bar</string>",
            "<?xml version=\"1.0\"?><!DOCTYPE plist><plist version=\"1.0\"><dict><key>foo</key><string>bar</string></dict></plist>",
        ] {
            let decoded: Foo =
                decode(body.as_bytes()).unwrap_or_else(|err| panic!("{body}: {err}"));
            assert_eq!(decoded.foo, "bar", "{body}");
        }
    }

    #[test]
    fn recognizes_plists_and_html() {
        assert!(looks_like_plist(b"bplist00"));
        assert!(looks_like_plist(b"  <?XML version"));
        assert!(!looks_like_plist(b"<html><h1>Error</h1></html>"));
        assert!(!looks_like_plist(b"   "));
    }

    #[test]
    fn snippets_strip_markup_and_truncate() {
        assert_eq!(
            snippet(b"<html><h1>Service\n  Unavailable</h1></html>"),
            "Service Unavailable"
        );
        let long = "é".repeat(150);
        let cut = snippet(long.as_bytes());
        assert!(cut.ends_with('…'));
        assert_eq!(cut.len(), 200 + '…'.len_utf8());
    }
}
