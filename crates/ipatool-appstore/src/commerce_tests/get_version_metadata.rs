//! `appstore_get_version_metadata_test.go` and the `GetVersionMetadata` cases of
//! `appstore_macos_version_metadata_test.go` and `appstore_macos_preflight_metadata_test.go`.

use std::sync::atomic::Ordering;

use plist::Value as PValue;
use time::OffsetDateTime;
use time::macros::datetime;

use super::download_product::{REDOWNLOAD, bag_with, empty, item, items, route, sent_kind};
use super::partial_zip::{serve, test_ipa};
use super::*;
use crate::macos_preflight_metadata::tests::{preflight_distribution, preflight_xar};
use crate::macos_version_metadata::tests::{INFO, MODIFIED, job, metadata_xar};
use crate::{Account, App, Error, Platform, VersionMetadata};

const IPA_URL: &str = "https://example.test/app.ipa";

async fn metadata(
    world: &World,
    account: &Account,
    app: &App,
    version: &str,
    platform: Option<Platform>,
) -> crate::Result<VersionMetadata> {
    world
        .session()
        .get_version_metadata(account, app, version, platform)
        .await
}

async fn default_metadata(world: &World) -> crate::Result<VersionMetadata> {
    metadata(world, &Account::default(), &App::default(), "", None).await
}

#[tokio::test]
async fn fails_without_a_mac_address() {
    let world = World::new();
    let mut session = world.session();
    session.mac_address = || Err(Error::Invalid("mac error".to_owned()));
    let error = session
        .get_version_metadata(&Account::default(), &App::default(), "", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failed to get mac address"));
}

#[tokio::test]
async fn sends_the_pod_request_and_reports_request_errors() {
    let world = World::new();
    world.fake.push(Reply::Fail("request error"));
    let account = Account {
        pod: "42".to_owned(),
        ..Account::default()
    };
    let error = metadata(&world, &account, &App::default(), "", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failed to send http request"));
    assert_eq!(
        world.fake.sent()[0].request.url,
        "https://p42-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/volumeStoreDownloadProduct?guid=001122334455"
    );
}

#[tokio::test]
async fn maps_store_failures() {
    for (entries, expected) in [
        (
            vec![("failureType", s("2034"))],
            "password token is expired",
        ),
        (
            vec![("failureType", s("2042"))],
            "password token is expired",
        ),
        (vec![("failureType", s("9610"))], "license is required"),
        (
            vec![
                ("failureType", s("SOME_ERROR")),
                ("customerMessage", s("Customer error message")),
            ],
            "received error: Customer error message",
        ),
        (
            vec![("failureType", s("SOME_ERROR"))],
            "received error: SOME_ERROR",
        ),
    ] {
        let world = World::new();
        world.fake.push(plist(200, &entries));
        let error = default_metadata(&world).await.unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    let world = World::new();
    world.fake.push(plist(200, &[("failureType", s("2034"))]));
    assert!(matches!(
        default_metadata(&world).await,
        Err(Error::PasswordTokenExpired)
    ));
    let world = World::new();
    world.fake.push(plist(200, &[("failureType", s("9610"))]));
    assert!(matches!(
        default_metadata(&world).await,
        Err(Error::LicenseRequired)
    ));
}

#[tokio::test]
async fn reports_an_empty_response() {
    let world = World::new();
    route(&world, vec![empty()], vec![bag_with(&[])], vec![]);
    let error = default_metadata(&world).await.unwrap_err();
    assert!(error.to_string().contains("invalid response"), "{error}");
}

fn ipa_item(metadata: &[(&str, PValue)]) -> Reply {
    items(vec![item(IPA_URL, metadata)])
}

#[tokio::test]
async fn fails_to_parse_the_release_date() {
    let world = World::new();
    serve(
        &world,
        test_ipa(
            "1.0.0",
            Some(s("invalid-date")),
            datetime!(2024-03-19 12:00 UTC),
        ),
    );
    world.fake.push(ipa_item(&[
        ("bundleShortVersionString", s("1.0.0")),
        ("releaseDate", s("invalid-date")),
    ]));
    let error = default_metadata(&world).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to read version metadata: failed to parse release date: invalid release date: invalid-date"
    );
}

#[tokio::test]
async fn does_not_fall_back_to_api_metadata_when_the_ipa_cannot_be_read() {
    let world = World::new();
    world
        .fake
        .handle_http(|_| Ok(transfer(200, &[], Vec::new())));
    world.fake.push(ipa_item(&[
        ("bundleShortVersionString", s("1.0.0")),
        ("releaseDate", s("2024-03-20T12:00:00Z")),
    ]));
    let error = default_metadata(&world).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to read version metadata: failed to read remote file size: expected partial content response, got status 200"
    );
}

#[tokio::test]
async fn preserves_the_requested_historical_version_through_the_redownload_fallback() {
    const VERSION: &str = "818970197";
    let release = datetime!(2016-12-15 12:00 UTC);
    let world = World::new();
    serve(
        &world,
        test_ipa("1.0", Some(s("2016-12-15T12:00:00Z")), release),
    );
    route(
        &world,
        vec![empty(), ipa_item(&[])],
        vec![bag_with(&[("redownloadProduct", REDOWNLOAD)])],
        vec![],
    );
    let app = App {
        id: 547_702_041,
        ..App::default()
    };
    let out = metadata(&world, &Account::default(), &app, VERSION, None)
        .await
        .unwrap();
    assert_eq!(out.display_version, "1.0");
    assert_eq!(out.release_date, release);
    let downloads = sent_kind(&world, "download");
    assert_eq!(downloads[0].param_str("externalVersionId"), VERSION);
    assert_eq!(downloads[1].param_str("appExtVrsId"), VERSION);
    assert!(downloads[1].param("externalVersionId").is_none());
}

#[tokio::test]
async fn reads_the_info_plist_with_range_requests() {
    let release = datetime!(2024-04-02 12:00 UTC);
    for platform in [None, Some(Platform::WatchOs)] {
        let world = World::new();
        let ipa = test_ipa(
            "2.0.0",
            Some(s(" \n2024-04-02T12:00:00Z\t")),
            datetime!(2020-01-01 0:00 UTC),
        );
        let size = ipa.len() as u64;
        let served = serve(&world, ipa);
        world.fake.push(ipa_item(&[
            ("releaseDate", s("2020-01-01T00:00:00Z")),
            ("bundleShortVersionString", s("1.0.0")),
        ]));
        let account = Account {
            directory_services_id: "test-dsid".to_owned(),
            ..Account::default()
        };
        let app = App {
            id: 1_234_567_890,
            ..App::default()
        };
        let out = metadata(&world, &account, &app, "test-version", platform)
            .await
            .unwrap();
        assert_eq!(out.display_version, "2.0.0");
        assert_eq!(out.release_date, release);
        assert_eq!(served.whole.load(Ordering::SeqCst), 0);
        assert!(served.bytes.load(Ordering::SeqCst) < size / 2);
    }
}

#[tokio::test]
async fn falls_back_to_the_entry_timestamp_and_rejects_bad_plists() {
    let modified = datetime!(2021-06-07 08:09:10 UTC);
    let world = World::new();
    serve(&world, test_ipa("3.1", None, modified));
    world.fake.push(ipa_item(&[]));
    let out = default_metadata(&world).await.unwrap();
    assert_eq!(out.release_date, modified);

    // Go's zip writer stores no time for `Create` entries: MS-DOS zero is 1979-11-30.
    let world = World::new();
    let info = {
        let mut data = Vec::new();
        plist::to_writer_xml(
            &mut data,
            &PValue::Dictionary(dict(&[("bundleShortVersionString", s("1"))])),
        )
        .unwrap();
        data
    };
    serve(
        &world,
        super::zip::ipa(&[("Payload/Test.app/Info.plist", &info)]),
    );
    world.fake.push(ipa_item(&[]));
    let out = default_metadata(&world).await.unwrap();
    assert_eq!(out.display_version, "1");
    assert_eq!(out.release_date, datetime!(1979-11-30 0:00 UTC));

    let world = World::new();
    serve(&world, test_ipa(" ", None, modified));
    world.fake.push(ipa_item(&[]));
    let error = default_metadata(&world).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to read version metadata: CFBundleShortVersionString is empty"
    );

    let world = World::new();
    serve(
        &world,
        super::zip::ipa(&[("Payload/Test.app/Other.plist", b"x")]),
    );
    world.fake.push(ipa_item(&[]));
    let error = default_metadata(&world).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to read version metadata: could not find Info.plist"
    );
}

#[test]
fn parses_release_date_values_like_go() {
    use crate::partial_zip::parse_release_date_string;
    for (text, expected) in [
        ("2024-04-02T12:00:00Z", datetime!(2024-04-02 12:00 UTC)),
        (
            "2024-04-02T14:00:00.5+02:00",
            datetime!(2024-04-02 12:00:00.5 UTC),
        ),
        ("Tuesday, April 2, 2024", datetime!(2024-04-02 0:00 UTC)),
        ("monday, january 15, 2024", datetime!(2024-01-15 0:00 UTC)),
        ("2024-04-02", datetime!(2024-04-02 0:00 UTC)),
    ] {
        assert_eq!(parse_release_date_string(text).unwrap(), expected, "{text}");
    }
    for text in [
        "2024-4-2",
        "2024-02-30",
        "Tuesday, April 2, 24",
        "April 2, 2024",
    ] {
        assert!(parse_release_date_string(text).is_err(), "{text}");
    }
}

#[tokio::test]
async fn reads_mac_compatible_ipa_metadata_with_the_ipa_reader() {
    let date = datetime!(2025-07-24 08:38:42 UTC);
    let world = World::new();
    serve(
        &world,
        test_ipa(
            "2.0",
            Some(PValue::Date(std::time::SystemTime::from(date).into())),
            date,
        ),
    );
    world.fake.push(items(vec![PValue::Dictionary(dict(&[
        ("URL", s(IPA_URL)),
        (
            "sinfs",
            PValue::Array(vec![PValue::Dictionary(dict(&[(
                "sinf",
                PValue::Data(b"sinf".to_vec()),
            )]))]),
        ),
        (
            "metadata",
            PValue::Dictionary(dict(&[("software-platform", s("ios"))])),
        ),
    ]))]));
    let app = App {
        id: 42,
        ..App::default()
    };
    let out = metadata(
        &world,
        &Account::default(),
        &app,
        "123",
        Some(Platform::MacOs),
    )
    .await
    .unwrap();
    assert_eq!(out.display_version, "2.0");
    assert_eq!(out.release_date, date);
}

#[tokio::test]
async fn decrypts_the_selected_mac_version_and_ignores_stale_api_metadata() {
    let world = World::new();
    world
        .fake
        .handle_http(|_| Ok(transfer(200, &[], b"encrypted".to_vec())));
    world.fake.push(items(vec![PValue::Dictionary(dict(&[
        ("URL", s("https://example.test/encrypted.pkg")),
        (
            "sinfs",
            PValue::Array(vec![PValue::Dictionary(dict(&[(
                "dpInfo",
                PValue::Data(b"dpInfo".to_vec()),
            )]))]),
        ),
        (
            "metadata",
            PValue::Dictionary(dict(&[
                ("software-platform", s("macos")),
                ("bundleShortVersionString", s("9.9")),
                ("releaseDate", s("2023-12-26T08:00:00Z")),
            ])),
        ),
    ]))]));
    let calls = Arc::default();
    let mut session = world.session();
    session.mac_package = job(metadata_xar(INFO, MODIFIED), Arc::clone(&calls));
    let app = App {
        id: 42,
        bundle_id: "com.example.mac".to_owned(),
        ..App::default()
    };
    let out = session
        .get_version_metadata(
            &Account::default(),
            &app,
            "876660716",
            Some(Platform::MacOs),
        )
        .await
        .unwrap();
    assert_eq!(out.display_version, "1.2.1");
    assert_eq!(out.release_date, datetime!(2025-07-24 08:38:42 UTC));
    assert_eq!(*calls.lock().unwrap(), 1);
    assert_eq!(
        world.fake.sent()[0].param_str("externalVersionId"),
        "876660716"
    );
}

#[tokio::test]
async fn reads_the_selected_legacy_version_without_downloading_the_encrypted_app() {
    let cases = [
        (
            None,
            "macos",
            "3235485",
            "4.0.5",
            "4.0.5.0.1.1292967101",
            1_292_967_101,
        ),
        (
            Some(Platform::MacOs),
            "macos",
            "3235485",
            "4.0.5",
            "4.0.5.0.1.1292967101",
            1_292_967_101,
        ),
        (
            None,
            "macos",
            "12013122",
            "4.2",
            "4.2.0.0.1.1341568473",
            1_341_568_473,
        ),
        (
            Some(Platform::MacOs),
            "macos",
            "12013122",
            "4.2",
            "4.2.0.0.1.1341568473",
            1_341_568_473,
        ),
        (
            None,
            "",
            "12013122",
            "4.2",
            "4.2.0.0.1.1341568473",
            1_341_568_473,
        ),
    ];
    for (platform, software_platform, version, display_version, package_version, timestamp) in cases
    {
        let world = World::new();
        let data = preflight_xar(&preflight_distribution(display_version, package_version));
        world.fake.handle_http(move |request| {
            if request.url.ends_with("/preflight.pkg") {
                Ok(transfer(200, &[], data.clone()))
            } else {
                Ok(transfer(500, &[], Vec::new()))
            }
        });
        world.fake.push(items(vec![PValue::Dictionary(dict(&[
            ("URL", s("https://example.test/encrypted.pkg")),
            (
                "preflightPackageURL",
                s("https://example.test/preflight.pkg"),
            ),
            (
                "metadata",
                PValue::Dictionary(dict(&[
                    ("software-platform", s(software_platform)),
                    ("product-type", s("mac-os-app")),
                    ("softwareVersionBundleId", s("com.example.mac")),
                    ("bundleShortVersionString", s("4.3")),
                    ("releaseDate", s("2011-01-03T08:00:00Z")),
                ])),
            ),
        ]))]));
        let app = App {
            id: 409_201_541,
            ..App::default()
        };
        let out = metadata(&world, &Account::default(), &app, version, platform)
            .await
            .unwrap();
        assert_eq!(out.display_version, display_version);
        assert_eq!(
            out.release_date,
            OffsetDateTime::from_unix_timestamp(timestamp).unwrap()
        );
        assert_eq!(world.fake.sent()[0].param_str("externalVersionId"), version);
        assert!(
            world
                .fake
                .http_sent
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.url.ends_with("/preflight.pkg"))
        );
    }
}
