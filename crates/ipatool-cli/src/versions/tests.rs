//! Ports of `cmd/list_versions_test.go` and `cmd/get_version_metadata_test.go`, with the
//! fork's `--with-metadata` cases.

#![allow(clippy::unused_async_trait_impl, reason = "test fixtures")]

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use ipatool_appstore::{
    CatalogVersion, DownloadInput, DownloadOutput, Error, ListVersionsOutput, OwnedAppsInput,
    OwnedAppsOutput, Sinf,
};
use time::macros::datetime;

use super::*;
use crate::output::Format;

#[derive(Default)]
struct Fake {
    account_reads: RefCell<usize>,
    ids: Vec<String>,
    fail_id: Option<String>,
    expire_once: RefCell<bool>,
    catalog: Vec<CatalogVersion>,
    catalog_error: bool,
    lookups: RefCell<Vec<(String, Option<Platform>)>>,
    listed: RefCell<Vec<(i64, Option<Platform>)>>,
    metadata: RefCell<Vec<(String, i64, Option<Platform>)>>,
    logins: RefCell<usize>,
    tokens: RefCell<Vec<String>>,
    in_flight: RefCell<usize>,
    max_in_flight: RefCell<usize>,
}

impl Fake {
    fn with_ids(ids: &[&str]) -> Self {
        Self {
            ids: ids.iter().map(ToString::to_string).collect(),
            ..Self::default()
        }
    }

    fn fetched(&self) -> Vec<String> {
        self.metadata
            .borrow()
            .iter()
            .map(|(id, ..)| id.clone())
            .collect()
    }
}

impl Store for Fake {
    async fn account_info(&self) -> AppResult<Account> {
        *self.account_reads.borrow_mut() += 1;
        Ok(Account {
            password_token: "stale".to_owned(),
            ..Account::default()
        })
    }

    async fn login(&self, account: &Account) -> AppResult<Account> {
        assert_eq!(account.password_token, "stale");
        *self.logins.borrow_mut() += 1;
        Ok(Account {
            password_token: "fresh".to_owned(),
            ..Account::default()
        })
    }

    async fn lookup(
        &self,
        _: &Account,
        bundle_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<App> {
        self.lookups
            .borrow_mut()
            .push((bundle_id.to_owned(), platform));
        Ok(App {
            id: 42,
            bundle_id: bundle_id.to_owned(),
            ..App::default()
        })
    }

    async fn purchase(&self, _: &mut Account, _: &App, _: Option<Platform>) -> AppResult<()> {
        unreachable!("not a version command")
    }

    async fn download(&self, _: DownloadInput) -> AppResult<DownloadOutput> {
        unreachable!("not a version command")
    }

    async fn replicate_sinf(&self, _: Vec<Sinf>, _: PathBuf) -> AppResult<()> {
        unreachable!("not a version command")
    }

    async fn owned_apps(&self, _: OwnedAppsInput) -> AppResult<OwnedAppsOutput> {
        unreachable!("not a version command")
    }

    async fn list_versions(
        &self,
        account: &Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<ListVersionsOutput> {
        self.listed.borrow_mut().push((app.id, platform));
        self.tokens
            .borrow_mut()
            .push(account.password_token.clone());
        if self.expire_once.replace(false) {
            return Err(Error::PasswordTokenExpired);
        }
        Ok(ListVersionsOutput {
            external_version_identifiers: if self.ids.is_empty() {
                vec!["123".to_owned()]
            } else {
                self.ids.clone()
            },
            latest_external_version_id: String::new(),
        })
    }

    async fn get_version_metadata(
        &self,
        _: &Account,
        app: &App,
        version_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<ipatool_appstore::VersionMetadata> {
        self.metadata
            .borrow_mut()
            .push((version_id.to_owned(), app.id, platform));
        let in_flight = {
            let mut in_flight = self.in_flight.borrow_mut();
            *in_flight += 1;
            *in_flight
        };
        let max = (*self.max_in_flight.borrow()).max(in_flight);
        *self.max_in_flight.borrow_mut() = max;
        tokio::time::sleep(Duration::from_millis(20)).await;
        *self.in_flight.borrow_mut() -= 1;
        if self.fail_id.as_deref() == Some(version_id) {
            return Err(Error::Invalid("boom".to_owned()));
        }
        Ok(ipatool_appstore::VersionMetadata {
            display_version: format!("v{version_id}"),
            release_date: datetime!(2026-01-02 3:04:05 UTC),
        })
    }

    async fn catalog_version_history(
        &self,
        _: &Account,
        _: &App,
        _: Option<Platform>,
    ) -> AppResult<Vec<CatalogVersion>> {
        if self.catalog_error {
            return Err(Error::Invalid("catalog down".to_owned()));
        }
        Ok(self.catalog.clone())
    }
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    ListVersions(ListVersionsArgs),
    GetVersionMetadata(GetVersionMetadataArgs),
}

fn parse(args: &[&str]) -> Result<Command, clap::Error> {
    Cli::try_parse_from(std::iter::once("ipatool").chain(args.iter().copied()))
        .map(|cli| cli.command)
}

fn out() -> Output {
    Output::new(Format::Json, false)
}

async fn execute(fake: &Fake, args: &[&str], cache: Option<&Path>) -> Result<(), BoxError> {
    match parse(args)? {
        Command::ListVersions(args) => list_versions(&args, fake, cache, &out()).await,
        Command::GetVersionMetadata(args) => get_version_metadata(&args, fake, &out()).await,
    }
}

const PLATFORM_CASES: [(&[&str], Option<Platform>, bool); 4] = [
    (
        &["-b", "com.example.mac", "--platform", "macos"],
        Some(Platform::MacOs),
        true,
    ),
    (
        &["-i", "42", "--platform", "mac"],
        Some(Platform::MacOs),
        false,
    ),
    (&["-i", "42"], None, false),
    (
        &["-i", "42", "--platform", "ios"],
        Some(Platform::IPhone),
        false,
    ),
];

#[tokio::test]
async fn list_versions_passes_the_selected_platform_to_lookup_and_version_listing() {
    for (args, expected, lookup) in PLATFORM_CASES {
        let fake = Fake::default();
        let args = [&["list-versions"], args].concat();
        execute(&fake, &args, None).await.unwrap();
        assert_eq!(*fake.listed.borrow(), [(42, expected)], "{args:?}");
        assert_eq!(fake.lookups.borrow().len(), usize::from(lookup));
        if lookup {
            assert_eq!(fake.lookups.borrow()[0].1, expected);
        }
    }
}

#[tokio::test]
async fn get_version_metadata_passes_the_platform_and_exact_version() {
    for (args, expected, lookup) in &PLATFORM_CASES[..3] {
        let fake = Fake::default();
        let args = [
            &["get-version-metadata"],
            *args,
            &["--external-version-id", "876660716"],
        ]
        .concat();
        execute(&fake, &args, None).await.unwrap();
        assert_eq!(
            *fake.metadata.borrow(),
            [("876660716".to_owned(), 42, *expected)]
        );
        assert_eq!(fake.lookups.borrow().len(), usize::from(*lookup));
        if *lookup {
            assert_eq!(fake.lookups.borrow()[0].1, *expected);
        }
    }
}

#[test]
fn rejects_unknown_as_a_platform() {
    for args in [
        &["list-versions", "-i", "42", "--platform", "unknown"][..],
        &[
            "get-version-metadata",
            "-i",
            "42",
            "--external-version-id",
            "123",
            "--platform",
            "unknown",
        ],
    ] {
        let error = parse(args).err().unwrap().to_string();
        assert!(error.contains(r#"invalid platform "unknown""#), "{error}");
    }
    let error = parse(&["get-version-metadata", "-i", "42"]).err().unwrap();
    assert!(error.to_string().contains("--external-version-id"));
}

#[tokio::test]
async fn requires_an_app_and_a_positive_concurrency() {
    let fake = Fake::default();
    let error = execute(&fake, &["list-versions"], None).await.unwrap_err();
    assert_eq!(error.to_string(), MISSING_APP);
    let error = execute(
        &fake,
        &["get-version-metadata", "--external-version-id", "1"],
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), MISSING_APP);
    for concurrency in ["0", "-1"] {
        let error = execute(
            &fake,
            &["list-versions", "-i", "42", "--concurrency", concurrency],
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "concurrency must be at least 1");
    }
    assert!(fake.listed.borrow().is_empty());
}

#[tokio::test]
async fn reads_the_account_once_and_retries_with_the_refreshed_one() {
    let fake = Fake::default();
    *fake.expire_once.borrow_mut() = true;
    execute(&fake, &["list-versions", "-i", "42"], None)
        .await
        .unwrap();
    assert_eq!(*fake.account_reads.borrow(), 1);
    assert_eq!(*fake.logins.borrow(), 1);
    assert_eq!(*fake.tokens.borrow(), ["stale", "fresh"]);
}

fn ids(fake: &Fake) -> Vec<String> {
    fake.ids.clone()
}

fn app() -> App {
    App {
        id: 42,
        ..App::default()
    }
}

async fn metadata(
    fake: &Fake,
    concurrency: usize,
    cache: Option<&Path>,
) -> AppResult<Vec<VersionMetadata>> {
    list_version_metadata(
        fake,
        &Account::default(),
        &app(),
        None,
        &ids(fake),
        concurrency,
        cache,
        &out(),
    )
    .await
}

#[tokio::test]
async fn fetches_every_versions_metadata_in_order_within_the_concurrency_limit() {
    let fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    let (versions, error) =
        fetch_version_metadata(&fake, &Account::default(), &app(), None, &ids(&fake), 2).await;
    assert!(error.is_none());
    assert_eq!(versions.len(), 5);
    for (version, id) in versions.iter().zip(&fake.ids) {
        assert_eq!(&version.external_version_id, id);
        assert_eq!(version.display_version, format!("v{id}"));
    }
    assert_eq!(*fake.max_in_flight.borrow(), 2);
}

#[tokio::test]
async fn returns_the_first_failure() {
    let mut fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    fake.fail_id = Some("3".to_owned());
    let error = execute(
        &fake,
        &[
            "list-versions",
            "-i",
            "42",
            "--with-metadata",
            "--concurrency",
            "1",
        ],
        None,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed to get metadata for version 3"),
        "{error}"
    );
    assert_eq!(fake.fetched(), ["1", "2", "3"]);
}

#[tokio::test]
async fn takes_catalog_versions_from_one_request_and_fetches_only_the_rest() {
    let mut fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    let zero = VersionMetadata::default().release_date;
    fake.catalog = vec![
        CatalogVersion {
            external_version_id: "5".to_owned(),
            display_version: "5.0".to_owned(),
            release_date: zero,
        },
        CatalogVersion {
            external_version_id: "4".to_owned(),
            display_version: "4.0".to_owned(),
            release_date: zero,
        },
    ];
    let versions = metadata(&fake, 2, None).await.unwrap();
    let mut fetched = fake.fetched();
    fetched.sort();
    assert_eq!(fetched, ["1", "2", "3"]);
    assert_eq!(
        versions[3],
        VersionMetadata {
            external_version_id: "4".to_owned(),
            display_version: "4.0".to_owned(),
            release_date: zero,
            source: "catalog".to_owned(),
        }
    );
    assert_eq!(versions[0].display_version, "v1");
    assert_eq!(versions[0].source, "store");
}

#[tokio::test]
async fn fetches_every_version_from_the_store_when_the_catalog_fails() {
    let mut fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    fake.catalog_error = true;
    let versions = metadata(&fake, 2, None).await.unwrap();
    assert_eq!(fake.fetched().len(), 5);
    assert_eq!(versions.len(), 5);
}

#[tokio::test]
async fn reuses_cached_versions_on_the_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join(VERSION_METADATA_CACHE_FILE_NAME);
    let mut fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    let first = metadata(&fake, 2, Some(&cache)).await.unwrap();
    assert_eq!(fake.fetched().len(), 5);

    fake.metadata.borrow_mut().clear();
    fake.ids.push("6".to_owned());
    let second = metadata(&fake, 2, Some(&cache)).await.unwrap();
    assert_eq!(fake.fetched(), ["6"]);
    assert_eq!(second[..5], first[..]);
}

#[tokio::test]
async fn caches_the_versions_fetched_before_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join(VERSION_METADATA_CACHE_FILE_NAME);
    let mut fake = Fake::with_ids(&["1", "2", "3", "4", "5"]);
    fake.fail_id = Some("3".to_owned());
    assert!(metadata(&fake, 1, Some(&cache)).await.is_err());

    fake.fail_id = None;
    fake.metadata.borrow_mut().clear();
    metadata(&fake, 1, Some(&cache)).await.unwrap();
    assert!(!fake.fetched().contains(&"1".to_owned()));
    assert!(!fake.fetched().contains(&"2".to_owned()));
}

/// The file Go's `json.Marshal` writes for the same cache: sorted keys, RFC 3339 with
/// trimmed nanoseconds, no trailing newline.
const GO_CACHE: &str = r#"{"42":{"10":{"externalVersionID":"10","displayVersion":"1.0","releaseDate":"2026-11-18T19:21:02.5Z","source":"store"},"9":{"externalVersionID":"9","displayVersion":"0.9","releaseDate":"0001-01-01T00:00:00Z","source":"catalog"}},"7":{}}"#;

#[test]
fn reads_and_writes_the_go_cache_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(VERSION_METADATA_CACHE_FILE_NAME);
    std::fs::write(&path, GO_CACHE).unwrap();
    let cache = load_version_metadata_cache(Some(&path)).unwrap();
    let version = cache.get(42, "10").unwrap();
    assert_eq!(version.display_version, "1.0");
    assert_eq!(version.release_date, datetime!(2026-11-18 19:21:02.5 UTC));
    assert!(cache.get(7, "10").is_none());
    cache.save(Some(&path)).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), GO_CACHE);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn reports_unreadable_caches_and_ignores_missing_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(VERSION_METADATA_CACHE_FILE_NAME);
    assert!(load_version_metadata_cache(Some(&path)).is_ok());
    assert!(load_version_metadata_cache(None).is_ok());
    std::fs::write(&path, "null").unwrap();
    assert!(load_version_metadata_cache(Some(&path)).is_ok());
    std::fs::write(&path, "{").unwrap();
    let error = load_version_metadata_cache(Some(&path)).err().unwrap();
    assert!(
        error
            .to_string()
            .starts_with("failed to parse version metadata cache: ")
    );
}
