//! `appstore_update_product_test.go` and `appstore_redownload_test.go` (delisted apps).

use plist::Value as PValue;

use super::download_product::{
    REDOWNLOAD, UPDATE, empty, item, items, lookup_json, route, sent_kind,
};
use super::*;
use crate::{Account, App, DownloadProduct, Platform};

const VERSION: &str = "123456789";
const APP_ID: i64 = 987_654_321;

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
        bundle_id: "com.example.app".to_owned(),
        ..App::default()
    }
}

fn uint(value: u64) -> PValue {
    PValue::Integer(value.into())
}

fn expected() -> Reply {
    items(vec![item(
        "https://example.com/ios.ipa",
        &[
            ("itemId", uint(APP_ID as u64)),
            ("softwareVersionExternalIdentifier", uint(123_456_789)),
            ("softwareVersionBundleId", s("com.example.app")),
        ],
    )])
}

fn bag(update: &str) -> Reply {
    let mut entries = vec![("redownloadProduct", s(REDOWNLOAD))];
    if !update.is_empty() {
        entries.push(("updateProduct", s(update)));
    }
    plist(200, &entries)
}

fn empty_500() -> Reply {
    Reply::Body(500, Vec::new(), Vec::new())
}

fn unavailable() -> Reply {
    plist(
        200,
        &[("customerMessage", s("“Messenger” No Longer Available"))],
    )
}

async fn send(
    world: &World,
    version: &str,
    platform: Option<Platform>,
) -> crate::Result<(DownloadProduct, Option<Platform>)> {
    world
        .session()
        .download_product(&account(), &app(), GUID, version, platform)
        .await
}

fn downloads(world: &World) -> Vec<Sent> {
    sent_kind(world, "download")
}

#[tokio::test]
async fn retries_once_with_the_same_pinned_request() {
    for (version, platform) in [
        ("", None),
        ("", Some(Platform::IPhone)),
        ("", Some(Platform::IPad)),
        (VERSION, Some(Platform::IPhone)),
        (VERSION, Some(Platform::MacOs)),
        (VERSION, Some(Platform::AppleTv)),
        (VERSION, Some(Platform::WatchOs)),
        (VERSION, None),
    ] {
        let world = World::new();
        let lookups = if version.is_empty() {
            vec![lookup_json(APP_ID, VERSION)]
        } else {
            vec![]
        };
        route(
            &world,
            vec![empty(), empty_500(), expected()],
            vec![bag(UPDATE)],
            lookups,
        );
        let (response, resolved) = send(&world, version, platform).await.unwrap();
        assert_eq!(response.data.items[0].url, "https://example.com/ios.ipa");
        assert_eq!(resolved, Some(platform.unwrap_or(Platform::IPhone)));
        let sent = downloads(&world);
        let (redownload, update) = (&sent[1], &sent[2]);
        assert_eq!(redownload.request.url, format!("{REDOWNLOAD}?guid={GUID}"));
        assert_eq!(update.request.url, format!("{UPDATE}?guid={GUID}"));
        assert_eq!(update.request.method, "POST");
        assert_eq!(update.request.headers, redownload.request.headers);
        assert_eq!(update.header("Content-Type"), "application/x-apple-plist");
        assert_eq!(update.request.payload, redownload.request.payload);
        assert_eq!(update.param_str("appExtVrsId"), VERSION);
    }
}

#[tokio::test]
async fn tries_update_once_after_an_availability_message() {
    for (platform, primary_unavailable, update_unavailable) in [
        (Platform::IPhone, false, false),
        (Platform::IPhone, true, false),
        (Platform::IPhone, false, true),
        (Platform::AppleTv, false, false),
        (Platform::AppleTv, true, false),
        (Platform::AppleTv, false, true),
    ] {
        let world = World::new();
        let primary = if primary_unavailable {
            unavailable()
        } else {
            empty()
        };
        let update = if update_unavailable {
            unavailable()
        } else {
            expected()
        };
        route(
            &world,
            vec![primary, unavailable(), update],
            vec![bag(UPDATE)],
            vec![],
        );
        let result = send(&world, VERSION, Some(platform)).await;
        let sent = downloads(&world);
        assert_eq!(sent[1].param_str("appExtVrsId"), VERSION);
        assert_eq!(sent[2].request.url, format!("{UPDATE}?guid={GUID}"));
        assert_eq!(sent[2].request.payload, sent[1].request.payload);
        if update_unavailable {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("“Messenger” No Longer Available"),
                "{error}"
            );
        } else {
            assert_eq!(result.unwrap().1, Some(platform));
        }
    }
}

#[tokio::test]
async fn keeps_availability_responses_when_update_is_ineligible() {
    for (platform, update, failure, message, status) in [
        (
            Platform::IPhone,
            "",
            "",
            "“Messenger” No Longer Available",
            200,
        ),
        (
            Platform::VisionOs,
            UPDATE,
            "",
            "“Messenger” No Longer Available",
            200,
        ),
        (
            Platform::IPhone,
            UPDATE,
            "9610",
            "“Messenger” No Longer Available",
            200,
        ),
        (Platform::IPhone, UPDATE, "", "Sign in required", 200),
        (
            Platform::IPhone,
            UPDATE,
            "",
            "“Messenger” No Longer Available",
            403,
        ),
    ] {
        let world = World::new();
        let mut entries = vec![("customerMessage", s(message))];
        if !failure.is_empty() {
            entries.push(("failureType", s(failure)));
        }
        route(
            &world,
            vec![empty(), plist(status, &entries)],
            vec![bag(update)],
            vec![],
        );
        let (response, _) = send(&world, VERSION, Some(platform)).await.unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.data.customer_message, message);
        assert_eq!(downloads(&world).len(), 2);
    }
}

#[tokio::test]
async fn keeps_other_redownload_errors_without_retrying() {
    for (reply, text) in [
        (Reply::Body(403, Vec::new(), Vec::new()), "HTTP 403"),
        (Reply::Body(429, Vec::new(), Vec::new()), "HTTP 429"),
        (Reply::Body(503, Vec::new(), Vec::new()), "HTTP 503"),
        (
            Reply::Body(500, Vec::new(), b"<p>maintenance</p>".to_vec()),
            "maintenance",
        ),
        (Reply::Fail("connection reset"), "connection reset"),
    ] {
        let world = World::new();
        route(&world, vec![empty(), reply], vec![bag(UPDATE)], vec![]);
        let error = send(&world, VERSION, Some(Platform::IPhone))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(text), "{error}");
        assert_eq!(downloads(&world).len(), 2);
    }
}

#[tokio::test]
async fn does_not_retry_a_decoded_redownload_response() {
    for reply in [
        items(vec![item("https://example.com/app.ipa", &[])]),
        plist(200, &[("failureType", s("9610"))]),
        empty(),
        plist(500, &[]),
    ] {
        let world = World::new();
        route(&world, vec![empty(), reply], vec![bag(UPDATE)], vec![]);
        send(&world, VERSION, Some(Platform::IPhone)).await.unwrap();
        assert_eq!(downloads(&world).len(), 2);
    }
}

#[tokio::test]
async fn keeps_the_redownload_error_without_an_update_endpoint() {
    let world = World::new();
    route(&world, vec![empty(), empty_500()], vec![bag("")], vec![]);
    let error = send(&world, VERSION, Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to send redownload request"),
        "{error}"
    );
}

#[tokio::test]
async fn rejects_an_invalid_update_endpoint() {
    let world = World::new();
    route(
        &world,
        vec![empty(), empty_500()],
        vec![bag("https://example.com/up/updateProduct")],
        vec![],
    );
    let error = send(&world, VERSION, Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid download endpoint in bag");
}

#[tokio::test]
async fn does_not_update_unpinned_tvos_or_visionos() {
    for (version, platform) in [("", Platform::AppleTv), (VERSION, Platform::VisionOs)] {
        let world = World::new();
        route(
            &world,
            vec![empty(), empty_500()],
            vec![bag(UPDATE)],
            vec![],
        );
        let error = send(&world, version, Some(platform)).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 500"), "{error}");
        assert_eq!(downloads(&world).len(), 2);
    }
    let world = World::new();
    route(
        &world,
        vec![
            empty(),
            plist(200, &[("customerMessage", s("No Longer Available"))]),
        ],
        vec![bag(UPDATE)],
        vec![],
    );
    let (response, platform) = send(&world, "", Some(Platform::AppleTv)).await.unwrap();
    assert_eq!(response.data.customer_message, "No Longer Available");
    assert_eq!(platform, Some(Platform::AppleTv));
}

#[tokio::test]
async fn reports_an_update_error_without_retrying_again() {
    let world = World::new();
    route(
        &world,
        vec![empty(), empty_500(), empty_500()],
        vec![bag(UPDATE)],
        vec![],
    );
    let error = send(&world, VERSION, Some(Platform::IPhone))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("failed to send update request"),
        "{error}"
    );
    assert_eq!(downloads(&world).len(), 3);
}

async fn update_only(
    world: &World,
    app: &App,
    reply: Reply,
) -> crate::Result<(DownloadProduct, Option<Platform>)> {
    route(
        world,
        vec![empty(), empty_500(), reply],
        vec![bag(UPDATE)],
        vec![],
    );
    world
        .session()
        .download_product(&account(), app, GUID, VERSION, Some(Platform::IPhone))
        .await
}

#[tokio::test]
async fn validates_update_responses() {
    for failure in ["9610", "2034", "2042"] {
        let world = World::new();
        let reply = plist(
            200,
            &[
                ("failureType", s(failure)),
                ("customerMessage", s("App Store error")),
            ],
        );
        let (response, _) = update_only(&world, &app(), reply).await.unwrap();
        assert_eq!(response.data.failure_type, failure);
    }
    let world = World::new();
    let error = update_only(&world, &app(), plist(500, &[]))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "received unexpected update status code: 500"
    );

    let world = World::new();
    let with_message = plist(
        200,
        &[
            ("customerMessage", s("Unable to process your request")),
            (
                "songList",
                PValue::Array(vec![item("https://example.com/ios.ipa", &[])]),
            ),
        ],
    );
    let error = update_only(&world, &app(), with_message).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "received update error: Unable to process your request"
    );

    for count in [0, 2] {
        let world = World::new();
        let list = vec![item("https://example.com/ios.ipa", &[]); count];
        let error = update_only(&world, &app(), items(list)).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "update response must contain exactly one item"
        );
    }
}

#[tokio::test]
async fn rejects_mismatched_update_identity() {
    let base = || {
        vec![
            ("itemId", uint(APP_ID as u64)),
            ("softwareVersionExternalIdentifier", uint(123_456_789)),
            ("softwareVersionBundleId", s("com.example.app")),
        ]
    };
    for (field, value) in [
        ("itemId", Some(uint(42))),
        ("itemId", None),
        ("softwareVersionExternalIdentifier", Some(uint(987_654_321))),
        ("softwareVersionExternalIdentifier", None),
        ("softwareVersionBundleId", Some(s("com.example.other"))),
        ("softwareVersionBundleId", None),
        ("softwareVersionBundleId", Some(s(""))),
    ] {
        let mut metadata: Vec<_> = base()
            .into_iter()
            .filter(|(key, _)| *key != field)
            .collect();
        if let Some(value) = value {
            metadata.push((field, value));
        }
        let world = World::new();
        let reply = items(vec![item("https://example.com/ios.ipa", &metadata)]);
        let error = update_only(&world, &app(), reply).await.unwrap_err();
        assert!(
            error.to_string().contains("update response does not match"),
            "{field}: {error}"
        );
    }
    let world = World::new();
    let id_only = App {
        bundle_id: String::new(),
        ..app()
    };
    let reply = items(vec![item(
        "https://example.com/ios.ipa",
        &[
            ("itemId", s(&APP_ID.to_string())),
            ("softwareVersionExternalIdentifier", s(VERSION)),
            ("softwareVersionBundleId", s("com.example.app")),
        ],
    )]);
    update_only(&world, &id_only, reply).await.unwrap();
}

// Delisted apps (`appstore_redownload_test.go`).

fn delisted_account() -> Account {
    Account {
        store_front: "143467-2,29".to_owned(),
        directory_services_id: "1234".to_owned(),
        pod: "42".to_owned(),
        ..Account::default()
    }
}

fn arcade() -> App {
    App {
        id: 42,
        bundle_id: "com.example.arcade".to_owned(),
        ..App::default()
    }
}

fn delisted_response() -> PValue {
    item(
        "https://example.test/app.ipa",
        &[
            ("itemId", uint(42)),
            ("softwareVersionBundleId", s("com.example.arcade")),
            ("softwareVersionExternalIdentifier", uint(1001)),
            ("bundleShortVersionString", s("1.0")),
        ],
    )
}

fn delisted_world(no_offers: bool, final_catalog: Reply, downloads: Vec<Reply>) -> World {
    let world = World::new();
    world.client.cache_storefronts(&[("143467", "IN")]);
    let missing = || {
        if no_offers {
            json(200, r#"{"results":{"42":{}}}"#)
        } else {
            json(200, "{}")
        }
    };
    let mut replies = vec![empty()];
    replies.extend(downloads);
    route(
        &world,
        replies,
        vec![bag(UPDATE)],
        vec![missing(), missing(), missing(), final_catalog],
    );
    world
}

fn not_found() -> Reply {
    Reply::Body(404, Vec::new(), b"{}".to_vec())
}

#[tokio::test]
async fn tries_one_unpinned_redownload_after_a_catalog_404() {
    for (platform, no_offers, bundle_known) in [
        (None, false, true),
        (Some(Platform::IPhone), false, true),
        (Some(Platform::IPad), true, true),
        (Some(Platform::IPhone), false, false),
    ] {
        let world = delisted_world(
            no_offers,
            not_found(),
            vec![items(vec![delisted_response()])],
        );
        let app = App {
            bundle_id: if bundle_known {
                "com.example.arcade".to_owned()
            } else {
                String::new()
            },
            ..arcade()
        };
        let (response, resolved) = world
            .session()
            .download_product(&delisted_account(), &app, GUID, "", platform)
            .await
            .unwrap();
        assert_eq!(response.data.items[0].url, "https://example.test/app.ipa");
        assert_eq!(resolved, Some(platform.unwrap_or(Platform::IPhone)));
        let catalog = &sent_kind(&world, "platform")[3];
        assert_eq!(
            catalog.request.url,
            format!(
                "https://apps.apple.com/api/apps/v1/catalog/in/apps/42?platform={}",
                platform.unwrap_or(Platform::IPhone)
            )
        );
        let sent = downloads(&world);
        let redownload = &sent[1];
        assert_eq!(redownload.request.url, format!("{REDOWNLOAD}?guid={GUID}"));
        assert_eq!(
            redownload.request.timeout,
            Some(std::time::Duration::from_secs(30))
        );
        let Some(Payload::Xml(payload)) = &redownload.request.payload else {
            panic!("xml payload expected");
        };
        assert_eq!(
            payload.keys().collect::<Vec<_>>(),
            ["creditDisplay", "guid", "salableAdamId", "serialNumber"]
        );
        let mut primary_headers = sent[0].request.headers.clone();
        primary_headers.remove("X-Apple-AMD");
        let mut headers = redownload.request.headers.clone();
        headers.remove("X-Apple-AMD");
        assert_eq!(headers, primary_headers);
    }
}

const ARCADE_CATALOG: &str = r#"{"data":[{"id":"42","type":"apps","attributes":{"deviceFamilies":["iphone","ipad"],"platformAttributes":{"ios":{"bundleId":"com.example.arcade","externalVersionId":1002}}}}]}"#;

#[tokio::test]
async fn keeps_public_catalog_errors_without_redownloading() {
    for (reply, message) in [
        (Reply::Body(503, Vec::new(), b"{}".to_vec()), "HTTP 503"),
        (Reply::Fail("connection reset"), "connection reset"),
        (json(200, "{"), "failed to unmarshal json"),
        (
            Reply::Body(404, Vec::new(), b"{".to_vec()),
            "failed to unmarshal json",
        ),
        (json(200, "{}"), "exactly one app"),
        (json(200, r#"{"data":[]}"#), "exactly one app"),
        (
            json(200, &ARCADE_CATALOG.replace(r#""42""#, r#""43""#)),
            "different app",
        ),
        (
            json(200, &ARCADE_CATALOG.replace("com.example.arcade", "other")),
            "different bundle",
        ),
        (
            json(200, &ARCADE_CATALOG.replace("1002", "null")),
            "no valid external version id",
        ),
        (
            json(200, &ARCADE_CATALOG.replace(r#""ios""#, r#""tvos""#)),
            "requested platform",
        ),
    ] {
        let world = delisted_world(false, reply, vec![]);
        let error = world
            .session()
            .download_product(
                &delisted_account(),
                &arcade(),
                GUID,
                "",
                Some(Platform::IPhone),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{message}: {error}");
        assert!(error.has_message(crate::version_lookup::PLATFORM_APP_NOT_FOUND));
        assert!(!error.has_message(crate::version_lookup::IOS_CATALOG_APP_NOT_FOUND));
        assert_eq!(downloads(&world).len(), 1);
    }
}

#[tokio::test]
async fn rejects_unusable_delisted_responses() {
    let with = |metadata: Vec<(&'static str, PValue)>, url: &str| items(vec![item(url, &metadata)]);
    let base = || {
        vec![
            ("itemId", uint(42)),
            ("softwareVersionBundleId", s("com.example.arcade")),
            ("softwareVersionExternalIdentifier", uint(1001)),
        ]
    };
    let replace = |key: &'static str, value: Option<PValue>| {
        let mut metadata: Vec<_> = base().into_iter().filter(|(k, _)| *k != key).collect();
        if let Some(value) = value {
            metadata.push((key, value));
        }
        metadata
    };
    for (reply, message) in [
        (
            with(
                replace("itemId", Some(uint(43))),
                "https://example.test/app.ipa",
            ),
            "requested app",
        ),
        (
            with(
                replace("softwareVersionBundleId", Some(s("com.example.other"))),
                "https://example.test/app.ipa",
            ),
            "requested bundle identifier",
        ),
        (
            with(
                replace("softwareVersionBundleId", None),
                "https://example.test/app.ipa",
            ),
            "requested bundle identifier",
        ),
        (with(base(), ""), "no package URL"),
        (empty(), "exactly one item"),
        (
            items(vec![delisted_response(), delisted_response()]),
            "exactly one item",
        ),
        (
            Reply::Plist(
                500,
                dict(&[("songList", PValue::Array(vec![delisted_response()]))]),
            ),
            "status code: 500",
        ),
        (
            plist(200, &[("customerMessage", s("App No Longer Available"))]),
            "App No Longer Available",
        ),
    ] {
        let world = delisted_world(false, not_found(), vec![reply]);
        let error = world
            .session()
            .download_product(
                &delisted_account(),
                &arcade(),
                GUID,
                "",
                Some(Platform::IPhone),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{message}: {error}");
        assert_eq!(downloads(&world).len(), 2);
    }
    for reply in [empty_500(), Reply::Fail("connection reset")] {
        let world = delisted_world(false, not_found(), vec![reply]);
        assert!(
            world
                .session()
                .download_product(
                    &delisted_account(),
                    &arcade(),
                    GUID,
                    "",
                    Some(Platform::IPhone)
                )
                .await
                .is_err()
        );
        assert_eq!(downloads(&world).len(), 2);
    }
}

#[tokio::test]
async fn delisted_downloads_keep_license_and_token_recovery() {
    for (failure, license) in [("9610", true), ("2034", false), ("2042", false)] {
        let world = delisted_world(
            false,
            not_found(),
            vec![plist(200, &[("failureType", s(failure))])],
        );
        let error = world
            .session()
            .download(crate::DownloadInput {
                account: delisted_account(),
                app: arcade(),
                platform: Some(Platform::IPhone),
                output_path: world.dir.path().join("app.ipa").display().to_string(),
                ..crate::DownloadInput::default()
            })
            .await
            .unwrap_err();
        if license {
            assert!(error.is_license_required(), "{error}");
        } else {
            assert!(error.is_password_token_expired(), "{error}");
        }
    }
}
