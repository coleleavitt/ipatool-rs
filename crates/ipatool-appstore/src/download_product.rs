//! Go's `appstore_download_product.go` and `appstore_kbsync.go`: the download request chain
//! (ent/download with a kbsync, volumeStoreDownloadProduct, redownload, updateProduct).

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ipatool_store::gourl;

use crate::commerce::{CommerceRequest, Payload};
use crate::download_types::{DownloadProduct, DownloadResult, sprint, string};
use crate::session::{
    DOWNLOAD_DISPATCH_DOMAIN, ENT_DOWNLOAD_PATH, REDOWNLOAD_PATH, StoreSession,
    UPDATE_PRODUCT_PATH, VOLUME_STORE_DOWNLOAD_PATH,
};
use crate::store::STORE_USER_AGENT;
use crate::version_lookup::IOS_CATALOG_APP_NOT_FOUND;
use crate::{Account, App, Error, Platform, Result, UrlBag, Value};

/// Go's `FailureType*` and `CustomerMessage*` constants.
pub(crate) mod failure {
    pub const PASSWORD_TOKEN_EXPIRED: &str = "2034";
    pub const SIGN_IN_REQUIRED: &str = "2042";
    pub const LICENSE_NOT_FOUND: &str = "9610";
    pub const TEMPORARILY_UNAVAILABLE: &str = "2059";
    pub const LICENSE_ALREADY_EXISTS: &str = "5002";
    pub const DEVICE_VERIFICATION_FAILED: &str = "1008";
    pub const SUBSCRIPTION_REQUIRED: &str = "Subscription Required";
    pub const PASSWORD_CHANGED: &str = "Your password has changed.";
}

/// An error with the response Go returns beside it.
type Failure = Box<(DownloadProduct, Error)>;

/// `http.DefaultAuthenticationTimeout`, which Go's request contexts apply here.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const VERSION_KEY_VOLUME_STORE: &str = "externalVersionId";
const VERSION_KEY_REDOWNLOAD: &str = "appExtVrsId";

/// Go's `downloadProductEndpoint`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub base_url: String,
    pub version_key: &'static str,
}

/// Go's `newDownloadEndpoint`.
pub(crate) fn new_download_endpoint(endpoint: &str, path: &str) -> Result<Endpoint> {
    let valid = gourl::parse_request_uri(endpoint).is_ok_and(|parsed| {
        parsed.scheme == "https"
            && parsed.host == DOWNLOAD_DISPATCH_DOMAIN
            && parsed.path == path
            && parsed.raw_path.is_empty()
            && parsed.raw_query.is_empty()
            && !parsed.force_query
            && parsed.fragment.is_empty()
            && parsed.user.is_none()
    });
    if !valid {
        return Err(Error::Invalid(
            "invalid download endpoint in bag".to_owned(),
        ));
    }
    Ok(Endpoint {
        base_url: endpoint.to_owned(),
        version_key: VERSION_KEY_REDOWNLOAD,
    })
}

/// Go's `volumeStoreEndpoint`.
pub(crate) fn volume_store_endpoint(account: &Account) -> Endpoint {
    Endpoint {
        base_url: format!(
            "https://{}buy.itunes.apple.com{VOLUME_STORE_DOWNLOAD_PATH}",
            pod_prefix(account)
        ),
        version_key: VERSION_KEY_VOLUME_STORE,
    }
}

pub(crate) fn pod_prefix(account: &Account) -> String {
    if account.pod.is_empty() {
        String::new()
    } else {
        format!("p{}-", account.pod)
    }
}

/// Go's `downloadProductRequest`.
pub(crate) fn download_product_request(
    endpoint: &Endpoint,
    account: &Account,
    app: &App,
    guid: &str,
    external_version_id: &str,
) -> CommerceRequest {
    let mut payload = BTreeMap::from([
        ("creditDisplay".to_owned(), Value::String(String::new())),
        ("guid".to_owned(), Value::String(guid.to_owned())),
        ("salableAdamId".to_owned(), Value::Integer(app.id)),
        ("serialNumber".to_owned(), Value::String("0".to_owned())),
    ]);
    if !external_version_id.is_empty() {
        payload.insert(
            endpoint.version_key.to_owned(),
            Value::String(external_version_id.to_owned()),
        );
    }
    let dsid = &account.directory_services_id;
    CommerceRequest {
        method: "POST".to_owned(),
        url: format!("{}?guid={guid}", endpoint.base_url),
        headers: BTreeMap::from([
            (
                "Content-Type".to_owned(),
                "application/x-apple-plist".to_owned(),
            ),
            ("User-Agent".to_owned(), STORE_USER_AGENT.to_owned()),
            ("iCloud-DSID".to_owned(), dsid.clone()),
            (
                "X-Apple-Store-Front".to_owned(),
                account.store_front.clone(),
            ),
            ("X-Dsid".to_owned(), dsid.clone()),
            ("X-Token".to_owned(), account.password_token.clone()),
        ]),
        payload: Some(Payload::Xml(payload)),
        no_redirects: true,
        ..CommerceRequest::default()
    }
}

/// Go's `normalizeDownloadFailure`.
pub(crate) fn normalize_download_failure(mut data: DownloadResult) -> DownloadResult {
    if data.items.is_empty()
        && data.failure_type.is_empty()
        && let Some(code) = data.metrics.get("messageCode")
    {
        sprint(Some(code)).trim().clone_into(&mut data.failure_type);
    }
    if data.failure_type == failure::SIGN_IN_REQUIRED
        || data.failure_type == failure::DEVICE_VERIFICATION_FAILED
    {
        failure::PASSWORD_TOKEN_EXPIRED.clone_into(&mut data.failure_type);
    }
    if !data.items.is_empty() {
        return data;
    }
    let message = fields(&data.customer_message);
    if message.eq_ignore_ascii_case("sign in to the itunes store")
        || message.eq_ignore_ascii_case("sign-in required")
    {
        failure::PASSWORD_TOKEN_EXPIRED.clone_into(&mut data.failure_type);
    }
    if data.failure_type == failure::PASSWORD_TOKEN_EXPIRED {
        return data;
    }
    #[allow(clippy::items_after_statements, reason = "Go's local constant")]
    const UNAVAILABLE: &str = "Redownload Unavailable with This Apple Account";
    let dialog_message = fields(&string(&data.dialog, "message"));
    let dialog_explanation = fields(&string(&data.dialog, "explanation"));
    if message == UNAVAILABLE
        || (dialog_message == UNAVAILABLE
            && dialog_explanation
                == "This redownload is not available for this Apple Account either because it was bought by a different user or the item was refunded or cancelled.")
    {
        failure::LICENSE_NOT_FOUND.clone_into(&mut data.failure_type);
    }
    data
}

/// Go's `strings.Join(strings.Fields(s), " ")`.
fn fields(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Go's `isEmptyRedownloadError`.
fn is_empty_redownload_error(error: &Error) -> bool {
    error
        .unexpected_response()
        .is_some_and(|unexpected| unexpected.status == 500 && unexpected.snippet.is_empty())
}

/// Go's `isUnavailableDownloadProductResponse`.
fn is_unavailable(response: &DownloadProduct) -> bool {
    let message = response.data.customer_message.trim().to_lowercase();
    response.status == 200
        && response.data.failure_type.is_empty()
        && response.data.items.is_empty()
        && (message == "no longer available" || message.ends_with(" no longer available"))
}

/// Go's `isEmptyDownloadProductResponse`.
fn is_empty(response: &DownloadProduct) -> bool {
    response.status == 200
        && response.data.failure_type.is_empty()
        && response.data.customer_message.is_empty()
        && response.data.items.is_empty()
}

/// Go's `NewErrorWithMetadata(err, res)`.
pub(crate) fn store_error(message: impl Into<String>, response: &DownloadProduct) -> Error {
    Error::Store {
        message: message.into(),
        status: response.status,
    }
}

/// Go's `validateDownloadResponse`.
pub(crate) fn validate_download_response(
    response: &DownloadProduct,
    app: &App,
    external_version_id: &str,
    source: &str,
) -> Result<()> {
    let data = &response.data;
    if !data.failure_type.is_empty() {
        return Ok(());
    }
    if !data.customer_message.is_empty() {
        return Err(store_error(
            format!("received {source} error: {}", data.customer_message),
            response,
        ));
    }
    if response.status != 200 {
        return Err(Error::Invalid(format!(
            "received unexpected {source} status code: {}",
            response.status
        )));
    }
    let [item] = data.items.as_slice() else {
        return Err(Error::Invalid(format!(
            "{source} response must contain exactly one item"
        )));
    };
    let metadata = &item.metadata;
    if sprint(metadata.get("itemId")) != app.id.to_string()
        || (!external_version_id.is_empty()
            && sprint(metadata.get("softwareVersionExternalIdentifier")) != external_version_id)
    {
        return Err(Error::Invalid(format!(
            "{source} response does not match the requested app or version"
        )));
    }
    let bundle_id = match metadata.get("softwareVersionBundleId") {
        Some(plist::Value::String(id)) => id.as_str(),
        _ => "",
    };
    if bundle_id.is_empty() || (!app.bundle_id.is_empty() && bundle_id != app.bundle_id) {
        return Err(Error::Invalid(format!(
            "{source} response does not match the requested bundle identifier"
        )));
    }
    Ok(())
}

fn is_ios(platform: Option<Platform>) -> bool {
    matches!(platform, None | Some(Platform::IPhone | Platform::IPad))
}

impl StoreSession<'_> {
    /// Go's `sendDownloadProduct`: the download response for `app` and the platform the
    /// package was resolved for.
    ///
    /// # Errors
    /// Go's request, lookup and validation errors.
    pub async fn download_product(
        &self,
        account: &Account,
        app: &App,
        guid: &str,
        external_version_id: &str,
        platform: Option<Platform>,
    ) -> Result<(DownloadProduct, Option<Platform>)> {
        let mut external_version_id = external_version_id.to_owned();
        let mut platform = platform;
        // An unpinned request can return the iOS build of a universal app.
        if external_version_id.is_empty() && platform == Some(Platform::MacOs) {
            external_version_id = self
                .client
                .lookup_latest_macos_external_version_id(account, app)
                .await
                .map_err(|error| {
                    error.context("failed to resolve latest macOS version for download")
                })?;
        }

        // Prefer ent/download; keep the bag for the legacy chain if it cannot serve the app.
        let mut bag: Option<UrlBag> = None;
        if self.kbsync_generator.is_some()
            && let Ok(fetched) = self.client.url_bag(guid).await
        {
            if !fetched.ent_download_endpoint.is_empty()
                && let Ok(result) = self
                    .send_preferred_download(
                        &fetched.ent_download_endpoint,
                        account,
                        app,
                        guid,
                        &external_version_id,
                        platform,
                    )
                    .await
            {
                return Ok(result);
            }
            bag = Some(fetched);
        }

        let request = download_product_request(
            &volume_store_endpoint(account),
            account,
            app,
            guid,
            &external_version_id,
        );
        let response = self
            .send_commerce_download_request(request, guid)
            .await
            .map_err(|error| error.context("failed to send http request"))?;
        if !is_empty(&response) && !is_unavailable(&response) {
            return Ok((response, platform));
        }

        // Try redownload when volumeStore returns no items, silently or with the
        // message-only "No Longer Available" response.
        let bag = match bag {
            Some(bag) => bag,
            None => self
                .client
                .url_bag(guid)
                .await
                .map_err(|error| error.context("failed to get bag for redownload fallback"))?,
        };
        if bag.redownload_endpoint.is_empty() {
            return Ok((response, platform));
        }
        let redownload = new_download_endpoint(&bag.redownload_endpoint, REDOWNLOAD_PATH)?;

        if external_version_id.is_empty() && is_ios(platform) {
            // Unpinned redownloads can fail or return a tvOS package.
            let resolved = platform.unwrap_or(Platform::IPhone);
            platform = Some(resolved);
            match self
                .client
                .lookup_latest_external_version_id(account, app, resolved)
                .await
            {
                Ok(version) => external_version_id = version,
                Err(error) if error.has_message(IOS_CATALOG_APP_NOT_FOUND) => {
                    // A delisted app can still be available to this account.
                    let response = self
                        .send_delisted_download(
                            &redownload,
                            &bag.ent_download_endpoint,
                            account,
                            app,
                            guid,
                        )
                        .await?;
                    return Ok((response, platform));
                }
                Err(error) => {
                    return Err(
                        error.context("failed to resolve latest iOS version for redownload")
                    );
                }
            }
        }

        let request =
            download_product_request(&redownload, account, app, guid, &external_version_id);
        let result = self.send_commerce_download_request(request, guid).await;
        let update_needed = match &result {
            Err(error) => is_empty_redownload_error(error),
            Ok(response) => is_unavailable(response),
        };
        if !bag.update_endpoint.is_empty()
            && !external_version_id.is_empty()
            && platform != Some(Platform::VisionOs)
            && platform != Some(Platform::Unknown)
            && update_needed
        {
            let response = self
                .send_update_product(
                    &bag.update_endpoint,
                    account,
                    app,
                    guid,
                    &external_version_id,
                )
                .await?;
            return Ok((response, platform.or(Some(Platform::IPhone))));
        }
        let response =
            result.map_err(|error| error.context("failed to send redownload request"))?;
        Ok((response, platform))
    }

    /// Go's `sendDelistedDownload`.
    async fn send_delisted_download(
        &self,
        redownload: &Endpoint,
        ent_endpoint: &str,
        account: &Account,
        app: &App,
        guid: &str,
    ) -> Result<DownloadProduct> {
        let (response, error) = match self
            .send_unpinned_redownload(redownload, account, app, guid)
            .await
        {
            Ok(response) if response.data.failure_type != failure::LICENSE_NOT_FOUND => {
                return Ok(response);
            }
            Ok(response) => (response, None),
            Err(failure) => {
                let (response, error) = *failure;
                (response, Some(error))
            }
        };
        let fail = |error: Option<Error>| match error {
            Some(error) => Err(error),
            None => Ok(response.clone()),
        };
        if self.kbsync_generator.is_none() || ent_endpoint.is_empty() {
            return fail(error);
        }
        let endpoint = match new_download_endpoint(ent_endpoint, ENT_DOWNLOAD_PATH) {
            Ok(endpoint) => endpoint,
            Err(endpoint_error) => {
                return Err(Error::Joined(
                    error.into_iter().chain([endpoint_error]).collect(),
                ));
            }
        };
        // Catalog discovery already confirmed that there is no offer.
        let ent = self
            .send_ent_download(&endpoint, account, app, guid, "")
            .await;
        match ent {
            Ok(ent) => Ok(ent),
            Err(failure) => {
                let (ent, ent_error) = *failure;
                if ent.data.failure_type == failure::PASSWORD_TOKEN_EXPIRED
                    || ent.data.failure_type == failure::LICENSE_NOT_FOUND
                {
                    return Ok(ent);
                }
                if response.data.failure_type == failure::LICENSE_NOT_FOUND {
                    return Ok(response);
                }
                Err(Error::Joined(error.into_iter().chain([ent_error]).collect()).context(
                    format!(
                        "app {} has no offer in the account's App Store catalog and the kbsync fallback failed",
                        app.id
                    ),
                ))
            }
        }
    }

    /// Go's `sendUnpinnedRedownload`. Errors keep the response, as Go returns both.
    async fn send_unpinned_redownload(
        &self,
        endpoint: &Endpoint,
        account: &Account,
        app: &App,
        guid: &str,
    ) -> std::result::Result<DownloadProduct, Failure> {
        let mut request = download_product_request(endpoint, account, app, guid, "");
        request.timeout = Some(REQUEST_TIMEOUT);
        let mut response = self
            .send_commerce_download_request(request, guid)
            .await
            .map_err(|error| {
                Box::new((
                    DownloadProduct::default(),
                    error.context("failed to send redownload request"),
                ))
            })?;
        response.data = normalize_download_failure(std::mem::take(&mut response.data));
        if let Err(error) = validate_download_response(&response, app, "", "redownload") {
            return Err(Box::new((response, error)));
        }
        if response.data.failure_type.is_empty() && response.data.items[0].url.is_empty() {
            return Err(Box::new((
                response,
                Error::Invalid("redownload response has no package URL".to_owned()),
            )));
        }
        Ok(response)
    }

    /// Go's `sendUpdateProductContext`.
    async fn send_update_product(
        &self,
        endpoint: &str,
        account: &Account,
        app: &App,
        guid: &str,
        external_version_id: &str,
    ) -> Result<DownloadProduct> {
        let update = new_download_endpoint(endpoint, UPDATE_PRODUCT_PATH)?;
        let request = download_product_request(&update, account, app, guid, external_version_id);
        let response = self
            .send_commerce_download_request(request, guid)
            .await
            .map_err(|error| error.context("failed to send update request"))?;
        validate_download_response(&response, app, external_version_id, "update")?;
        Ok(response)
    }

    /// Go's `sendCommerceDownloadRequest`: the Store AMD proof, then the `request` executor.
    async fn send_commerce_download_request(
        &self,
        mut request: CommerceRequest,
        guid: &str,
    ) -> Result<DownloadProduct> {
        let hardware_id = decode_guid(guid).ok_or_else(|| {
            Error::Invalid("store download requires a six-byte device GUID".to_owned())
        })?;
        let proof = self.store_amd_proof(&request.url, &hardware_id).await?;
        request.headers.extend(proof);
        self.send_download(request)
            .await
            .map_err(|error| error.context("send Store commerce request"))
    }

    async fn send_download(&self, request: CommerceRequest) -> Result<DownloadProduct> {
        let response = self
            .client
            .store_xml::<plist::Dictionary>("request", request)
            .await?;
        Ok(DownloadProduct {
            status: response.status,
            data: DownloadResult::from_plist(&response.data),
        })
    }

    /// Go's `sendPreferredDownload`.
    async fn send_preferred_download(
        &self,
        endpoint: &str,
        account: &Account,
        app: &App,
        guid: &str,
        version_id: &str,
        platform: Option<Platform>,
    ) -> Result<(DownloadProduct, Option<Platform>)> {
        let endpoint = new_download_endpoint(endpoint, ENT_DOWNLOAD_PATH)?;
        let platform = platform.unwrap_or(Platform::IPhone);
        let version_id = if version_id.is_empty() {
            self.client
                .lookup_latest_external_version_id(account, app, platform)
                .await
                .map_err(|error| {
                    error.context("failed to resolve platform version for ent/download")
                })?
        } else {
            version_id.to_owned()
        };
        let response = self
            .send_ent_download(&endpoint, account, app, guid, &version_id)
            .await
            .map_err(|failure| failure.1)?;
        Ok((response, Some(platform)))
    }

    /// Go's `sendEntDownload`: a cached kbsync first, retried once with a fresh blob.
    async fn send_ent_download(
        &self,
        endpoint: &Endpoint,
        account: &Account,
        app: &App,
        guid: &str,
        version_id: &str,
    ) -> std::result::Result<DownloadProduct, Failure> {
        let bare = |error: Error| Box::new((DownloadProduct::default(), error));
        let hardware_id = decode_guid(guid).ok_or_else(|| {
            bare(Error::Invalid(
                "ent/download requires a six-byte device GUID".to_owned(),
            ))
        })?;
        let dsid = account
            .directory_services_id
            .parse::<u64>()
            .ok()
            .filter(|dsid| *dsid != 0)
            .ok_or_else(|| {
                bare(Error::Invalid(
                    "ent/download requires a nonzero numeric account DSID".to_owned(),
                ))
            })?;
        let cached = self.cached_kbsync(dsid, guid).await;
        if !cached.is_empty() {
            if let Ok(response) = self
                .send_ent_download_request(
                    endpoint,
                    account,
                    app,
                    guid,
                    version_id,
                    &hardware_id,
                    &cached,
                )
                .await
            {
                return Ok(response);
            }
            // Retry a rejected cached blob once; fresh blobs are never retried.
            self.invalidate_kbsync(dsid, guid, &cached).await;
        }
        let blob = self.generate_kbsync(&hardware_id, dsid).map_err(bare)?;
        let response = self
            .send_ent_download_request(
                endpoint,
                account,
                app,
                guid,
                version_id,
                &hardware_id,
                &blob,
            )
            .await?;
        self.store_kbsync(dsid, guid, &blob).await;
        Ok(response)
    }

    /// Go's `sendEntDownloadRequest`.
    #[allow(clippy::too_many_arguments, reason = "Go's parameter list")]
    async fn send_ent_download_request(
        &self,
        endpoint: &Endpoint,
        account: &Account,
        app: &App,
        guid: &str,
        version_id: &str,
        hardware_id: &[u8],
        blob: &str,
    ) -> std::result::Result<DownloadProduct, Failure> {
        let bare = |error: Error| Box::new((DownloadProduct::default(), error));
        let mut serial = vec![0x54, 0xc8, 0xb0, 0xa9, 0x88];
        serial.extend_from_slice(&hardware_id[2..]);
        let mut request = download_product_request(endpoint, account, app, guid, "");
        request.timeout = Some(REQUEST_TIMEOUT);
        // X-Token must not be forwarded to redirect destinations.
        request.no_redirects = true;
        request.headers.insert(
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded; charset=utf-8".to_owned(),
        );
        let proof = self
            .store_amd_proof(&request.url, hardware_id)
            .await
            .map_err(bare)?;
        request.headers.extend(proof);
        let mut payload = BTreeMap::from([
            ("creditDisplay".to_owned(), Value::String(String::new())),
            ("guid".to_owned(), Value::String(guid.to_owned())),
            ("kbsync".to_owned(), Value::String(blob.to_owned())),
            (
                "salableAdamId".to_owned(),
                Value::String(app.id.to_string()),
            ),
            (
                "serialNumber".to_owned(),
                Value::String(STANDARD.encode(serial)),
            ),
        ]);
        if !version_id.is_empty() {
            payload.insert(
                "externalVersionId".to_owned(),
                Value::String(version_id.to_owned()),
            );
        }
        request.payload = Some(Payload::Xml(payload));
        let mut response = self
            .send_download(request)
            .await
            .map_err(|error| bare(error.context("send ent/download request")))?;
        response.data = normalize_download_failure(std::mem::take(&mut response.data));
        if !response.data.failure_type.is_empty() {
            let error = store_error(
                format!(
                    "received ent/download error: {}",
                    response.data.failure_type
                ),
                &response,
            );
            return Err(Box::new((response, error)));
        }
        if let Err(error) = validate_download_response(&response, app, version_id, "ent/download") {
            return Err(Box::new((response, error)));
        }
        if response.data.items[0].url.is_empty() {
            return Err(Box::new((
                response,
                Error::Invalid("ent/download response has no package URL".to_owned()),
            )));
        }
        Ok(response)
    }
}

/// Go's `hex.DecodeString(guid)` of a six-byte device GUID.
pub(crate) fn decode_guid(guid: &str) -> Option<Vec<u8>> {
    if guid.len() != 12 || !guid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..12)
        .step_by(2)
        .map(|i| u8::from_str_radix(&guid[i..i + 2], 16).ok())
        .collect()
}
