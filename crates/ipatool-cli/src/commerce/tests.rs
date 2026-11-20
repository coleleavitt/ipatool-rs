//! Ports of `cmd/download_test.go` and `cmd/purchase_test.go` with a scripted store.

#![allow(
    clippy::unused_async_trait_impl,
    clippy::assert_is_empty,
    clippy::items_after_statements,
    reason = "test fixtures"
)]

use std::cell::RefCell;
use std::collections::VecDeque;

use ipatool_appstore::Error;

use super::*;
use crate::output::Format;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    AccountInfo,
    Login(String),
    Lookup(String, Option<Platform>, String),
    Purchase(i64, Option<Platform>, String),
    Download(i64, String, Option<Platform>, String, String),
    Replicate(usize, PathBuf),
}

#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<Call>>,
    accounts: RefCell<VecDeque<AppResult<Account>>>,
    logins: RefCell<VecDeque<Account>>,
    lookups: RefCell<VecDeque<AppResult<App>>>,
    purchases: RefCell<VecDeque<AppResult<()>>>,
    downloads: RefCell<VecDeque<AppResult<DownloadOutput>>>,
    replicate_error: RefCell<Option<Error>>,
}

impl Fake {
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    fn account(&self, token: &str, store_front: &str) {
        self.accounts.borrow_mut().push_back(Ok(Account {
            email: "user@example.com".to_owned(),
            password: "secret".to_owned(),
            password_token: token.to_owned(),
            store_front: store_front.to_owned(),
            ..Account::default()
        }));
    }

    fn output(&self, sinfs: usize) {
        self.downloads.borrow_mut().push_back(Ok(DownloadOutput {
            destination_path: PathBuf::from("/tmp/app.ipa"),
            sinfs: vec![Sinf::default(); sinfs],
        }));
    }
}

impl Store for Fake {
    async fn account_info(&self) -> AppResult<Account> {
        self.calls.borrow_mut().push(Call::AccountInfo);
        self.accounts
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(Error::Invalid("no account".to_owned())))
    }

    async fn login(&self, account: &Account) -> AppResult<Account> {
        self.calls
            .borrow_mut()
            .push(Call::Login(account.password_token.clone()));
        Ok(self
            .logins
            .borrow_mut()
            .pop_front()
            .expect("login scripted"))
    }

    async fn lookup(
        &self,
        account: &Account,
        bundle_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<App> {
        self.calls.borrow_mut().push(Call::Lookup(
            bundle_id.to_owned(),
            platform,
            account.password_token.clone(),
        ));
        self.lookups
            .borrow_mut()
            .pop_front()
            .expect("lookup scripted")
    }

    async fn purchase(
        &self,
        account: &mut Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<()> {
        self.calls.borrow_mut().push(Call::Purchase(
            app.id,
            platform,
            account.password_token.clone(),
        ));
        self.purchases
            .borrow_mut()
            .pop_front()
            .expect("purchase scripted")
    }

    async fn download(&self, input: DownloadInput) -> AppResult<DownloadOutput> {
        self.calls.borrow_mut().push(Call::Download(
            input.app.id,
            input.app.bundle_id.clone(),
            input.platform,
            input.account.password_token.clone(),
            input.account.store_front.clone(),
        ));
        assert!(input.progress.is_none());
        self.downloads
            .borrow_mut()
            .pop_front()
            .expect("download scripted")
    }

    async fn replicate_sinf(&self, sinfs: Vec<Sinf>, package: PathBuf) -> AppResult<()> {
        self.calls
            .borrow_mut()
            .push(Call::Replicate(sinfs.len(), package));
        self.replicate_error.borrow_mut().take().map_or(Ok(()), Err)
    }
}

fn out() -> Output {
    Output::new(Format::Json, false)
}

fn app(id: i64, bundle: &str) -> App {
    App {
        id,
        bundle_id: bundle.to_owned(),
        ..App::default()
    }
}

fn download_args(
    app_id: i64,
    bundle: &str,
    platform: Option<Platform>,
    purchase: bool,
) -> DownloadArgs {
    DownloadArgs {
        app_id,
        bundle_identifier: bundle.to_owned(),
        output: String::new(),
        external_version_id: String::new(),
        platform,
        purchase,
    }
}

async fn run_download(fake: &Fake, args: &DownloadArgs) -> Result<(), BoxError> {
    download(args, fake, false, &out()).await
}

#[tokio::test]
async fn uses_the_app_id_when_the_bundle_is_not_in_the_catalog() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.lookups.borrow_mut().push_back(Err(Error::AppNotFound));
    fake.output(1);
    run_download(
        &fake,
        &download_args(42, "com.example.delisted", None, false),
    )
    .await
    .unwrap();
    assert!(fake.calls().contains(&Call::Download(
        42,
        "com.example.delisted".to_owned(),
        None,
        "t".to_owned(),
        "143441".to_owned()
    )));
}

#[tokio::test]
async fn lets_a_found_bundle_take_precedence() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.lookups
        .borrow_mut()
        .push_back(Ok(app(7, "com.example.app")));
    fake.output(1);
    run_download(&fake, &download_args(42, "com.example.app", None, false))
        .await
        .unwrap();
    assert!(fake.calls().contains(&Call::Download(
        7,
        "com.example.app".to_owned(),
        None,
        "t".to_owned(),
        "143441".to_owned()
    )));

    let fake = Fake::default();
    fake.account("t", "143441");
    fake.lookups
        .borrow_mut()
        .push_back(Ok(app(7, "com.example.app")));
    fake.output(1);
    run_download(&fake, &download_args(0, "com.example.app", None, false))
        .await
        .unwrap();
    assert!(matches!(fake.calls()[2], Call::Download(7, ..)));
}

#[tokio::test]
async fn does_not_look_up_an_explicit_app_id() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.output(1);
    let mut args = download_args(42, "", None, false);
    args.external_version_id = "123".to_owned();
    run_download(&fake, &args).await.unwrap();
    assert!(
        !fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Lookup(..)))
    );
}

#[tokio::test]
async fn keeps_lookup_errors() {
    for (app_id, error) in [
        (0, Error::AppNotFound),
        (42, Error::Invalid("request failed".to_owned())),
    ] {
        let text = error.to_string();
        let fake = Fake::default();
        fake.account("t", "143441");
        fake.lookups.borrow_mut().push_back(Err(error));
        let result = run_download(
            &fake,
            &download_args(app_id, "com.example.app", None, false),
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), text);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Download(..)))
        );
    }
}

#[tokio::test]
async fn requires_an_app_before_reading_the_account() {
    let fake = Fake::default();
    let error = run_download(&fake, &download_args(0, "", None, false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), MISSING_APP);
    let purchase_args = PurchaseArgs {
        app_id: 0,
        bundle_identifier: String::new(),
        platform: None,
    };
    let error = purchase(&purchase_args, &fake, &out()).await.unwrap_err();
    assert_eq!(error.to_string(), MISSING_APP);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn replicates_sinfs_except_for_macos_packages_without_them() {
    for (platform, sinfs, replicated) in [
        (Some(Platform::MacOs), 0, false),
        (Some(Platform::MacOs), 1, true),
        (None, 1, true),
        (None, 0, true),
    ] {
        let fake = Fake::default();
        fake.account("t", "143441");
        fake.output(sinfs);
        run_download(&fake, &download_args(42, "", platform, false))
            .await
            .unwrap();
        let did = fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Replicate(..)));
        assert_eq!(did, replicated, "{platform:?} {sinfs}");
    }
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.output(1);
    *fake.replicate_error.borrow_mut() = Some(Error::Invalid("replicate failed".to_owned()));
    let error = run_download(&fake, &download_args(42, "", None, false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "replicate failed");
}

#[tokio::test]
async fn keeps_the_platform_when_the_purchase_retry_downloads() {
    for platform in [Some(Platform::MacOs), Some(Platform::WatchOs)] {
        let fake = Fake::default();
        fake.account("t", "143441");
        fake.account("t", "143441");
        fake.downloads
            .borrow_mut()
            .push_back(Err(Error::LicenseRequired));
        fake.purchases.borrow_mut().push_back(Ok(()));
        fake.output(1);
        run_download(&fake, &download_args(42, "", platform, true))
            .await
            .unwrap();
        let calls = fake.calls();
        assert!(calls.contains(&Call::Purchase(42, platform, "t".to_owned())));
        let downloads: Vec<_> = calls
            .iter()
            .filter(|call| matches!(call, Call::Download(..)))
            .collect();
        assert_eq!(downloads.len(), 2);
        assert!(
            downloads
                .iter()
                .all(|call| matches!(call, Call::Download(_, _, p, ..) if *p == platform))
        );
    }
}

#[tokio::test]
async fn does_not_purchase_without_the_flag() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.downloads
        .borrow_mut()
        .push_back(Err(Error::LicenseRequired));
    let error = run_download(&fake, &download_args(42, "", None, false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "license is required");
    assert!(
        !fake
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Purchase(..)))
    );
}

#[tokio::test]
async fn keeps_the_pending_purchase_after_refreshing_authentication() {
    let fake = Fake::default();
    fake.account("old", "143441");
    fake.account("new", "143441");
    fake.logins.borrow_mut().push_back(Account {
        password_token: "new".to_owned(),
        store_front: "143441".to_owned(),
        ..Account::default()
    });
    fake.downloads
        .borrow_mut()
        .push_back(Err(Error::PasswordTokenExpired));
    fake.downloads
        .borrow_mut()
        .push_back(Err(Error::LicenseRequired));
    fake.purchases.borrow_mut().push_back(Ok(()));
    fake.output(1);
    run_download(&fake, &download_args(42, "", None, true))
        .await
        .unwrap();
    let calls = fake.calls();
    assert_eq!(
        calls,
        [
            Call::AccountInfo,
            Call::Download(
                42,
                String::new(),
                None,
                "old".to_owned(),
                "143441".to_owned()
            ),
            Call::Login("old".to_owned()),
            Call::Download(
                42,
                String::new(),
                None,
                "new".to_owned(),
                "143441".to_owned()
            ),
            Call::Purchase(42, None, "new".to_owned()),
            Call::AccountInfo,
            Call::Download(
                42,
                String::new(),
                None,
                "new".to_owned(),
                "143441".to_owned()
            ),
            Call::Replicate(1, PathBuf::from("/tmp/app.ipa")),
        ]
    );
}

#[tokio::test]
async fn logs_in_again_when_the_purchase_finds_an_expired_token() {
    let fake = Fake::default();
    fake.account("old", "143441");
    fake.account("new", "143441");
    fake.logins.borrow_mut().push_back(Account {
        password_token: "new".to_owned(),
        store_front: "143441".to_owned(),
        ..Account::default()
    });
    fake.downloads
        .borrow_mut()
        .push_back(Err(Error::LicenseRequired));
    fake.purchases
        .borrow_mut()
        .push_back(Err(Error::PasswordTokenExpired));
    fake.purchases.borrow_mut().push_back(Ok(()));
    fake.output(1);
    run_download(&fake, &download_args(42, "", None, true))
        .await
        .unwrap();
    let download = |token: &str| {
        Call::Download(
            42,
            String::new(),
            None,
            token.to_owned(),
            "143441".to_owned(),
        )
    };
    assert_eq!(
        fake.calls(),
        [
            Call::AccountInfo,
            download("old"),
            Call::Purchase(42, None, "old".to_owned()),
            Call::Login("old".to_owned()),
            Call::Purchase(42, None, "new".to_owned()),
            Call::AccountInfo,
            download("new"),
            Call::Replicate(1, PathBuf::from("/tmp/app.ipa")),
        ]
    );
}

#[tokio::test]
async fn stops_after_three_attempts() {
    let fake = Fake::default();
    fake.account("t", "143441");
    for _ in 0..3 {
        fake.logins.borrow_mut().push_back(Account::default());
        fake.downloads
            .borrow_mut()
            .push_back(Err(Error::PasswordTokenExpired));
    }
    let error = run_download(&fake, &download_args(42, "", None, false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "password token is expired");
    let downloads = fake
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Download(..)))
        .count();
    assert_eq!(downloads, 3);
}

#[tokio::test]
async fn downloads_with_the_storefront_saved_by_the_purchase() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.account("t", "143444");
    fake.downloads
        .borrow_mut()
        .push_back(Err(Error::LicenseRequired));
    fake.purchases
        .borrow_mut()
        .push_back(Err(Error::LicenseAlreadyExists));
    fake.output(1);
    run_download(&fake, &download_args(42, "", None, true))
        .await
        .unwrap();
    assert!(fake.calls().contains(&Call::Download(
        42,
        String::new(),
        None,
        "t".to_owned(),
        "143444".to_owned()
    )));
}

#[tokio::test]
async fn returns_an_account_read_error_before_downloading() {
    let fake = Fake::default();
    let error = run_download(&fake, &download_args(42, "", None, false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "no account");
    assert_eq!(fake.calls(), [Call::AccountInfo]);
}

fn purchase_args(app_id: i64, bundle: &str, platform: Option<Platform>) -> PurchaseArgs {
    PurchaseArgs {
        app_id,
        bundle_identifier: bundle.to_owned(),
        platform,
    }
}

#[tokio::test]
async fn purchases_by_app_id_or_bundle() {
    let fake = Fake::default();
    fake.account("t", "143441");
    fake.purchases
        .borrow_mut()
        .push_back(Err(Error::LicenseAlreadyExists));
    purchase(&purchase_args(42, "", None), &fake, &out())
        .await
        .unwrap();
    assert_eq!(
        fake.calls(),
        [Call::AccountInfo, Call::Purchase(42, None, "t".to_owned())]
    );

    let fake = Fake::default();
    fake.account("t", "143441");
    fake.lookups
        .borrow_mut()
        .push_back(Ok(app(7, "com.example.app")));
    fake.purchases.borrow_mut().push_back(Ok(()));
    purchase(
        &purchase_args(42, "com.example.app", Some(Platform::MacOs)),
        &fake,
        &out(),
    )
    .await
    .unwrap();
    assert_eq!(
        fake.calls(),
        [
            Call::AccountInfo,
            Call::Lookup(
                "com.example.app".to_owned(),
                Some(Platform::MacOs),
                "t".to_owned()
            ),
            Call::Purchase(7, Some(Platform::MacOs), "t".to_owned()),
        ]
    );
}

#[tokio::test]
async fn purchase_logs_in_again_once_after_an_expired_token() {
    let fake = Fake::default();
    fake.account("old", "143441");
    fake.account("old", "143441");
    fake.logins.borrow_mut().push_back(Account {
        password_token: "new".to_owned(),
        ..Account::default()
    });
    fake.purchases
        .borrow_mut()
        .push_back(Err(Error::PasswordTokenExpired));
    fake.purchases
        .borrow_mut()
        .push_back(Err(Error::PasswordTokenExpired));
    let error = purchase(&purchase_args(42, "", None), &fake, &out())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "password token is expired");
    assert_eq!(
        fake.calls(),
        [
            Call::AccountInfo,
            Call::Purchase(42, None, "old".to_owned()),
            Call::AccountInfo,
            Call::Login("old".to_owned()),
            Call::Purchase(42, None, "new".to_owned()),
        ]
    );
}

#[tokio::test]
async fn purchase_logs_in_again_when_the_lookup_finds_an_expired_token() {
    let fake = Fake::default();
    fake.account("old", "143441");
    fake.account("old", "143441");
    fake.logins.borrow_mut().push_back(Account {
        password_token: "new".to_owned(),
        ..Account::default()
    });
    fake.lookups
        .borrow_mut()
        .push_back(Err(Error::PasswordTokenExpired));
    fake.lookups
        .borrow_mut()
        .push_back(Ok(app(7, "com.example.app")));
    fake.purchases.borrow_mut().push_back(Ok(()));
    purchase(&purchase_args(0, "com.example.app", None), &fake, &out())
        .await
        .unwrap();
    let lookup = |token: &str| Call::Lookup("com.example.app".to_owned(), None, token.to_owned());
    assert_eq!(
        fake.calls(),
        [
            Call::AccountInfo,
            lookup("old"),
            Call::AccountInfo,
            Call::Login("old".to_owned()),
            lookup("new"),
            Call::Purchase(7, None, "new".to_owned()),
        ]
    );
}

#[test]
fn exposes_the_go_flags() {
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Wrapper {
        #[command(flatten)]
        download: DownloadArgs,
    }
    let parsed = Wrapper::try_parse_from([
        "x",
        "-i",
        "42",
        "-b",
        "com.example.app",
        "-o",
        "/tmp/out",
        "--external-version-id",
        "9",
        "--platform",
        "macos",
        "--purchase",
    ])
    .unwrap();
    assert_eq!(parsed.download.app_id, 42);
    assert_eq!(parsed.download.bundle_identifier, "com.example.app");
    assert_eq!(parsed.download.output, "/tmp/out");
    assert_eq!(parsed.download.external_version_id, "9");
    assert_eq!(parsed.download.platform, Some(Platform::MacOs));
    assert!(parsed.download.purchase);
    let help = Wrapper::command().render_long_help().to_string();
    assert!(help.contains("macos"));

    #[derive(Parser)]
    struct PurchaseWrapper {
        #[command(flatten)]
        purchase: PurchaseArgs,
    }
    for flag in ["--app-id", "-i"] {
        let parsed = PurchaseWrapper::try_parse_from(["x", flag, "42"]).unwrap();
        assert_eq!(parsed.purchase.app_id, 42);
    }
}
