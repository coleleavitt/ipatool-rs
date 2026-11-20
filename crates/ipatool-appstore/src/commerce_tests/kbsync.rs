//! `appstore_kbsync_test.go` and `appstore_kbsync_cache_test.go`.

use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use plist::Value as PValue;

use super::download_product::{
    REDOWNLOAD, UPDATE, bag_with, empty, items, lookup_json, route, sent_kind,
};
use super::*;
use crate::session::KbsyncEntry;
use crate::{Account, App, Error, Platform, keys};

const ENT: &str =
    "https://downloaddispatch.itunes.apple.com/WebObjects/DownloadDispatch.woa/wa/ent/download";
const VERSION: &str = "123456789";
const DSID: u64 = 123_456_789_012;

fn account() -> Account {
    Account {
        directory_services_id: DSID.to_string(),
        password_token: "test-token".to_owned(),
        store_front: "143441-1,29".to_owned(),
        ..Account::default()
    }
}

fn app() -> App {
    App {
        id: 42,
        bundle_id: "com.example.app".to_owned(),
        ..App::default()
    }
}

fn success_item(item_id: PValue, version: &str, bundle: Option<&str>, url: &str) -> PValue {
    let mut metadata = vec![
        ("itemId", item_id),
        ("softwareVersionExternalIdentifier", s(version)),
    ];
    if let Some(bundle) = bundle {
        metadata.push(("softwareVersionBundleId", s(bundle)));
    }
    super::download_product::item(url, &metadata)
}

fn success() -> Reply {
    items(vec![success_item(
        PValue::Integer(42.into()),
        VERSION,
        Some("com.example.app"),
        "https://example.com/app.ipa",
    )])
}

fn unavailable() -> Reply {
    plist(200, &[("customerMessage", s("App No Longer Available"))])
}

fn ent_bag() -> Reply {
    bag_with(&[("volumeStoreDownloadProduct", ENT)])
}

struct Kb {
    world: World,
    generated: Arc<AtomicUsize>,
}

impl Kb {
    fn new() -> Self {
        Self {
            world: World::new(),
            generated: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn session(&self) -> StoreSession<'_> {
        let mut session = self.world.session();
        let generated = Arc::clone(&self.generated);
        session.kbsync_generator = Some(Box::new(move |hardware_id, dsid| {
            assert_eq!(hardware_id, [0, 0x11, 0x22, 0x33, 0x44, 0x55]);
            assert_eq!(dsid, DSID);
            generated.fetch_add(1, Ordering::SeqCst);
            Ok(b"kbsync blob".to_vec())
        }));
        session
    }

    fn generated(&self) -> usize {
        self.generated.load(Ordering::SeqCst)
    }

    async fn send(
        &self,
        version: &str,
        platform: Option<Platform>,
    ) -> crate::Result<(crate::DownloadProduct, Option<Platform>)> {
        self.session()
            .download_product(&account(), &app(), GUID, version, platform)
            .await
    }

    fn downloads(&self) -> Vec<Sent> {
        sent_kind(&self.world, "download")
    }
}

fn fresh() -> String {
    STANDARD.encode(b"kbsync blob")
}

fn cached() -> String {
    STANDARD.encode(b"cached blob")
}

#[tokio::test]
async fn sends_kbsync_to_the_bags_ent_endpoint() {
    let kb = Kb::new();
    let nested = PValue::Dictionary(dict(&[("volumeStoreDownloadProduct", s(ENT))]));
    route(
        &kb.world,
        vec![success()],
        vec![plist(200, &[("urlBag", nested)])],
        vec![],
    );
    let (response, _) = kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
    assert_eq!(response.data.items[0].url, "https://example.com/app.ipa");
    assert_eq!(kb.generated(), 1);
    let ent = &kb.downloads()[0];
    assert_eq!(ent.request.url, format!("{ENT}?guid={GUID}"));
    assert_eq!(ent.param_str("kbsync"), fresh());
}

#[test]
fn normal_sessions_generate_kbsync() {
    let world = World::new();
    assert!(
        world
            .client
            .store_session(&world.keychain, &world.amd)
            .kbsync_generator
            .is_some()
    );
}

#[tokio::test]
async fn uses_volume_store_without_an_ent_endpoint_or_bag() {
    for bag in [bag_with(&[]), Reply::Fail("bag unavailable")] {
        let kb = Kb::new();
        route(&kb.world, vec![success()], vec![bag], vec![]);
        kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
        assert!(
            kb.downloads()[0]
                .request
                .url
                .contains("volumeStoreDownloadProduct")
        );
        assert_eq!(kb.generated(), 0);
    }
}

#[tokio::test]
async fn falls_back_when_generation_fails() {
    let kb = Kb::new();
    route(&kb.world, vec![success()], vec![ent_bag()], vec![]);
    let mut session = kb.world.session();
    session.kbsync_generator = Some(Box::new(|_, _| {
        Err(Error::Invalid("guest unavailable".to_owned()))
    }));
    session
        .download_product(&account(), &app(), GUID, VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    let downloads = kb.downloads();
    assert_eq!(downloads.len(), 1);
    assert!(
        downloads[0]
            .request
            .url
            .contains("volumeStoreDownloadProduct")
    );
}

fn unusable(outcome: &str) -> Reply {
    let wrong = |item_id: i64, version: &str, url: &str| {
        items(vec![success_item(
            PValue::Integer(item_id.into()),
            version,
            Some("com.example.app"),
            url,
        )])
    };
    match outcome {
        "unavailable" => unavailable(),
        "HTML 403" => Reply::Body(403, Vec::new(), b"<html>Forbidden</html>".to_vec()),
        "HTTP 500" => Reply::Body(500, Vec::new(), Vec::new()),
        "timeout" => Reply::Fail("context deadline exceeded"),
        "wrong app" => wrong(99, VERSION, "https://example.com/wrong.ipa"),
        "wrong version" => wrong(42, "other", "https://example.com/wrong.ipa"),
        "missing URL" => wrong(42, VERSION, ""),
        "plist 500" => plist(500, &[]),
        "structured failure" => plist(200, &[("failureType", s("2034"))]),
        _ => empty(),
    }
}

#[tokio::test]
async fn falls_back_from_unusable_ent_responses() {
    for outcome in [
        "empty",
        "unavailable",
        "HTML 403",
        "HTTP 500",
        "timeout",
        "wrong app",
        "wrong version",
        "missing URL",
    ] {
        let kb = Kb::new();
        route(
            &kb.world,
            vec![unusable(outcome), success()],
            vec![ent_bag()],
            vec![],
        );
        let (response, _) = kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
        assert_eq!(
            response.data.items[0].url, "https://example.com/app.ipa",
            "{outcome}"
        );
        let downloads = kb.downloads();
        assert_eq!(downloads[0].request.url, format!("{ENT}?guid={GUID}"));
        assert!(
            downloads[1]
                .request
                .url
                .contains("volumeStoreDownloadProduct")
        );
        assert_eq!(downloads[1].param_str("externalVersionId"), VERSION);
        assert_eq!(kb.generated(), 1);
    }
}

#[tokio::test]
async fn keeps_the_legacy_chain_and_the_bag_after_an_ent_miss() {
    let kb = Kb::new();
    route(
        &kb.world,
        vec![unavailable(), empty(), unavailable(), success()],
        vec![bag_with(&[
            ("volumeStoreDownloadProduct", ENT),
            ("redownloadProduct", REDOWNLOAD),
            ("updateProduct", UPDATE),
        ])],
        vec![],
    );
    let (response, _) = kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
    assert_eq!(response.data.items[0].url, "https://example.com/app.ipa");
    let urls: Vec<_> = kb
        .downloads()
        .iter()
        .map(|sent| sent.request.url.clone())
        .collect();
    assert!(urls[1].contains("volumeStoreDownloadProduct"));
    assert_eq!(urls[2], format!("{REDOWNLOAD}?guid={GUID}"));
    assert_eq!(urls[3], format!("{UPDATE}?guid={GUID}"));
    assert_eq!(sent_kind(&kb.world, "bag").len(), 1);
    assert_eq!(kb.generated(), 1);
}

#[tokio::test]
async fn tries_ent_first_for_a_pinned_version() {
    for platform in [
        Platform::IPhone,
        Platform::IPad,
        Platform::MacOs,
        Platform::AppleTv,
        Platform::VisionOs,
        Platform::WatchOs,
    ] {
        let kb = Kb::new();
        route(&kb.world, vec![success()], vec![ent_bag()], vec![]);
        let (_, resolved) = kb.send(VERSION, Some(platform)).await.unwrap();
        assert_eq!(resolved, Some(platform));
        let ent = &kb.downloads()[0];
        assert_eq!(ent.request.url, format!("{ENT}?guid={GUID}"));
        assert_eq!(ent.request.method, "POST");
        assert!(ent.request.no_redirects);
        assert_eq!(
            ent.request.timeout,
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(ent.header("X-Token"), "test-token");
        assert_eq!(ent.header("X-Dsid"), DSID.to_string());
        assert_eq!(ent.header("iCloud-DSID"), DSID.to_string());
        assert_eq!(ent.header("X-Apple-Store-Front"), "143441-1,29");
        assert_eq!(
            ent.header("Content-Type"),
            "application/x-www-form-urlencoded; charset=utf-8"
        );
        assert!(ent.header("User-Agent").starts_with("Configurator/2.21 "));
        let Some(Payload::Xml(payload)) = &ent.request.payload else {
            panic!("xml payload expected");
        };
        let serial = STANDARD.encode([0x54, 0xc8, 0xb0, 0xa9, 0x88, 0x22, 0x33, 0x44, 0x55]);
        let expected: std::collections::BTreeMap<String, Value> = [
            ("creditDisplay", ""),
            ("guid", GUID),
            ("salableAdamId", "42"),
            ("externalVersionId", VERSION),
            ("serialNumber", serial.as_str()),
            ("kbsync", fresh().as_str()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
        .collect();
        assert_eq!(payload, &expected);
    }
}

#[tokio::test]
async fn pins_the_ios_offer_before_ent() {
    let kb = Kb::new();
    route(
        &kb.world,
        vec![success()],
        vec![ent_bag()],
        vec![lookup_json(42, VERSION)],
    );
    let (_, platform) = kb.send("", None).await.unwrap();
    assert_eq!(platform, Some(Platform::IPhone));
    assert_eq!(kb.downloads()[0].param_str("externalVersionId"), VERSION);
}

#[tokio::test]
async fn rejects_invalid_ent_destinations_before_generating() {
    for endpoint in [
        "http://downloaddispatch.itunes.apple.com/WebObjects/DownloadDispatch.woa/wa/ent/download".to_owned(),
        "https://example.com/WebObjects/DownloadDispatch.woa/wa/ent/download".to_owned(),
        "https://downloaddispatch.itunes.apple.com.example.com/WebObjects/DownloadDispatch.woa/wa/ent/download".to_owned(),
        "https://user@downloaddispatch.itunes.apple.com/WebObjects/DownloadDispatch.woa/wa/ent/download".to_owned(),
        format!("{ENT}?guid=other"),
        format!("{ENT}#fragment"),
        UPDATE.to_owned(),
        "https://downloaddispatch.itunes.apple.com/ent/download".to_owned(),
        "https://downloaddispatch.itunes.apple.com/WebObjects/DownloadDispatch.woa/wa/ent/%64ownload".to_owned(),
    ] {
        let kb = Kb::new();
        route(&kb.world, vec![success()], vec![bag_with(&[("volumeStoreDownloadProduct", &endpoint)])], vec![]);
        kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
        assert_eq!(kb.generated(), 0, "{endpoint}");
        let downloads = kb.downloads();
        assert_eq!(downloads.len(), 1);
        assert!(downloads[0].request.url.contains("volumeStoreDownloadProduct"));
    }
}

#[tokio::test]
async fn validates_the_ent_response_identity() {
    for (item_id, version, bundle) in [
        (PValue::Integer(99.into()), VERSION, Some("com.example.app")),
        (
            PValue::Integer(42.into()),
            "987654321",
            Some("com.example.app"),
        ),
        (PValue::Integer(42.into()), VERSION, Some("com.other.app")),
        (PValue::Integer(42.into()), VERSION, None),
    ] {
        let kb = Kb::new();
        let reply = items(vec![success_item(
            item_id,
            version,
            bundle,
            "https://example.com/app.ipa",
        )]);
        route(&kb.world, vec![reply, empty()], vec![ent_bag()], vec![]);
        // The invalid ent answer falls back to volumeStore, which returns no items.
        let (response, _) = kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
        assert!(response.data.items.is_empty());
        assert_eq!(kb.downloads().len(), 2);
    }
}

#[tokio::test]
async fn keeps_authentication_and_license_failures_from_the_fallback() {
    for failure in ["2034", "9610"] {
        let kb = Kb::new();
        let reply = || plist(200, &[("failureType", s(failure))]);
        route(&kb.world, vec![reply(), reply()], vec![ent_bag()], vec![]);
        let (response, _) = kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
        assert_eq!(response.data.failure_type, failure);
        assert!(
            kb.downloads()[1]
                .request
                .url
                .contains("volumeStoreDownloadProduct")
        );
    }
}

#[tokio::test]
async fn keeps_unpinned_legacy_downloads_when_the_offer_lookup_fails() {
    let kb = Kb::new();
    route(
        &kb.world,
        vec![success()],
        vec![ent_bag()],
        vec![Reply::Fail("catalog unavailable")],
    );
    kb.send("", Some(Platform::IPhone)).await.unwrap();
    let downloads = kb.downloads();
    assert_eq!(downloads.len(), 1);
    assert!(
        downloads[0]
            .request
            .url
            .contains("volumeStoreDownloadProduct")
    );
    assert!(downloads[0].param("externalVersionId").is_none());
    assert_eq!(kb.generated(), 0);
}

#[tokio::test]
async fn reaches_unpinned_redownload_after_catalog_misses() {
    let kb = Kb::new();
    let miss = || json(200, "{}");
    let not_found = || Reply::Body(404, Vec::new(), b"{}".to_vec());
    route(
        &kb.world,
        vec![unavailable(), success()],
        vec![bag_with(&[
            ("volumeStoreDownloadProduct", ENT),
            ("redownloadProduct", REDOWNLOAD),
            ("updateProduct", UPDATE),
        ])],
        vec![
            miss(),
            miss(),
            miss(),
            not_found(),
            miss(),
            miss(),
            miss(),
            not_found(),
        ],
    );
    let (_, platform) = kb.send("", None).await.unwrap();
    assert_eq!(platform, Some(Platform::IPhone));
    let downloads = kb.downloads();
    assert!(
        downloads[0]
            .request
            .url
            .contains("volumeStoreDownloadProduct")
    );
    assert_eq!(
        downloads[1].request.url,
        format!("{REDOWNLOAD}?guid={GUID}")
    );
    assert!(downloads[1].param("appExtVrsId").is_none());
    assert_eq!(kb.generated(), 0);
}

async fn store_cache(world: &World, data: &str) {
    let entry = KbsyncEntry {
        dsid: DSID,
        guid: GUID.to_owned(),
        data: data.to_owned(),
    };
    world
        .keychain
        .set(keys::KBSYNC, &serde_json::to_vec(&entry).unwrap())
        .await
        .unwrap();
}

async fn cached_entry(world: &World) -> Option<KbsyncEntry> {
    let data = world.keychain.get(keys::KBSYNC).await.ok()?;
    serde_json::from_slice(&data).ok()
}

#[tokio::test]
async fn uses_a_working_cached_blob() {
    let kb = Kb::new();
    store_cache(&kb.world, &cached()).await;
    route(&kb.world, vec![success()], vec![ent_bag()], vec![]);
    kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
    assert_eq!(kb.downloads()[0].param_str("kbsync"), cached());
    assert_eq!(kb.generated(), 0);
    assert_eq!(cached_entry(&kb.world).await.unwrap().data, cached());
}

#[tokio::test]
async fn saves_a_fresh_blob_after_success_for_the_next_session() {
    let kb = Kb::new();
    route(
        &kb.world,
        vec![success(), success()],
        vec![ent_bag(), ent_bag()],
        vec![],
    );
    kb.send(VERSION, Some(Platform::IPhone)).await.unwrap();
    assert_eq!(cached_entry(&kb.world).await.unwrap().data, fresh());
    let mut restarted = kb.world.session();
    restarted.kbsync_generator = Some(Box::new(|_, _| {
        panic!("the new session must reuse the blob")
    }));
    restarted
        .download_product(&account(), &app(), GUID, VERSION, Some(Platform::IPhone))
        .await
        .unwrap();
    assert_eq!(kb.downloads()[1].param_str("kbsync"), fresh());
    assert_eq!(kb.generated(), 1);
}

#[tokio::test]
async fn retries_a_rejected_cached_blob_once() {
    for outcome in [
        "HTTP 500",
        "HTML 403",
        "timeout",
        "empty",
        "unavailable",
        "structured failure",
        "missing URL",
        "wrong version",
    ] {
        let kb = Kb::new();
        store_cache(&kb.world, &cached()).await;
        route(
            &kb.world,
            vec![unusable(outcome), success(), success()],
            vec![ent_bag(), ent_bag()],
            vec![],
        );
        let session = kb.session();
        session
            .download_product(&account(), &app(), GUID, VERSION, Some(Platform::IPhone))
            .await
            .unwrap();
        let blobs: Vec<_> = kb
            .downloads()
            .iter()
            .map(|sent| sent.param_str("kbsync").to_owned())
            .collect();
        assert_eq!(blobs, [cached(), fresh()], "{outcome}");
        assert_eq!(kb.generated(), 1);
        assert_eq!(cached_entry(&kb.world).await.unwrap().data, fresh());
        assert_eq!(session.cached_kbsync(DSID, GUID).await, fresh());
        // The replacement serves the next download without generating.
        session
            .download_product(&account(), &app(), GUID, VERSION, Some(Platform::IPhone))
            .await
            .unwrap();
        assert_eq!(kb.generated(), 1);
    }
}

#[tokio::test]
async fn falls_back_without_caching_a_failed_fresh_blob() {
    for (outcome, has_cache) in [
        ("HTTP 500", true),
        ("HTML 403", true),
        ("timeout", true),
        ("empty", true),
        ("unavailable", true),
        ("structured failure", true),
        ("missing URL", true),
        ("wrong version", true),
        ("HTTP 500", false),
        ("structured failure", false),
    ] {
        let kb = Kb::new();
        let mut replies = Vec::new();
        if has_cache {
            store_cache(&kb.world, &cached()).await;
            replies.push(empty());
        }
        replies.extend([unusable(outcome), success()]);
        route(&kb.world, replies, vec![ent_bag()], vec![]);
        let session = kb.session();
        session
            .download_product(&account(), &app(), GUID, VERSION, Some(Platform::IPhone))
            .await
            .unwrap();
        assert!(
            kb.downloads()
                .last()
                .unwrap()
                .request
                .url
                .contains("volumeStoreDownloadProduct")
        );
        assert_eq!(kb.generated(), 1);
        assert!(cached_entry(&kb.world).await.is_none(), "{outcome}");
        assert!(session.cached_kbsync(DSID, GUID).await.is_empty());
    }
}

#[tokio::test]
async fn cache_ignores_other_accounts_devices_and_bad_entries() {
    let world = World::new();
    let session = world.session();
    store_cache(&world, &cached()).await;
    assert_eq!(session.cached_kbsync(DSID, GUID).await, cached());
    assert!(session.cached_kbsync(DSID + 1, GUID).await.is_empty());
    assert!(session.cached_kbsync(DSID, "AABBCCDDEEFF").await.is_empty());
    let world = World::new();
    store_cache(&world, "not base64!").await;
    assert!(world.session().cached_kbsync(DSID, GUID).await.is_empty());
    let world = World::new();
    store_cache(&world, "").await;
    assert!(world.session().cached_kbsync(DSID, GUID).await.is_empty());
    let world = World::new();
    world.keychain.set(keys::KBSYNC, b"{").await.unwrap();
    assert!(world.session().cached_kbsync(DSID, GUID).await.is_empty());
}

#[tokio::test]
async fn invalidates_only_the_rejected_entry() {
    let world = World::new();
    let session = world.session();
    store_cache(&world, &cached()).await;
    session.invalidate_kbsync(DSID, GUID, &fresh()).await;
    assert!(cached_entry(&world).await.is_some());
    session.invalidate_kbsync(DSID, GUID, &cached()).await;
    assert!(cached_entry(&world).await.is_none());
}

#[test]
fn generation_reports_go_errors() {
    let world = World::new();
    let mut session = world.session();
    assert_eq!(
        session.generate_kbsync(&[0; 6], 1).unwrap_err().to_string(),
        "kbsync generator is unavailable"
    );
    session.kbsync_generator = Some(Box::new(|_, _| Ok(Vec::new())));
    assert_eq!(
        session.generate_kbsync(&[0; 6], 1).unwrap_err().to_string(),
        "generated download kbsync is empty"
    );
    session.kbsync_generator = Some(Box::new(|_, _| {
        Err(Error::Invalid("guest failed".to_owned()))
    }));
    assert_eq!(
        session.generate_kbsync(&[0; 6], 1).unwrap_err().to_string(),
        "generate download kbsync: guest failed"
    );
}
