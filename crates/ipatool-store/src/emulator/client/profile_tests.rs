//! Go's `generated_profile_test.go` (the Execute and `NSURLComponents` tables; the
//! missing-page case is in `emulator/tests.rs`). Opt-in: `IPATOOL_TEST_SAP_CACHE=1` (needs
//! the SAP asset cache filled; nothing leaves the process).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use super::*;
use crate::emulator::bridge::import_value;
use crate::emulator::runtime;

fn enabled() -> bool {
    std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_some()
}

fn guest() -> Runtime {
    let bundle = ipatool_sap::assets::load().expect("cached Apple frameworks");
    let prepared = crate::profile::load().expect("embedded profile");
    runtime::open(&bundle.apple_media_services, prepared, true).expect("guest")
}

fn strings(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), Value::String((*value).to_owned())))
        .collect()
}

fn bag() -> BTreeMap<String, Value> {
    strings(&[("buyProduct", "https://buy.itunes.apple.com/request")])
}

const CREDENTIALS: &[(&str, &str)] = &[
    ("salableAdamId", "1234"),
    ("pricingParameters", "STDQ"),
    ("price", "0"),
    ("productType", "C"),
    ("appleId", "test@example.com"),
    ("password", "test"),
];

#[test]
fn encodes_and_handles_responses_through_the_pinned_frameworks() {
    if !enabled() {
        return;
    }
    for (operation, saved_account) in [
        ("authenticate", false),
        ("authenticate", true),
        ("purchase", true),
        ("download", true),
        ("search", true),
        ("lookup", false),
    ] {
        let mut guest = guest();
        let calls = Rc::new(Cell::new(0));
        let counter = Rc::clone(&calls);
        let mut headers = Header::new();
        if saved_account {
            headers.set("X-Dsid", "123456");
            headers.set("X-Token", "test-token");
            headers.set("X-Apple-Store-Front", "143441-1,29");
        }
        let response = guest
            .execute(Request {
                operation: operation.to_owned(),
                url: "https://buy.itunes.apple.com/request".to_owned(),
                method: "POST".to_owned(),
                hardware_id: vec![0, 1, 2, 3, 4, 5],
                headers,
                parameters: Some(strings(CREDENTIALS)),
                bag: Some(bag()),
                sign: Some(Box::new(|_: &[u8]| Ok(vec![1, 2, 3]))),
                transport: Some(Box::new(move |request| {
                    counter.set(counter.get() + 1);
                    assert!(request.url.starts_with("https://buy.itunes.apple.com/"), "{}", request.url);
                    assert_eq!(request.method, "POST");
                    let mut headers = Header::new();
                    headers.set("Content-Type", "application/x-apple-plist");
                    Ok(HttpResponse::new(
                        200,
                        headers,
                        br#"<?xml version="1.0"?><plist version="1.0"><dict><key>status</key><integer>0</integer></dict></plist>"#.to_vec(),
                    ))
                })),
                ..Request::default()
            })
            .unwrap_or_else(|err| panic!("{operation}: {err}"));
        assert_eq!(calls.get(), 1, "{operation}");
        assert_eq!(response.status, 200, "{operation}");
    }
}

#[test]
fn preserves_catalog_transfer_and_finance_responses() {
    if !enabled() {
        return;
    }
    let cases: [(&str, &str, &str, &[u8], u16, bool); 6] = [
        ("search", "GET", "application/json", br#"{"resultCount":1,"results":[{"bundleId":"fixture-value"}]}"#, 200, false),
        ("lookup", "GET", "application/json", br#"{"results":{"fixture-value":{"offers":[]}}}"#, 200, false),
        ("download", "GET", "application/octet-stream", b"fixture-value\x00\xff", 206, true),
        ("authenticate", "POST", "application/x-apple-plist", br#"<plist version="1.0"><dict><key>failureType</key><string>2034</string><key>customerMessage</key><string>fixture-value</string></dict></plist>"#, 200, false),
        ("authenticate", "POST", "application/x-apple-plist", br#"<plist version="1.0"><dict><key>customerMessage</key><string>fixture-value</string><key>dialog</key><dict><key>message</key><string>Account locked</string><key>explanation</key><string>Reset the account to continue.</string><key>okButtonString</key><string>Reset</string><key>okButtonAction</key><dict><key>kind</key><string>OpenURL</string><key>url</key><string>https://iforgot.apple.com/cgi-bin/iforgot.cgi</string></dict></dict></dict></plist>"#, 200, false),
        ("purchase", "POST", "application/x-apple-plist", br#"<plist version="1.0"><dict><key>failureType</key><string>9610</string><key>customerMessage</key><string>fixture-value</string></dict></plist>"#, 200, false),
    ];
    for (operation, method, content_type, body, status, stream) in cases {
        let mut guest = guest();
        let finance = matches!(operation, "authenticate" | "purchase");
        let mut response = guest
            .execute(Request {
                operation: operation.to_owned(),
                url: "https://buy.itunes.apple.com/request".to_owned(),
                method: method.to_owned(),
                hardware_id: vec![0, 1, 2, 3, 4, 5],
                stream,
                bag: Some(bag()),
                parameters: finance.then(|| strings(CREDENTIALS)),
                sign: finance.then(|| Box::new(|_: &[u8]| Ok(vec![1u8, 2, 3])) as Signer),
                transport: Some(Box::new(move |request| {
                    assert_eq!(request.method, method);
                    let mut headers = Header::new();
                    headers.set("Content-Type", content_type);
                    Ok(HttpResponse::new(status, headers, body.to_vec()))
                })),
                ..Request::default()
            })
            .unwrap_or_else(|err| panic!("{operation} {method}: {err}"));
        assert_eq!(response.status, status, "{operation}");
        let actual = response.read_body().expect("body");
        if stream {
            assert_eq!(actual, body, "{operation}");
        } else {
            assert!(
                String::from_utf8_lossy(&actual).contains("fixture-value"),
                "{operation}: {}",
                String::from_utf8_lossy(&actual)
            );
        }
    }
}

#[test]
fn preserves_query_values_through_the_runtime_bridge() {
    if !enabled() {
        return;
    }
    let cases: [(&str, Option<&str>, &str, &str); 7] = [
        ("setPercentEncodedQuery:", None, "", ""),
        ("setPercentEncodedQuery:", Some(""), "", ""),
        (
            "setPercentEncodedQuery:",
            Some("q=a%20b%2Bc+%26%3D%25%23&symbol=%e2%9c%93"),
            "q=a%20b%2Bc+%26%3D%25%23&symbol=%e2%9c%93",
            "q=a b+c+&=%#&symbol=✓",
        ),
        (
            "setPercentEncodedQuery:",
            Some("q=/?@!$&'()*+,;=:"),
            "q=/?@!$&'()*+,;=:",
            "q=/?@!$&'()*+,;=:",
        ),
        ("setQuery:", None, "", ""),
        ("setQuery:", Some(""), "", ""),
        (
            "setQuery:",
            Some("q=a b+%&symbol=✓"),
            "q=a%20b+%25&symbol=%E2%9C%93",
            "q=a b+%&symbol=✓",
        ),
    ];
    for (selector, value, encoded, decoded) in cases {
        let mut guest = guest();
        let url = gourl::parse("https://example.com/path?old=value#fragment").expect("URL");
        let components = guest
            .new_object("NSURLComponents", Payload::Host(Rc::new(RefCell::new(url))))
            .expect("components");
        let old = guest
            .new_object("NSString", Payload::String("old=value".to_owned()))
            .expect("string");
        guest
            .invoke(components, "setQuery:", &[old])
            .expect("setQuery:");
        let argument = match value {
            None => 0,
            Some(value) => {
                import_value(&mut guest.uc, &Value::String(value.to_owned())).expect("import")
            }
        };
        guest
            .invoke(components, selector, &[argument])
            .expect("setter");
        let encoded_query = guest
            .invoke(components, "percentEncodedQuery", &[])
            .expect("percentEncodedQuery");
        let query = guest.invoke(components, "query", &[]).expect("query");
        let mut expected = "https://example.com/path".to_owned();
        if value.is_none() {
            assert_eq!((encoded_query, query), (0, 0), "{selector} {value:?}");
        } else {
            assert_ne!(encoded_query, 0);
            assert_ne!(query, 0);
            assert_eq!(
                guest.string(encoded_query).expect("string"),
                encoded,
                "{selector} {value:?}"
            );
            assert_eq!(
                guest.string(query).expect("string"),
                decoded,
                "{selector} {value:?}"
            );
            expected = format!("{expected}?{encoded}");
        }
        let result = guest.invoke(components, "URL", &[]).expect("URL");
        assert_eq!(
            guest.string(result).expect("string"),
            format!("{expected}#fragment")
        );
    }
}
