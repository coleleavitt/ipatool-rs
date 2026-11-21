//! Port of Go `cmd/mcp_tools.go`: the tool inputs, results and handlers.

use std::cell::Cell;

use ipatool_appstore::{
    Account, App, DEFAULT_OWNED_APPS_LIMIT, DownloadInput, Error, LookupQuery,
    MAX_OWNED_APPS_LIMIT, OwnedAppsInput, Platform,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::app::{AppParameters, McpApp, app_results, complete_app, platform_name};
use crate::commerce::{AppResult, Store, parse_platform_filter, replicate_download_sinf};

const PLATFORM_ANY: &str =
    "iphone (ios), ipad (ipados), appletv (tvos), watchos, visionos, or macos";

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct SearchInput {
    #[schemars(description = "Search term")]
    pub(crate) term: String,
    #[serde(default)]
    #[schemars(description = "Maximum results; defaults to 5. visionOS supports up to 12.")]
    pub(crate) limit: Option<i64>,
    #[serde(default)]
    #[schemars(description = format!("{PLATFORM_ANY}; defaults to iOS and iPadOS"))]
    pub(crate) platform: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct SearchOutput {
    term: String,
    limit: i64,
    platform: &'static str,
    count: usize,
    #[schemars(with = "Option<Vec<McpApp>>")]
    apps: Vec<McpApp>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct AppInput {
    #[serde(default)]
    #[schemars(description = "Numeric app ID; required unless bundle_identifier is provided")]
    pub(crate) app_id: i64,
    #[serde(default, rename = "bundle_identifier")]
    #[schemars(description = "Bundle identifier; overrides app_id when found")]
    pub(crate) bundle_id: String,
    #[serde(default)]
    #[schemars(description = format!("{PLATFORM_ANY}; uses the CLI default when omitted"))]
    pub(crate) platform: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct DownloadAppInput {
    #[serde(flatten)]
    pub(crate) app: AppInput,
    #[serde(default)]
    #[schemars(
        description = "Destination file or directory; defaults to the server's working directory"
    )]
    pub(crate) output: String,
    #[serde(default)]
    #[schemars(description = "External version identifier; defaults to the latest version")]
    pub(crate) external_version_id: String,
    #[serde(default)]
    #[schemars(description = "Acquire a free license if needed; defaults to false")]
    pub(crate) purchase: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct DownloadAppOutput {
    #[serde(flatten)]
    parameters: AppParameters,
    app: McpApp,
    output: String,
    external_version_id: String,
    purchase: bool,
    purchased: bool,
    success: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct VersionsOutput {
    #[serde(flatten)]
    parameters: AppParameters,
    app: McpApp,
    #[serde(rename = "externalVersionIdentifiers")]
    #[schemars(with = "Option<Vec<String>>")]
    external_version_identifiers: Vec<String>,
    #[serde(rename = "latestExternalVersionID")]
    latest_external_version_id: String,
    #[serde(rename = "bundleID")]
    bundle_id: String,
    success: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct VersionMetadataInput {
    #[serde(flatten)]
    pub(crate) app: AppInput,
    #[schemars(description = "External version identifier of the target app (required)")]
    pub(crate) external_version_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct VersionMetadataOutput {
    #[serde(flatten)]
    parameters: AppParameters,
    app: McpApp,
    external_version_id: String,
    #[serde(rename = "displayVersion")]
    #[schemars(description = "Display version of the requested external version identifier")]
    display_version: String,
    // Go's `time.Time` JSON form.
    #[serde(rename = "releaseDate", with = "time::serde::rfc3339")]
    #[schemars(with = "String")]
    release_date: OffsetDateTime,
    success: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct PurchasesInput {
    #[serde(default)]
    #[schemars(description = "Page number starting at 1; defaults to 1")]
    pub(crate) page: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Apps per page from 1 to 100; defaults to 10")]
    pub(crate) max_results: Option<i64>,
    #[serde(default)]
    #[schemars(
        description = "iphone (ios), ipad (ipados), appletv (tvos), visionos, or macos; defaults to all platforms"
    )]
    pub(crate) platform: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct PurchasesOutput {
    count: usize,
    #[serde(rename = "totalCount")]
    total_count: usize,
    page: i64,
    max_results: i64,
    platform: &'static str,
    #[schemars(with = "Option<Vec<McpApp>>")]
    apps: Vec<McpApp>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct PurchaseOutput {
    #[serde(flatten)]
    parameters: AppParameters,
    app: McpApp,
    #[serde(rename = "alreadyOwned")]
    already_owned: bool,
    success: bool,
}

/// Go's `withMCPAccount`: an expired token is refreshed once with the saved
/// credentials, as the CLI does. Account details never reach a tool result.
async fn with_mcp_account<T>(
    store: &impl Store,
    mut operation: impl AsyncFnMut(&Account) -> AppResult<T>,
) -> Result<T, String> {
    let account = store.account_info().await.map_err(|error| {
        format!("account unavailable; run 'ipatool auth login' before using MCP: {error}")
    })?;
    match operation(&account).await {
        Err(error) if error.is_password_token_expired() => {}
        result => return result.map_err(|error| error.to_string()),
    }
    let account = store.login(&account).await.map_err(|error| {
        format!("session refresh failed; run 'ipatool auth login' outside MCP: {error}")
    })?;
    operation(&account)
        .await
        .map_err(|error| format!("operation failed after session refresh: {error}"))
}

/// Go's `search`.
pub(crate) async fn search(store: &impl Store, input: SearchInput) -> Result<SearchOutput, String> {
    if input.term.trim().is_empty() {
        return Err("search term must not be empty".to_owned());
    }
    let limit = input.limit.unwrap_or(5);
    if limit < 1 {
        return Err("limit must be greater than 0".to_owned());
    }
    let platform = parse_platform_filter(&input.platform).map_err(|error| error.to_string())?;
    with_mcp_account(store, async |account: &Account| {
        let apps = store.search(account, &input.term, limit, platform).await?;
        Ok(SearchOutput {
            term: input.term.clone(),
            limit,
            platform: platform_name(platform),
            count: apps.len(),
            apps: app_results(store, account, apps, platform).await,
        })
    })
    .await
}

impl AppInput {
    /// Go's `mcpAppInput.validate`.
    fn validate(&self) -> Result<Option<Platform>, String> {
        if self.app_id < 0 || (self.app_id == 0 && self.bundle_id.trim().is_empty()) {
            return Err("a positive app ID or a bundle identifier must be specified".to_owned());
        }
        parse_platform_filter(&self.platform).map_err(|error| error.to_string())
    }
}

/// Go's `resolveApp`: the catalog's app for a bundle identifier (delisted apps fall
/// back to their ID when `allow_delisted`), else the app by ID with completed metadata.
async fn resolve_app(
    store: &impl Store,
    account: &Account,
    input: &AppInput,
    platform: Option<Platform>,
    allow_delisted: bool,
) -> AppResult<App> {
    let app = App {
        id: input.app_id,
        bundle_id: input.bundle_id.clone(),
        ..App::default()
    };
    if input.bundle_id.is_empty() {
        return Ok(complete_app(store, account, app, platform).await);
    }
    match store
        .lookup(
            account,
            &LookupQuery::BundleId(input.bundle_id.clone()),
            platform,
        )
        .await
    {
        Err(Error::AppNotFound) if allow_delisted && input.app_id > 0 => Ok(app),
        Err(error) => Err(error),
        Ok(mut found) => {
            // Keep a known bundle identifier even when the catalog omits it.
            if found.bundle_id.is_empty() {
                found.bundle_id.clone_from(&input.bundle_id);
            }
            Ok(found)
        }
    }
}

/// Go's `download`: with `purchase`, a missing license is obtained once and the
/// download repeated with the account as the purchase left it.
pub(crate) async fn download(
    store: &impl Store,
    input: DownloadAppInput,
) -> Result<DownloadAppOutput, String> {
    let platform = input.app.validate()?;
    let purchased = Cell::new(false);
    with_mcp_account(store, async |account: &Account| {
        let app = resolve_app(store, account, &input.app, platform, true).await?;
        let mut download_input = DownloadInput {
            account: account.clone(),
            app: app.clone(),
            output_path: input.output.clone(),
            progress: None,
            external_version_id: input.external_version_id.clone(),
            platform,
            disable_parallel_download: false,
        };
        let mut output = store.download(download_input.clone()).await;
        if let Err(error) = &output
            && error.is_license_required()
            && input.purchase
            && !purchased.get()
        {
            let mut purchase_account = account.clone();
            if let Err(error) = store.purchase(&mut purchase_account, &app, platform).await
                && !error.is_license_already_exists()
            {
                return Err(error);
            }
            download_input.account = store.account_info().await?;
            purchased.set(true);
            output = store.download(download_input).await;
        }
        let output = output?;
        replicate_download_sinf(store, platform, &output).await?;
        Ok(DownloadAppOutput {
            parameters: AppParameters::new(&app, platform),
            app: app.into(),
            output: output.destination_path.display().to_string(),
            external_version_id: input.external_version_id.clone(),
            purchase: input.purchase,
            purchased: purchased.get(),
            success: true,
        })
    })
    .await
}

/// Go's `listVersions`.
pub(crate) async fn list_versions(
    store: &impl Store,
    input: AppInput,
) -> Result<VersionsOutput, String> {
    let platform = input.validate()?;
    with_mcp_account(store, async |account: &Account| {
        let app = resolve_app(store, account, &input, platform, false).await?;
        let output = store.list_versions(account, &app, platform).await?;
        Ok(VersionsOutput {
            parameters: AppParameters::new(&app, platform),
            bundle_id: app.bundle_id.clone(),
            app: app.into(),
            external_version_identifiers: output.external_version_identifiers,
            latest_external_version_id: output.latest_external_version_id,
            success: true,
        })
    })
    .await
}

/// Go's `getVersionMetadata`.
pub(crate) async fn get_version_metadata(
    store: &impl Store,
    input: VersionMetadataInput,
) -> Result<VersionMetadataOutput, String> {
    let platform = input.app.validate()?;
    if input.external_version_id.trim().is_empty() {
        return Err("external version identifier must not be empty".to_owned());
    }
    with_mcp_account(store, async |account: &Account| {
        let app = resolve_app(store, account, &input.app, platform, false).await?;
        let metadata = store
            .get_version_metadata(account, &app, &input.external_version_id, platform)
            .await?;
        Ok(VersionMetadataOutput {
            parameters: AppParameters::new(&app, platform),
            app: app.into(),
            external_version_id: input.external_version_id.clone(),
            display_version: metadata.display_version,
            release_date: metadata.release_date,
            success: true,
        })
    })
    .await
}

/// Go's `listPurchases`.
pub(crate) async fn list_purchases(
    store: &impl Store,
    input: PurchasesInput,
) -> Result<PurchasesOutput, String> {
    let page = input.page.unwrap_or(1);
    let limit = input.max_results.unwrap_or(DEFAULT_OWNED_APPS_LIMIT);
    if page < 1 {
        return Err("page must be greater than 0".to_owned());
    }
    if !(1..=MAX_OWNED_APPS_LIMIT).contains(&limit) {
        return Err(format!(
            "max results must be between 1 and {MAX_OWNED_APPS_LIMIT}"
        ));
    }
    let platform = parse_platform_filter(&input.platform).map_err(|error| error.to_string())?;
    with_mcp_account(store, async |account: &Account| {
        let output = store
            .owned_apps(OwnedAppsInput {
                account: account.clone(),
                page,
                limit,
                platform,
            })
            .await?;
        Ok(PurchasesOutput {
            count: output.count,
            total_count: output.total_count,
            page: output.page,
            max_results: limit,
            platform: platform_name(platform),
            apps: app_results(store, account, output.results, platform).await,
        })
    })
    .await
}

/// Go's `purchase`: an existing license is a success with `alreadyOwned`.
pub(crate) async fn purchase(
    store: &impl Store,
    input: AppInput,
) -> Result<PurchaseOutput, String> {
    let platform = input.validate()?;
    with_mcp_account(store, async |account: &Account| {
        let app = resolve_app(store, account, &input, platform, false).await?;
        let already_owned = match store.purchase(&mut account.clone(), &app, platform).await {
            Ok(()) => false,
            Err(error) if error.is_license_already_exists() => true,
            Err(error) => return Err(error),
        };
        Ok(PurchaseOutput {
            parameters: AppParameters::new(&app, platform),
            app: app.into(),
            already_owned,
            success: true,
        })
    })
    .await
}
