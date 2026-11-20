//! Public, unauthenticated catalog endpoints: iTunes search/lookup and the
//! `apps.apple.com` catalog proxy that carries the recent version history.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use crate::app::ItunesApp;
use crate::{App, Client, Error, Platform, Result};

const ITUNES_SEARCH: &str = "https://itunes.apple.com/search";
const ITUNES_LOOKUP: &str = "https://itunes.apple.com/lookup";
const CATALOG_APPS: &str = "https://apps.apple.com/api/apps/v1/catalog";

/// A two-letter storefront country code, stored upper case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountryCode(String);

impl CountryCode {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CountryCode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let code = value.trim().to_ascii_uppercase();
        if code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase()) {
            Ok(Self(code))
        } else {
            Err(Error::InvalidCountry(value.to_owned()))
        }
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What to look an app up by.
#[derive(Clone, Debug)]
pub enum LookupQuery {
    BundleId(String),
    AppId(i64),
}

/// One entry of the catalog version history.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CatalogVersion {
    #[serde(rename = "externalVersionID")]
    pub external_version_id: String,
    #[serde(rename = "displayVersion")]
    pub display_version: String,
    #[serde(rename = "releaseDate", with = "time::serde::rfc3339")]
    pub release_date: OffsetDateTime,
}

#[derive(Deserialize)]
struct ItunesResults {
    #[serde(default)]
    results: Vec<ItunesApp>,
}

pub(crate) fn search_url(
    term: &str,
    country: &CountryCode,
    limit: u32,
    platform: Option<Platform>,
) -> Result<Url> {
    let entity = Platform::search_entity(platform)?;
    let limit = limit.to_string();
    Ok(Url::parse_with_params(
        ITUNES_SEARCH,
        [
            ("entity", entity),
            ("limit", &limit),
            ("media", "software"),
            ("term", term),
            ("country", country.as_str()),
        ],
    )
    .expect("constant base URL is valid"))
}

pub(crate) fn lookup_url(
    query: &LookupQuery,
    country: &CountryCode,
    platform: Option<Platform>,
) -> Result<Url> {
    let entity = Platform::lookup_entity(platform)?;
    let url = match query {
        LookupQuery::BundleId(bundle_id) => Url::parse_with_params(
            ITUNES_LOOKUP,
            [
                ("entity", entity),
                ("limit", "1"),
                ("media", "software"),
                ("bundleId", bundle_id),
                ("country", country.as_str()),
            ],
        ),
        LookupQuery::AppId(id) => Url::parse_with_params(
            ITUNES_LOOKUP,
            [
                ("entity", entity),
                ("id", &id.to_string()),
                ("country", country.as_str()),
            ],
        ),
    };
    Ok(url.expect("constant base URL is valid"))
}

pub(crate) fn parse_apps(body: &[u8]) -> Result<Vec<App>> {
    let results: ItunesResults = serde_json::from_slice(body)?;
    Ok(results.results.into_iter().map(App::from).collect())
}

/// The catalog sends version IDs as numbers or strings.
fn version_id<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Number(u64),
        Text(String),
    }
    Ok(match Id::deserialize(deserializer)? {
        Id::Number(n) => n.to_string(),
        Id::Text(s) => s,
    })
}

#[derive(Deserialize)]
struct CatalogResponse {
    data: Vec<CatalogItem>,
}

#[derive(Deserialize)]
struct CatalogItem {
    id: String,
    attributes: CatalogAttributes,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogAttributes {
    #[serde(default)]
    platform_attributes: std::collections::HashMap<String, PlatformAttributes>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlatformAttributes {
    #[serde(default)]
    version_history: Vec<HistoryItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryItem {
    #[serde(deserialize_with = "version_id")]
    external_version_id: String,
    version_display: String,
    release_timestamp: String,
}

pub(crate) fn catalog_history_url(country: &CountryCode, app_id: i64, platform: Platform) -> Url {
    let mut url = Url::parse(&format!(
        "{CATALOG_APPS}/{}/apps/{app_id}",
        country.as_str().to_ascii_lowercase()
    ))
    .expect("country code and app ID are URL safe");
    url.query_pairs_mut()
        .append_pair("platform", platform.as_str())
        .append_pair("extend", "versionHistory");
    url
}

pub(crate) fn parse_catalog_history(body: &[u8], app_id: i64) -> Result<Vec<CatalogVersion>> {
    let response: CatalogResponse = serde_json::from_slice(body)?;
    let [item] = response.data.as_slice() else {
        return Err(Error::DifferentApp);
    };
    if item.id != app_id.to_string() {
        return Err(Error::DifferentApp);
    }
    let Some(ios) = item.attributes.platform_attributes.get("ios") else {
        return Ok(Vec::new());
    };
    ios.version_history
        .iter()
        .map(|entry| {
            let release_date = OffsetDateTime::parse(&entry.release_timestamp, &Rfc3339)
                .map_err(|_| Error::InvalidTimestamp(entry.release_timestamp.clone()))?;
            Ok(CatalogVersion {
                external_version_id: entry.external_version_id.clone(),
                display_version: entry.version_display.clone(),
                release_date,
            })
        })
        .collect()
}

impl Client {
    /// Searches the iTunes catalog.
    ///
    /// # Errors
    /// Network, HTTP status, or decode failures. visionOS search is not ported.
    pub async fn search(
        &self,
        term: &str,
        country: &CountryCode,
        limit: u32,
        platform: Option<Platform>,
    ) -> Result<Vec<App>> {
        if platform == Some(Platform::VisionOs) {
            return Err(Error::NotPorted("visionOS search"));
        }
        let url = search_url(term, country, limit, platform)?;
        let mut apps = parse_apps(&self.get_ok(self.http.get(url)).await?)?;
        if platform == Some(Platform::WatchOs) {
            apps.retain(|app| app.platforms.contains(&Platform::WatchOs));
        }
        Ok(apps)
    }

    /// Looks one app up by bundle ID or numeric app ID.
    ///
    /// # Errors
    /// [`Error::AppNotFound`] when the catalog has no match.
    pub async fn lookup(
        &self,
        query: &LookupQuery,
        country: &CountryCode,
        platform: Option<Platform>,
    ) -> Result<App> {
        let url = lookup_url(query, country, platform)?;
        let app = parse_apps(&self.get_ok(self.http.get(url)).await?)?
            .into_iter()
            .next()
            .ok_or(Error::AppNotFound)?;
        if platform == Some(Platform::WatchOs) && !app.platforms.contains(&Platform::WatchOs) {
            return Err(Error::AppNotFound);
        }
        Ok(app)
    }

    /// The newest versions (Apple caps this at 25) with their display names, in
    /// one unauthenticated request. Only iPhone and iPad apps have a history;
    /// other platforms return an empty list.
    ///
    /// # Errors
    /// Network, HTTP status, decode failures, or a response for another app.
    pub async fn catalog_version_history(
        &self,
        app_id: i64,
        country: &CountryCode,
        platform: Option<Platform>,
    ) -> Result<Vec<CatalogVersion>> {
        let platform = platform.unwrap_or(Platform::IPhone);
        if !matches!(platform, Platform::IPhone | Platform::IPad) {
            return Ok(Vec::new());
        }
        let url = catalog_history_url(country, app_id, platform);
        let body = self.get_ok(self.http.get(url)).await?;
        parse_catalog_history(&body, app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us() -> CountryCode {
        "us".parse().unwrap()
    }

    #[test]
    fn country_codes_are_validated() {
        assert_eq!(us().as_str(), "US");
        assert!("usa".parse::<CountryCode>().is_err());
        assert!("u1".parse::<CountryCode>().is_err());
    }

    #[test]
    fn search_url_matches_go_parameters() {
        let url = search_url("perplexity ai", &us(), 5, None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://itunes.apple.com/search?entity=software%2CiPadSoftware&limit=5&media=software&term=perplexity+ai&country=US"
        );
    }

    #[test]
    fn lookup_url_by_bundle_and_id() {
        let by_bundle = lookup_url(
            &LookupQuery::BundleId("ai.perplexity.app".into()),
            &us(),
            None,
        )
        .unwrap();
        assert!(by_bundle.as_str().contains("bundleId=ai.perplexity.app"));
        assert!(by_bundle.as_str().contains("limit=1"));
        let by_id = lookup_url(&LookupQuery::AppId(1_668_000_334), &us(), None).unwrap();
        assert!(by_id.as_str().contains("id=1668000334"));
    }

    #[test]
    fn platforms_come_from_supported_devices() {
        let body = br#"{"resultCount":2,"results":[
            {"trackId":1,"bundleId":"a","trackName":"A","version":"1.0","price":0,
             "supportedDevices":["iPhone15-iPhone15","iPadPro-iPadPro","Watch7-Watch7"]},
            {"trackId":2,"kind":"mac-software"}]}"#;
        let apps = parse_apps(body).unwrap();
        assert_eq!(
            apps[0].platforms,
            [Platform::IPhone, Platform::IPad, Platform::WatchOs]
        );
        assert_eq!(apps[1].platforms, [Platform::MacOs]);
    }

    #[test]
    fn apps_without_device_info_are_unknown() {
        let apps = parse_apps(br#"{"results":[{"trackId":3}]}"#).unwrap();
        assert_eq!(apps[0].platforms, [Platform::Unknown]);
    }

    const HISTORY: &[u8] = br#"{"data":[{"id":"1668000334","type":"apps","attributes":{
        "platformAttributes":{"ios":{"versionHistory":[
          {"externalVersionId":895516672,"versionDisplay":"26.46.0","releaseTimestamp":"2026-11-18T19:21:02Z"},
          {"externalVersionId":"895199582","versionDisplay":"26.45.0","releaseTimestamp":"2026-11-13T17:00:00Z"}]}}}}]}"#;

    #[test]
    fn parses_catalog_history_with_numeric_and_string_ids() {
        let versions = parse_catalog_history(HISTORY, 1_668_000_334).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].external_version_id, "895516672");
        assert_eq!(versions[0].display_version, "26.46.0");
        assert_eq!(versions[1].external_version_id, "895199582");
        assert_eq!(
            versions[0].release_date,
            time::macros::datetime!(2026-11-18 19:21:02 UTC)
        );
    }

    #[tokio::test]
    async fn platforms_without_a_catalog_history_return_no_versions() {
        let client = Client::new().unwrap();
        for platform in [
            Platform::MacOs,
            Platform::AppleTv,
            Platform::WatchOs,
            Platform::VisionOs,
        ] {
            let versions = client
                .catalog_version_history(42, &us(), Some(platform))
                .await
                .unwrap();
            assert_eq!(versions, []);
        }
    }

    #[test]
    fn rejects_history_for_another_app() {
        assert!(matches!(
            parse_catalog_history(HISTORY, 1),
            Err(Error::DifferentApp)
        ));
    }

    #[test]
    fn catalog_url_shape() {
        assert_eq!(
            catalog_history_url(&us(), 42, Platform::IPad).as_str(),
            "https://apps.apple.com/api/apps/v1/catalog/us/apps/42?platform=ipad&extend=versionHistory"
        );
    }
}
