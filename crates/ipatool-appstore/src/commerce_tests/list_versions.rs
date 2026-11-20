//! `appstore_list_versions_test.go`.

use plist::Value as PValue;

use super::download_product::{
    REDOWNLOAD, bag_with, empty, item, items, lookup_json, route, sent_kind,
};
use super::version_lookup::{KARING, karing, mac_account, mac_page, new_world};
use super::*;
use crate::{Account, App, Error, ListVersionsOutput, Platform};

fn versions(identifiers: &[PValue], latest: Option<PValue>) -> Reply {
    let mut metadata = vec![(
        "softwareVersionExternalIdentifiers",
        PValue::Array(identifiers.to_vec()),
    )];
    metadata.extend(latest.map(|latest| ("softwareVersionExternalIdentifier", latest)));
    items(vec![item("", &metadata)])
}

fn number(value: u64) -> PValue {
    PValue::Integer(value.into())
}

async fn list(
    world: &World,
    account: &Account,
    app: &App,
    platform: Option<Platform>,
) -> crate::Result<ListVersionsOutput> {
    world.session().list_versions(account, app, platform).await
}

async fn list_default(world: &World) -> crate::Result<ListVersionsOutput> {
    list(world, &Account::default(), &App::default(), None).await
}

#[tokio::test]
async fn pins_the_mac_offer_before_requesting_a_universal_apps_version_history() {
    let world = new_world();
    route(
        &world,
        vec![versions(
            &[number(876_660_700), number(876_660_716)],
            Some(number(876_660_716)),
        )],
        vec![],
        vec![mac_page(KARING)],
    );
    let out = list(&world, &mac_account(), &karing(), Some(Platform::MacOs))
        .await
        .unwrap();
    assert_eq!(out.external_version_identifiers, ["876660700", "876660716"]);
    assert_eq!(out.latest_external_version_id, "876660716");
    assert_eq!(
        sent_kind(&world, "platform")[0].request.url,
        "https://apps.apple.com/de/app/id6472431552?platform=mac"
    );
    let download = &sent_kind(&world, "download")[0];
    assert!(download.request.url.contains("volumeStoreDownloadProduct"));
    assert_eq!(download.param_str("externalVersionId"), "876660716");
}

#[tokio::test]
async fn does_not_fall_back_to_ios_when_the_mac_version_cannot_be_resolved() {
    let world = new_world();
    route(&world, vec![], vec![], vec![mac_page("{}")]);
    let app = App {
        id: 42,
        ..App::default()
    };
    let error = list(&world, &mac_account(), &app, Some(Platform::MacOs))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to resolve platform version"),
        "{error}"
    );
    assert!(sent_kind(&world, "download").is_empty());
}

#[tokio::test]
async fn pins_the_watchos_version_before_requesting_version_history() {
    let world = World::new();
    route(
        &world,
        vec![versions(
            &[number(123_455), number(123_456)],
            Some(number(123_456)),
        )],
        vec![],
        vec![lookup_json(42, "123456")],
    );
    let account = Account {
        store_front: "143441".to_owned(),
        ..Account::default()
    };
    let app = App {
        id: 42,
        ..App::default()
    };
    let out = list(&world, &account, &app, Some(Platform::WatchOs))
        .await
        .unwrap();
    assert_eq!(out.external_version_identifiers, ["123455", "123456"]);
    assert_eq!(out.latest_external_version_id, "123456");
    assert_eq!(
        sent_kind(&world, "download")[0].param_str("externalVersionId"),
        "123456"
    );
}

#[tokio::test]
async fn rejects_unsupported_platforms_before_making_requests() {
    let world = World::new();
    let error = list(
        &world,
        &Account::default(),
        &App::default(),
        Some(Platform::Unknown),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), r#"invalid platform "unknown""#);
    assert!(world.fake.sent().is_empty());
}

#[tokio::test]
async fn fails_without_a_mac_address_or_a_response() {
    let world = World::new();
    let mut session = world.session();
    session.mac_address = || Err(Error::Invalid(String::new()));
    assert!(
        session
            .list_versions(&Account::default(), &App::default(), None)
            .await
            .is_err()
    );

    let world = World::new();
    world.fake.push(Reply::Fail("boom"));
    assert!(list_default(&world).await.is_err());
}

#[tokio::test]
async fn sends_the_request_to_the_pod_specific_host() {
    let world = World::new();
    world.fake.push(Reply::Fail("boom"));
    let account = Account {
        pod: "42".to_owned(),
        ..Account::default()
    };
    assert!(list(&world, &account, &App::default(), None).await.is_err());
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
                ("failureType", s("test-failure")),
                ("customerMessage", s("test error message")),
            ],
            "received error: test error message",
        ),
        (
            vec![("failureType", s("test-failure"))],
            "received error: test-failure",
        ),
    ] {
        let world = World::new();
        world.fake.push(plist(200, &entries));
        assert_eq!(
            list_default(&world).await.unwrap_err().to_string(),
            expected
        );
    }
    let world = World::new();
    route(&world, vec![empty()], vec![bag_with(&[])], vec![]);
    assert_eq!(
        list_default(&world).await.unwrap_err().to_string(),
        "invalid response"
    );
}

#[tokio::test]
async fn requires_the_version_metadata() {
    let world = World::new();
    world
        .fake
        .push(items(vec![item("", &[("someOtherKey", s("someValue"))])]));
    assert_eq!(
        list_default(&world).await.unwrap_err().to_string(),
        "failed to get version identifiers from item metadata"
    );

    let world = World::new();
    world
        .fake
        .push(versions(&[s("12345678"), s("87654321")], None));
    assert_eq!(
        list_default(&world).await.unwrap_err().to_string(),
        "failed to get latest version from item metadata"
    );
}

#[tokio::test]
async fn lists_the_ios_history_from_a_version_pinned_fallback() {
    const LATEST: &str = "890598805";
    let world = World::new();
    route(
        &world,
        vec![
            empty(),
            versions(&[s("9660833"), s(LATEST)], Some(s(LATEST))),
        ],
        vec![bag_with(&[("redownloadProduct", REDOWNLOAD)])],
        vec![lookup_json(547_702_041, LATEST)],
    );
    let account = Account {
        store_front: "143441-1,34".to_owned(),
        ..Account::default()
    };
    let app = App {
        id: 547_702_041,
        ..App::default()
    };
    let out = list(&world, &account, &app, None).await.unwrap();
    assert_eq!(out.external_version_identifiers, ["9660833", LATEST]);
    assert_eq!(out.latest_external_version_id, LATEST);
    assert_eq!(
        sent_kind(&world, "download")[1].param_str("appExtVrsId"),
        LATEST
    );
}

#[tokio::test]
async fn lists_versions() {
    let world = World::new();
    world.fake.push(versions(
        &[s("12345678"), s("87654321")],
        Some(s("87654321")),
    ));
    let out = list_default(&world).await.unwrap();
    assert_eq!(out.external_version_identifiers, ["12345678", "87654321"]);
    assert_eq!(out.latest_external_version_id, "87654321");
}
