//! Go's `appstore_platform_version_lookup.go` and `appstore_ios_version_lookup.go`: the
//! latest external version ID of an app, from the MDM catalogs and the App Store catalog API.

use std::collections::BTreeMap;

use ipatool_store::gourl;
use serde::{Deserialize, Deserializer};

use crate::commerce::CommerceRequest;
use crate::{Account, App, Client, Error, Platform, Result};

/// Go's `errPlatformAppNotFound`.
pub(crate) const PLATFORM_APP_NOT_FOUND: &str = "platform version lookup returned no app";
/// Go's `errPlatformOffersNotFound`.
pub(crate) const PLATFORM_OFFERS_NOT_FOUND: &str = "platform version lookup returned no offers";
/// Go's `errIOSCatalogAppNotFound`.
pub(crate) const IOS_CATALOG_APP_NOT_FOUND: &str = "iOS catalog lookup returned HTTP 404";

/// Go's `platformVersionExternalID`: a JSON string or number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ExternalId(String);

impl<'de> Deserialize<'de> for ExternalId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Null => Ok(Self::default()),
            serde_json::Value::String(text) => Ok(Self(text)),
            serde_json::Value::Number(number) => Ok(Self(number.to_string())),
            other => Err(serde::de::Error::custom(format!(
                "invalid external version id {other}"
            ))),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct LookupResult {
    #[serde(default)]
    results: BTreeMap<String, LookupItem>,
    #[serde(default)]
    data: Vec<CatalogItem>,
}

#[derive(Debug, Default, Deserialize)]
struct LookupItem {
    #[serde(default)]
    offers: Vec<LookupOffer>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupOffer {
    #[serde(default)]
    buy_params: String,
    #[serde(default)]
    version: LookupVersion,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupVersion {
    #[serde(default)]
    external_id: ExternalId,
}

#[derive(Debug, Default, Deserialize)]
struct CatalogItem {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    attributes: CatalogAttributes,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogAttributes {
    #[serde(default)]
    device_families: Vec<String>,
    #[serde(default)]
    platform_attributes: BTreeMap<String, CatalogPlatformAttributes>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogPlatformAttributes {
    #[serde(default)]
    bundle_id: String,
    #[serde(default)]
    external_version_id: ExternalId,
}

/// Go's `Platform.metadataPlatform`.
fn metadata_platform(platform: Platform) -> Result<&'static str> {
    match platform {
        Platform::IPhone | Platform::IPad | Platform::WatchOs => Ok("enterprisestore"),
        Platform::AppleTv => Ok("atv9"),
        Platform::VisionOs => Ok("realityDevice"),
        other => Err(Error::Invalid(format!(
            "invalid platform {:?}",
            other.as_str()
        ))),
    }
}

fn get(url: String) -> CommerceRequest {
    CommerceRequest {
        method: "GET".to_owned(),
        url,
        ..CommerceRequest::default()
    }
}

/// Go's `platformVersionLookupRequest` URL (`url.Values.Encode` sorts the keys).
pub(crate) fn platform_version_lookup_url(app_id: i64, country: &str, catalog: &str) -> String {
    let mut params = gourl::Values::default();
    for (key, value) in [
        ("version", "2".to_owned()),
        ("id", app_id.to_string()),
        ("p", "mdm-lockup".to_owned()),
        ("caller", "MDM".to_owned()),
        ("platform", catalog.to_owned()),
        ("cc", country.to_lowercase()),
        ("l", "en".to_owned()),
    ] {
        params.set(key, value);
    }
    format!(
        "https://uclient-api.itunes.apple.com/WebObjects/MZStorePlatform.woa/wa/lookup?{}",
        params.encode()
    )
}

/// Go's `externalVersionIDFromBuyParams`.
fn external_version_id_from_buy_params(buy_params: &str) -> Result<String> {
    if buy_params.is_empty() {
        return Ok(String::new());
    }
    let (values, error) = gourl::parse_query(buy_params);
    if let Some(error) = error {
        return Err(Error::Invalid(format!("failed to parse query: {error}")));
    }
    Ok(values.get("appExtVrsId").to_owned())
}

fn store_error(message: &str, status: u16) -> Error {
    Error::Store {
        message: message.to_owned(),
        status,
    }
}

impl Client {
    /// Go's `lookupLatestExternalVersionID`.
    ///
    /// # Errors
    /// Go's lookup errors; a missing app or offer ends with Go's sentinel texts.
    pub async fn lookup_latest_external_version_id(
        &self,
        account: &Account,
        app: &App,
        platform: Platform,
    ) -> Result<String> {
        if app.id == 0 {
            return Err(Error::Invalid(
                "app ID is required for platform version lookup".to_owned(),
            ));
        }
        let country = self
            .country_code_from_store_front(&account.store_front)
            .await
            .map_err(|error| error.context("failed to resolve the country code"))?;
        if platform == Platform::VisionOs {
            return Err(Error::NotPorted("visionOS version lookup"));
        }
        let metadata = metadata_platform(platform)
            .map_err(|error| error.context("failed to create platform version lookup request"))?;
        let mut catalogs = vec![metadata];
        if matches!(
            platform,
            Platform::IPhone | Platform::IPad | Platform::WatchOs
        ) {
            // Some storefronts have no enterprise listing even when the consumer catalogs
            // contain the app.
            catalogs.extend(["iphone", "ipad"]);
        }
        let mut last_error = None;
        for catalog in &catalogs {
            let response = self
                .store_json::<LookupResult>(
                    "request",
                    get(platform_version_lookup_url(app.id, &country, catalog)),
                )
                .await
                .map_err(|error| error.context("platform version lookup request failed"))?;
            if response.status != 200 {
                return Err(store_error(
                    "platform version lookup request failed",
                    response.status,
                ));
            }
            let Some(item) = response.data.results.get(&app.id.to_string()) else {
                last_error = Some(store_error(PLATFORM_APP_NOT_FOUND, response.status));
                continue;
            };
            let Some(offer) = item.offers.first() else {
                last_error = Some(store_error(PLATFORM_OFFERS_NOT_FOUND, response.status));
                continue;
            };
            let mut external = offer.version.external_id.0.clone();
            if external.is_empty() {
                external = external_version_id_from_buy_params(&offer.buy_params)
                    .map_err(|error| error.context("failed to parse buy params"))?;
            }
            if external.is_empty() {
                return Err(store_error(
                    "platform version lookup returned no external version id",
                    response.status,
                ));
            }
            return Ok(external);
        }
        let mut last_error = last_error.unwrap_or_else(|| Error::Invalid(String::new()));
        if matches!(platform, Platform::IPhone | Platform::IPad) {
            // Apple Arcade apps can be absent from every MDM catalog.
            match self
                .lookup_latest_ios_external_version_id(app, &country, platform)
                .await
            {
                Ok(version) => return Ok(version),
                // Go wraps both errors, so `errors.Is` finds either sentinel.
                Err(error) => {
                    last_error = Error::Wrapped {
                        message: format!("{last_error}; iOS catalog fallback failed: {error}"),
                        sources: vec![last_error, error],
                    };
                }
            }
        }
        Err(last_error.context(format!(
            "app {} in storefront {country} (catalogs: {})",
            app.id,
            catalogs.join(", ")
        )))
    }

    /// Go's `lookupLatestIOSExternalVersionID`: the JSON catalog API of the App Store client.
    async fn lookup_latest_ios_external_version_id(
        &self,
        app: &App,
        country: &str,
        platform: Platform,
    ) -> Result<String> {
        let url = format!(
            "https://apps.apple.com/api/apps/v1/catalog/{}/apps/{}?platform={platform}",
            country.to_lowercase(),
            app.id
        );
        let response = self
            .store_json::<LookupResult>("request", get(url))
            .await
            .map_err(|error| error.context("iOS catalog lookup request failed"))?;
        if response.status == 404 {
            return Err(Error::Invalid(IOS_CATALOG_APP_NOT_FOUND.to_owned()));
        }
        if response.status != 200 {
            return Err(Error::Invalid(format!(
                "iOS catalog lookup returned HTTP {}",
                response.status
            )));
        }
        let [item] = response.data.data.as_slice() else {
            return Err(Error::Invalid(
                "iOS catalog lookup must return exactly one app".to_owned(),
            ));
        };
        if item.id != app.id.to_string() || item.kind != "apps" {
            return Err(Error::Invalid(
                "iOS catalog lookup returned a different app".to_owned(),
            ));
        }
        let attributes = item.attributes.platform_attributes.get("ios");
        let Some(attributes) = attributes.filter(|_| {
            item.attributes
                .device_families
                .iter()
                .any(|family| family == platform.as_str())
        }) else {
            return Err(Error::Invalid(
                "iOS catalog lookup returned no metadata for the requested platform".to_owned(),
            ));
        };
        if !app.bundle_id.is_empty() && attributes.bundle_id != app.bundle_id {
            return Err(Error::Invalid(
                "iOS catalog lookup returned a different bundle identifier".to_owned(),
            ));
        }
        let version = &attributes.external_version_id.0;
        if version.parse::<u64>().map_or(true, |id| id == 0) {
            return Err(Error::Invalid(
                "iOS catalog lookup returned no valid external version id".to_owned(),
            ));
        }
        Ok(version.clone())
    }
}
