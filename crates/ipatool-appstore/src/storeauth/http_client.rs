//! Go's `NewHTTPClient` and its Apple root fallback (`http_client.go`).

use std::error::Error as StdError;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use reqwest::redirect::{Attempt, Policy};
use reqwest::tls::{Certificate, Version};
use x509_cert::der::asn1::ObjectIdentifier;
use x509_cert::der::{Any, Decode, Encode, Tag, Tagged};
use x509_cert::ext::pkix::{BasicConstraints, KeyUsage};

use crate::{CookieJar, Error, Result};

const APPLE_ROOT_URL: &str = "https://www.apple.com/appleca/AppleIncRootCertificate.cer";
const ROOT_SIZE_LIMIT: usize = 64 << 10;
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const ROOT_TIMEOUT: Duration = Duration::from_secs(15);
const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
const ORGANIZATION: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.10");

/// The isolated HTTP client for Apple authentication: Go's `NewHTTPClient(jar)`. 30 s timeout,
/// redirects returned instead of followed, TLS 1.2 or newer, the shared cookie jar.
///
/// `gsa.apple.com` chains to Apple Root CA, which common system stores (including this
/// machine's) do not hold; the machine-provisioning requests then download Apple's root once,
/// as Go does, and every later request of this client trusts it too.
#[derive(Debug)]
pub struct AuthHttp {
    client: Mutex<reqwest::Client>,
    jar: Option<Arc<CookieJar>>,
}

impl AuthHttp {
    /// # Errors
    /// Fails when the TLS backend cannot be initialized.
    pub fn new(jar: Option<Arc<CookieJar>>) -> Result<Self> {
        Ok(Self {
            client: Mutex::new(build_client(jar.as_ref(), None)?),
            jar,
        })
    }

    /// The current client; it changes once the Apple root has been installed.
    #[must_use]
    pub fn client(&self) -> reqwest::Client {
        self.client
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Go's `installAppleRoot`: download Apple Root CA, check it and trust it in addition to
    /// the system roots.
    pub(crate) async fn install_apple_root(&self) -> Result<()> {
        let certificate = download_apple_root(APPLE_ROOT_URL).await?;
        let client = build_client(self.jar.as_ref(), Some(&certificate))?;
        *self.client.lock().unwrap_or_else(PoisonError::into_inner) = client;
        Ok(())
    }
}

fn build_client(jar: Option<&Arc<CookieJar>>, root: Option<&[u8]>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(AUTH_TIMEOUT)
        .redirect(Policy::none())
        .min_tls_version(Version::TLS_1_2);
    if let Some(jar) = jar {
        builder = builder.cookie_provider(Arc::clone(jar));
    }
    if let Some(root) = root {
        builder = builder.tls_certs_merge([Certificate::from_der(root)?]);
    }
    Ok(builder.build()?)
}

/// Go's `isUnknownAuthority`: the server chain does not reach a trusted root.
pub(crate) fn is_unknown_authority(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(error) = current {
        if matches!(
            error.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer
            ))
        ) {
            return true;
        }
        // `io::Error::source` skips the error it wraps, which is where rustls puts its own.
        current = match error.downcast_ref::<std::io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn StdError + 'static)),
            None => error.source(),
        };
    }
    false
}

async fn download_apple_root(url: &str) -> Result<Vec<u8>> {
    let downloader = reqwest::Client::builder()
        .timeout(ROOT_TIMEOUT)
        .min_tls_version(Version::TLS_1_2)
        .redirect(Policy::custom(apple_root_redirect))
        .build()?;
    let response = downloader
        .get(url)
        .send()
        .await
        .map_err(|error| Error::from(error).context("download Apple root"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(Error::Invalid(format!(
            "download Apple root: HTTP {status}"
        )));
    }
    let data = read_limited(response, ROOT_SIZE_LIMIT + 1)
        .await
        .map_err(|error| Error::from(error).context("read Apple root"))?;
    if data.len() > ROOT_SIZE_LIMIT {
        return Err(Error::Invalid("apple root exceeds size limit".to_owned()));
    }
    let certificate = x509_cert::Certificate::from_der(&data)
        .map_err(|error| Error::Invalid(format!("parse Apple root: {error}")))?;
    if !valid_apple_root(&certificate, SystemTime::now()) {
        return Err(Error::Invalid(
            "downloaded Apple root is invalid".to_owned(),
        ));
    }
    Ok(data)
}

/// Go's root download `CheckRedirect`: any number of hops, all to `https://www.apple.com`.
fn apple_root_redirect(attempt: Attempt) -> reqwest::redirect::Action {
    if trusted_root_location(attempt.url()) {
        attempt.follow()
    } else {
        attempt.error("apple root redirect is not trusted")
    }
}

fn trusted_root_location(url: &url::Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("www.apple.com")
}

/// Go's `validAppleRoot`.
pub(crate) fn valid_apple_root(certificate: &x509_cert::Certificate, now: SystemTime) -> bool {
    let tbs = certificate.tbs_certificate();
    let is_ca = matches!(
        tbs.get_extension::<BasicConstraints>(),
        Ok(Some((_, BasicConstraints { ca: true, .. })))
    );
    let cert_sign = matches!(
        tbs.get_extension::<KeyUsage>(),
        Ok(Some((_, usage))) if usage.key_cert_sign()
    );
    let validity = tbs.validity();
    let self_issued = matches!(
        (tbs.subject().to_der(), tbs.issuer().to_der()),
        (Ok(subject), Ok(issuer)) if subject == issuer
    );
    // Go's `CommonName` is the last CN attribute; `Organization` collects every O.
    let common_name = tbs
        .subject()
        .iter()
        .filter(|attribute| attribute.oid == COMMON_NAME)
        .filter_map(|attribute| directory_string(&attribute.value))
        .last();
    let apple_organization = tbs
        .subject()
        .iter()
        .filter(|attribute| attribute.oid == ORGANIZATION)
        .any(|attribute| directory_string(&attribute.value).as_deref() == Some("Apple Inc."));
    is_ca
        && cert_sign
        && common_name.as_deref() == Some("Apple Root CA")
        && self_issued
        && now >= validity.not_before.to_system_time()
        && now <= validity.not_after.to_system_time()
        && apple_organization
}

/// The string types Go's `parseASN1String` reads in names.
fn directory_string(value: &Any) -> Option<String> {
    let bytes = value.value();
    match value.tag() {
        Tag::PrintableString | Tag::Utf8String | Tag::Ia5String | Tag::NumericString => {
            String::from_utf8(bytes.to_vec()).ok()
        }
        Tag::TeletexString => Some(bytes.iter().map(|&byte| char::from(byte)).collect()),
        Tag::BmpString => {
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_be_bytes(*pair))
                .collect();
            String::from_utf16(&units).ok()
        }
        _ => None,
    }
}

/// Go's `io.ReadAll(io.LimitReader(body, limit))`.
pub(crate) async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
) -> reqwest::Result<Vec<u8>> {
    let mut data = Vec::new();
    while data.len() < limit {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let room = limit - data.len();
        data.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const APPLE_ROOT: &[u8] = include_bytes!("../../tests/fixtures/AppleIncRootCertificate.cer");

    fn parse(der: &[u8]) -> x509_cert::Certificate {
        x509_cert::Certificate::from_der(der).unwrap()
    }

    #[test]
    fn accepts_apple_root_inside_its_validity() {
        let root = parse(APPLE_ROOT);
        assert!(valid_apple_root(&root, SystemTime::now()));
        let not_before = root
            .tbs_certificate()
            .validity()
            .not_before
            .to_system_time();
        let not_after = root.tbs_certificate().validity().not_after.to_system_time();
        assert!(valid_apple_root(&root, not_before));
        assert!(valid_apple_root(&root, not_after));
        assert!(!valid_apple_root(
            &root,
            not_before - Duration::from_secs(1)
        ));
        assert!(!valid_apple_root(&root, not_after + Duration::from_secs(1)));
    }

    #[test]
    fn rejects_other_roots() {
        let others = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| x509_cert::Certificate::from_der(der).ok())
            .collect::<Vec<_>>();
        assert!(others.len() > 50);
        for other in &others {
            assert!(!valid_apple_root(other, SystemTime::now()));
        }
    }

    #[test]
    fn trusts_root_redirects_only_to_apple_over_https() {
        for (url, trusted) in [
            (
                "https://www.apple.com/appleca/AppleIncRootCertificate.cer",
                true,
            ),
            ("https://www.apple.com:8443/root.cer", true),
            (
                "http://www.apple.com/appleca/AppleIncRootCertificate.cer",
                false,
            ),
            ("https://apple.com/root.cer", false),
            ("https://www.apple.com.evil.example/root.cer", false),
        ] {
            assert_eq!(
                trusted_root_location(&url.parse().unwrap()),
                trusted,
                "{url}"
            );
        }
    }

    #[test]
    fn builds_with_and_without_the_apple_root() {
        build_client(None, None).unwrap();
        build_client(None, Some(APPLE_ROOT)).unwrap();
    }

    /// Whether reqwest's roots need the Apple root fallback for `gsa.apple.com`: the first
    /// handshake fails before anything is sent; after the fallback a GET of `/` gets through.
    #[tokio::test]
    #[ignore = "contacts gsa.apple.com and www.apple.com (read-only GETs)"]
    async fn gsa_needs_the_apple_root() {
        let http = AuthHttp::new(None).unwrap();
        let error = http
            .client()
            .get("https://gsa.apple.com/")
            .send()
            .await
            .unwrap_err();
        assert!(is_unknown_authority(&error), "{error:?}");
        http.install_apple_root().await.unwrap();
        http.client()
            .get("https://gsa.apple.com/")
            .send()
            .await
            .unwrap();
    }
}
