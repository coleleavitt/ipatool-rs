//! Go's visionOS search (`searchVisionOS` in `appstore_search.go` and
//! `storefrontVisionApps` in `appstore_storefront.go`): the iTunes search has no visionOS
//! entity, so the native apps come from the storefront's search page and their metadata
//! from one lookup.

use std::collections::HashMap;

use ipatool_store::gourl;
use serde::Deserialize;

use crate::catalog::{lookup_ids_url, parse_apps};
use crate::commerce::CommerceRequest;
use crate::macos_version_lookup::serialized_server_data;
use crate::version_lookup::find_vision_external_version_id;
use crate::{App, Client, CountryCode, Error, Platform, Result};

/// Go's `visionSearchURL`.
pub(crate) fn vision_search_url(term: &str, country: &CountryCode) -> String {
    let mut params = gourl::Values::default();
    params.set("term", term);
    format!(
        "https://apps.apple.com/{}/vision/search?{}",
        country.as_str().to_lowercase(),
        params.encode()
    )
}

/// Go's `maxVisionOSSearchResults`.
const MAX_VISION_SEARCH_RESULTS: i64 = 12;

#[derive(Deserialize)]
struct StorefrontSearchPage {
    #[serde(default)]
    data: Vec<StorefrontPageData>,
}

#[derive(Deserialize)]
struct StorefrontPageData {
    #[serde(default)]
    data: StorefrontShelves,
}

#[derive(Default, Deserialize)]
struct StorefrontShelves {
    #[serde(default)]
    shelves: Vec<StorefrontShelf>,
}

#[derive(Deserialize)]
struct StorefrontShelf {
    #[serde(default)]
    items: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct StorefrontSearchItem {
    #[serde(rename = "$kind", default)]
    kind: String,
    lockup: StorefrontAppLockup,
}

#[derive(Deserialize)]
struct StorefrontAppLockup {
    #[serde(rename = "adamId", default)]
    adam_id: String,
    #[serde(rename = "bundleId", default)]
    bundle_id: String,
    #[serde(default)]
    title: String,
}

/// Go's `storefrontVisionApps`: the native visionOS apps of a storefront search page, in
/// page order, without duplicates, at most `limit` (capped at 12).
pub(crate) fn storefront_vision_apps(body: &[u8], limit: i64) -> Result<Vec<App>> {
    let page: StorefrontSearchPage = serde_json::from_slice(serialized_server_data(body)?)
        .map_err(|error| {
            Error::Invalid(format!("failed to decode serialized server data: {error}"))
        })?;
    let Ok(limit) = usize::try_from(limit.min(MAX_VISION_SEARCH_RESULTS)) else {
        return Ok(Vec::new());
    };
    let mut apps: Vec<App> = Vec::new();
    let items = page
        .data
        .iter()
        .flat_map(|data| &data.data.shelves)
        .flat_map(|shelf| &shelf.items);
    for raw in items {
        if apps.len() == limit {
            break;
        }
        let Ok(item) = StorefrontSearchItem::deserialize(raw) else {
            continue;
        };
        let id = match item.lockup.adam_id.parse::<i64>() {
            Ok(id) if id != 0 && item.kind == "AppSearchResult" => id,
            _ => continue,
        };
        if apps.iter().any(|app| app.id == id) || find_vision_external_version_id(raw, id).is_none()
        {
            continue;
        }
        apps.push(App {
            id,
            bundle_id: item.lockup.bundle_id,
            name: item.lockup.title,
            ..App::default()
        });
    }
    Ok(apps)
}

/// Go's `searchVisionOS` merge: catalog metadata where the lookup has it, storefront order,
/// and visionOS among the platforms.
fn hydrate_vision_apps(apps: Vec<App>, metadata: Vec<App>) -> Vec<App> {
    let mut metadata: HashMap<i64, App> = metadata.into_iter().map(|app| (app.id, app)).collect();
    apps.into_iter()
        .map(|app| {
            let mut app = metadata.remove(&app.id).unwrap_or(app);
            app.platforms
                .retain(|platform| *platform != Platform::Unknown);
            if !app.platforms.contains(&Platform::VisionOs) {
                app.platforms.push(Platform::VisionOs);
            }
            app
        })
        .collect()
}

impl Client {
    /// Go's `searchVisionOS`.
    pub(crate) async fn search_vision_os(
        &self,
        term: &str,
        country: &CountryCode,
        limit: i64,
    ) -> Result<Vec<App>> {
        let request = CommerceRequest {
            method: "GET".to_owned(),
            url: vision_search_url(term, country),
            ..CommerceRequest::default()
        };
        let response = self
            .store_raw("request", request)
            .await
            .map_err(|error| error.context("visionOS search request failed"))?;
        if response.status != 200 {
            return Err(Error::Store {
                message: "visionOS search request failed".to_owned(),
                status: response.status,
            });
        }
        let apps = storefront_vision_apps(&response.data, limit)
            .map_err(|error| error.context("failed to parse visionOS search results"))?;
        if apps.is_empty() {
            return Ok(apps);
        }
        let ids: Vec<i64> = apps.iter().map(|app| app.id).collect();
        let url = lookup_ids_url(&ids, country, Some(Platform::VisionOs))
            .map_err(|error| error.context("failed to create visionOS metadata request"))?;
        let (status, body, _) = self
            .get_bytes(self.http.get(url))
            .await
            .map_err(|error| error.context("visionOS metadata request failed"))?;
        if status != 200 {
            return Err(Error::Store {
                message: "visionOS metadata request failed".to_owned(),
                status,
            });
        }
        Ok(hydrate_vision_apps(apps, parse_apps(&body)?))
    }
}

#[cfg(test)]
mod tests;
