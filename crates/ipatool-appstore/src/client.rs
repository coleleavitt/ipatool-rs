use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

use std::sync::{Arc, Mutex};

use crate::store::STORE_TIMEOUT;
use crate::storefront::StorefrontCache;
use crate::{CookieJar, Error, Result};

/// Same user agent as the Go client (`pkg/http/constants.go`).
pub(crate) const DEFAULT_USER_AGENT: &str =
    "Configurator/2.17 (Macintosh; OS X 15.2; 24C5089c) AppleWebKit/0620.1.16.11.6";

/// App Store client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    /// Public catalog endpoints, without cookies.
    pub(crate) http: reqwest::Client,
    /// Store endpoints: shares the cookie jar and, like Go's Store transport,
    /// returns redirects instead of following them.
    pub(crate) store: reqwest::Client,
    /// Login requests: like `store`, but on fresh connections, as Go's auth transport
    /// disables keep-alives.
    pub(crate) auth: reqwest::Client,
    pub(crate) jar: Option<Arc<CookieJar>>,
    pub(crate) storefronts: Arc<Mutex<StorefrontCache>>,
}

impl Client {
    /// A client without persistent cookies.
    ///
    /// # Errors
    /// Fails when the TLS backend cannot be initialized.
    pub fn new() -> Result<Self> {
        Self::build(None)
    }

    /// A client whose Store requests read and save `jar`, which is shared with the Go tool.
    ///
    /// # Errors
    /// Fails when the TLS backend cannot be initialized.
    pub fn with_cookie_jar(jar: Arc<CookieJar>) -> Result<Self> {
        Self::build(Some(jar))
    }

    fn build(jar: Option<Arc<CookieJar>>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        let http = reqwest::Client::builder()
            .default_headers(headers.clone())
            .build()?;
        let mut store = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(STORE_TIMEOUT);
        if let Some(jar) = &jar {
            store = store.cookie_provider(Arc::clone(jar));
        }
        let store = store.build()?;
        let mut auth = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(STORE_TIMEOUT)
            .pool_max_idle_per_host(0);
        if let Some(jar) = &jar {
            auth = auth.cookie_provider(Arc::clone(jar));
        }
        Ok(Self {
            http,
            store,
            auth: auth.build()?,
            jar,
            storefronts: Arc::default(),
        })
    }

    pub(crate) async fn get_bytes(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(u16, Vec<u8>, String)> {
        let response = request.send().await?;
        let status = response.status().as_u16();
        let url = response.url().to_string();
        let body = response.bytes().await?.to_vec();
        Ok((status, body, url))
    }

    pub(crate) async fn get_ok(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let (status, body, url) = self.get_bytes(request).await?;
        if status != 200 {
            return Err(Error::Status { status, url });
        }
        Ok(body)
    }
}
