//! `Runtime::execute` against `tests/fixtures/network-oracle.json`, which Go recorded with
//! the same requests and fake transport (`scripts/network-oracle.sh`). Opt-in:
//! `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP asset cache filled; nothing leaves the process).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value as Json;

use super::*;
use crate::emulator::runtime;
use crate::http::HttpRequest;

fn string_map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn header(pairs: &[(&str, &str)]) -> Header {
    let mut headers = Header::new();
    for (key, value) in pairs {
        headers.add(key, *value);
    }
    headers
}

const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>status</key><integer>0</integer><key>items</key><array><string>a</string><real>1.5</real></array><key>empty</key><string></string></dict></plist>"#;
const JSON: &str = r#"{"results":[{"trackId":1,"name":"x <y>"}],"count":1.5}"#;

/// The requests of Go's `oracleExecute`, with the response body and content type each
/// fake transport answers.
#[allow(clippy::too_many_lines, reason = "one table of Go's cases")]
fn specs() -> Vec<(
    &'static str,
    Request,
    &'static str,
    &'static str,
    &'static str,
)> {
    let purchase = |price: &str| {
        string_map(&[
            ("price", text(price)),
            ("productType", text("C")),
            ("salableAdamId", text("1234")),
            ("pricingParameters", text("STDQ")),
            ("guid", text("000102030405")),
        ])
    };
    let buy_bag = || {
        Some(string_map(&[(
            "buyProduct",
            text("https://buy.itunes.apple.com/request"),
        )]))
    };
    vec![
        (
            "plain GET",
            Request {
                operation: "request".to_owned(),
                method: "GET".to_owned(),
                url: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/ping?a=1&b=two"
                    .to_owned(),
                headers: header(&[
                    ("X-Dsid", "12345"),
                    ("X-Token", "token"),
                    ("X-Apple-Store-Front", "143441-1,29"),
                    ("User-Agent", "oracle"),
                ]),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
        (
            "encoded POST",
            Request {
                operation: "request".to_owned(),
                method: "POST".to_owned(),
                url: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases"
                    .to_owned(),
                parameters: Some(string_map(&[
                    ("guid", text("000102030405")),
                    ("count", Value::Integer(3)),
                    ("flag", Value::Boolean(true)),
                    ("nested", Value::Dictionary(string_map(&[("k", text("v"))]))),
                ])),
                headers: header(&[("User-Agent", "oracle")]),
                ..Request::default()
            },
            PLIST,
            "text/xml; charset=UTF-8",
            "",
        ),
        (
            "json response",
            Request {
                operation: "request".to_owned(),
                method: "GET".to_owned(),
                url: "https://itunes.apple.com/lookup?id=1".to_owned(),
                format: "json".to_owned(),
                headers: header(&[("User-Agent", "oracle")]),
                ..Request::default()
            },
            JSON,
            "text/javascript; charset=utf-8",
            "",
        ),
        (
            "raw body PUT",
            Request {
                operation: "request".to_owned(),
                method: "PUT".to_owned(),
                url: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/raw".to_owned(),
                body: Some(b"raw-bytes".to_vec()),
                raw: true,
                headers: header(&[
                    ("User-Agent", "oracle"),
                    ("Content-Type", "application/octet-stream"),
                    ("Content-Length", "9"),
                ]),
                ..Request::default()
            },
            "opaque",
            "application/octet-stream",
            "",
        ),
        (
            "purchase",
            Request {
                operation: "purchase".to_owned(),
                method: "POST".to_owned(),
                url: "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct"
                    .to_owned(),
                bag: buy_bag(),
                parameters: Some(purchase("0")),
                headers: header(&[
                    ("User-Agent", "oracle"),
                    ("X-Dsid", "12345"),
                    ("X-Token", "token"),
                ]),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
        (
            "paid purchase",
            Request {
                operation: "purchase".to_owned(),
                method: "POST".to_owned(),
                url: "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct"
                    .to_owned(),
                bag: buy_bag(),
                parameters: Some(string_map(&[
                    ("price", text("990")),
                    ("productType", text("C")),
                ])),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
        (
            "disallowed host",
            Request {
                operation: "request".to_owned(),
                method: "GET".to_owned(),
                url: "https://example.com/x".to_owned(),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
        (
            "unsupported method",
            Request {
                operation: "request".to_owned(),
                method: "DELETE".to_owned(),
                url: "https://p25-buy.itunes.apple.com/x".to_owned(),
                parameters: Some(BTreeMap::new()),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
        (
            "redirect to another pod",
            Request {
                operation: "request".to_owned(),
                method: "POST".to_owned(),
                url: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases"
                    .to_owned(),
                parameters: Some(string_map(&[("a", text("b"))])),
                headers: header(&[
                    ("User-Agent", "oracle"),
                    ("X-Dsid", "12345"),
                    ("X-Token", "token"),
                    ("Cookie", "c=d"),
                ]),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "https://p26-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/listPurchases?x=1",
        ),
        (
            "redirect without following",
            Request {
                operation: "request".to_owned(),
                method: "GET".to_owned(),
                url: "https://p25-buy.itunes.apple.com/a".to_owned(),
                no_redirects: true,
                headers: header(&[("User-Agent", "oracle")]),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "/b",
        ),
        (
            "redirect to a foreign host",
            Request {
                operation: "request".to_owned(),
                method: "GET".to_owned(),
                url: "https://p25-buy.itunes.apple.com/a".to_owned(),
                headers: header(&[("User-Agent", "oracle")]),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "https://example.com/b",
        ),
        (
            "stream transfer",
            Request {
                operation: "transfer".to_owned(),
                method: "GET".to_owned(),
                url: "http://downloads.example.net/file.ipa".to_owned(),
                stream: true,
                headers: header(&[("User-Agent", "oracle")]),
                ..Request::default()
            },
            "PK-package-bytes",
            "application/octet-stream",
            "",
        ),
        (
            "authenticate",
            Request {
                operation: "authenticate".to_owned(),
                method: "POST".to_owned(),
                url: "https://p25-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate"
                    .to_owned(),
                parameters: Some(string_map(&[
                    ("appleId", text("user@example.com")),
                    ("password", text("secret")),
                    ("attempt", text("4")),
                    ("why", text("signIn")),
                    ("guid", text("000102030405")),
                ])),
                headers: header(&[("User-Agent", "oracle")]),
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "Go's byte(len(data) % 256)"
                )]
                sign: Some(Box::new(|data: &[u8]| {
                    Ok(vec![1, 2, 3, (data.len() % 256) as u8])
                })),
                ..Request::default()
            },
            PLIST,
            "text/xml",
            "",
        ),
    ]
}

/// Purchase bodies carry a fresh `clientCorrelationKey` UUID.
fn mask(body: &str) -> String {
    let key = "<key>clientCorrelationKey</key><string>";
    match body.find(key) {
        Some(start) => {
            let value = start + key.len();
            let end = body[value..].find('<').map_or(body.len(), |i| value + i);
            format!("{}UUID{}", &body[..value], &body[end..])
        }
        None => body.to_owned(),
    }
}

fn go_headers(value: &Json) -> BTreeMap<String, Vec<String>> {
    let strings = |values: &Json| -> Vec<String> {
        values.as_array().map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
    };
    value.as_object().map_or_else(BTreeMap::new, |map| {
        map.iter()
            .map(|(key, values)| (key.clone(), strings(values)))
            .collect()
    })
}

fn rust_headers(headers: &Header) -> BTreeMap<String, Vec<String>> {
    headers
        .iter()
        .map(|(key, values)| (key.to_owned(), values.to_vec()))
        .collect()
}

#[test]
fn executes_like_go() {
    if std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_none() {
        return;
    }
    let oracle: Json =
        serde_json::from_str(include_str!("../../../tests/fixtures/network-oracle.json"))
            .expect("the oracle fixture is valid JSON");
    let cases = oracle["execute"].as_array().expect("execute cases");
    let bundle = ipatool_sap::assets::load().expect("cached Apple frameworks");
    let prepared = crate::profile::load().expect("embedded profile");
    let specs = specs();
    assert_eq!(specs.len(), cases.len());
    for ((name, mut request, body, content_type, redirect), want) in specs.into_iter().zip(cases) {
        assert_eq!(want["Name"], name);
        let mut guest = runtime::open(&bundle.apple_media_services, prepared, true).expect("guest");
        let sent: Rc<RefCell<Vec<HttpRequest>>> = Rc::default();
        let log = Rc::clone(&sent);
        request.hardware_id = vec![0, 1, 2, 3, 4, 5];
        request.transport = Some(Box::new(move |request| {
            let url = request.url.clone();
            log.borrow_mut().push(request);
            if !redirect.is_empty() && log.borrow().len() == 1 {
                let headers = header(&[("Location", redirect), ("Set-Cookie", "r=1; Path=/")]);
                let mut response = HttpResponse::new(302, headers, Vec::new());
                response.request_url = Some(url);
                return Ok(response);
            }
            let headers = header(&[
                ("Content-Type", content_type),
                ("Set-Cookie", "a=b; Path=/"),
            ]);
            Ok(HttpResponse::new(200, headers, body.as_bytes().to_vec()))
        }));
        let result = guest.execute(request);
        let requests = want["Requests"].as_array().cloned().unwrap_or_default();
        let sent = sent.borrow();
        assert_eq!(sent.len(), requests.len(), "{name}: requests");
        for (got, want) in sent.iter().zip(&requests) {
            assert_eq!(
                got.method,
                want["Method"].as_str().unwrap_or_default(),
                "{name}"
            );
            assert_eq!(got.url, want["URL"].as_str().unwrap_or_default(), "{name}");
            assert_eq!(
                rust_headers(&got.headers),
                go_headers(&want["Headers"]),
                "{name}"
            );
            assert_eq!(
                mask(&String::from_utf8_lossy(&got.body)),
                mask(want["Body"].as_str().unwrap_or_default()),
                "{name}"
            );
        }
        match result {
            Ok(mut response) => {
                assert_eq!(want["Error"], "", "{name}");
                assert_eq!(
                    i64::from(response.status),
                    want["Status"].as_i64().unwrap_or_default()
                );
                assert_eq!(
                    rust_headers(&response.headers),
                    go_headers(&want["Headers"]),
                    "{name}"
                );
                let body = response.read_body().expect("body");
                assert_eq!(
                    String::from_utf8_lossy(&body),
                    want["Body"].as_str().unwrap_or_default(),
                    "{name}"
                );
            }
            Err(err) => assert_eq!(
                err.to_string(),
                want["Error"].as_str().unwrap_or_default(),
                "{name}"
            ),
        }
    }
}
