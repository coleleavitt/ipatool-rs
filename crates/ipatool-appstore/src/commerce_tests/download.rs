//! `appstore_download_test.go` (iOS paths), `appstore_download_artwork_test.go` and the
//! catalog-less tvOS cases.

use plist::Value as PValue;

use super::download_product::{
    REDOWNLOAD, UPDATE, bag_with, empty, item, items, lookup_json, route, sent_kind,
};
use super::zip::ipa;
use super::*;
use crate::download::validate_package_platform;
use crate::{Account, App, DownloadInput, Error, Platform};

fn info(platforms: Option<&[&str]>) -> Vec<u8> {
    let mut entries = vec![("CFBundleIdentifier", s("com.example.app"))];
    if let Some(platforms) = platforms {
        entries.push((
            "CFBundleSupportedPlatforms",
            PValue::Array(platforms.iter().map(|p| s(p)).collect()),
        ));
    }
    let mut data = Vec::new();
    plist::to_writer_binary(&mut data, &PValue::Dictionary(dict(&entries))).unwrap();
    data
}

fn package(platforms: Option<&[&str]>) -> Vec<u8> {
    ipa(&[("Payload/Test.app/Info.plist", &info(platforms))])
}

fn serve_package(world: &World, data: Vec<u8>) {
    world.fake.handle_http(move |request| {
        assert_eq!(request.method, "GET");
        Ok(transfer(200, &[], data.clone()))
    });
}

fn input(world: &World, platform: Option<Platform>, version: &str) -> DownloadInput {
    DownloadInput {
        account: Account {
            store_front: "143441".to_owned(),
            email: "test@example.com".to_owned(),
            ..Account::default()
        },
        app: App {
            id: 42,
            ..App::default()
        },
        output_path: world.dir.path().join("app.ipa").display().to_string(),
        platform,
        external_version_id: version.to_owned(),
        ..DownloadInput::default()
    }
}

fn package_item(metadata: &[(&str, PValue)]) -> Reply {
    let mut entries = vec![("bundleShortVersionString", s("1.0"))];
    entries.extend(metadata.iter().cloned());
    items(vec![item("https://example.test/app.ipa", &entries)])
}

#[tokio::test]
async fn fails_without_a_mac_address_or_a_response() {
    let world = World::new();
    let mut session = world.session();
    session.mac_address = || Err(Error::Invalid("no mac".to_owned()));
    let error = session
        .download(DownloadInput::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "failed to get mac address: no mac");

    let world = World::new();
    world.fake.push(Reply::Fail("boom"));
    let error = world
        .session()
        .download(DownloadInput::default())
        .await
        .unwrap_err();
    assert!(
        error.to_string().starts_with(
            "failed to send http request: send Store commerce request: request failed"
        ),
        "{error}"
    );
}

#[tokio::test]
async fn sends_the_pod_request_and_maps_store_failures() {
    let world = World::new();
    world.fake.push(Reply::Fail("boom"));
    let pod = DownloadInput {
        account: Account {
            pod: "42".to_owned(),
            ..Account::default()
        },
        ..DownloadInput::default()
    };
    let _ = world.session().download(pod).await;
    assert_eq!(
        world.fake.sent()[0].request.url,
        "https://p42-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/volumeStoreDownloadProduct?guid=001122334455"
    );
    for (entries, expected) in [
        (
            vec![("failureType", s("2034"))],
            "password token is expired",
        ),
        (
            vec![("failureType", s("2042"))],
            "password token is expired",
        ),
        (
            vec![("failureType", s("1008"))],
            "password token is expired",
        ),
        (
            vec![("failureType", s("5002"))],
            "password token is expired",
        ),
        (vec![("failureType", s("9610"))], "license is required"),
        (
            vec![
                ("failureType", s("test-failure")),
                ("customerMessage", s("Nope")),
            ],
            "received error: Nope",
        ),
        (
            vec![("failureType", s("test-failure"))],
            "received error: test-failure",
        ),
    ] {
        let world = World::new();
        world.fake.push(plist(200, &entries));
        let error = world
            .session()
            .download(DownloadInput::default())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    let world = World::new();
    route(
        &world,
        vec![plist(200, &[("songList", PValue::Array(vec![]))])],
        vec![bag_with(&[])],
        vec![],
    );
    let error = world
        .session()
        .download(DownloadInput::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid response");
}

#[tokio::test]
async fn suggests_logging_in_without_a_provisioned_store_amd_state() {
    for state in [
        None,
        Some(br#"{"Identifier":"AAECAwQFBgcICQoLDA0ODw=="}"#.as_slice()),
    ] {
        let world = World::new();
        if let Some(state) = state {
            world
                .keychain
                .set(crate::keys::ANISETTE, state)
                .await
                .unwrap();
        }
        let session = world
            .client
            .store_session(&world.keychain, &crate::storeauth::EmulatorStoreAmd);
        let mut session = session;
        session.kbsync_generator = None;
        session.mac_address = || Ok("00:11:22:33:44:55".to_owned());
        let app = App {
            id: 1_641_486_558,
            ..App::default()
        };
        let error = session
            .download(DownloadInput {
                app,
                ..DownloadInput::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "failed to send http request: generate Store AMD proof: store AMD state is not provisioned; run 'ipatool auth login' to authenticate"
        );
        assert!(world.fake.sent().is_empty());
    }
}

#[tokio::test]
async fn pins_tvos_and_watchos_versions_before_downloading() {
    let world = World::new();
    route(
        &world,
        vec![Reply::Fail("request error")],
        vec![],
        vec![lookup_json(42, "123456")],
    );
    let _ = world
        .session()
        .download(input(&world, Some(Platform::AppleTv), ""))
        .await;
    let lookup = &sent_kind(&world, "platform")[0];
    assert!(lookup.request.url.contains("platform=atv9"));
    assert!(lookup.request.url.contains("cc=us"));
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "123456"
    );

    let world = World::new();
    route(
        &world,
        vec![plist(200, &[("failureType", s("9610"))])],
        vec![],
        vec![lookup_json(6_764_448_634, "885050618")],
    );
    let mut watch = input(&world, Some(Platform::WatchOs), "");
    watch.app.id = 6_764_448_634;
    let error = world.session().download(watch).await.unwrap_err();
    assert!(error.is_license_required());
    let lookup = &sent_kind(&world, "platform")[0];
    assert!(lookup.request.url.contains("platform=enterprisestore"));
    assert!(lookup.request.url.contains("id=6764448634"));
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "885050618"
    );
}

#[tokio::test]
async fn resolves_visionos_from_the_product_page() {
    let world = World::new();
    let page = r#"<script type="application/json" id="serialized-server-data">[{"data":{"app":{"purchaseConfiguration":{"metricsPlatformDisplayStyle":"vision","appPlatforms":["vision"],"buyParams":"salableAdamId=42&appExtVrsId=987654"}}}}]</script>"#;
    route(
        &world,
        vec![Reply::Fail("request error")],
        vec![],
        vec![Reply::Body(200, Vec::new(), page.as_bytes().to_vec())],
    );
    let _ = world
        .session()
        .download(input(&world, Some(Platform::VisionOs), ""))
        .await;
    let lookup = &sent_kind(&world, "platform")[0];
    assert_eq!(
        lookup.request.url,
        "https://apps.apple.com/us/app/id42?platform=vision"
    );
    assert_eq!(lookup.request.method, "GET");
    assert_eq!(lookup.request.response_format, crate::ResponseFormat::Raw);
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "987654"
    );

    let world = World::new();
    route(&world, vec![Reply::Fail("request error")], vec![], vec![]);
    let _ = world
        .session()
        .download(input(&world, Some(Platform::VisionOs), "123456"))
        .await;
    assert!(sent_kind(&world, "platform").is_empty());
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "123456"
    );

    for platform in [Platform::IPhone, Platform::IPad] {
        let world = World::new();
        route(&world, vec![Reply::Fail("request error")], vec![], vec![]);
        let _ = world
            .session()
            .download(input(&world, Some(platform), ""))
            .await;
        assert!(
            sent_kind(&world, "download")[0]
                .param("externalVersionId")
                .is_none()
        );
    }
}

#[test]
fn reads_visionos_purchase_configurations() {
    use crate::version_lookup::vision_external_version_id;
    let page = |json: &str| {
        format!(r#"<script type="application/json" id="serialized-server-data">{json}</script>"#)
    };
    let config = |style: &str, platforms: &str, params: &str| {
        format!(
            r#"{{"purchaseConfiguration":{{"metricsPlatformDisplayStyle":"{style}","appPlatforms":{platforms},"buyParams":"{params}"}}}}"#
        )
    };
    let good = config(
        "vision",
        r#"["vision"]"#,
        "salableAdamId=42&appExtVrsId=987654",
    );
    assert_eq!(
        vision_external_version_id(page(&format!("[{good}]")).as_bytes(), 42).unwrap(),
        "987654"
    );
    let unversioned = config("vision", r#"["vision"]"#, "salableAdamId=42");
    assert_eq!(
        vision_external_version_id(page(&format!("[{unversioned},{good}]")).as_bytes(), 42)
            .unwrap(),
        "987654"
    );
    assert_eq!(
        vision_external_version_id(page(&format!("[{unversioned}]")).as_bytes(), 42)
            .unwrap_err()
            .to_string(),
        "visionOS purchase configuration has no external version id"
    );
    for other in [
        config("ios", r#"["vision"]"#, "salableAdamId=42&appExtVrsId=1"),
        config("vision", r#"["ios"]"#, "salableAdamId=42&appExtVrsId=1"),
        config("vision", r#"["vision"]"#, "salableAdamId=43&appExtVrsId=1"),
        config("vision", r#"["vision"]"#, ""),
    ] {
        assert_eq!(
            vision_external_version_id(page(&format!("[{other}]")).as_bytes(), 42)
                .unwrap_err()
                .to_string(),
            "visionOS purchase configuration was not found"
        );
    }
}

#[tokio::test]
async fn downloads_patches_and_names_the_package() {
    let world = World::new();
    world.fake.push(items(vec![PValue::Dictionary(dict(&[
        ("URL", s("https://example.test/app.ipa")),
        ("artworkURL", s("https://example.test/art.png")),
        (
            "metadata",
            PValue::Dictionary(dict(&[
                ("bundleShortVersionString", s("xyz")),
                ("itemId", PValue::Integer(42.into())),
            ])),
        ),
        (
            "sinfs",
            PValue::Array(vec![PValue::Dictionary(dict(&[
                ("id", PValue::Integer(0.into())),
                ("sinf", PValue::Data(b"test-sinf-data".to_vec())),
            ]))]),
        ),
    ]))]));
    let data = package(None);
    world.fake.handle_http(move |request| {
        Ok(if request.url.ends_with("art.png") {
            transfer(200, &[], b"artwork".to_vec())
        } else {
            transfer(200, &[], data.clone())
        })
    });
    let mut download = input(&world, None, "");
    download.app.bundle_id = "com.example.app".to_owned();
    download.output_path = world.dir.path().display().to_string();
    let output = world.session().download(download).await.unwrap();
    let expected = world.dir.path().join("com.example.app_42_xyz.ipa");
    assert_eq!(output.destination_path, expected);
    assert_eq!(output.sinfs[0].data, b"test-sinf-data");
    assert!(
        !world
            .dir
            .path()
            .join("com.example.app_42_xyz.ipa.tmp")
            .exists()
    );
    let archive = crate::zipfile::Archive::open(&expected).unwrap();
    let names: Vec<_> = archive
        .files
        .iter()
        .map(|file| file.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "Payload/Test.app/Info.plist",
            "iTunesArtwork",
            "iTunesMetadata.plist"
        ]
    );
    let transfers = world.fake.http_sent.lock().unwrap().clone();
    assert_eq!(transfers[0].headers.get("Range"), "bytes=0-");
    assert_eq!(transfers[0].headers.get("Accept-Encoding"), "identity");
    assert_eq!(transfers[1].url, "https://example.test/art.png");
}

#[tokio::test]
async fn reports_artwork_failures() {
    for (status, body, message) in [
        (
            404,
            b"x".to_vec(),
            "failed to download artwork: unexpected artwork response status: 404",
        ),
        (
            200,
            Vec::new(),
            "failed to download artwork: artwork response is empty",
        ),
    ] {
        let world = World::new();
        world.fake.push(items(vec![PValue::Dictionary(dict(&[
            ("URL", s("https://example.test/app.ipa")),
            ("artworkURL", s("https://example.test/art.png")),
        ]))]));
        let data = package(None);
        world.fake.handle_http(move |request| {
            Ok(if request.url.ends_with("art.png") {
                transfer(status, &[], body.clone())
            } else {
                transfer(200, &[], data.clone())
            })
        });
        let error = world
            .session()
            .download(input(&world, None, ""))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), message);
    }
}

#[tokio::test]
async fn validates_platforms_before_replacing_the_output() {
    let cases: Vec<(Option<Platform>, &str, &str, Option<&str>, bool)> = vec![
        (Some(Platform::IPhone), "", "", None, true),
        (Some(Platform::IPad), "", "", None, true),
        (None, "", "redownload", None, true),
        (None, "", "update", None, true),
        (None, "18372", "update", None, true),
        (Some(Platform::IPhone), "", "", Some("iPhoneOS"), true),
        (Some(Platform::IPad), "", "", Some("iPhoneOS"), true),
        (Some(Platform::IPhone), "", "", Some("AppleTVOS"), false),
        (Some(Platform::IPad), "", "", Some("AppleTVOS"), false),
        (Some(Platform::IPhone), "", "", Some("XROS"), false),
        (
            Some(Platform::IPhone),
            "818970197",
            "",
            Some("AppleTVOS"),
            false,
        ),
        (None, "", "redownload", Some("iPhoneOS"), true),
        (None, "", "redownload", Some("AppleTVOS"), false),
        (None, "", "update", Some("iPhoneOS"), true),
        (None, "818970197", "update", Some("iPhoneOS"), true),
        (None, "818970197", "update", Some("AppleTVOS"), false),
        (None, "818970197", "redownload", Some("AppleTVOS"), true),
        (None, "", "", Some("AppleTVOS"), true),
        (None, "818970197", "", Some("AppleTVOS"), true),
        (
            Some(Platform::AppleTv),
            "818970197",
            "",
            Some("AppleTVOS"),
            true,
        ),
        (
            Some(Platform::AppleTv),
            "818970197",
            "update",
            Some("AppleTVOS"),
            true,
        ),
        (
            Some(Platform::AppleTv),
            "818970197",
            "update",
            Some("iPhoneOS"),
            false,
        ),
        (
            Some(Platform::AppleTv),
            "818970197",
            "update",
            Some("XROS"),
            false,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "",
            Some("WatchOS"),
            true,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "",
            Some("iPhoneOS"),
            false,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "",
            Some("AppleTVOS"),
            false,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "redownload",
            Some("WatchOS"),
            true,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "update",
            Some("WatchOS"),
            true,
        ),
        (
            Some(Platform::WatchOs),
            "818970197",
            "update",
            Some("iPhoneOS"),
            false,
        ),
        (Some(Platform::IPhone), "", "", Some("WatchOS"), false),
    ];
    for (platform, version, fallback, supported, succeeds) in cases {
        let world = World::new();
        let output = world.dir.path().join("app.ipa");
        std::fs::write(&output, b"previous output").unwrap();
        let resolved = if version.is_empty() {
            "890598805"
        } else {
            version
        };
        let mut metadata = vec![];
        if fallback == "update" {
            metadata = vec![
                ("itemId", PValue::Integer(42.into())),
                ("softwareVersionExternalIdentifier", s(resolved)),
                ("softwareVersionBundleId", s("com.example.app")),
            ];
        }
        let response = package_item(&metadata);
        let (downloads, bags, lookups) = match fallback {
            "" => (vec![response], vec![], vec![]),
            _ => {
                let mut downloads = vec![empty()];
                if fallback == "update" {
                    downloads.push(plist(
                        200,
                        &[("customerMessage", s("“Messenger” No Longer Available"))],
                    ));
                }
                downloads.push(response);
                let lookups = if version.is_empty() {
                    vec![lookup_json(42, resolved)]
                } else {
                    vec![]
                };
                (
                    downloads,
                    vec![bag_with(&[
                        ("redownloadProduct", REDOWNLOAD),
                        ("updateProduct", UPDATE),
                    ])],
                    lookups,
                )
            }
        };
        route(&world, downloads, bags, lookups);
        let supported_list = supported.map(|value| vec![value]);
        serve_package(&world, package(supported_list.as_deref()));
        let result = world
            .session()
            .download(input(&world, platform, version))
            .await;
        let label = format!("{platform:?} {version} {fallback} {supported:?}");
        if fallback != "" {
            assert_eq!(
                sent_kind(&world, "download")
                    .last()
                    .unwrap()
                    .param_str("appExtVrsId"),
                resolved,
                "{label}"
            );
        }
        if succeeds {
            let out = result.unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_eq!(out.destination_path, output);
            assert_ne!(std::fs::read(&output).unwrap(), b"previous output");
        } else {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("failed to validate package platform"),
                "{label}: {error}"
            );
            assert_eq!(std::fs::read(&output).unwrap(), b"previous output");
        }
        assert!(!world.dir.path().join("app.ipa.tmp").exists(), "{label}");
    }
}

fn package_file(
    dir: &tempfile::TempDir,
    entries: &[(&str, Option<&[&str]>)],
) -> std::path::PathBuf {
    let infos: Vec<(String, Vec<u8>)> = entries
        .iter()
        .map(|(name, platforms)| ((*name).to_owned(), info(*platforms)))
        .collect();
    let refs: Vec<(&str, &[u8])> = infos
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    let path = dir.path().join("platform.ipa");
    std::fs::write(&path, ipa(&refs)).unwrap();
    path
}

#[test]
fn validates_package_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let top = "Payload/Test.app/Info.plist";
    let check = |entries: &[(&str, Option<&[&str]>)], platform: Platform| {
        validate_package_platform(&package_file(&dir, entries), Some(platform))
    };
    assert!(check(&[(top, Some(&["AppleTVOS"]))], Platform::AppleTv).is_ok());
    for (platform, supported, ok) in [
        (Platform::IPhone, Some(&["iPhoneOS"][..]), true),
        (Platform::IPad, Some(&["iPhoneOS"][..]), true),
        (Platform::IPhone, Some(&["AppleTVOS"][..]), false),
        (Platform::IPad, Some(&["XROS"][..]), false),
        (Platform::IPhone, None, true),
        (Platform::IPad, None, true),
        (Platform::IPhone, Some(&[][..]), false),
        (Platform::IPad, Some(&[][..]), false),
    ] {
        let result = check(&[(top, supported)], platform);
        assert_eq!(result.is_ok(), ok, "{platform:?} {supported:?}");
        if !ok {
            assert!(result.unwrap_err().to_string().contains("iPhoneOS"));
        }
    }
    for platform in [Platform::AppleTv, Platform::WatchOs, Platform::VisionOs] {
        assert!(check(&[(top, None)], platform).is_err());
    }
    assert!(
        check(
            &[("Payload/Test.app/PlugIns/Legacy.app/Info.plist", None)],
            Platform::IPhone
        )
        .is_err()
    );
    assert!(
        check(
            &[
                (top, Some(&["AppleTVOS"])),
                (
                    "Payload/Test.app/PlugIns/iOS.app/Info.plist",
                    Some(&["iPhoneOS"])
                )
            ],
            Platform::IPhone
        )
        .is_err()
    );
    assert_eq!(
        check(&[(top, Some(&["iPhoneOS"]))], Platform::AppleTv)
            .unwrap_err()
            .to_string(),
        "downloaded package does not declare AppleTVOS support"
    );
    assert!(check(&[(top, Some(&["XROS"]))], Platform::VisionOs).is_ok());
    assert!(check(&[(top, Some(&["iPhoneOS"]))], Platform::VisionOs).is_err());
    assert!(
        check(
            &[
                (top, Some(&["iPhoneOS"])),
                (
                    "Payload/Test.app/Watch/W.app/Info.plist",
                    Some(&["WatchOS"])
                )
            ],
            Platform::WatchOs
        )
        .is_err()
    );
    for body in [
        b"not a plist".as_slice(),
        br#"<plist version="1.0"><dict><key>CFBundleSupportedPlatforms</key><integer>1</integer></dict></plist>"#,
    ] {
        let path = dir.path().join("malformed.ipa");
        std::fs::write(&path, ipa(&[(top, body)])).unwrap();
        let error = validate_package_platform(&path, Some(Platform::IPhone)).unwrap_err();
        assert!(error.to_string().contains("failed to decode info plist"), "{error}");
    }
}

#[tokio::test]
async fn downloads_tvos_without_a_catalog_offer_and_validates_the_package() {
    for (lookup, redownload, supported, ok) in [
        (r#"{}"#, false, "AppleTVOS", true),
        (r#"{"results":{"42":{}}}"#, false, "AppleTVOS", true),
        (r#"{}"#, true, "AppleTVOS", true),
        (r#"{}"#, false, "iPhoneOS", false),
        (r#"{}"#, true, "iPhoneOS", false),
    ] {
        let world = World::new();
        let downloads = if redownload {
            vec![empty(), package_item(&[])]
        } else {
            vec![package_item(&[])]
        };
        let bags = if redownload {
            vec![bag_with(&[("redownloadProduct", REDOWNLOAD)])]
        } else {
            vec![]
        };
        route(&world, downloads, bags, vec![json(200, lookup)]);
        serve_package(&world, package(Some(&[supported])));
        let result = world
            .session()
            .download(input(&world, Some(Platform::AppleTv), ""))
            .await;
        let sent = sent_kind(&world, "download");
        assert!(sent[0].param("externalVersionId").is_none());
        if redownload {
            assert!(sent[1].param("appExtVrsId").is_none());
        }
        assert_eq!(result.is_ok(), ok, "{lookup} {redownload} {supported}");
        if !ok {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("does not declare AppleTVOS support")
            );
        }
    }
}

#[tokio::test]
async fn keeps_tvos_catalog_failures() {
    for (reply, message) in [
        (Reply::Fail("connection reset"), "connection reset"),
        (json(200, "{"), "failed to unmarshal json"),
        (
            Reply::Body(503, Vec::new(), b"{}".to_vec()),
            "platform version lookup request failed",
        ),
        (
            json(200, r#"{"results":{"42":{"offers":[{}]}}}"#),
            "no external version id",
        ),
    ] {
        let world = World::new();
        route(&world, vec![], vec![], vec![reply]);
        let error = world
            .session()
            .download(input(&world, Some(Platform::AppleTv), ""))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert!(
            error
                .to_string()
                .starts_with("failed to resolve platform version"),
            "{error}"
        );
        assert!(sent_kind(&world, "download").is_empty());
    }
    let world = World::new();
    route(
        &world,
        vec![plist(200, &[("failureType", s("9610"))])],
        vec![],
        vec![json(200, "{}")],
    );
    assert!(
        world
            .session()
            .download(input(&world, Some(Platform::AppleTv), ""))
            .await
            .unwrap_err()
            .is_license_required()
    );
    let world = World::new();
    route(
        &world,
        vec![plist(200, &[("failureType", s("9610"))])],
        vec![],
        vec![],
    );
    let error = world
        .session()
        .download(input(&world, Some(Platform::AppleTv), "818970197"))
        .await
        .unwrap_err();
    assert!(error.is_license_required());
    assert!(sent_kind(&world, "platform").is_empty());
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "818970197"
    );
}

#[tokio::test]
async fn resolves_the_destination_path() {
    use crate::download::resolve_destination_path;
    let dir = tempfile::tempdir().unwrap();
    let app = App {
        id: 42,
        bundle_id: "com.example.mac".to_owned(),
        ..App::default()
    };
    let path = dir.path().display().to_string();
    assert_eq!(
        resolve_destination_path(&app, "1.2.3", &path, Some(Platform::MacOs)).unwrap(),
        dir.path().join("com.example.mac_42_1.2.3.pkg")
    );
    assert_eq!(
        resolve_destination_path(&app, "1.2.3", &path, None).unwrap(),
        dir.path().join("com.example.mac_42_1.2.3.ipa")
    );
    let custom = dir.path().join("custom-output").display().to_string();
    assert_eq!(
        resolve_destination_path(&App::default(), "1.2.3", &custom, Some(Platform::MacOs)).unwrap(),
        dir.path().join("custom-output")
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        resolve_destination_path(&App::default(), "", "", None).unwrap(),
        std::path::PathBuf::from(format!("{}/.ipa", cwd.display()))
    );
}
