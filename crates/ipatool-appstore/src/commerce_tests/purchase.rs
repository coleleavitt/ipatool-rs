//! `appstore_purchase_test.go`, `account_storefront_test.go` and
//! `appstore_purchase_storefront_integration_test.go`.

use plist::Value as PValue;

use super::*;
use crate::purchase::{PurchaseResult, purchase_storefront_correction};
use crate::{Account, App, Error, Platform, keys};

fn account() -> Account {
    Account {
        store_front: "143441".to_owned(),
        ..Account::default()
    }
}

fn int(value: i64) -> PValue {
    PValue::Integer(value.into())
}

async fn purchase(world: &World, app: App, platform: Option<Platform>) -> crate::Result<()> {
    world
        .session()
        .purchase(&mut account(), &app, platform)
        .await
}

#[tokio::test]
async fn fails_when_the_mac_address_is_unavailable() {
    let world = World::new();
    let mut session = world.session();
    session.mac_address = || Err(Error::Invalid(String::new()));
    let error = session
        .purchase(&mut account(), &App::default(), None)
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("failed to get mac address"));
}

#[tokio::test]
async fn refuses_paid_apps() {
    let world = World::new();
    let app = App {
        price: 0.99,
        ..App::default()
    };
    let error = purchase(&world, app, None).await.unwrap_err();
    assert_eq!(error.to_string(), "purchasing paid apps is not supported");
    assert!(world.fake.sent().is_empty());
}

#[tokio::test]
async fn reports_a_failed_request() {
    let world = World::new();
    world.fake.push(Reply::Fail("boom"));
    let error = purchase(&world, App::default(), None).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to purchase item with param 'STDQ': request failed: request failed: boom"
    );
}

#[tokio::test]
async fn sends_to_the_pod_host_with_go_parameters() {
    let world = World::new();
    world.fake.push(Reply::Fail(""));
    let mut account = Account {
        pod: "42".to_owned(),
        directory_services_id: "7".to_owned(),
        password_token: "token".to_owned(),
        ..account()
    };
    let app = App {
        id: 42,
        ..App::default()
    };
    let _ = world.session().purchase(&mut account, &app, None).await;
    let sent = &world.fake.sent()[0];
    assert_eq!(sent.operation, "purchase");
    assert_eq!(
        sent.request.url,
        "https://p42-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/buyProduct"
    );
    assert_eq!(sent.request.method, "POST");
    assert_eq!(sent.header("User-Agent"), crate::store::STORE_USER_AGENT);
    assert_eq!(sent.header("Content-Type"), "application/x-apple-plist");
    for key in ["iCloud-DSID", "X-Dsid"] {
        assert_eq!(sent.header(key), "7");
    }
    assert_eq!(sent.header("X-Apple-Store-Front"), "143441");
    assert_eq!(sent.header("X-Token"), "token");
    assert_eq!(sent.param_str("guid"), GUID.to_uppercase());
    for (key, value) in [
        ("appExtVrsId", "0"),
        ("hasAskedToFulfillPreorder", "true"),
        ("buyWithoutAuthorization", "true"),
        ("hasDoneAgeCheck", "true"),
        ("needDiv", "0"),
        ("origPage", "Software-42"),
        ("origPageLocation", "Buy"),
        ("price", "0"),
        ("pricingParameters", "STDQ"),
        ("productType", "C"),
    ] {
        assert_eq!(sent.param_str(key), value, "{key}");
    }
    assert_eq!(sent.param("salableAdamId"), Some(&Value::Integer(42)));
}

#[tokio::test]
async fn maps_failures_like_go() {
    let cases: Vec<(Vec<(&str, PValue)>, u16, &str)> = vec![
        (
            vec![("failureType", s("2034"))],
            200,
            "password token is expired",
        ),
        (
            vec![("failureType", s("2042"))],
            200,
            "password token is expired",
        ),
        (
            vec![
                ("failureType", s("some_other_failure")),
                ("customerMessage", s("Your password has changed.")),
            ],
            200,
            "password token is expired",
        ),
        (
            vec![
                ("failureType", s("failure")),
                (
                    "customerMessage",
                    s("MZFinance.BadLogin.Configurator_message"),
                ),
            ],
            200,
            "MZFinance.BadLogin.Configurator_message",
        ),
        (
            vec![("failureType", s("failure"))],
            200,
            "something went wrong",
        ),
        (
            vec![
                ("failureType", s("5002")),
                ("customerMessage", s("An unknown error has occurred")),
            ],
            200,
            "license already exists",
        ),
        (vec![], 500, "license already exists"),
        (
            vec![
                ("failureType", s("1008")),
                (
                    "customerMessage",
                    s("Your device or computer could not be verified."),
                ),
            ],
            200,
            "password token is expired",
        ),
        (
            vec![("jingleDocType", s("failure")), ("status", int(-1))],
            200,
            "failed to purchase app",
        ),
    ];
    for (entries, status, message) in cases {
        let world = World::new();
        world.fake.push(plist(status, &entries));
        let error = purchase(&world, App::default(), None).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("failed to purchase item with param 'STDQ': {message}"),
            "{entries:?}"
        );
    }
}

#[tokio::test]
async fn license_already_exists_is_detectable() {
    let world = World::new();
    world.fake.push(plist(200, &[("failureType", s("5002"))]));
    let error = purchase(&world, App::default(), None).await.unwrap_err();
    assert!(error.is_license_already_exists());
}

fn pricing(sent: &Sent) -> String {
    sent.param_str("pricingParameters").to_owned()
}

#[tokio::test]
async fn subscription_required_after_the_game_retry() {
    let world = World::new();
    world.fake.push(plist(
        200,
        &[
            (
                "customerMessage",
                s("This item is temporarily unavailable."),
            ),
            ("failureType", s("2059")),
        ],
    ));
    world.fake.push(plist(
        200,
        &[("customerMessage", s("Subscription Required"))],
    ));
    let error = purchase(&world, App::default(), None).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to purchase item with param 'GAME': subscription required"
    );
    let sent: Vec<_> = world.fake.sent().iter().map(pricing).collect();
    assert_eq!(sent, ["STDQ", "GAME"]);
}

fn success() -> Reply {
    plist(
        200,
        &[("jingleDocType", s("purchaseSuccess")), ("status", int(0))],
    )
}

#[tokio::test]
async fn macos_uses_stdq_without_the_game_fallback() {
    let world = World::new();
    let app = App {
        id: 42,
        ..App::default()
    };
    world.fake.push(success());
    purchase(&world, app.clone(), Some(Platform::MacOs))
        .await
        .unwrap();
    let world = World::new();
    world.fake.push(plist(200, &[("failureType", s("2059"))]));
    let error = purchase(&world, app, Some(Platform::MacOs))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("STDQ"), "{error}");
    assert_eq!(world.fake.sent().len(), 1);
}

#[tokio::test]
async fn other_platforms_fall_back_to_game() {
    for platform in [
        None,
        Some(Platform::IPhone),
        Some(Platform::IPad),
        Some(Platform::AppleTv),
        Some(Platform::VisionOs),
    ] {
        let world = World::new();
        world.fake.push(plist(200, &[("failureType", s("2059"))]));
        world.fake.push(success());
        let app = App {
            id: 42,
            ..App::default()
        };
        purchase(&world, app, platform).await.unwrap();
        let sent: Vec<_> = world.fake.sent().iter().map(pricing).collect();
        assert_eq!(sent, ["STDQ", "GAME"], "{platform:?}");
    }
}

fn mismatch(url: &str) -> PurchaseResult {
    PurchaseResult {
        failure_type: "-128".to_owned(),
        dialog_id: "MZCommerce.CountryMismatch".to_owned(),
        dialog_ok_kind: "Goto".to_owned(),
        dialog_ok_url: url.to_owned(),
        ..PurchaseResult::default()
    }
}

const RESET: &str = "https://itunes.apple.com/WebObjects/MZStore.woa/wa/resetAndRedirect";

#[test]
fn corrects_only_apples_country_switch() {
    let good = format!("{RESET}?dsf=143444&mt=8");
    assert_eq!(
        purchase_storefront_correction(&mismatch(&good), "143441-1,29"),
        "143444-1,29"
    );
    assert_eq!(
        purchase_storefront_correction(&mismatch(&good), " 143441 "),
        "143444"
    );
    for (url, current) in [
        (format!("{RESET}?dsf=143441"), "143441-1,29"),
        (format!("{RESET}?dsf=0"), "143441"),
        (format!("{RESET}?dsf=0143444"), "143441"),
        (format!("{RESET}?dsf=1&dsf=2"), "143441"),
        (format!("{RESET}?dsf=99999999999"), "143441"),
        (
            "http://itunes.apple.com/WebObjects/MZStore.woa/wa/resetAndRedirect?dsf=143444"
                .to_owned(),
            "143441",
        ),
        (
            "https://evil.example/WebObjects/MZStore.woa/wa/resetAndRedirect?dsf=143444".to_owned(),
            "143441",
        ),
        (
            "https://itunes.apple.com:8443/WebObjects/MZStore.woa/wa/resetAndRedirect?dsf=143444"
                .to_owned(),
            "143441",
        ),
        (
            "https://u@itunes.apple.com/WebObjects/MZStore.woa/wa/resetAndRedirect?dsf=143444"
                .to_owned(),
            "143441",
        ),
        (format!("{RESET}?dsf=143444#frag"), "143441"),
        (
            "https://itunes.apple.com/other?dsf=143444".to_owned(),
            "143441",
        ),
        (format!("{RESET}?dsf=143444;x"), "143441"),
    ] {
        assert_eq!(
            purchase_storefront_correction(&mismatch(&url), current),
            "",
            "{url}"
        );
    }
    let mut other = mismatch(&good);
    other.failure_type = "1".to_owned();
    assert_eq!(purchase_storefront_correction(&other, "143441"), "");
    let mut other = mismatch(&good);
    other.dialog_id = "other".to_owned();
    assert_eq!(purchase_storefront_correction(&other, "143441"), "");
    let mut other = mismatch(&good);
    other.dialog_ok_kind = "Buy".to_owned();
    assert_eq!(purchase_storefront_correction(&other, "143441"), "");
}

fn country_mismatch() -> Reply {
    let ok = dict(&[
        ("kind", s("Goto")),
        ("url", s(&format!("{RESET}?dsf=143444"))),
    ]);
    plist(
        200,
        &[
            ("failureType", s("-128")),
            (
                "metrics",
                PValue::Dictionary(dict(&[("dialogId", s("MZCommerce.CountryMismatch"))])),
            ),
            (
                "dialog",
                PValue::Dictionary(dict(&[("okButtonAction", PValue::Dictionary(ok))])),
            ),
        ],
    )
}

async fn save_account(world: &World, account: &Account) {
    world
        .keychain
        .set(keys::ACCOUNT, &serde_json::to_vec(account).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn corrects_and_saves_the_storefront_once() {
    let world = World::new();
    let mut account = Account {
        directory_services_id: "7".to_owned(),
        store_front: "143441-1,29".to_owned(),
        email: "a@b".to_owned(),
        ..Account::default()
    };
    save_account(&world, &account).await;
    world.fake.push(country_mismatch());
    world.fake.push(success());
    world
        .session()
        .purchase(&mut account, &App::default(), None)
        .await
        .unwrap();
    assert_eq!(account.store_front, "143444-1,29");
    let fronts: Vec<_> = world
        .fake
        .sent()
        .iter()
        .map(|sent| sent.header("X-Apple-Store-Front").to_owned())
        .collect();
    assert_eq!(fronts, ["143441-1,29", "143444-1,29"]);
    let saved = world.session().account_info().await.unwrap();
    assert_eq!(saved.store_front, "143444-1,29");
    assert_eq!(saved.email, "a@b");

    // A second mismatch is not corrected again.
    let world = World::new();
    account.store_front = "143441-1,29".to_owned();
    save_account(&world, &account).await;
    world.fake.push(country_mismatch());
    world.fake.push(country_mismatch());
    let error = world
        .session()
        .purchase(&mut account, &App::default(), None)
        .await
        .unwrap_err();
    assert_eq!(world.fake.sent().len(), 2);
    assert_eq!(
        error.to_string(),
        "failed to purchase item with param 'STDQ': something went wrong"
    );
}

#[tokio::test]
async fn storefront_correction_requires_the_same_saved_account() {
    let world = World::new();
    save_account(
        &world,
        &Account {
            directory_services_id: "8".to_owned(),
            ..Account::default()
        },
    )
    .await;
    let mut account = Account {
        directory_services_id: "7".to_owned(),
        ..account()
    };
    world.fake.push(country_mismatch());
    let error = world
        .session()
        .purchase(&mut account, &App::default(), None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "failed to purchase item with param 'STDQ': failed to correct account storefront: saved account changed during storefront recovery"
    );
}
