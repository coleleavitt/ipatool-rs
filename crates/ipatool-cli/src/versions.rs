//! `list-versions` and `get-version-metadata` (Go `cmd/list_versions.go`,
//! `cmd/get_version_metadata.go`), with the fork's `--with-metadata` listing and its
//! `version-metadata.json` cache (`cmd/version_metadata_cache.go`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use clap::Args;
use futures_util::stream::{self, StreamExt as _};
use ipatool_appstore::{Account, App, LookupQuery, Platform};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::BoxError;
use crate::commerce::{AppResult, MISSING_APP, Store, parse_platform_filter};
use crate::output::Output;

#[derive(Args, Clone)]
pub struct ListVersionsArgs {
    /// ID of the target app (required).
    #[arg(short = 'i', long, default_value_t = 0, allow_hyphen_values = true, value_parser = crate::cobra::parse_int)]
    app_id: i64,
    /// The bundle identifier of the target app (overrides the app ID).
    #[arg(short, long, default_value = "")]
    bundle_identifier: String,
    /// Platform to list versions for: iphone (iOS), ipad (iPadOS), appletv (tvOS), watchos,
    /// visionos, or macos.
    #[arg(long, default_value = "")]
    platform: String,
    /// Also fetch the display version and release date of every version (recent versions
    /// come from the public catalog in one request; older versions are cached locally
    /// after the first fetch).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = crate::cobra::parse_bool)]
    with_metadata: bool,
    /// Number of version metadata requests to run at once (with --with-metadata).
    #[arg(long, default_value_t = 4, allow_hyphen_values = true, value_parser = crate::cobra::parse_int)]
    concurrency: i64,
    #[command(flatten)]
    _ignored: crate::cobra::IgnoredArgs,
}

#[derive(Args, Clone)]
pub struct GetVersionMetadataArgs {
    /// ID of the target app (required).
    #[arg(short = 'i', long, default_value_t = 0, allow_hyphen_values = true, value_parser = crate::cobra::parse_int)]
    app_id: i64,
    /// The bundle identifier of the target app (overrides the app ID).
    #[arg(short, long, default_value = "")]
    bundle_identifier: String,
    /// External version identifier of the target app (required).
    #[arg(long)]
    external_version_id: Option<String>,
    /// Platform to retrieve metadata for: iphone (iOS), ipad (iPadOS), appletv (tvOS),
    /// watchos, visionos, or macos.
    #[arg(long, default_value = "")]
    platform: String,
    #[command(flatten)]
    _ignored: crate::cobra::IgnoredArgs,
}

/// Go's retry of both commands (with fork fix aee638e): the account is read once; two
/// attempts, and after an expired password token the second logs in again with the
/// saved credentials and keeps the refreshed account.
async fn with_login<T>(
    store: &impl Store,
    mut attempt: impl AsyncFnMut(&Account) -> AppResult<T>,
) -> AppResult<T> {
    let mut account = store.account_info().await?;
    match attempt(&account).await {
        Err(error) if error.is_password_token_expired() => {
            account = store.login(&account).await?;
            attempt(&account).await
        }
        result => result,
    }
}

/// The app by ID, or the catalog's app for a bundle identifier.
async fn target_app(
    store: &impl Store,
    account: &Account,
    app_id: i64,
    bundle_id: &str,
    platform: Option<Platform>,
) -> AppResult<App> {
    if bundle_id.is_empty() {
        return Ok(App {
            id: app_id,
            ..App::default()
        });
    }
    store
        .lookup(
            account,
            &LookupQuery::BundleId(bundle_id.to_owned()),
            platform,
        )
        .await
}

/// Go's `ListVersionsCmd`.
pub(crate) async fn list_versions(
    args: &ListVersionsArgs,
    store: &impl Store,
    cache_path: Option<&Path>,
    out: &Output,
) -> Result<(), BoxError> {
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let concurrency = usize::try_from(args.concurrency)
        .ok()
        .filter(|&concurrency| concurrency >= 1)
        .ok_or("concurrency must be at least 1")?;
    let platform = parse_platform_filter(&args.platform)?;
    let fields = with_login(store, async |account: &Account| {
        let app = target_app(
            store,
            account,
            args.app_id,
            &args.bundle_identifier,
            platform,
        )
        .await?;
        let listed = store.list_versions(account, &app, platform).await?;
        let mut fields = serde_json::Map::new();
        fields.insert(
            "externalVersionIdentifiers".to_owned(),
            json!(listed.external_version_identifiers),
        );
        fields.insert("bundleID".to_owned(), json!(app.bundle_id));
        if args.with_metadata {
            let versions = list_version_metadata(
                store,
                account,
                &app,
                platform,
                &listed.external_version_identifiers,
                concurrency,
                cache_path,
                out,
            )
            .await?;
            fields.insert("versions".to_owned(), json!(versions));
        }
        fields.insert("success".to_owned(), json!(true));
        Ok(fields)
    })
    .await?;
    out.info(serde_json::Value::Object(fields));
    Ok(())
}

/// Go's `getVersionMetadataCmd`.
pub(crate) async fn get_version_metadata(
    args: &GetVersionMetadataArgs,
    store: &impl Store,
    out: &Output,
) -> Result<(), BoxError> {
    // Cobra checks required flags before the command runs.
    let Some(external_version_id) = &args.external_version_id else {
        return Err(r#"required flag(s) "external-version-id" not set"#.into());
    };
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let platform = parse_platform_filter(&args.platform)?;
    let metadata = with_login(store, async |account: &Account| {
        let app = target_app(
            store,
            account,
            args.app_id,
            &args.bundle_identifier,
            platform,
        )
        .await?;
        store
            .get_version_metadata(account, &app, external_version_id, platform)
            .await
    })
    .await?;
    // zerolog's `Time` field uses time.RFC3339, in whole seconds.
    let release_date = metadata
        .release_date
        .truncate_to_second()
        .format(&Rfc3339)
        .unwrap_or_default();
    out.info(json!({
        "externalVersionID": args.external_version_id,
        "displayVersion": metadata.display_version,
        "releaseDate": release_date,
        "success": true,
    }));
    Ok(())
}

/// Go's `versionMetadata`: one entry of `--with-metadata` and of the cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VersionMetadata {
    #[serde(rename = "externalVersionID")]
    pub external_version_id: String,
    #[serde(rename = "displayVersion")]
    pub display_version: String,
    /// Go's `time.Time` JSON: RFC 3339 with nanoseconds when present.
    #[serde(rename = "releaseDate", with = "time::serde::rfc3339")]
    pub release_date: OffsetDateTime,
    /// "catalog" (public App Store catalog, release date as shown on the store page) or
    /// "store" (per-version store request, release date from the package).
    pub source: String,
}

impl Default for VersionMetadata {
    /// Go's zero value: `0001-01-01T00:00:00Z` for the release date.
    fn default() -> Self {
        Self {
            external_version_id: String::new(),
            display_version: String::new(),
            release_date: time::macros::datetime!(0001-01-01 0:00 UTC),
            source: String::new(),
        }
    }
}

/// Go's `listVersionMetadata`: the recent versions from the public catalog in one
/// request, older versions seen before from the local cache, and only the rest one by
/// one from the store.
#[allow(
    clippy::too_many_arguments,
    reason = "Go's parameters plus the cache and logger"
)]
async fn list_version_metadata(
    store: &impl Store,
    account: &Account,
    app: &App,
    platform: Option<Platform>,
    version_ids: &[String],
    concurrency: usize,
    cache_path: Option<&Path>,
    out: &Output,
) -> AppResult<Vec<VersionMetadata>> {
    let mut known = HashMap::new();
    let mut cache = match load_version_metadata_cache(cache_path) {
        Ok(cache) => cache,
        Err(error) => {
            out.verbose(
                json!({ "error": error.to_string() }),
                "ignoring version metadata cache",
            );
            VersionMetadataCache::default()
        }
    };
    match store.catalog_version_history(account, app, platform).await {
        Ok(catalog) => known.extend(catalog.into_iter().map(|version| {
            (
                version.external_version_id.clone(),
                VersionMetadata {
                    external_version_id: version.external_version_id,
                    display_version: version.display_version,
                    release_date: version.release_date,
                    source: "catalog".to_owned(),
                },
            )
        })),
        Err(error) => out.verbose(
            json!({ "error": error.to_string() }),
            "catalog version history unavailable; fetching every version from the store",
        ),
    }
    let mut missing = Vec::new();
    for id in version_ids {
        if known.contains_key(id) {
            continue;
        }
        if let Some(version) = cache.get(app.id, id) {
            known.insert(id.clone(), version.clone());
            continue;
        }
        missing.push(id.clone());
    }

    let (fetched, fetch_error) =
        fetch_version_metadata(store, account, app, platform, &missing, concurrency).await;
    for version in fetched {
        if !version.display_version.is_empty() {
            known.insert(version.external_version_id.clone(), version);
        }
    }
    for version in known.values() {
        cache.put(app.id, version.clone());
    }
    if let Err(error) = cache.save(cache_path) {
        out.verbose(
            json!({ "error": error.to_string() }),
            "could not save version metadata cache",
        );
    }
    if let Some(error) = fetch_error {
        return Err(error);
    }
    Ok(version_ids
        .iter()
        .map(|id| known.get(id).cloned().unwrap_or_default())
        .collect())
}

/// Go's `fetchVersionMetadata`: at most `concurrency` requests at once; the first
/// failure stops the rest. Versions fetched before it are returned with it so the caller
/// can cache them.
async fn fetch_version_metadata(
    store: &impl Store,
    account: &Account,
    app: &App,
    platform: Option<Platform>,
    version_ids: &[String],
    concurrency: usize,
) -> (Vec<VersionMetadata>, Option<ipatool_appstore::Error>) {
    let mut results = vec![VersionMetadata::default(); version_ids.len()];
    let mut requests = stream::iter(version_ids.iter().enumerate())
        .map(|(index, id)| async move {
            let metadata = store
                .get_version_metadata(account, app, id, platform)
                .await
                .map_err(|error| error.context(format!("failed to get metadata for version {id}")));
            (index, id, metadata)
        })
        .buffer_unordered(concurrency);
    while let Some((index, id, metadata)) = requests.next().await {
        match metadata {
            Ok(metadata) => {
                results[index] = VersionMetadata {
                    external_version_id: id.clone(),
                    display_version: metadata.display_version,
                    release_date: metadata.release_date,
                    source: "store".to_owned(),
                };
            }
            // Dropping the stream cancels the requests still running, as Go's context does.
            Err(error) => return (results, Some(error)),
        }
    }
    (results, None)
}

/// `versionMetadataCacheFileName`.
pub(crate) const VERSION_METADATA_CACHE_FILE_NAME: &str = "version-metadata.json";

/// Go's `versionMetadataCache`: app ID -> external version ID -> metadata. An external
/// version ID always names the same release, so entries never expire. Sorted maps keep
/// the file byte-identical to Go's `json.Marshal`.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct VersionMetadataCache(BTreeMap<String, BTreeMap<String, VersionMetadata>>);

/// Go's `loadVersionMetadataCache`; no path disables the cache.
pub(crate) fn load_version_metadata_cache(
    path: Option<&Path>,
) -> Result<VersionMetadataCache, BoxError> {
    let Some(path) = path else {
        return Ok(VersionMetadataCache::default());
    };
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VersionMetadataCache::default());
        }
        Err(error) => return Err(format!("failed to read version metadata cache: {error}").into()),
    };
    // Go's json.Unmarshal of `null` leaves the map empty.
    let cache: Option<VersionMetadataCache> = serde_json::from_slice(&data)
        .map_err(|error| format!("failed to parse version metadata cache: {error}"))?;
    Ok(cache.unwrap_or_default())
}

impl VersionMetadataCache {
    pub(crate) fn get(&self, app_id: i64, version_id: &str) -> Option<&VersionMetadata> {
        self.0.get(&app_id.to_string())?.get(version_id)
    }

    pub(crate) fn put(&mut self, app_id: i64, version: VersionMetadata) {
        self.0
            .entry(app_id.to_string())
            .or_default()
            .insert(version.external_version_id.clone(), version);
    }

    /// Go's `save`: written to a `.tmp` file with mode 0600, then renamed.
    pub(crate) fn save(&self, path: Option<&Path>) -> Result<(), BoxError> {
        let Some(path) = path else {
            return Ok(());
        };
        let data = serde_json::to_vec(&self.0)
            .map_err(|error| format!("failed to encode version metadata cache: {error}"))?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        write_private(&tmp, &data)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|error| format!("failed to write version metadata cache: {error}"))?;
        Ok(())
    }
}

/// `os.WriteFile(path, data, 0600)`.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

#[cfg(test)]
mod tests;
