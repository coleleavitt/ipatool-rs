//! `appstore_platform_version_lookup_test.go`, `appstore_ios_version_lookup_test.go` and the
//! `sendDownloadProduct` cases of `appstore_macos_version_lookup_test.go`.

use super::download_product::{REDOWNLOAD, bag_with, empty, item, items, route, sent_kind};
use super::*;
use crate::{Account, App, Platform};

const ID: i64 = 6_472_431_552;

pub(crate) fn new_world() -> World {
    let world = World::new();
    world
        .client
        .cache_storefronts(&[("143469", "RU"), ("143443", "DE")]);
    world
}

fn account() -> Account {
    Account {
        store_front: "143469-1,34".to_owned(),
        ..Account::default()
    }
}

fn app() -> App {
    App {
        id: ID,
        ..App::default()
    }
}

const MISSING: &str = "{}";
const NO_OFFERS: &str = r#"{"results":{"6472431552":{}}}"#;
const VALID: &str =
    r#"{"results":{"6472431552":{"offers":[{"version":{"externalId":"891116578"}}]}}}"#;
const BUY_PARAMS: &str =
    r#"{"results":{"6472431552":{"offers":[{"buyParams":"appExtVrsId=891116578"}]}}}"#;

async fn lookup(world: &World, platform: Platform) -> crate::Result<String> {
    world
        .client
        .lookup_latest_external_version_id(&account(), &app(), platform)
        .await
}

fn catalogs(world: &World) -> Vec<String> {
    sent_kind(world, "platform")
        .iter()
        .map(|sent| {
            let url = &sent.request.url;
            assert!(url.contains("cc=ru"), "{url}");
            assert!(url.contains("id=6472431552"), "{url}");
            let (_, rest) = url.split_once("platform=").unwrap();
            rest.split('&').next().unwrap().to_owned()
        })
        .collect()
}

#[tokio::test]
async fn stops_at_the_first_available_catalog() {
    for (platform, empty_body, success, offer) in [
        (Platform::IPhone, MISSING, 0, VALID),
        (Platform::IPhone, MISSING, 1, VALID),
        (Platform::IPhone, NO_OFFERS, 1, VALID),
        (Platform::IPhone, MISSING, 2, VALID),
        (Platform::IPad, NO_OFFERS, 2, VALID),
        (Platform::IPad, MISSING, 1, VALID),
        (Platform::IPhone, MISSING, 1, BUY_PARAMS),
        (Platform::WatchOs, MISSING, 0, VALID),
        (Platform::WatchOs, MISSING, 1, VALID),
    ] {
        let world = new_world();
        let replies = (0..=success)
            .map(|index| json(200, if index == success { offer } else { empty_body }))
            .collect();
        route(&world, vec![], vec![], replies);
        assert_eq!(lookup(&world, platform).await.unwrap(), "891116578");
        let expected: Vec<_> = ["enterprisestore", "iphone", "ipad"][..=success].to_vec();
        assert_eq!(catalogs(&world), expected);
    }
}

#[tokio::test]
async fn reports_exhausted_catalogs() {
    for (body, message) in [
        (MISSING, "returned no app"),
        (NO_OFFERS, "returned no offers"),
    ] {
        let world = new_world();
        route(
            &world,
            vec![],
            vec![],
            vec![
                json(200, body),
                json(200, body),
                json(200, body),
                json(200, "{}"),
            ],
        );
        let error = lookup(&world, Platform::IPhone)
            .await
            .unwrap_err()
            .to_string();
        for part in [message, "6472431552", "RU", "enterprisestore, iphone, ipad"] {
            assert!(error.contains(part), "{error}");
        }
    }
}

#[tokio::test]
async fn does_not_fall_back_after_a_request_or_offer_error() {
    for (reply, message) in [
        (Reply::Fail("connection reset"), "request failed"),
        (json(200, "{"), "request failed"),
        (
            Reply::Body(503, Vec::new(), b"{}".to_vec()),
            "request failed",
        ),
        (
            json(200, r#"{"results":{"6472431552":{"offers":[{}]}}}"#),
            "no external version id",
        ),
        (
            json(
                200,
                r#"{"results":{"6472431552":{"offers":[{"buyParams":"appExtVrsId=%zz"}]}}}"#,
            ),
            "failed to parse buy params",
        ),
    ] {
        let world = new_world();
        route(&world, vec![], vec![], vec![reply]);
        let error = lookup(&world, Platform::IPhone).await.unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(catalogs(&world).len(), 1);
    }
    let world = new_world();
    route(
        &world,
        vec![],
        vec![],
        vec![
            json(200, MISSING),
            Reply::Body(503, Vec::new(), b"{}".to_vec()),
        ],
    );
    assert!(
        lookup(&world, Platform::IPhone)
            .await
            .unwrap_err()
            .to_string()
            .contains("request failed")
    );
    let world = new_world();
    route(&world, vec![], vec![], vec![json(200, MISSING)]);
    assert!(
        lookup(&world, Platform::AppleTv)
            .await
            .unwrap_err()
            .to_string()
            .contains("returned no app")
    );
    assert_eq!(catalogs(&world), ["atv9"]);
}

const ARCADE: &str = r#"{"data":[{"id":"6472431552","type":"apps","attributes":{"deviceFamilies":["iphone","ipad"],"platformAttributes":{"ios":{"bundleId":"com.example.arcade","externalVersionId":1002}}}}]}"#;

fn arcade_world(platform_reply: Reply) -> World {
    let world = new_world();
    route(
        &world,
        vec![],
        vec![],
        vec![
            json(200, MISSING),
            json(200, MISSING),
            json(200, MISSING),
            platform_reply,
        ],
    );
    world
}

#[tokio::test]
async fn resolves_arcade_versions_from_the_app_store_catalog() {
    for (platform, quoted) in [
        (Platform::IPhone, false),
        (Platform::IPad, false),
        (Platform::IPhone, true),
    ] {
        let body = if quoted {
            ARCADE.replace("1002", r#""1002""#)
        } else {
            ARCADE.to_owned()
        };
        let world = arcade_world(json(200, &body));
        let app = App {
            bundle_id: "com.example.arcade".to_owned(),
            ..app()
        };
        let version = world
            .client
            .lookup_latest_external_version_id(&account(), &app, platform)
            .await
            .unwrap();
        assert_eq!(version, "1002");
        let last = sent_kind(&world, "platform").pop().unwrap();
        assert_eq!(
            last.request.url,
            format!(
                "https://apps.apple.com/api/apps/v1/catalog/ru/apps/6472431552?platform={platform}"
            )
        );
    }
    let world = arcade_world(json(200, ARCADE));
    assert_eq!(lookup(&world, Platform::IPhone).await.unwrap(), "1002");
}

#[tokio::test]
async fn rejects_unusable_app_store_catalog_answers() {
    for (reply, message) in [
        (Reply::Body(503, Vec::new(), b"{}".to_vec()), "HTTP 503"),
        (Reply::Fail("connection reset"), "connection reset"),
        (json(200, "{"), "failed to unmarshal json"),
        (json(200, "{}"), "exactly one app"),
        (json(200, r#"{"data":[]}"#), "exactly one app"),
        (
            json(200, &ARCADE.replace("6472431552", "1")),
            "different app",
        ),
        (
            json(200, &ARCADE.replace(r#""apps""#, r#""app-bundles""#)),
            "different app",
        ),
        (
            json(200, &ARCADE.replace("com.example.arcade", "other")),
            "different bundle",
        ),
        (
            json(200, &ARCADE.replace(r#""ios""#, r#""macos""#)),
            "requested platform",
        ),
        (
            json(200, &ARCADE.replace(r#""ios""#, r#""tvos""#)),
            "requested platform",
        ),
        (
            json(200, &ARCADE.replace(r#""iphone","#, "")),
            "requested platform",
        ),
        (
            json(200, &ARCADE.replace("1002", "null")),
            "no valid external version id",
        ),
        (
            json(200, &ARCADE.replace("1002", "0")),
            "no valid external version id",
        ),
        (
            json(200, &ARCADE.replace("1002", "-1")),
            "no valid external version id",
        ),
        (
            json(200, &ARCADE.replace("1002", "1.5")),
            "no valid external version id",
        ),
        (
            json(200, &ARCADE.replace("1002", r#""invalid""#)),
            "no valid external version id",
        ),
    ] {
        let world = arcade_world(reply);
        let app = App {
            bundle_id: "com.example.arcade".to_owned(),
            ..app()
        };
        let error = world
            .client
            .lookup_latest_external_version_id(&account(), &app, Platform::IPhone)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{message}: {error}");
        assert!(
            error.to_string().contains("iOS catalog fallback failed"),
            "{error}"
        );
    }
}

pub(crate) const KARING: &str = r#"{"purchaseConfiguration":{"bundleId":"com.nebula.karing","appPlatforms":["mac","tv","phone","pad"],"metricsPlatformDisplayStyle":"ios","buyParams":"salableAdamId=6472431552&appExtVrsId=876660716"}}"#;

pub(crate) fn mac_page(configuration: &str) -> Reply {
    Reply::Body(
        200,
        Vec::new(),
        format!(r#"<script id="serialized-server-data">{configuration}</script>"#).into_bytes(),
    )
}

pub(crate) fn karing() -> App {
    App {
        id: ID,
        bundle_id: "com.nebula.karing".to_owned(),
        ..App::default()
    }
}

pub(crate) fn mac_account() -> Account {
    Account {
        store_front: "143443-2,34".to_owned(),
        ..Account::default()
    }
}

#[tokio::test]
async fn pins_the_mac_offer_before_requesting_a_package() {
    for failed in [false, true] {
        let world = new_world();
        let reply = if failed {
            Reply::Body(500, Vec::new(), Vec::new())
        } else {
            items(vec![item("https://example.test/mac.pkg", &[])])
        };
        route(&world, vec![reply], vec![], vec![mac_page(KARING)]);
        let result = world
            .session()
            .download_product(&mac_account(), &karing(), GUID, "", Some(Platform::MacOs))
            .await;
        let page = &sent_kind(&world, "platform")[0];
        assert_eq!(
            page.request.url,
            "https://apps.apple.com/de/app/id6472431552?platform=mac"
        );
        let download = &sent_kind(&world, "download")[0];
        assert_eq!(
            download.request.url,
            "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/volumeStoreDownloadProduct?guid=001122334455"
        );
        assert_eq!(download.param_str("externalVersionId"), "876660716");
        if failed {
            assert!(result.unwrap_err().to_string().contains("HTTP 500"));
        } else {
            let (response, platform) = result.unwrap();
            assert_eq!(response.data.items[0].url, "https://example.test/mac.pkg");
            assert_eq!(platform, Some(Platform::MacOs));
        }
    }
}

#[tokio::test]
async fn keeps_the_mac_version_pinned_through_redownload() {
    for message in ["", "No Longer Available"] {
        let world = new_world();
        let primary = if message.is_empty() {
            empty()
        } else {
            plist(200, &[("customerMessage", s(message))])
        };
        route(
            &world,
            vec![
                primary,
                items(vec![item("https://example.test/mac.pkg", &[])]),
            ],
            vec![bag_with(&[("redownloadProduct", REDOWNLOAD)])],
            vec![mac_page(KARING)],
        );
        let (response, platform) = world
            .session()
            .download_product(&mac_account(), &karing(), GUID, "", Some(Platform::MacOs))
            .await
            .unwrap();
        assert_eq!(response.data.items[0].url, "https://example.test/mac.pkg");
        assert_eq!(platform, Some(Platform::MacOs));
        let downloads = sent_kind(&world, "download");
        assert_eq!(downloads[0].param_str("externalVersionId"), "876660716");
        assert_eq!(
            downloads[1].request.url,
            format!("{REDOWNLOAD}?guid={GUID}")
        );
        assert_eq!(downloads[1].param_str("appExtVrsId"), "876660716");
    }
}

#[tokio::test]
async fn propagates_mac_lookup_failures() {
    for reply in [
        Reply::Body(500, Vec::new(), Vec::new()),
        Reply::Fail("connection reset"),
        mac_page("{}"),
    ] {
        let world = new_world();
        route(&world, vec![], vec![], vec![reply]);
        let error = world
            .session()
            .download_product(&mac_account(), &karing(), GUID, "", Some(Platform::MacOs))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("failed to resolve latest macOS version for download"),
            "{error}"
        );
        assert!(sent_kind(&world, "download").is_empty());
    }
}
