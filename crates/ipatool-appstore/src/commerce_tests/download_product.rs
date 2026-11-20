//! `appstore_download_product_test.go`.

use plist::Value as PValue;

use super::*;
use crate::download_product::new_download_endpoint;
use crate::session::{REDOWNLOAD_PATH, UPDATE_PRODUCT_PATH};
use crate::{Account, App, DownloadProduct, Platform};

pub(crate) const REDOWNLOAD: &str = "https://downloaddispatch.itunes.apple.com/r/redownload";
pub(crate) const UPDATE: &str = "https://downloaddispatch.itunes.apple.com/up/updateProduct";
const VERSION: &str = "123456789";
const APP_ID: i64 = 987_654_321;

/// Answers by endpoint kind, each from its own queue, as Go's separate mocked clients.
pub(crate) struct Routes {
    pub download: Mutex<VecDeque<Reply>>,
    pub bag: Mutex<VecDeque<Reply>>,
    pub platform: Mutex<VecDeque<Reply>>,
}

pub(crate) fn kind(url: &str) -> &'static str {
    if url.contains("init.itunes.apple.com") {
        "bag"
    } else if url.contains("uclient-api") || url.contains("apps.apple.com") {
        "platform"
    } else {
        "download"
    }
}

pub(crate) fn route(world: &World, download: Vec<Reply>, bag: Vec<Reply>, platform: Vec<Reply>) {
    let routes = Routes {
        download: Mutex::new(download.into()),
        bag: Mutex::new(bag.into()),
        platform: Mutex::new(platform.into()),
    };
    world.fake.handle(move |sent| {
        let kind = kind(&sent.request.url);
        let queue = match kind {
            "bag" => &routes.bag,
            "platform" => &routes.platform,
            _ => &routes.download,
        };
        queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected {kind} request {}", sent.request.url))
    });
}

pub(crate) fn sent_kind(world: &World, wanted: &str) -> Vec<Sent> {
    world
        .fake
        .sent()
        .into_iter()
        .filter(|sent| kind(&sent.request.url) == wanted)
        .collect()
}

pub(crate) fn item(url: &str, metadata: &[(&str, PValue)]) -> PValue {
    PValue::Dictionary(dict(&[
        ("URL", s(url)),
        ("metadata", PValue::Dictionary(dict(metadata))),
    ]))
}

pub(crate) fn items(list: Vec<PValue>) -> Reply {
    plist(200, &[("songList", PValue::Array(list))])
}

pub(crate) fn empty() -> Reply {
    plist(200, &[])
}

/// Go's `validBagResult`: SAP configuration and no download endpoints.
pub(crate) fn valid_bag() -> Vec<(&'static str, PValue)> {
    vec![
        (
            "authenticateAccount",
            s("https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate"),
        ),
        (
            "sign-sap-setup",
            s("https://fpinit.example.com/v1/signSapSetup/legacy"),
        ),
        (
            "sign-sap-setup-cert",
            s("https://static.example.com/sap/setupCert.plist"),
        ),
        ("sign-sap-version", s("200")),
    ]
}

pub(crate) fn bag_with(extra: &[(&'static str, &str)]) -> Reply {
    let mut entries = valid_bag();
    entries.extend(extra.iter().map(|(key, value)| (*key, s(value))));
    plist(200, &entries)
}

pub(crate) fn lookup_json(app_id: i64, version: &str) -> Reply {
    json(
        200,
        &format!(
            r#"{{"results":{{"{app_id}":{{"offers":[{{"version":{{"externalId":"{version}"}}}}]}}}}}}"#
        ),
    )
}

fn account() -> Account {
    Account {
        directory_services_id: "test-dsid".to_owned(),
        pod: "42".to_owned(),
        store_front: "143441-1,34".to_owned(),
        ..Account::default()
    }
}

fn app() -> App {
    App {
        id: APP_ID,
        ..App::default()
    }
}

async fn send(
    world: &World,
    account: &Account,
    version: &str,
    platform: Option<Platform>,
) -> crate::Result<(DownloadProduct, Option<Platform>)> {
    world
        .session()
        .download_product(account, &app(), GUID, version, platform)
        .await
}

fn one_item() -> Reply {
    items(vec![item("https://example.com/app.ipa", &[])])
}

#[tokio::test]
async fn uses_volume_store_without_kbsync() {
    let world = World::new();
    route(&world, vec![one_item()], vec![], vec![]);
    let (response, platform) = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(response.data.items[0].url, "https://example.com/app.ipa");
    assert_eq!(platform, Some(Platform::IPhone));
    let sent = &world.fake.sent()[0];
    assert_eq!(sent.operation, "request");
    assert_eq!(
        sent.request.url,
        "https://p42-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/volumeStoreDownloadProduct?guid=001122334455"
    );
    assert!(sent.request.no_redirects);
    assert_eq!(sent.header("iCloud-DSID"), "test-dsid");
    assert_eq!(sent.header("X-Apple-AMD"), "proof");
    assert_eq!(sent.param_str("externalVersionId"), VERSION);
    assert_eq!(sent.param_str("serialNumber"), "0");
    assert_eq!(sent.param_str("creditDisplay"), "");
    assert_eq!(sent.param("salableAdamId"), Some(&Value::Integer(APP_ID)));
    assert!(sent.param("appExtVrsId").is_none());
    // The proof is for the request URL, and the state is saved.
    assert_eq!(world.amd.endpoints.lock().unwrap()[0], sent.request.url);
    assert_eq!(
        world.keychain.get(crate::keys::ANISETTE).await.unwrap(),
        b"x"
    );
}

#[tokio::test]
async fn does_not_fall_back_for_an_error_response() {
    let world = World::new();
    route(
        &world,
        vec![plist(
            200,
            &[
                ("failureType", s("9610")),
                ("customerMessage", s("License not found")),
            ],
        )],
        vec![],
        vec![],
    );
    let (response, _) = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(response.data.failure_type, "9610");
    assert_eq!(world.fake.sent().len(), 1);
}

#[tokio::test]
async fn falls_back_to_redownload() {
    let world = World::new();
    route(
        &world,
        vec![
            empty(),
            items(vec![item("https://example.com/redownload.ipa", &[])]),
        ],
        vec![plist(200, &[("redownloadProduct", s(REDOWNLOAD))])],
        vec![],
    );
    let (response, _) = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(
        response.data.items[0].url,
        "https://example.com/redownload.ipa"
    );
    let bag = &sent_kind(&world, "bag")[0];
    assert_eq!(
        bag.request.url,
        "https://init.itunes.apple.com/bag.xml?ix=6&guid=001122334455"
    );
    let redownload = &sent_kind(&world, "download")[1];
    assert_eq!(redownload.request.url, format!("{REDOWNLOAD}?guid={GUID}"));
    assert_eq!(redownload.param_str("appExtVrsId"), VERSION);
    assert_eq!(redownload.param_str("serialNumber"), "0");
    assert!(redownload.param("externalVersionId").is_none());
}

#[tokio::test]
async fn keeps_the_empty_response_without_a_redownload_endpoint() {
    let world = World::new();
    route(&world, vec![empty()], vec![bag_with(&[])], vec![]);
    let (response, _) = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(
        response,
        DownloadProduct {
            status: 200,
            ..DownloadProduct::default()
        }
    );
}

#[tokio::test]
async fn reports_bag_and_redownload_failures() {
    let world = World::new();
    route(
        &world,
        vec![empty()],
        vec![Reply::Fail("bag request failed")],
        vec![],
    );
    let error = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to get bag for redownload fallback"),
        "{error}"
    );

    let world = World::new();
    route(
        &world,
        vec![empty(), Reply::Fail("redownload failed")],
        vec![bag_with(&[("redownloadProduct", REDOWNLOAD)])],
        vec![],
    );
    let error = send(&world, &account(), VERSION, Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to send redownload request"),
        "{error}"
    );
}

#[tokio::test]
async fn keeps_the_platform_on_the_volume_store_path() {
    for platform in [
        None,
        Some(Platform::IPhone),
        Some(Platform::IPad),
        Some(Platform::MacOs),
    ] {
        let world = World::new();
        route(&world, vec![one_item()], vec![], vec![]);
        let (_, resolved) = send(&world, &account(), VERSION, platform).await.unwrap();
        assert_eq!(resolved, platform);
    }
}

fn pinned_world(storefront: &str) -> (World, Account) {
    let world = World::new();
    let account = Account {
        store_front: storefront.to_owned(),
        ..account()
    };
    (world, account)
}

fn redownload_bag() -> Reply {
    bag_with(&[("redownloadProduct", REDOWNLOAD)])
}

#[tokio::test]
async fn pins_the_latest_ios_version_before_redownload() {
    for (platform, storefront, country) in [
        (None, "143441-1,34", "us"),
        (Some(Platform::IPhone), "143441-1,34", "us"),
        (Some(Platform::IPad), "143441-1,34", "us"),
        (Some(Platform::IPhone), "143444-2,29", "gb"),
    ] {
        let (world, account) = pinned_world(storefront);
        route(
            &world,
            vec![
                empty(),
                items(vec![item(
                    "https://example.com/ios.ipa",
                    &[(
                        "softwareVersionExternalIdentifiers",
                        PValue::Array(vec![s("123"), s(VERSION)]),
                    )],
                )]),
            ],
            vec![redownload_bag()],
            vec![lookup_json(APP_ID, VERSION)],
        );
        let (response, resolved) = send(&world, &account, "", platform).await.unwrap();
        assert_eq!(response.data.items[0].url, "https://example.com/ios.ipa");
        assert_eq!(resolved, Some(platform.unwrap_or(Platform::IPhone)));
        let lookup = &sent_kind(&world, "platform")[0];
        assert_eq!(lookup.operation, "request");
        assert_eq!(lookup.request.method, "GET");
        assert_eq!(
            lookup.request.url,
            format!(
                "https://uclient-api.itunes.apple.com/WebObjects/MZStorePlatform.woa/wa/lookup?caller=MDM&cc={country}&id={APP_ID}&l=en&p=mdm-lockup&platform=enterprisestore&version=2"
            )
        );
        let redownload = &sent_kind(&world, "download")[1];
        assert_eq!(redownload.request.url, format!("{REDOWNLOAD}?guid={GUID}"));
        assert_eq!(redownload.request.method, "POST");
        assert_eq!(
            redownload.param("salableAdamId"),
            Some(&Value::Integer(APP_ID))
        );
        assert_eq!(redownload.param_str("guid"), GUID);
        assert_eq!(redownload.param_str("appExtVrsId"), VERSION);
        assert!(redownload.param("externalVersionId").is_none());
        assert!(redownload.param("pricingParameters").is_none());
    }
}

#[tokio::test]
async fn pins_the_consumer_catalog_version() {
    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty(), empty()],
        vec![redownload_bag()],
        vec![json(200, "{}"), lookup_json(APP_ID, VERSION)],
    );
    send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap();
    let lookups = sent_kind(&world, "platform");
    assert!(lookups[1].request.url.contains("platform=iphone"));
    assert_eq!(
        sent_kind(&world, "download")[1].param_str("appExtVrsId"),
        VERSION
    );
}

#[tokio::test]
async fn keeps_the_pin_after_a_redownload_error() {
    for (reply, text) in [
        (
            Reply::Body(500, Vec::new(), Vec::new()),
            "unexpected response from Apple (HTTP 500): empty or non-plist body",
        ),
        (
            Reply::Body(403, Vec::new(), Vec::new()),
            "unexpected response from Apple (HTTP 403)",
        ),
        (
            Reply::Body(429, Vec::new(), Vec::new()),
            "rate limited by Apple (HTTP 429)",
        ),
        (
            Reply::Body(503, Vec::new(), Vec::new()),
            "unexpected response from Apple (HTTP 503)",
        ),
        (
            Reply::Body(500, Vec::new(), b"<html>maintenance</html>".to_vec()),
            "maintenance",
        ),
        (Reply::Fail("connection reset"), "connection reset"),
    ] {
        let (world, account) = pinned_world("143441-1,34");
        route(
            &world,
            vec![empty(), reply],
            vec![redownload_bag()],
            vec![lookup_json(APP_ID, VERSION)],
        );
        let error = send(&world, &account, "", Some(Platform::IPhone))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(text), "{error}");
        assert_eq!(sent_kind(&world, "download").len(), 2);
        assert_eq!(
            sent_kind(&world, "download")[1].param_str("appExtVrsId"),
            VERSION
        );
    }
}

#[tokio::test]
async fn resolves_no_version_for_pinned_or_other_platform_requests() {
    for (version, platform) in [
        (VERSION, Some(Platform::IPhone)),
        ("818970197", None),
        ("818970197", Some(Platform::AppleTv)),
        (VERSION, Some(Platform::MacOs)),
        ("", Some(Platform::AppleTv)),
        ("", Some(Platform::VisionOs)),
    ] {
        let (world, account) = pinned_world("143441-1,34");
        route(
            &world,
            vec![empty(), one_item()],
            vec![redownload_bag()],
            vec![],
        );
        let (_, resolved) = send(&world, &account, version, platform).await.unwrap();
        assert_eq!(resolved, platform);
        let redownload = &sent_kind(&world, "download")[1];
        if version.is_empty() {
            assert!(redownload.param("appExtVrsId").is_none());
        } else {
            assert_eq!(redownload.param_str("appExtVrsId"), version);
        }
    }
}

#[tokio::test]
async fn keeps_an_actionable_pinned_failure() {
    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty(), plist(200, &[("failureType", s("9610"))])],
        vec![redownload_bag()],
        vec![lookup_json(APP_ID, VERSION)],
    );
    let (response, _) = send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(response.data.failure_type, "9610");
}

#[tokio::test]
async fn stops_when_the_catalog_lookup_fails() {
    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty()],
        vec![redownload_bag()],
        vec![Reply::Fail("catalog unavailable")],
    );
    let error = send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("failed to resolve latest iOS version for redownload"),
        "{text}"
    );
    assert!(text.contains("catalog unavailable"), "{text}");
    assert_eq!(sent_kind(&world, "download").len(), 1);
}

#[tokio::test]
async fn sends_no_unpinned_request_without_a_catalog_version() {
    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty()],
        vec![redownload_bag()],
        vec![
            json(200, "{}"),
            json(200, "{}"),
            json(200, "{}"),
            json(200, "{}"),
        ],
    );
    let error = send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("platform version lookup returned no app"),
        "{error}"
    );
    assert_eq!(sent_kind(&world, "platform").len(), 4);

    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty()],
        vec![redownload_bag()],
        vec![json(
            200,
            &format!(r#"{{"results":{{"{APP_ID}":{{"offers":[{{}}]}}}}}}"#),
        )],
    );
    let error = send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("platform version lookup returned no external version id"),
        "{error}"
    );
}

#[tokio::test]
async fn does_not_retry_a_plist_error_response() {
    let (world, account) = pinned_world("143441-1,34");
    route(
        &world,
        vec![empty(), plist(500, &[])],
        vec![redownload_bag()],
        vec![lookup_json(APP_ID, VERSION)],
    );
    let (response, _) = send(&world, &account, "", Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(response.status, 500);
}

#[tokio::test]
async fn download_keeps_apples_message_when_no_fallback_exists() {
    let world = World::new();
    let message = "“Messenger” No Longer Available";
    route(
        &world,
        vec![plist(200, &[("customerMessage", s(message))])],
        vec![bag_with(&[])],
        vec![],
    );
    let error = world
        .session()
        .download(crate::DownloadInput::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), format!("received error: {message}"));
    assert!(matches!(error, crate::Error::Store { status: 200, .. }));
}

#[test]
fn validates_bag_download_endpoints() {
    for path in [REDOWNLOAD_PATH, UPDATE_PRODUCT_PATH] {
        let endpoint = format!("https://downloaddispatch.itunes.apple.com{path}");
        assert!(new_download_endpoint(&endpoint, path).is_ok());
        for value in [
            String::new(),
            format!("http://downloaddispatch.itunes.apple.com{path}"),
            format!("https://example.com{path}"),
            format!("{endpoint}/other"),
            format!("https://user@downloaddispatch.itunes.apple.com{path}"),
            format!("https://downloaddispatch.itunes.apple.com:443{path}"),
            format!("{endpoint}?guid=other"),
            format!("{endpoint}?"),
            format!("{endpoint}#fragment"),
            format!(
                "https://downloaddispatch.itunes.apple.com/%{:02x}{}",
                path.as_bytes()[1],
                &path[2..]
            ),
        ] {
            assert_eq!(
                new_download_endpoint(&value, path).unwrap_err().to_string(),
                "invalid download endpoint in bag",
                "{value}"
            );
        }
    }
}
