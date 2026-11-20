//! Checks against `tests/fixtures/network-oracle.json`, recorded from Go by
//! `scripts/network-oracle.sh`.

use serde_json::Value;

use super::*;
use crate::terms;

pub(crate) fn oracle() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/network-oracle.json"))
        .expect("the oracle fixture is valid JSON")
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

#[test]
fn parses_and_formats_like_go() {
    let oracle = oracle();
    let urls = oracle["urls"].as_object().expect("urls");
    assert!(urls.len() > 50);
    for (raw, want) in urls {
        assert_eq!(
            terms::is_page_url(raw),
            want["IsPageURL"],
            "IsPageURL {raw:?}"
        );
        match parse(raw) {
            Err(err) => assert_eq!(err.to_string(), text(want, "error"), "{raw:?}"),
            Ok(u) => {
                assert_eq!(text(want, "error"), "", "{raw:?} should fail");
                let user = u.user.as_ref().map(ToString::to_string);
                let got = [
                    ("Scheme", u.scheme.clone()),
                    ("Opaque", u.opaque.clone()),
                    ("Host", u.host.clone()),
                    ("Path", u.path.clone()),
                    ("RawPath", u.raw_path.clone()),
                    ("Fragment", u.fragment.clone()),
                    ("RawQuery", u.raw_query.clone()),
                    ("RawFragment", u.raw_fragment.clone()),
                    ("String", u.to_string()),
                    ("EscapedPath", u.escaped_path()),
                    ("Hostname", u.hostname().to_owned()),
                    ("Port", u.port().to_owned()),
                ];
                for (key, value) in got {
                    assert_eq!(value, text(want, key), "{key} of {raw:?}");
                }
                assert_eq!(user.as_deref(), want["User"].as_str(), "User of {raw:?}");
                assert_eq!(u.force_query, want["ForceQuery"], "ForceQuery of {raw:?}");
                assert_eq!(u.omit_host, want["OmitHost"], "OmitHost of {raw:?}");
                assert_eq!(
                    terms::is_buy_host(u.hostname()),
                    want["IsBuyHost"],
                    "{raw:?}"
                );
            }
        }
    }
}

#[test]
fn buy_hosts_match_go() {
    for (host, want) in oracle()["buy_hosts"].as_object().expect("buy_hosts") {
        assert_eq!(terms::is_buy_host(host), *want, "{host:?}");
    }
}

#[test]
fn resolves_references_like_go() {
    for case in oracle()["resolve"].as_array().expect("resolve") {
        let base = parse(&text(case, "Base")).expect("base parses");
        match parse(&text(case, "Reference")) {
            Err(err) => assert_eq!(err.to_string(), text(case, "Error")),
            Ok(reference) => assert_eq!(
                base.resolve_reference(&reference).to_string(),
                text(case, "Result"),
                "{case}"
            ),
        }
    }
}

#[test]
fn joins_paths_like_go() {
    for case in oracle()["join"].as_array().expect("join") {
        let elements: Vec<String> = case["Elements"].as_array().map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect()
        });
        let elements: Vec<&str> = elements.iter().map(String::as_str).collect();
        match join_path(&text(case, "Base"), &elements) {
            Ok(result) => assert_eq!(result, text(case, "Result"), "{case}"),
            Err(err) => assert_eq!(err.to_string(), text(case, "Error"), "{case}"),
        }
    }
}

#[test]
fn escapes_like_go() {
    for case in oracle()["escape"].as_array().expect("escape") {
        let input = text(case, "Input");
        assert_eq!(query_escape(&input), text(case, "QueryEscape"), "{input:?}");
        let check = |got: Result<String, String>, value: &str, error: &str| match got {
            Ok(got) => assert_eq!(got, text(case, value), "{input:?}"),
            Err(err) => assert_eq!(err, text(case, error), "{input:?}"),
        };
        check(
            query_unescape(&input),
            "QueryUnescape",
            "QueryUnescapeError",
        );
        check(path_unescape(&input), "PathUnescape", "PathUnescapeError");
    }
}

#[test]
fn parses_request_uris_like_go() {
    let u = parse_request_uri("https://downloaddispatch.itunes.apple.com/r/redownload#x").unwrap();
    // ParseRequestURI keeps `#` in the path.
    assert_eq!(u.path, "/r/redownload#x");
    assert_eq!(u.fragment, "");
    assert_eq!(
        parse_request_uri("").unwrap_err().to_string(),
        "parse \"\": empty url"
    );
    assert_eq!(
        parse_request_uri("relative/path").unwrap_err().to_string(),
        "parse \"relative/path\": invalid URI for request"
    );
    let u = parse_request_uri("https://h/p?").unwrap();
    assert!(u.force_query);
    let u = parse_request_uri("//host/path").unwrap();
    assert_eq!(u.host, "");
    assert_eq!(u.path, "//host/path");
}

#[test]
fn parses_queries_like_go() {
    let (values, error) = parse_query("a=1&b=2&a=3&&c");
    assert!(error.is_none());
    assert_eq!(values.values("a"), ["1", "3"]);
    assert_eq!(values.get("c"), "");
    assert_eq!(values.len(), 3);
    let (values, error) = parse_query("a=1;b=2&c=%zz&d=4");
    assert_eq!(
        error.as_deref(),
        Some("invalid semicolon separator in query")
    );
    assert_eq!(values.get("d"), "4");
    assert_eq!(values.values("c"), [] as [String; 0]);
    let mut values = Values::default();
    values.set("termsType", "MZStore.New Terms");
    values.set("cc", "JP");
    assert_eq!(values.encode(), "cc=JP&termsType=MZStore.New+Terms");
}
