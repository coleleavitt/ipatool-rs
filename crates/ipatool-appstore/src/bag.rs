//! The Store URL bag (`pkg/appstore/appstore_bag.go`).

use plist::{Dictionary, Value};
use reqwest::header::HeaderMap;
use url::Url;

use crate::{Client, Error, Result};

pub(crate) const BAG_URL: &str = "https://init.itunes.apple.com/bag.xml?ix=6";
const AUTH_DOMAIN: &str = "buy.itunes.apple.com";
const AUTH_PATH: &str = "/WebObjects/MZFinance.woa/wa/authenticate";
const NATIVE_DOMAIN: &str = "auth.itunes.apple.com";
const NATIVE_PATH: &str = "/auth/v1/native";

/// Service endpoints from the bag. Empty strings mean the bag omitted them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlBag {
    pub auth_endpoint: String,
    pub ent_download_endpoint: String,
    pub redownload_endpoint: String,
    pub update_endpoint: String,
    pub sap_setup_endpoint: String,
    pub sap_setup_cert_endpoint: String,
    pub sap_version: String,
    pub terms_info_endpoint: String,
    pub terms_check_endpoint: String,
}

/// What login needs from the bag (Go's `SAPConfig`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SapConfig {
    pub auth_endpoint: String,
    pub setup_url: String,
    pub certificate_url: String,
    pub version: u32,
}

pub(crate) fn string(dict: &Dictionary, key: &str) -> String {
    dict.get(key)
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_owned()
}

impl UrlBag {
    fn from_dict(dict: &Dictionary) -> Self {
        Self {
            auth_endpoint: string(dict, "authenticateAccount"),
            ent_download_endpoint: string(dict, "volumeStoreDownloadProduct"),
            redownload_endpoint: string(dict, "redownloadProduct"),
            update_endpoint: string(dict, "updateProduct"),
            sap_setup_endpoint: string(dict, "sign-sap-setup"),
            sap_setup_cert_endpoint: string(dict, "sign-sap-setup-cert"),
            sap_version: string(dict, "sign-sap-version"),
            terms_info_endpoint: string(dict, "getTermsByCountryAndTypeSrv"),
            terms_check_endpoint: string(dict, "terms-check"),
        }
    }

    /// Go's `fetchURLBag` merge: the nested `urlBag` wins when it has any service endpoint;
    /// otherwise the top-level keys are used. Terms endpoints fall back individually.
    pub(crate) fn from_bag(bag: &Dictionary) -> Self {
        let top = Self::from_dict(bag);
        let mut nested = bag
            .get("urlBag")
            .and_then(Value::as_dictionary)
            .map_or_else(Self::default, Self::from_dict);
        if nested.terms_info_endpoint.is_empty() {
            nested
                .terms_info_endpoint
                .clone_from(&top.terms_info_endpoint);
        }
        if nested.terms_check_endpoint.is_empty() {
            nested
                .terms_check_endpoint
                .clone_from(&top.terms_check_endpoint);
        }
        let has_service = [
            &nested.auth_endpoint,
            &nested.ent_download_endpoint,
            &nested.redownload_endpoint,
            &nested.update_endpoint,
            &nested.sap_setup_endpoint,
            &nested.sap_setup_cert_endpoint,
            &nested.sap_version,
        ]
        .iter()
        .any(|value| !value.is_empty());
        if has_service {
            return nested;
        }
        Self {
            terms_info_endpoint: nested.terms_info_endpoint,
            terms_check_endpoint: nested.terms_check_endpoint,
            ..top
        }
    }

    /// Go's `bag`: parses and validates the SAP configuration and pins the legacy
    /// authentication path to its slashed form.
    ///
    /// # Errors
    /// Missing or unsupported endpoints, or a SAP version other than 200.
    pub fn sap_config(&self) -> Result<SapConfig> {
        let version = parse_sap_version(&self.sap_version)?;
        validate_auth_endpoint(&self.auth_endpoint)?;
        for (name, endpoint) in [
            ("SAP setup", &self.sap_setup_endpoint),
            ("SAP setup certificate", &self.sap_setup_cert_endpoint),
        ] {
            let valid = Url::parse(endpoint).is_ok_and(|url| {
                url.scheme() == "https" && url.host_str().is_some_and(|host| !host.is_empty())
            });
            if !valid {
                return Err(Error::Invalid(format!(
                    "invalid {name} endpoint {endpoint:?} in bag"
                )));
            }
        }
        if version != 200 {
            return Err(Error::Invalid(format!(
                "unsupported SAP version {version} in bag"
            )));
        }
        Ok(SapConfig {
            auth_endpoint: normalize_auth_endpoint(&self.auth_endpoint),
            setup_url: self.sap_setup_endpoint.clone(),
            certificate_url: self.sap_setup_cert_endpoint.clone(),
            version,
        })
    }
}

/// `strconv.ParseUint(value, 10, 32)` with Go's error text.
fn parse_sap_version(value: &str) -> Result<u32> {
    let reason = if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        "invalid syntax"
    } else if let Ok(version) = value.parse::<u32>() {
        return Ok(version);
    } else {
        "value out of range"
    };
    Err(Error::Invalid(format!(
        "invalid SAP version {value:?} in bag: strconv.ParseUint: parsing {value:?}: {reason}"
    )))
}

/// Go's `validateAuthenticationEndpoint`. Any escape in the path is rejected, as Go
/// rejects a non-empty `RawPath`; the valid paths contain none.
pub(crate) fn validate_auth_endpoint(endpoint: &str) -> Result<()> {
    let invalid = || Error::Invalid("invalid authentication endpoint".to_owned());
    let unsupported = || Error::Invalid("unsupported authentication endpoint".to_owned());
    let url = Url::parse(endpoint).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
        || url.path().contains('%')
        || url.port().is_some()
    {
        return Err(invalid());
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let path = url.path();
    if host == NATIVE_DOMAIN {
        let native = [
            NATIVE_PATH.to_owned(),
            format!("{NATIVE_PATH}/"),
            format!("{NATIVE_PATH}/fast"),
            format!("{NATIVE_PATH}/fast/"),
        ];
        return if native.iter().any(|candidate| candidate == path) {
            Ok(())
        } else {
            Err(unsupported())
        };
    }
    if host != AUTH_DOMAIN && !host.ends_with("-buy.itunes.apple.com") {
        return Err(unsupported());
    }
    if path != AUTH_PATH && path != format!("{AUTH_PATH}/") {
        return Err(unsupported());
    }
    Ok(())
}

/// Rewrites a validated legacy endpoint's path to `…/authenticate/`, keeping the
/// original scheme, host, port and query text as Go's `url.URL.String` does.
fn normalize_auth_endpoint(endpoint: &str) -> String {
    let Ok(url) = Url::parse(endpoint) else {
        return endpoint.to_owned();
    };
    if !url
        .host_str()
        .is_some_and(|host| host.to_ascii_lowercase().ends_with(AUTH_DOMAIN))
    {
        return endpoint.to_owned();
    }
    let (before_query, query) = match endpoint.split_once('?') {
        Some((before, query)) => (before, Some(query)),
        None => (endpoint, None),
    };
    let authority_start = before_query.find("://").map_or(0, |index| index + 3);
    let path_start = before_query[authority_start..]
        .find('/')
        .map_or(before_query.len(), |index| authority_start + index);
    let mut normalized = format!("{}{AUTH_PATH}/", &before_query[..path_start]);
    if let Some(query) = query {
        normalized.push('?');
        normalized.push_str(query);
    }
    normalized
}

pub(crate) fn bag_url(guid: &str) -> String {
    if guid.is_empty() {
        return BAG_URL.to_owned();
    }
    let guid: String = url::form_urlencoded::byte_serialize(guid.as_bytes()).collect();
    format!("{BAG_URL}&guid={guid}")
}

impl Client {
    /// Go's `fetchURLBag`.
    ///
    /// # Errors
    /// Transport, status or decode failures.
    pub async fn url_bag(&self, guid: &str) -> Result<UrlBag> {
        let response = self
            .store_get_xml::<Dictionary>(&bag_url(guid), HeaderMap::new())
            .await
            .map_err(|err| err.context("failed to send http request"))?;
        if response.status != 200 {
            return Err(Error::Invalid(format!(
                "received unexpected status code: {}",
                response.status
            )));
        }
        Ok(UrlBag::from_bag(&response.data.unwrap_or_default()))
    }

    /// Go's `bag`: the URL bag's validated SAP configuration.
    ///
    /// # Errors
    /// See [`Client::url_bag`] and [`UrlBag::sap_config`].
    pub async fn sap_config(&self, guid: &str) -> Result<SapConfig> {
        self.url_bag(guid).await?.sap_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTH: &str = "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate";
    const SETUP: &str = "https://fpinit.example.com/v1/signSapSetup/legacy";
    const CERT: &str = "https://static.example.com/sap/setupCert.plist";

    fn valid_bag(auth: &str) -> UrlBag {
        UrlBag {
            auth_endpoint: auth.to_owned(),
            sap_setup_endpoint: SETUP.to_owned(),
            sap_setup_cert_endpoint: CERT.to_owned(),
            sap_version: "200".to_owned(),
            ..UrlBag::default()
        }
    }

    #[test]
    fn normalizes_the_authentication_path() {
        let pod = "https://p7-buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate";
        for (endpoint, expected) in [
            (AUTH.to_owned(), format!("{AUTH}/")),
            (format!("{AUTH}/"), format!("{AUTH}/")),
            (
                format!("{AUTH}?Pod=7&routing=a%2Fb+c"),
                format!("{AUTH}/?Pod=7&routing=a%2Fb+c"),
            ),
            (format!("{pod}?Pod=7&PRH=7"), format!("{pod}/?Pod=7&PRH=7")),
            (
                format!("{pod}/?routing=opaque"),
                format!("{pod}/?routing=opaque"),
            ),
            (
                "https://buy.itunes.apple.com:443/WebObjects/MZFinance.woa/wa/authenticate"
                    .to_owned(),
                "https://buy.itunes.apple.com:443/WebObjects/MZFinance.woa/wa/authenticate/"
                    .to_owned(),
            ),
            (
                "https://auth.itunes.apple.com/auth/v1/native/fast".to_owned(),
                "https://auth.itunes.apple.com/auth/v1/native/fast".to_owned(),
            ),
        ] {
            let config = valid_bag(&endpoint)
                .sap_config()
                .unwrap_or_else(|err| panic!("{endpoint}: {err}"));
            assert_eq!(config.auth_endpoint, expected, "{endpoint}");
        }
    }

    #[test]
    fn rejects_invalid_authentication_endpoints() {
        for endpoint in [
            format!("{AUTH}//"),
            format!("{AUTH}/extra"),
            format!("{AUTH}%2f"),
            "https://buy.itunes.apple.com/other".to_owned(),
            "https://example.com/WebObjects/MZFinance.woa/wa/authenticate".to_owned(),
            "http://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate".to_owned(),
            "https://user@buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate".to_owned(),
            "https://buy.itunes.apple.com:8443/WebObjects/MZFinance.woa/wa/authenticate".to_owned(),
            format!("{AUTH}#fragment"),
            format!("{AUTH}%zz"),
            "https://auth.itunes.apple.com/auth/v1/other".to_owned(),
            String::new(),
        ] {
            assert!(valid_bag(&endpoint).sap_config().is_err(), "{endpoint}");
        }
    }

    #[test]
    fn validates_sap_fields_with_go_messages() {
        let mut bag = valid_bag(AUTH);
        bag.sap_version = "invalid".to_owned();
        assert_eq!(
            bag.sap_config().unwrap_err().to_string(),
            r#"invalid SAP version "invalid" in bag: strconv.ParseUint: parsing "invalid": invalid syntax"#
        );
        bag.sap_version = "201".to_owned();
        assert_eq!(
            bag.sap_config().unwrap_err().to_string(),
            "unsupported SAP version 201 in bag"
        );
        let mut bag = valid_bag(AUTH);
        bag.sap_setup_endpoint = "http://fpinit.example.com".to_owned();
        assert_eq!(
            bag.sap_config().unwrap_err().to_string(),
            r#"invalid SAP setup endpoint "http://fpinit.example.com" in bag"#
        );
        assert_eq!(
            valid_bag(AUTH).sap_config().unwrap(),
            SapConfig {
                auth_endpoint: format!("{AUTH}/"),
                setup_url: SETUP.to_owned(),
                certificate_url: CERT.to_owned(),
                version: 200,
            }
        );
    }

    #[test]
    fn merges_nested_and_top_level_bags() {
        let top: Dictionary = plist::from_bytes(
            b"<plist><dict>
                <key>authenticateAccount</key><string>top-auth</string>
                <key>terms-check</key><string>top-terms</string>
            </dict></plist>",
        )
        .unwrap();
        let merged = UrlBag::from_bag(&top);
        assert_eq!(merged.auth_endpoint, "top-auth");
        assert_eq!(merged.terms_check_endpoint, "top-terms");

        let nested: Dictionary = plist::from_bytes(
            b"<plist><dict>
                <key>authenticateAccount</key><string>top-auth</string>
                <key>terms-check</key><string>top-terms</string>
                <key>urlBag</key><dict><key>sign-sap-version</key><string>200</string></dict>
            </dict></plist>",
        )
        .unwrap();
        let merged = UrlBag::from_bag(&nested);
        assert_eq!(merged.auth_endpoint, "");
        assert_eq!(merged.sap_version, "200");
        assert_eq!(merged.terms_check_endpoint, "top-terms");
    }

    #[test]
    fn builds_bag_urls() {
        assert_eq!(bag_url(""), "https://init.itunes.apple.com/bag.xml?ix=6");
        assert_eq!(
            bag_url("AABBCCDDEEFF"),
            "https://init.itunes.apple.com/bag.xml?ix=6&guid=AABBCCDDEEFF"
        );
    }
}
