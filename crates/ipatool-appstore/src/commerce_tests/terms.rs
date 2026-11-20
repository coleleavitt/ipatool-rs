//! `appstore_terms_test.go`.

use std::sync::atomic::{AtomicUsize, Ordering};

use plist::Value as PValue;

use super::*;
use crate::purchase::{TermsResult, terms_endpoint, terms_version};
use crate::{Account, App, UrlBag};

const TERMS_PAGE: &str = "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage";
const PURCHASE_TERMS_FIXTURE: &str = r#"<plist version="1.0"><dict><key>pings</key><array></array><key>jingleDocType</key><string>buyProductFailure</string><key>jingleAction</key><string>buyProduct</string><key>action</key><dict><key>kind</key><string>Goto</string><key>url</key><string>https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage</string><key>target</key><string>main</string></dict></dict></plist>"#;

fn blocked() -> Reply {
    Reply::Body(200, Vec::new(), PURCHASE_TERMS_FIXTURE.as_bytes().to_vec())
}

fn blocked_with(kind: &str, document: &str, url: &str) -> Reply {
    let action = dict(&[("kind", s(kind)), ("url", s(url))]);
    plist(
        200,
        &[
            ("jingleDocType", s(document)),
            ("action", PValue::Dictionary(action)),
        ],
    )
}

fn success() -> Reply {
    plist(200, &[("jingleDocType", s("purchaseSuccess"))])
}

const INFO: &str = r#"{"status":0,"termsVersionId":290462}"#;
const ACCEPTED: &str = r#"{"status":0,"isCurrent":true}"#;

fn input() -> Account {
    Account {
        directory_services_id: "123456".to_owned(),
        store_front: "143462-1,34".to_owned(),
        password_token: "fixture-token".to_owned(),
        pod: "45".to_owned(),
        ..Account::default()
    }
}

fn app() -> App {
    App {
        id: 1234,
        ..App::default()
    }
}

fn bag() -> Reply {
    plist(
        200,
        &[
            (
                "getTermsByCountryAndTypeSrv",
                s("https://amp-account.itunes.apple.com/account/terms/info"),
            ),
            (
                "terms-check",
                s("https://buy.itunes.apple.com/account/terms"),
            ),
        ],
    )
}

/// Routes by endpoint: purchases, the bag, and the two terms calls from queues.
fn route(world: &World, purchases: Vec<Reply>, terms: Vec<Reply>) {
    let purchases = Mutex::new(VecDeque::from(purchases));
    let terms = Mutex::new(VecDeque::from(terms));
    world.fake.handle(move |sent| {
        if sent.request.url.contains("init.itunes.apple.com") {
            return bag();
        }
        let queue = if sent.operation == "purchase" {
            &purchases
        } else {
            &terms
        };
        queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected request {}", sent.request.url))
    });
}

fn world() -> World {
    let world = World::new();
    world.client.cache_storefronts(&[("143462", "JP")]);
    world
}

async fn purchase(world: &World) -> crate::Result<()> {
    world.session().purchase(&mut input(), &app(), None).await
}

#[tokio::test]
async fn accepts_the_current_version_and_retries_the_unchanged_purchase() {
    let world = world();
    route(
        &world,
        vec![blocked(), success()],
        vec![json(200, INFO), json(200, ACCEPTED)],
    );
    purchase(&world).await.unwrap();
    let sent: Vec<_> = world
        .fake
        .sent()
        .into_iter()
        .filter(|sent| !sent.request.url.contains("init.itunes"))
        .collect();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[0].request, sent[3].request);
    let info = &sent[1];
    assert_eq!(info.operation, "request");
    assert_eq!(info.request.method, "GET");
    assert_eq!(
        info.request.url,
        "https://amp-account.itunes.apple.com/account/terms/info?cc=JP&termsType=MZStore.NewTermsAndConditionsStatement"
    );
    assert_eq!(info.header("X-Apple-Store-Front"), "143462-1,34");
    assert_eq!(info.header("X-Token"), "fixture-token");
    assert_eq!(info.header("Accept"), "application/json");
    let check = &sent[2];
    assert_eq!(check.request.method, "POST");
    assert_eq!(
        check.request.url,
        "https://p45-buy.itunes.apple.com/account/terms"
    );
    assert_eq!(check.header("X-Dsid"), "123456");
    assert_eq!(check.header("Content-Type"), "application/json");
    let Some(Payload::Raw(body)) = &check.request.payload else {
        panic!("raw payload expected");
    };
    assert_eq!(body.as_slice(), br#"{"accepted":290462,"id":"123456"}"#);
}

#[tokio::test]
async fn recovers_the_storefront_before_accepting_terms() {
    let world = world();
    let mut account = Account {
        store_front: "143441-1,34".to_owned(),
        ..input()
    };
    world
        .keychain
        .set(crate::keys::ACCOUNT, &serde_json::to_vec(&account).unwrap())
        .await
        .unwrap();
    let ok = dict(&[
        ("kind", s("Goto")),
        (
            "url",
            s("https://itunes.apple.com/WebObjects/MZStore.woa/wa/resetAndRedirect?dsf=143462"),
        ),
    ]);
    let mismatch = plist(
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
    );
    route(
        &world,
        vec![mismatch, blocked(), success()],
        vec![json(200, INFO), json(200, ACCEPTED)],
    );
    world
        .session()
        .purchase(&mut account, &app(), None)
        .await
        .unwrap();
    let purchases: Vec<_> = world
        .fake
        .sent()
        .into_iter()
        .filter(|sent| sent.operation == "purchase")
        .collect();
    assert_eq!(purchases[2].header("X-Apple-Store-Front"), "143462-1,34");
    assert!(
        world.fake.sent_to("terms/info")[0]
            .request
            .url
            .contains("cc=JP")
    );
}

#[tokio::test]
async fn does_not_loop_when_terms_are_still_required() {
    let world = world();
    route(
        &world,
        vec![blocked(), blocked()],
        vec![json(200, INFO), json(200, ACCEPTED)],
    );
    let error = purchase(&world).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("still requires terms acceptance"),
        "{error}"
    );
}

#[tokio::test]
async fn does_not_accept_without_a_version() {
    let world = world();
    route(&world, vec![blocked()], vec![json(200, r#"{"status":0}"#)]);
    let error = purchase(&world).await.unwrap_err();
    assert!(
        error.to_string().contains("invalid terms version"),
        "{error}"
    );
}

#[tokio::test]
async fn does_not_retry_without_confirmed_acceptance() {
    for reply in [
        json(200, r#"{"status":0}"#),
        json(200, r#"{"isCurrent":true}"#),
        json(500, ACCEPTED),
        Reply::Fail("fixture network failure"),
    ] {
        let world = world();
        route(&world, vec![blocked()], vec![json(200, INFO), reply]);
        assert!(purchase(&world).await.is_err());
        let purchases = world
            .fake
            .sent()
            .iter()
            .filter(|sent| sent.operation == "purchase")
            .count();
        assert_eq!(purchases, 1);
    }
}

#[tokio::test]
async fn keeps_expired_token_recovery() {
    let expiration = r#"{"errorMessageKey":"MZFinance.TermsCheckLoginRequired"}"#;
    for at_acceptance in [false, true] {
        let world = world();
        let terms = if at_acceptance {
            vec![json(200, INFO), json(200, expiration)]
        } else {
            vec![json(200, expiration)]
        };
        route(&world, vec![blocked()], terms);
        assert!(
            purchase(&world)
                .await
                .unwrap_err()
                .is_password_token_expired()
        );
    }
}

#[tokio::test]
async fn ignores_unrelated_purchase_actions() {
    for (kind, document, url) in [
        ("OpenURL", "buyProductFailure", TERMS_PAGE),
        ("Goto", "other", TERMS_PAGE),
        (
            "Goto",
            "buyProductFailure",
            "https://example.com/WebObjects/MZFinance.woa/wa/com.apple.jingle.app.finance.DirectAction/termsPage",
        ),
        (
            "Goto",
            "buyProductFailure",
            "https://p45-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/accountSummary",
        ),
    ] {
        let world = world();
        route(&world, vec![blocked_with(kind, document, url)], vec![]);
        assert!(purchase(&world).await.is_err());
        assert_eq!(world.fake.sent().len(), 1, "{kind} {document} {url}");
    }
}

fn version(current: Option<&str>, latest: Option<&str>) -> crate::Result<serde_json::Value> {
    let parse = |text: Option<&str>| text.map(|text| serde_json::from_str(text).unwrap());
    terms_version(&TermsResult {
        terms_version_id: parse(current),
        latest_terms_version_id: parse(latest),
        ..TermsResult::default()
    })
}

#[test]
fn keeps_the_version_json_type() {
    for (current, latest, want) in [
        ("290462", None, "290462"),
        (r#""290462""#, None, r#""290462""#),
        ("290462", Some("290463"), "290463"),
    ] {
        assert_eq!(version(Some(current), latest).unwrap().to_string(), want);
    }
}

#[test]
fn rejects_invalid_versions() {
    assert!(version(None, None).is_err());
    for value in ["null", "0", "-1", "1.5", "{}", "true", r#""unknown""#] {
        assert!(version(Some(value), None).is_err(), "{value}");
    }
}

#[test]
fn rejects_untrusted_terms_endpoints() {
    for (endpoint, acceptance) in [
        ("https://example.com/account/terms/info", false),
        (
            "http://amp-account.itunes.apple.com/account/terms/info",
            false,
        ),
        (
            "https://user@amp-account.itunes.apple.com/account/terms/info",
            false,
        ),
        ("https://example.com/account/terms", true),
        ("https://buy.itunes.apple.com/account/delete", true),
        (
            "https://buy.itunes.apple.com.example.com/account/terms",
            true,
        ),
        ("https://evil-buy.itunes.apple.com/account/terms", true),
        ("https://buy.itunes.apple.com/account/%74erms", true),
    ] {
        assert!(
            terms_endpoint(endpoint, "45", acceptance).is_err(),
            "{endpoint}"
        );
    }
    let pod = terms_endpoint("https://buy.itunes.apple.com/account/terms", "45", true).unwrap();
    assert_eq!(
        pod.to_string(),
        "https://p45-buy.itunes.apple.com/account/terms"
    );
}

#[tokio::test]
async fn bags_keep_terms_endpoints_beside_authentication() {
    let cases = [
        (
            vec![
                (
                    "urlBag",
                    PValue::Dictionary(dict(&[("authenticateAccount", s("nested-auth"))])),
                ),
                ("getTermsByCountryAndTypeSrv", s("info")),
                ("terms-check", s("check")),
            ],
            ("nested-auth", "info", "check"),
        ),
        (
            vec![
                (
                    "urlBag",
                    PValue::Dictionary(dict(&[
                        ("getTermsByCountryAndTypeSrv", s("info")),
                        ("terms-check", s("check")),
                    ])),
                ),
                ("authenticateAccount", s("flat-auth")),
            ],
            ("flat-auth", "info", "check"),
        ),
        (
            vec![
                (
                    "urlBag",
                    PValue::Dictionary(dict(&[
                        ("getTermsByCountryAndTypeSrv", s("nested-info")),
                        ("terms-check", s("nested-check")),
                    ])),
                ),
                ("getTermsByCountryAndTypeSrv", s("flat-info")),
                ("terms-check", s("flat-check")),
            ],
            ("", "nested-info", "nested-check"),
        ),
    ];
    for (entries, (auth, info, check)) in cases {
        let world = World::new();
        world.fake.push(plist(200, &entries));
        let bag: UrlBag = world.client.url_bag(GUID).await.unwrap();
        assert_eq!(
            (
                bag.auth_endpoint.as_str(),
                bag.terms_info_endpoint.as_str(),
                bag.terms_check_endpoint.as_str()
            ),
            (auth, info, check)
        );
        let sent = &world.fake.sent()[0];
        assert_eq!(sent.operation, "bag-direct");
        assert_eq!(
            sent.request.url,
            "https://init.itunes.apple.com/bag.xml?ix=6&guid=001122334455"
        );
    }
    let _ = AtomicUsize::new(0).load(Ordering::Relaxed);
}
