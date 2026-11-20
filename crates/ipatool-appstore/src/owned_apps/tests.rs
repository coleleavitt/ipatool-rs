//! Port of Go `appstore_owned_apps_test.go`: Store requests go to the fake executor of
//! `commerce_tests`, the machine, bag and signer to a fake [`SapBackend`].

#![allow(
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    clippy::assert_is_empty,
    reason = "test fixtures"
)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use super::*;
use crate::commerce_tests::{Fake, Reply};
use crate::login::tests::valid_sap_config;
use crate::{ResponseFormat, SapConfig};

const TEST_DSID: &str = "123456789";
const TEST_GUID: &str = "AABBCCDDEEFF";
const TEST_STORE_FRONT: &str = "143441-1,34";
const TEST_TOKEN: &str = "password-token";
const TEST_SESSION_ID: u32 = 42;
const TEST_REVISION: u32 = 99;
const QUERY: &str = "('com.apple.itunes.extended\\-media\\-kind:131072','com.apple.itunes.extended\\-media\\-kind:262144','com.apple.itunes.extended\\-media\\-kind:67108864')";

fn account() -> Account {
    Account {
        directory_services_id: TEST_DSID.to_owned(),
        password_token: TEST_TOKEN.to_owned(),
        store_front: TEST_STORE_FRONT.to_owned(),
        ..Account::default()
    }
}

#[derive(Default)]
struct SignerState {
    close_calls: Cell<usize>,
    close_error: RefCell<Option<&'static str>>,
}

struct FakeSigner(Rc<SignerState>);

fn signature_of(body: &[u8]) -> Vec<u8> {
    [b"signed:".as_slice(), body].concat()
}

impl ActionSigner for FakeSigner {
    fn sign(&mut self, input: &[u8]) -> ipatool_sap::Result<Vec<u8>> {
        Ok(signature_of(input))
    }

    fn close(&mut self) -> ipatool_sap::Result<()> {
        self.0.close_calls.set(self.0.close_calls.get() + 1);
        match *self.0.close_error.borrow() {
            Some(message) => Err(ipatool_sap::Error::msg(message)),
            None => Ok(()),
        }
    }
}

struct Sap {
    mac_address: std::result::Result<&'static str, &'static str>,
    signer: Rc<SignerState>,
}

impl Default for Sap {
    fn default() -> Self {
        Self {
            mac_address: Ok("aa:bb:cc:dd:ee:ff"),
            signer: Rc::default(),
        }
    }
}

#[allow(clippy::unused_async_trait_impl, reason = "the fake answers at once")]
impl SapBackend for Sap {
    type Signer = FakeSigner;

    fn mac_address(&self) -> Result<String> {
        self.mac_address
            .map(str::to_owned)
            .map_err(|message| Error::Invalid(message.to_owned()))
    }

    async fn sap_config(&self, _: &Client, guid: &str) -> Result<SapConfig> {
        assert_eq!(guid, TEST_GUID);
        Ok(valid_sap_config())
    }

    async fn create_signer(
        &self,
        _: &Client,
        config: &SapConfig,
        machine_id: &[u8],
    ) -> Result<FakeSigner> {
        assert_eq!(*config, valid_sap_config());
        assert_eq!(machine_id, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        Ok(FakeSigner(Rc::clone(&self.signer)))
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

struct Harness {
    fake: Arc<Fake>,
    client: Client,
    sap: Sap,
}

impl Harness {
    fn new() -> Self {
        let fake = Fake::new();
        let client = Client::new()
            .unwrap()
            .with_store_transport(Arc::clone(&fake) as Arc<dyn crate::StoreTransport>);
        Self {
            fake,
            client,
            sap: Sap::default(),
        }
    }

    fn reply(&self, status: u16, data: Vec<u8>) {
        self.fake.push(Reply::Body(status, Vec::new(), data));
    }

    /// Go's `expectStorefront`: login, update and items answers for one storefront.
    fn storefront(&self, data: Vec<u8>) {
        self.reply(200, dmap_tag("mlog", &dmap_uint32("mlid", TEST_SESSION_ID)));
        self.reply(200, dmap_tag("mupd", &dmap_uint32("musr", TEST_REVISION)));
        self.reply(200, data);
    }

    async fn run(&self, input: OwnedAppsInput) -> Result<OwnedAppsOutput> {
        owned_apps(&self.client, &self.sap, input, now).await
    }

    fn store_fronts(&self) -> Vec<String> {
        self.fake
            .sent()
            .iter()
            .map(|sent| sent.header("X-Apple-Store-Front").to_owned())
            .collect()
    }

    fn close_calls(&self) -> usize {
        self.sap.signer.close_calls.get()
    }
}

fn input(platform: Option<Platform>, page: i64, limit: i64) -> OwnedAppsInput {
    OwnedAppsInput {
        account: account(),
        page,
        limit,
        platform,
    }
}

fn dmap_string(name: &str, value: &str) -> Vec<u8> {
    dmap_tag(name, value.as_bytes())
}

fn dmap_app_id_for_test(value: u64) -> Vec<u8> {
    dmap_tag("aeSI", &value.to_be_bytes())
}

fn listing(items: &[Vec<u8>]) -> Vec<u8> {
    let items: Vec<u8> = items
        .iter()
        .flat_map(|item| dmap_tag("mlit", item))
        .collect();
    dmap_tag("adbs", &dmap_tag("mlcl", &items))
}

fn date(seconds: i64) -> Option<OffsetDateTime> {
    Some(OffsetDateTime::from_unix_timestamp(seconds).unwrap())
}

/// Go's `ownedAppsDMAPResponse`.
fn owned_apps_dmap_response(apps: &[App]) -> Vec<u8> {
    let items: Vec<Vec<u8>> = apps
        .iter()
        .map(|app| {
            let mut item = dmap_app_id_for_test(app.id.cast_unsigned());
            item.extend(dmap_string("aeBI", &app.bundle_id));
            item.extend(dmap_string("aeLN", &app.name));
            item.extend(dmap_string("aePd", &app.version));
            if let Some(date) = app.purchase_date {
                item.extend(dmap_uint32(
                    "asdp",
                    u32::try_from(date.unix_timestamp()).unwrap(),
                ));
            }
            item
        })
        .collect();
    listing(&items)
}

fn ids(output: &OwnedAppsOutput) -> Vec<i64> {
    output.results.iter().map(|app| app.id).collect()
}

#[tokio::test]
async fn filters_merged_purchases_before_sorting_and_pagination() {
    let item = |id: u32, kind: u32, products: u32, date: u32| {
        let mut data = dmap_uint32("aeSI", id);
        data.extend(dmap_uint32("aeMk", kind));
        data.extend(dmap_uint32("aeSS", products));
        data.extend(dmap_uint32("asdp", date));
        data
    };
    for (name, platform, page, total, expected) in [
        ("no filter", None, 1, 4, vec![4]),
        ("iPhone", Some(Platform::IPhone), 1, 2, vec![1]),
        ("iPad", Some(Platform::IPad), 1, 1, vec![2]),
        ("visionOS", Some(Platform::VisionOs), 1, 1, vec![2]),
        (
            "Mac second page includes merged platform",
            Some(Platform::MacOs),
            2,
            2,
            vec![2],
        ),
        (
            "no matching Apple TV apps",
            Some(Platform::AppleTv),
            1,
            0,
            vec![],
        ),
        (
            "page past filtered results",
            Some(Platform::VisionOs),
            2,
            1,
            vec![],
        ),
    ] {
        let harness = Harness::new();
        harness.storefront(listing(&[
            item(1, 131_072, 1, 300),
            item(2, 131_072, 19, 200),
            item(3, 131_072, 0, 100),
        ]));
        harness.storefront(listing(&[
            item(2, 67_108_864, 0, 200),
            item(4, 67_108_864, 0, 400),
        ]));

        let output = harness.run(input(platform, page, 1)).await.unwrap();

        assert_eq!(output.total_count, total, "{name}");
        assert_eq!(output.count, expected.len(), "{name}");
        assert_eq!(output.page, page, "{name}");
        assert_eq!(ids(&output), expected, "{name}");
        if expected == [2] {
            assert_eq!(
                output.results[0].platforms,
                [
                    Platform::IPhone,
                    Platform::IPad,
                    Platform::VisionOs,
                    Platform::MacOs
                ],
                "{name}"
            );
        }
        let mut fronts = vec!["143441-1,34".to_owned(); 3];
        fronts.extend(vec!["143441-1,13".to_owned(); 3]);
        assert_eq!(harness.store_fronts(), fronts, "{name}");
    }
}

#[tokio::test]
async fn puts_a_new_mac_purchase_first_and_merges_apps_from_both_storefronts() {
    let harness = Harness::new();
    let mut mobile = dmap_uint32("aeSI", 123);
    mobile.extend(dmap_uint32("aeSS", 3));
    mobile.extend(dmap_uint32("aeMk", 131_072));
    mobile.extend(dmap_uint32("asdp", 100));
    let mut arcade = dmap_uint32("aeSI", 456);
    arcade.extend(dmap_uint32("aeMk", 262_144));
    arcade.extend(dmap_uint32("aeSS", 3));
    harness.storefront(listing(&[mobile.clone(), arcade]));
    // SnippetsLab's Mac media kind and empty supported-products field
    // reproduce the fields that were previously missing from the results.
    let mut mac = dmap_uint32("aeSI", 1_006_087_419);
    mac.extend(dmap_uint32("aeMk", 67_108_864));
    mac.extend(dmap_tag("aeSS", &[0, 0]));
    mac.extend(dmap_string("aeLN", "SnippetsLab"));
    mac.extend(dmap_uint32("asdp", 300));
    harness.storefront(listing(&[mobile, mac]));

    let output = harness.run(input(None, 0, 2)).await.unwrap();

    assert_eq!(output.total_count, 3);
    assert_eq!(output.count, 2);
    assert_eq!(output.results[0].name, "SnippetsLab");
    assert_eq!(output.results[0].platforms, [Platform::MacOs]);
    assert_eq!(output.results[1].id, 123);
    assert_eq!(
        output.results[1].platforms,
        [Platform::IPhone, Platform::IPad]
    );
    assert_eq!(harness.close_calls(), 1);
}

#[tokio::test]
async fn does_not_infer_a_platform_from_the_storefront_or_app_name() {
    let harness = Harness::new();
    harness.storefront(dmap_tag("adbs", &[]));
    let mut item = dmap_uint32("aeSI", 123);
    item.extend(dmap_string("aeLN", "Example for Mac"));
    harness.storefront(listing(&[item]));

    let output = harness.run(input(None, 0, 0)).await.unwrap();

    assert_eq!(output.results.len(), 1);
    assert_eq!(output.results[0].platforms, [Platform::Unknown]);
}

#[tokio::test]
async fn does_not_return_an_incomplete_list_if_the_mac_storefront_fails() {
    let harness = Harness::new();
    harness.storefront(owned_apps_dmap_response(&[App {
        id: 123,
        ..App::default()
    }]));
    harness.reply(401, Vec::new());

    let error = harness.run(input(None, 0, 0)).await.unwrap_err();

    assert!(error.is_password_token_expired());
    assert_eq!(
        error.to_string(),
        "password token is expired: purchase history login request returned HTTP 401"
    );
    assert_eq!(harness.close_calls(), 1);
}

#[tokio::test]
async fn sorts_all_apps_by_descending_purchase_date_before_returning_the_requested_partial_page() {
    let harness = Harness::new();
    let apps: Vec<App> = [5, 12, 1, 9, 3, 11, 2, 8, 4, 10, 6, 7]
        .into_iter()
        .map(|index: u8| App {
            id: 1000 + i64::from(index),
            bundle_id: format!("com.example.app{}", char::from(b'a' + index - 1)),
            name: "DAAP app".to_owned(),
            version: "1.0".to_owned(),
            purchase_date: date(1_700_000_000 + i64::from(index) * 60),
            ..App::default()
        })
        .collect();
    harness.storefront(owned_apps_dmap_response(&apps));
    harness.storefront(owned_apps_dmap_response(&[]));

    let output = harness.run(input(None, 2, 10)).await.unwrap();

    assert_eq!(output.count, 2);
    assert_eq!(output.total_count, 12);
    assert_eq!(output.page, 2);
    assert_eq!(ids(&output), [1002, 1001]);
    assert_eq!(output.results[0].purchase_date, date(1_700_000_120));
    assert_eq!(output.results[1].purchase_date, date(1_700_000_060));
    assert_eq!(harness.close_calls(), 1);
    assert_purchase_history_requests(&harness.fake.sent());
}

/// The login, update and items requests of the first storefront.
fn assert_purchase_history_requests(sent: &[crate::commerce_tests::Sent]) {
    let expect_headers = |index: usize| {
        let sent = &sent[index];
        assert_eq!(sent.operation, "request");
        assert_eq!(sent.request.method, "POST");
        assert_eq!(sent.request.response_format, ResponseFormat::Raw);
        for (key, value) in [
            ("X-Dsid", TEST_DSID),
            ("iCloud-DSID", TEST_DSID),
            ("X-Token", TEST_TOKEN),
            ("X-Apple-Store-Front", TEST_STORE_FRONT),
            ("X-Guid", TEST_GUID),
            ("Client-DAAP-Version", "3.12"),
            ("Client-Cloud-Purchase-Daap-Version", "1.1/Configurator-2.0"),
            ("User-Agent", STORE_USER_AGENT),
        ] {
            assert_eq!(sent.header(key), value, "{key}");
        }
    };

    expect_headers(0);
    let login = &sent[0].request;
    assert_eq!(login.url, format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/login"));
    assert!(login.signature.is_none());
    assert!(login.payload.is_none());

    expect_headers(1);
    let update = &sent[1].request;
    assert_eq!(
        update.url,
        format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/update")
    );
    assert_eq!(
        sent[1].header("Content-Type"),
        "application/x-www-form-urlencoded"
    );
    let Some(Payload::Raw(body)) = &update.payload else {
        panic!("update has a raw payload");
    };
    assert_eq!(
        String::from_utf8_lossy(body),
        format!("session-id=42&revision-number=(null)&query={QUERY}")
    );
    assert_eq!(update.signature, Some(Ok(signature_of(body))));

    expect_headers(2);
    let items = &sent[2].request;
    assert_eq!(
        items.url,
        format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/databases/99/items")
    );
    assert_eq!(sent[2].header("Content-Type"), "application/x-dmap-tagged");
    let Some(Payload::Raw(body)) = &items.payload else {
        panic!("items has a raw payload");
    };
    assert_eq!(items.signature, Some(Ok(signature_of(body))));
    assert_eq!(
        first_dmap_uint(body, "mlid").unwrap(),
        Some(u64::from(TEST_SESSION_ID))
    );
    assert_eq!(
        first_dmap_uint(body, "musr").unwrap(),
        Some(u64::from(TEST_REVISION))
    );
    let mut query = None;
    walk_dmap(body, 0, &mut |tag, data| {
        if tag == "mque" {
            query = Some(String::from_utf8_lossy(data).into_owned());
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(query.as_deref(), Some(QUERY));
}

#[tokio::test]
async fn returns_an_empty_page_past_the_end() {
    let harness = Harness::new();
    harness.storefront(owned_apps_dmap_response(&[App {
        id: 1,
        ..App::default()
    }]));
    harness.storefront(owned_apps_dmap_response(&[]));

    let output = harness.run(input(None, 2, 10)).await.unwrap();

    assert_eq!(output.count, 0);
    assert_eq!(output.total_count, 1);
    assert!(output.results.is_empty());
    assert_eq!(harness.close_calls(), 1);
}

#[tokio::test]
async fn returns_the_password_token_expired_error_and_closes_the_signer() {
    let harness = Harness::new();
    harness.reply(401, Vec::new());

    let error = harness.run(input(None, 0, 0)).await.unwrap_err();

    assert!(error.is_password_token_expired());
    assert_eq!(harness.close_calls(), 1);
}

#[tokio::test]
async fn preserves_both_errors_when_the_request_and_signer_cleanup_fail() {
    let harness = Harness::new();
    *harness.sap.signer.close_error.borrow_mut() = Some("cleanup error");
    harness.fake.push(Reply::Fail("request error"));

    let error = harness.run(input(None, 0, 0)).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "failed to open purchase history session: request failed: request error\nfailed to close SAP action signer: cleanup error"
    );
    assert!(matches!(&error, Error::Joined(errors) if errors.len() == 2));
    assert_eq!(harness.close_calls(), 1);
}

#[tokio::test]
async fn returns_an_error_before_creating_a_signer_when_the_machine_address_cannot_be_read() {
    let mut harness = Harness::new();
    harness.sap.mac_address = Err("machine error");

    let error = harness.run(input(None, 0, 0)).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "failed to get mac address: machine error"
    );
    assert!(harness.fake.sent().is_empty());
}

#[test]
fn validates_and_defaults_pagination() {
    let page = |page, limit, platform| OwnedAppsInput {
        page,
        limit,
        platform,
        ..OwnedAppsInput::default()
    };
    assert_eq!(
        normalize_owned_apps_input(page(0, 0, None)).unwrap(),
        page(1, 10, None)
    );
    assert_eq!(
        normalize_owned_apps_input(page(3, 100, None)).unwrap(),
        page(3, 100, None)
    );
    for (name, input, text) in [
        ("rejects a negative page", page(-1, 10, None), "page"),
        ("rejects a negative limit", page(1, -1, None), "limit"),
        ("rejects a limit over 100", page(1, 101, None), "100"),
        (
            "rejects watchOS",
            page(0, 0, Some(Platform::WatchOs)),
            "invalid platform",
        ),
        (
            "rejects unknown as a filter",
            page(0, 0, Some(Platform::Unknown)),
            "invalid platform",
        ),
    ] {
        let error = normalize_owned_apps_input(input).unwrap_err().to_string();
        assert!(error.contains(text), "{name}: {error}");
    }
}

#[test]
fn parses_app_fields_ignores_unknown_fields_and_removes_duplicate_ids() {
    let mut item = dmap_string("zzzz", "unknown");
    item.extend(dmap_app_id_for_test(123));
    item.extend(dmap_string("aeBI", "com.example.app"));
    item.extend(dmap_string("aeLN", "Example"));
    item.extend(dmap_string("aePd", "1.2.3"));
    item.extend(dmap_uint32("asdp", 1_700_000_000));

    let apps = parse_owned_apps(&listing(&[item.clone(), item])).unwrap();

    assert_eq!(
        apps,
        [App {
            id: 123,
            bundle_id: "com.example.app".to_owned(),
            name: "Example".to_owned(),
            version: "1.2.3".to_owned(),
            purchase_date: date(1_700_000_000),
            platforms: vec![Platform::Unknown],
            ..App::default()
        }]
    );
}

#[test]
fn decodes_supported_platforms() {
    use Platform::{IPad, IPhone, MacOs, Unknown, VisionOs};
    for (name, kind, products, expected) in [
        ("iPhone", 131_072, vec![0, 1], vec![IPhone]),
        ("iPad", 131_072, vec![0, 2], vec![IPad]),
        ("Mac universal purchase", 131_072, vec![0, 8], vec![MacOs]),
        ("visionOS", 131_072, vec![0, 16], vec![VisionOs]),
        (
            "Arcade does not imply Mac support",
            262_144,
            vec![0, 3],
            vec![IPhone, IPad],
        ),
        (
            "universal app",
            131_072,
            vec![0, 27],
            vec![IPhone, IPad, VisionOs, MacOs],
        ),
        (
            "Mac ignores mobile product bits",
            67_108_864,
            vec![0, 7],
            vec![MacOs],
        ),
        ("iPod touch only", 131_072, vec![0, 4], vec![Unknown]),
        (
            "unknown product bit does not imply visionOS",
            131_072,
            vec![0, 32],
            vec![Unknown],
        ),
        ("empty products", 131_072, vec![0, 0], vec![Unknown]),
        ("unknown media kind", 999, vec![0, 31], vec![Unknown]),
        ("unknown products", 131_072, vec![128, 0], vec![Unknown]),
        ("one-byte products", 131_072, vec![3], vec![IPhone, IPad]),
        (
            "four-byte products",
            131_072,
            vec![0, 0, 0, 3],
            vec![IPhone, IPad],
        ),
        (
            "eight-byte products",
            131_072,
            vec![0, 0, 0, 0, 0, 0, 0, 3],
            vec![IPhone, IPad],
        ),
    ] {
        let mut item = dmap_uint32("aeMk", kind);
        item.extend(dmap_tag("aeSS", &products));
        assert_eq!(
            parse_owned_app(&item).unwrap().platforms,
            expected,
            "{name}"
        );
    }
}

#[test]
fn returns_unknown_for_missing_platform_metadata() {
    for item in [dmap_uint32("aeMk", 131_072), dmap_uint32("aeSS", 31)] {
        assert_eq!(
            parse_owned_app(&item).unwrap().platforms,
            [Platform::Unknown]
        );
    }
}

#[test]
fn replaces_unknown_when_another_record_identifies_the_platform() {
    for platforms in [
        [Platform::Unknown, Platform::MacOs],
        [Platform::MacOs, Platform::Unknown],
    ] {
        let records = platforms
            .iter()
            .map(|platform| App {
                id: 123,
                platforms: vec![*platform],
                ..App::default()
            })
            .collect();
        let apps = merge_owned_apps(records);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].platforms, [Platform::MacOs]);
    }
}

#[test]
fn identifies_youtube_for_visionos_as_visionos_only() {
    let mut item = dmap_app_id_for_test(6_745_572_359);
    item.extend(dmap_string("aeBI", "com.google.visionosyoutube"));
    item.extend(dmap_string("aeLN", "YouTube for visionOS"));
    item.extend(dmap_uint32("aeMk", 131_072));
    item.extend(dmap_tag("aeSS", &[0, 16]));

    let apps = parse_owned_apps(&listing(&[item])).unwrap();

    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].id, 6_745_572_359);
    assert_eq!(apps[0].platforms, [Platform::VisionOs]);
}

#[test]
fn merges_platforms_and_keeps_the_latest_purchase_date_before_pagination() {
    let mut mobile = dmap_uint32("aeSI", 123);
    mobile.extend(dmap_uint32("aeSS", 3));
    mobile.extend(dmap_uint32("aeMk", 131_072));
    mobile.extend(dmap_uint32("asdp", 100));
    let mut mac = dmap_uint32("aeSI", 123);
    mac.extend(dmap_uint32("aeMk", 67_108_864));
    mac.extend(dmap_uint32("asdp", 300));
    let mut other = dmap_uint32("aeSI", 456);
    other.extend(dmap_uint32("asdp", 200));

    let apps = parse_owned_apps(&listing(&[mobile.clone(), other, mac, mobile])).unwrap();

    assert_eq!(apps.len(), 2);
    let page = owned_apps_page(&owned_apps_sorted_by_purchase_date(apps), 1, 1);
    assert_eq!(page[0].id, 123);
    assert_eq!(
        page[0].platforms,
        [Platform::IPhone, Platform::IPad, Platform::MacOs]
    );
    assert_eq!(page[0].purchase_date, date(300));
}

#[test]
fn rejects_malformed_platform_metadata() {
    for tag in ["aeMk", "aeSS"] {
        let error = parse_owned_app(&dmap_tag(tag, &[1, 2, 3])).unwrap_err();
        assert!(error.contains("invalid integer length 3"), "{tag}: {error}");
    }
}

#[test]
fn rejects_a_malformed_purchase_date() {
    let mut item = dmap_app_id_for_test(123);
    item.extend(dmap_tag("asdp", &[1, 2, 3]));
    let error = parse_owned_apps(&listing(&[item])).unwrap_err();
    assert_eq!(error, "owned app purchase date has invalid length 3");
}

#[test]
fn rejects_a_truncated_tag() {
    let error = parse_owned_apps(b"adbs\x00\x00\x00\x10short").unwrap_err();
    assert_eq!(
        error,
        "DMAP tag adbs length 16 exceeds remaining response length 5"
    );
}

#[test]
fn rejects_a_protocol_level_error_status() {
    let response = dmap_tag("mlog", &dmap_uint32("mstt", 403));
    let error = owned_apps_dmap_status_error("purchase history login", &response).unwrap_err();
    assert!(error.is_password_token_expired());
    assert_eq!(
        error.to_string(),
        "password token is expired: purchase history login response returned DAAP status 403"
    );
    let response = dmap_tag("mlog", &dmap_uint32("mstt", 500));
    let error = owned_apps_dmap_status_error("purchase history login", &response).unwrap_err();
    assert!(!error.is_password_token_expired());
    assert_eq!(
        error.to_string(),
        "purchase history login response returned DAAP status 500"
    );
}

#[test]
fn preserves_a_partial_last_page() {
    let apps: Vec<App> = (1..=3)
        .map(|id| App {
            id,
            ..App::default()
        })
        .collect();
    assert_eq!(ids_of(&owned_apps_page(&apps, 2, 2)), [3]);
    assert!(owned_apps_page(&apps, 3, 2).is_empty());
    assert!(owned_apps_page(&apps, i64::MAX, 100).is_empty());
}

fn ids_of(apps: &[App]) -> Vec<i64> {
    apps.iter().map(|app| app.id).collect()
}

#[test]
fn sorts_missing_dates_last_and_preserves_response_order_for_ties() {
    let app = |id, purchase_date| App {
        id,
        purchase_date,
        ..App::default()
    };
    let day = 1_700_000_000;
    let apps = vec![
        app(1, None),
        app(2, date(day)),
        app(3, date(day + 3600)),
        app(4, date(day)),
        app(5, None),
    ];
    assert_eq!(
        owned_apps_sorted_by_purchase_date(apps),
        [
            app(3, date(day + 3600)),
            app(2, date(day)),
            app(4, date(day)),
            app(1, None),
            app(5, None),
        ]
    );
}

#[test]
fn builds_the_dynamic_dmap_item_request_body() {
    let query = "('com.apple.itunes.extended\\-media\\-kind:131072')";
    let body = owned_apps_items_body(42, 99, query, now());
    assert_eq!(first_dmap_uint(&body, "mstc").unwrap(), Some(1_700_000_000));
    assert_eq!(first_dmap_uint(&body, "mlid").unwrap(), Some(42));
}

#[test]
fn formats_the_date_headers_like_go() {
    let headers = owned_apps_headers(&account(), TEST_GUID, now());
    assert_eq!(headers["Date"], "Tue, 14 Nov 2023 22:13:20 GMT");
    assert_eq!(headers["X-Apple-I-Client-Time"], "2023-11-14T22:13:20Z");
    assert_eq!(headers.len(), 16);
}

#[test]
fn names_the_local_zone_like_go() {
    assert_eq!(local_location_name(None), "Local");
    assert_eq!(local_location_name(Some("")), "UTC");
    assert_eq!(
        local_location_name(Some(":America/Detroit")),
        "America/Detroit"
    );
    assert_eq!(local_location_name(Some("/etc/localtime")), "Local");
}
