//! Port of Go `cmd/mcp_app.go`: the app metadata of tool results.

use ipatool_appstore::{Account, App, LookupQuery, Platform};
use schemars::JsonSchema;
use serde::Serialize;
use time::OffsetDateTime;

use crate::commerce::Store;

// Go's `mcpApp`: every metadata field stays in the result, zero values included. The
// App Store's own JSON form omits some of them for other callers. (Doc comments on
// schema types would become schema descriptions.)
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct McpApp {
    #[serde(rename = "trackId")]
    pub(crate) id: i64,
    #[serde(rename = "bundleId")]
    pub(crate) bundle_id: String,
    #[serde(rename = "trackName")]
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) price: f64,
    #[serde(rename = "purchaseDate", with = "time::serde::rfc3339::option")]
    #[schemars(with = "Option<String>")]
    pub(crate) purchase_date: Option<OffsetDateTime>,
    // A Go slice, so the schema allows null.
    #[schemars(with = "Option<Vec<String>>")]
    pub(crate) platforms: Vec<Platform>,
}

impl From<App> for McpApp {
    fn from(app: App) -> Self {
        Self {
            id: app.id,
            bundle_id: app.bundle_id,
            name: app.name,
            version: app.version,
            price: app.price,
            purchase_date: app.purchase_date,
            platforms: app.platforms,
        }
    }
}

/// Go's `completeApp`: fills sparse metadata from the catalog without making catalog
/// availability a prerequisite for owned or delisted apps.
pub(crate) async fn complete_app(
    store: &impl Store,
    account: &Account,
    mut app: App,
    platform: Option<Platform>,
) -> App {
    let known_platforms = app
        .platforms
        .first()
        .is_some_and(|platform| *platform != Platform::Unknown);
    if app.id <= 0
        || (!app.bundle_id.is_empty()
            && !app.name.is_empty()
            && !app.version.is_empty()
            && known_platforms)
    {
        return app;
    }
    // Purchase history can list several platforms when no filter was given; the
    // catalog lookup then uses the app's first known platform.
    let platform = match platform {
        None if known_platforms => app.platforms.first().copied(),
        platform => platform,
    };
    let Ok(found) = store
        .lookup(account, &LookupQuery::AppId(app.id), platform)
        .await
    else {
        return app;
    };
    if found.id != app.id {
        return app;
    }
    if app.bundle_id.is_empty() {
        app.bundle_id = found.bundle_id;
    }
    if app.name.is_empty() {
        app.name = found.name;
    }
    if app.version.is_empty() {
        app.version = found.version;
    }
    let unknown_platforms = app.platforms.is_empty() || app.platforms == [Platform::Unknown];
    if !found.platforms.is_empty() && unknown_platforms {
        app.platforms = found.platforms;
    }
    if app.price == 0.0 {
        app.price = found.price;
    }
    app
}

/// Go's `appResults`.
pub(crate) async fn app_results(
    store: &impl Store,
    account: &Account,
    apps: Vec<App>,
    platform: Option<Platform>,
) -> Vec<McpApp> {
    let mut results = Vec::with_capacity(apps.len());
    for app in apps {
        results.push(complete_app(store, account, app, platform).await.into());
    }
    results
}

// Go's `mcpAppParameters`: the app a tool acted on.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AppParameters {
    pub(crate) app_id: i64,
    pub(crate) bundle_identifier: String,
    pub(crate) platform: &'static str,
}

impl AppParameters {
    pub(crate) fn new(app: &App, platform: Option<Platform>) -> Self {
        Self {
            app_id: app.id,
            bundle_identifier: app.bundle_id.clone(),
            platform: platform_name(platform),
        }
    }
}

/// Go's `appstore.Platform` string, empty when none was chosen.
pub(crate) fn platform_name(platform: Option<Platform>) -> &'static str {
    platform.map_or("", Platform::as_str)
}
