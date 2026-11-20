//! Storefront ID ↔ country mapping (`pkg/appstore/storefront.go`). A scoped bag names
//! its storefront; the `available-storefronts` list is the fallback for older bags.

use std::collections::HashMap;
use std::sync::Mutex;

use plist::{Dictionary, Value};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use url::Url;

use crate::bag::{bag_url, string};
use crate::store::STORE_USER_AGENT;
use crate::{Client, Error, Result};

#[derive(Debug, Default)]
pub(crate) struct StorefrontCache {
    country_codes: HashMap<String, String>,
    list_loaded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapping {
    id: String,
    country: String,
}

/// The numeric part of a storefront header such as `143441-1,34`.
#[must_use]
pub fn storefront_identifier(store_front: &str) -> &str {
    let identifier = store_front.trim().split(',').next().unwrap_or_default();
    identifier.split('-').next().unwrap_or_default()
}

fn valid_identifier(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|identifier| identifier > 0 && identifier.to_string() == value)
}

fn valid_country(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|b| b.is_ascii_uppercase())
}

fn mapping_error(store_front: &str) -> Error {
    Error::Invalid(format!(
        "country code mapping for store front ({store_front}) was not found"
    ))
}

/// Go's `findStorefrontMapping`. A country can map to several IDs; Go picks one at
/// random map order, so the smallest ID is used here to stay deterministic.
fn find(codes: &HashMap<String, String>, identifier: &str, country: &str) -> Option<Mapping> {
    if !identifier.is_empty() {
        return codes
            .get(identifier)
            .filter(|code| !code.is_empty())
            .map(|code| Mapping {
                id: identifier.to_owned(),
                country: code.clone(),
            });
    }
    codes
        .iter()
        .filter(|(_, code)| *code == country)
        .min_by_key(|(id, _)| (id.len(), id.as_str()))
        .map(|(id, code)| Mapping {
            id: id.clone(),
            country: code.clone(),
        })
}

/// Go's `fmt.Sprint` of the decoded `storeFrontId`.
fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Real(number)) => number.to_string(),
        Some(Value::Boolean(flag)) => flag.to_string(),
        _ => "<nil>".to_owned(),
    }
}

fn validate_available_storefronts_endpoint(endpoint: &str) -> Result<Url> {
    let invalid = || Error::Invalid("invalid available-storefronts endpoint in bag".to_owned());
    let url = Url::parse(endpoint).map_err(|_| invalid())?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
        || url.port().is_some()
        || (host != "apple.com" && !host.ends_with(".apple.com"))
    {
        return Err(invalid());
    }
    Ok(url)
}

fn parse_available_storefronts(body: &[u8]) -> Result<HashMap<String, String>> {
    let decoded: Dictionary = plist::from_bytes(body)
        .map_err(|err| Error::Invalid(format!("failed to decode available storefronts: {err}")))?;
    let codes: HashMap<String, String> = decoded
        .get("storefronts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_dictionary)
        .filter_map(|storefront| {
            let id = storefront.get("id").and_then(Value::as_signed_integer)?;
            let code = string(storefront, "country-code")
                .trim()
                .to_ascii_uppercase();
            (id > 0 && valid_country(&code)).then(|| (id.to_string(), code))
        })
        .collect();
    if codes.is_empty() {
        return Err(Error::Invalid(
            "available storefronts response contains no storefronts".to_owned(),
        ));
    }
    Ok(codes)
}

impl Client {
    fn cached(&self, identifier: &str, country: &str) -> Option<Mapping> {
        find(&lock(&self.storefronts).country_codes, identifier, country)
    }

    /// Go's `countryCodeFromStoreFront`: the country of an account storefront.
    ///
    /// # Errors
    /// The storefront is malformed or Apple does not know it.
    pub async fn country_code_from_store_front(&self, store_front: &str) -> Result<String> {
        if !valid_identifier(storefront_identifier(store_front)) {
            return Err(mapping_error(store_front));
        }
        Ok(self.discover_storefront(store_front, "").await?.country)
    }

    /// Go's `discoverStorefrontID`: the storefront ID for a country code.
    ///
    /// # Errors
    /// The country code is malformed or Apple does not know it.
    pub async fn discover_storefront_id(&self, country_code: &str) -> Result<String> {
        let country = country_code.trim().to_ascii_uppercase();
        if !valid_country(&country) {
            return Err(Error::Invalid(format!(
                "invalid storefront country code ({country})"
            )));
        }
        Ok(self.discover_storefront("", &country).await?.id)
    }

    /// Go validates both fields of a scoped bag: Apple answers unknown IDs with the
    /// US bag while echoing the requested ID in the response headers.
    async fn discover_storefront(&self, store_front: &str, country: &str) -> Result<Mapping> {
        let identifier = storefront_identifier(store_front);
        if let Some(mapping) = self.cached(identifier, country) {
            return Ok(mapping);
        }
        let mut headers = HeaderMap::new();
        let mut url = bag_url("");
        if identifier.is_empty() {
            url.push_str("&cc=");
            url.extend(url::form_urlencoded::byte_serialize(country.as_bytes()));
        } else {
            let value = HeaderValue::from_str(store_front.trim())
                .map_err(|_| mapping_error(store_front))?;
            headers.insert("X-Apple-Store-Front", value);
        }
        let response = self
            .store_get_xml::<Dictionary>(&url, headers)
            .await
            .map_err(|err| err.context("storefront bag request failed"))?;
        if response.status != 200 {
            return Err(Error::Store {
                message: "storefront bag request failed".to_owned(),
                status: response.status,
            });
        }
        let bag = response.data.unwrap_or_default();
        let id = value_text(bag.get("storeFrontId"));
        let code = string(&bag, "currentStorefrontCountryCodeISO2A")
            .trim()
            .to_ascii_uppercase();
        if valid_identifier(&id)
            && valid_country(&code)
            && (identifier.is_empty() || id == identifier)
            && (country.is_empty() || code == country)
        {
            lock(&self.storefronts)
                .country_codes
                .insert(id.clone(), code.clone());
            return Ok(Mapping { id, country: code });
        }

        let nested = bag
            .get("urlBag")
            .and_then(Value::as_dictionary)
            .map_or_else(String::new, |nested| {
                string(nested, "available-storefronts")
            });
        let endpoint = if nested.is_empty() {
            string(&bag, "available-storefronts")
        } else {
            nested
        };
        self.load_available_storefronts(&endpoint).await?;
        if let Some(mapping) = self.cached(identifier, country) {
            return Ok(mapping);
        }
        if identifier.is_empty() {
            Err(Error::Invalid(format!(
                "store front mapping for country code ({country}) was not found"
            )))
        } else {
            Err(mapping_error(store_front))
        }
    }

    /// Go's `discoverAvailableStorefronts`: loads the full list once, keeping the
    /// mappings that scoped bags already confirmed.
    async fn load_available_storefronts(&self, endpoint: &str) -> Result<()> {
        if lock(&self.storefronts).list_loaded {
            return Ok(());
        }
        let url = validate_available_storefronts_endpoint(endpoint)?;
        let response = self
            .store
            .get(url)
            .header(USER_AGENT, STORE_USER_AGENT)
            .header(ACCEPT, "application/xml")
            .send()
            .await
            .map_err(|err| Error::from(err).context("available storefronts request failed"))?;
        let status = response.status().as_u16();
        let body = response.bytes().await?;
        if status != 200 {
            return Err(Error::Store {
                message: "available storefronts request failed".to_owned(),
                status,
            });
        }
        let codes = parse_available_storefronts(&body)?;
        let mut cache = lock(&self.storefronts);
        for (id, code) in codes {
            cache.country_codes.entry(id).or_insert(code);
        }
        cache.list_loaded = true;
        Ok(())
    }
}

fn lock(cache: &Mutex<StorefrontCache>) -> std::sync::MutexGuard<'_, StorefrontCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_storefront_identifiers() {
        for (storefront, identifier) in [
            ("143441,34", "143441"),
            ("143441-1,34", "143441"),
            ("143443", "143443"),
            (" 143444-2,32 ", "143444"),
        ] {
            assert_eq!(storefront_identifier(storefront), identifier);
        }
        assert!(valid_identifier("143441"));
        for invalid in ["", "0", "-1", "0143441", "abc", "+1"] {
            assert!(!valid_identifier(invalid), "{invalid}");
        }
    }

    #[test]
    fn finds_mappings() {
        let codes = HashMap::from([
            ("143441".to_owned(), "US".to_owned()),
            ("143443".to_owned(), "DE".to_owned()),
        ]);
        assert_eq!(find(&codes, "143443", "").unwrap().country, "DE");
        assert_eq!(find(&codes, "", "US").unwrap().id, "143441");
        assert!(find(&codes, "1", "").is_none());
        assert!(find(&codes, "", "GB").is_none());
    }

    #[test]
    fn parses_available_storefronts() {
        let codes = parse_available_storefronts(
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict><key>storefronts</key><array>
                <dict><key>country-code</key><string>US</string><key>id</key><integer>143441</integer></dict>
                <dict><key>country-code</key><string>de</string><key>id</key><integer>143443</integer></dict>
                <dict><key>country-code</key><string>XYZ</string><key>id</key><integer>1</integer></dict>
                <dict><key>country-code</key><string>GB</string><key>id</key><integer>0</integer></dict>
            </array></dict></plist>"#,
        )
        .unwrap();
        assert_eq!(
            codes,
            HashMap::from([
                ("143441".to_owned(), "US".to_owned()),
                ("143443".to_owned(), "DE".to_owned()),
            ])
        );
        assert_eq!(
            parse_available_storefronts(b"<plist><dict></dict></plist>")
                .unwrap_err()
                .to_string(),
            "available storefronts response contains no storefronts"
        );
    }

    #[test]
    fn validates_available_storefronts_endpoint() {
        assert!(
            validate_available_storefronts_endpoint(
                "https://itunes.apple.com/WebObjects/MZStore.woa/wa/availableStoreFronts"
            )
            .is_ok()
        );
        assert!(validate_available_storefronts_endpoint("https://apple.com:443/x").is_ok());
        for invalid in [
            "",
            "http://itunes.apple.com/x",
            "https://evilapple.com/x",
            "https://user@itunes.apple.com/x",
            "https://itunes.apple.com:8443/x",
            "https://itunes.apple.com/x#frag",
        ] {
            assert!(
                validate_available_storefronts_endpoint(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn prints_store_front_ids_like_go() {
        assert_eq!(value_text(Some(&Value::Integer(143_441.into()))), "143441");
        assert_eq!(value_text(Some(&Value::String("143441".into()))), "143441");
        assert_eq!(value_text(None), "<nil>");
    }
}
