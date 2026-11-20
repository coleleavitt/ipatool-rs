use std::sync::{Arc, Mutex};

use ipatool_store::http::{Header, HttpRequest, HttpResponse};

use super::*;
use crate::commerce::StoreTransport;

const KARING_MAC_CONFIGURATION: &str = r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac","tv","phone","pad"],"metricsPlatformDisplayStyle":"ios","buyParams":"salableAdamId=6472431552&appExtVrsId=876660716"}}"#;

/// Go's `macVersionPage`.
pub(crate) fn mac_version_page(configuration: &str) -> Vec<u8> {
    format!("<script id=\"serialized-server-data\">{configuration}</script>").into_bytes()
}

pub(crate) fn karing() -> App {
    App {
        id: 6_472_431_552,
        bundle_id: "com.nebula.karing".to_owned(),
        name: String::new(),
        version: String::new(),
        price: 0.0,
        platforms: Vec::new(),
    }
}

#[test]
fn selects_karings_mac_offer_and_ignores_unrelated_and_ios_offers() {
    let body = mac_version_page(&format!(
        r#"[{{"purchaseConfiguration":{{"bundleId":"com.nebula.karing","appPlatforms":["phone","pad"],"buyParams":"salableAdamId=6472431552&appExtVrsId=891116578"}}}}, {{"purchaseConfiguration":{{"appPlatforms":["mac"],"buyParams":"salableAdamId=100&appExtVrsId=123"}}}}, {KARING_MAC_CONFIGURATION}]"#
    ));
    assert_eq!(
        macos_external_version_id(&body, &karing()).expect("version"),
        "876660716"
    );
}

#[test]
fn rejects_invalid_catalog_metadata() {
    let conflicting = format!(
        r#"[{KARING_MAC_CONFIGURATION},{{"purchaseConfiguration":{{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123"}}}}]"#
    );
    let cases: &[(&str, &str, &str)] = &[
        ("invalid JSON", "{", "failed to decode Mac product page"),
        ("no offer", "{}", "has no external version id"),
        (
            "iOS only",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["phone","pad"],"buyParams":"salableAdamId=6472431552&appExtVrsId=891116578"}}"#,
            "has no external version id",
        ),
        (
            "wrong bundle",
            r#"{"purchaseConfiguration":{"bundleId":"other","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123"}}"#,
            "has no external version id",
        ),
        (
            "invalid version",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=invalid"}}"#,
            "has no external version id",
        ),
        (
            "bad query",
            r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac"],"buyParams":"salableAdamId=6472431552&appExtVrsId=123&x=%zz"}}"#,
            "has no external version id",
        ),
        (
            "conflicting offers",
            &conflicting,
            "conflicting external version ids",
        ),
    ];
    for (name, configuration, expected) in cases {
        let err = macos_external_version_id(&mac_version_page(configuration), &karing())
            .expect_err(name)
            .to_string();
        assert!(err.contains(expected), "{name}: {err}");
    }
}

#[test]
fn repeated_identical_offers_are_one_version() {
    let body = mac_version_page(&format!(
        "[{KARING_MAC_CONFIGURATION},{KARING_MAC_CONFIGURATION}]"
    ));
    assert_eq!(
        macos_external_version_id(&body, &karing()).expect("version"),
        "876660716"
    );
}

#[test]
fn finds_the_serialized_server_data_script() {
    assert_eq!(
        serialized_server_data(b"<html><script type=\"application/json\" id='serialized-server-data'>\n {} \n</script>")
            .expect("data"),
        b"{}"
    );
    let errors: &[(&[u8], &str)] = &[
        (b"<html>", "serialized server data was not found"),
        (
            b"<div id=\"serialized-server-data\">{}</div>",
            "serialized server data script was not found",
        ),
        (
            b"<script id=\"serialized-server-data\"",
            "serialized server data script is malformed",
        ),
        (
            b"<script id=\"serialized-server-data\">{}",
            "serialized server data script is not closed",
        ),
    ];
    for (body, expected) in errors {
        assert_eq!(
            serialized_server_data(body)
                .expect_err(expected)
                .to_string(),
            *expected
        );
    }
}

/// Live: `IPATOOL_TEST_LIVE=1 cargo test -p ipatool-appstore mac_product_page_live -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "live Apple request"]
async fn mac_product_page_live() {
    let client = crate::Client::new().expect("client");
    let response = client
        .store_raw(
            "request",
            CommerceRequest {
                method: "GET".to_owned(),
                url: "https://apps.apple.com/us/app/id899247664?platform=mac".to_owned(),
                ..CommerceRequest::default()
            },
        )
        .await
        .expect("request");
    eprintln!("status {} headers {:?}", response.status, response.headers);
    eprintln!(
        "{}",
        String::from_utf8_lossy(&response.data[..response.data.len().min(400)])
    );
}

/// Answers every `request` with one scripted page and records the requests.
#[derive(Debug, Default)]
struct PageTransport {
    status: u16,
    body: Vec<u8>,
    fail: bool,
    requests: Mutex<Vec<(String, String, String)>>,
}

impl StoreTransport for PageTransport {
    fn send(
        &self,
        operation: &str,
        request: CommerceRequest,
    ) -> std::result::Result<HttpResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.requests.lock().expect("requests").push((
            operation.to_owned(),
            request.method,
            request.url,
        ));
        if self.fail {
            return Err("connection reset".into());
        }
        Ok(HttpResponse::new(
            self.status,
            Header::new(),
            self.body.clone(),
        ))
    }

    fn http(
        &self,
        _: &str,
        _: HttpRequest,
    ) -> std::result::Result<HttpResponse, Box<dyn std::error::Error + Send + Sync>> {
        Err("unexpected transfer".into())
    }
}

fn german_account() -> Account {
    Account {
        store_front: "143443-2,34".to_owned(),
        ..Account::default()
    }
}

async fn lookup(transport: PageTransport) -> (Result<String>, Vec<(String, String, String)>) {
    let transport = Arc::new(transport);
    let client = Client::new()
        .expect("client")
        .with_store_transport(Arc::clone(&transport) as Arc<dyn StoreTransport>);
    client.cache_storefronts(&[("143443", "DE")]);
    let result = client
        .lookup_latest_macos_external_version_id(&german_account(), &karing())
        .await;
    let requests = transport.requests.lock().expect("requests").clone();
    (result, requests)
}

#[tokio::test]
async fn requests_the_mac_product_page_of_the_account_storefront() {
    let (result, requests) = lookup(PageTransport {
        status: 200,
        body: mac_version_page(KARING_MAC_CONFIGURATION),
        ..PageTransport::default()
    })
    .await;
    assert_eq!(result.expect("version"), "876660716");
    assert_eq!(
        requests,
        [(
            "request".to_owned(),
            "GET".to_owned(),
            "https://apps.apple.com/de/app/id6472431552?platform=mac".to_owned()
        )]
    );
}

#[tokio::test]
async fn propagates_lookup_failures() {
    let (result, _) = lookup(PageTransport {
        status: 500,
        ..PageTransport::default()
    })
    .await;
    assert_eq!(
        result.expect_err("HTTP failure").to_string(),
        "macOS version lookup returned HTTP 500"
    );
    let (result, _) = lookup(PageTransport {
        fail: true,
        ..PageTransport::default()
    })
    .await;
    assert_eq!(
        result.expect_err("network failure").to_string(),
        "macOS version lookup request failed: request failed: connection reset"
    );
    let (result, _) = lookup(PageTransport {
        status: 200,
        body: mac_version_page("{}"),
        ..PageTransport::default()
    })
    .await;
    assert_eq!(
        result.expect_err("missing offer").to_string(),
        "macOS purchase configuration has no external version id for the requested app"
    );
}
