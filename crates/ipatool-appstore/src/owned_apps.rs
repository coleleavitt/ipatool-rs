//! Go's `appstore_owned_apps.go`: the apps the account owns, from the Configurator purchase
//! history (DAAP over `pd.itunes.apple.com`), merged across the mobile and Mac storefronts,
//! sorted by purchase date and paged.

use std::collections::{BTreeMap, HashMap};

use ipatool_sap::signer::ActionSigner;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::commerce::{CommerceRequest, Payload, StoreResult};
use crate::login::{Live, SapBackend};
use crate::store::STORE_USER_AGENT;
use crate::{Account, App, Client, Error, Platform, Result, machine};

pub const DEFAULT_OWNED_APPS_LIMIT: i64 = 10;
pub const MAX_OWNED_APPS_LIMIT: i64 = 100;

const OWNED_APPS_MEDIA_KIND: u64 = 131_072;
const OWNED_ARCADE_APPS_MEDIA_KIND: u64 = 262_144;
const OWNED_MAC_APPS_MEDIA_KIND: u64 = 67_108_864;

/// `PrivatePurchaseDAAPBaseURL`.
pub(crate) const PRIVATE_PURCHASE_DAAP_BASE_URL: &str =
    "https://pd.itunes.apple.com/WebObjects/MZPurchaseDaap.woa/purchase";

/// Go's `http.TimeFormat`.
const HTTP_DATE: &[FormatItem<'_>] = format_description!(
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
);
const CLIENT_TIME: &[FormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// Go's `OwnedAppsInput`. A zero page or limit takes the default; `None` lists every platform.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnedAppsInput {
    pub account: Account,
    pub page: i64,
    pub limit: i64,
    pub platform: Option<Platform>,
}

/// Go's `OwnedAppsOutput`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnedAppsOutput {
    pub count: usize,
    pub total_count: usize,
    pub page: i64,
    pub results: Vec<App>,
}

impl Client {
    /// Go's `OwnedApps`: the account's apps, newest purchase first, one page of them.
    ///
    /// The SAP signer runs Apple's code on this thread, so the future is not `Send`.
    ///
    /// # Errors
    /// Go's validation errors; `password token is expired: …` when Apple rejects the token
    /// ([`Error::is_password_token_expired`]); request, DAAP and signer errors with Go's text.
    pub async fn owned_apps(&self, input: OwnedAppsInput) -> Result<OwnedAppsOutput> {
        owned_apps(self, &Live, input, OffsetDateTime::now_utc).await
    }
}

pub(crate) async fn owned_apps<B: SapBackend>(
    client: &Client,
    backend: &B,
    input: OwnedAppsInput,
    now: fn() -> OffsetDateTime,
) -> Result<OwnedAppsOutput> {
    let input = normalize_owned_apps_input(input)?;
    let mac_address = backend
        .mac_address()
        .map_err(|error| error.context("failed to get mac address"))?;
    let (guid, machine_id) = machine::identity(&mac_address)?;
    let config = backend
        .sap_config(client, &guid)
        .await
        .map_err(|error| error.context("failed to get bag"))?;
    let mut signer = backend
        .create_signer(client, &config, &machine_id)
        .await
        .map_err(|error| error.context("failed to initialize SAP action signer"))?;
    let fetched = OwnedAppsFetch {
        client,
        guid: &guid,
        signer: &mut signer,
        now,
    }
    .fetch_owned_apps(&input.account)
    .await;
    let close = signer
        .close()
        .map_err(|error| Error::from(error).context("failed to close SAP action signer"));
    let mut apps = match (fetched, close) {
        (Ok(apps), Ok(())) => apps,
        (Ok(_), Err(close)) => return Err(close),
        (Err(fetch), Ok(())) => return Err(fetch),
        (Err(fetch), Err(close)) => return Err(Error::Joined(vec![fetch, close])),
    };
    if let Some(platform) = input.platform {
        apps.retain(|app| app.platforms.contains(&platform));
    }
    let apps = owned_apps_sorted_by_purchase_date(apps);
    let results = owned_apps_page(&apps, input.page, input.limit);
    Ok(OwnedAppsOutput {
        count: results.len(),
        total_count: apps.len(),
        page: input.page,
        results,
    })
}

fn normalize_owned_apps_input(mut input: OwnedAppsInput) -> Result<OwnedAppsInput> {
    match input.platform {
        None
        | Some(
            Platform::IPhone
            | Platform::IPad
            | Platform::AppleTv
            | Platform::VisionOs
            | Platform::MacOs,
        ) => {}
        Some(other) => return Err(Error::InvalidPlatform(other.as_str().to_owned())),
    }
    if input.page == 0 {
        input.page = 1;
    }
    if input.limit == 0 {
        input.limit = DEFAULT_OWNED_APPS_LIMIT;
    }
    if input.page < 1 {
        return Err(Error::Invalid("page must be greater than 0".to_owned()));
    }
    if input.limit < 1 {
        return Err(Error::Invalid("limit must be greater than 0".to_owned()));
    }
    if input.limit > MAX_OWNED_APPS_LIMIT {
        return Err(Error::Invalid(format!(
            "limit must not exceed {MAX_OWNED_APPS_LIMIT}"
        )));
    }
    Ok(input)
}

/// The purchase-history requests of one `OwnedApps` call.
struct OwnedAppsFetch<'a, S> {
    client: &'a Client,
    guid: &'a str,
    signer: &'a mut S,
    now: fn() -> OffsetDateTime,
}

impl<S: ActionSigner> OwnedAppsFetch<'_, S> {
    /// Go's `fetchOwnedApps`.
    async fn fetch_owned_apps(&mut self, account: &Account) -> Result<Vec<App>> {
        let mut apps = Vec::new();
        let (store_front, _) = account
            .store_front
            .split_once(',')
            .unwrap_or((&account.store_front, ""));
        // Configurator (34) lists mobile apps, while the Mac App Store (13)
        // also lists Mac software and mobile apps available on Apple silicon.
        for store in ["34", "13"] {
            let account = Account {
                store_front: format!("{store_front},{store}"),
                ..account.clone()
            };
            apps.extend(self.fetch_owned_apps_for_storefront(&account).await?);
        }
        Ok(merge_owned_apps(apps))
    }

    /// Go's `fetchOwnedAppsForStorefront`.
    async fn fetch_owned_apps_for_storefront(&mut self, account: &Account) -> Result<Vec<App>> {
        let login = self
            .send(owned_apps_login_request(account, self.guid, (self.now)()))
            .await
            .map_err(|error| error.context("failed to open purchase history session"))?;
        check_response("purchase history login", &login)?;
        let session_id = match first_dmap_uint(&login.data, "mlid").map_err(|error| {
            parse_error("failed to parse purchase history login response", &error)
        })? {
            Some(id) => u32::try_from(id).ok(),
            None => None,
        }
        .ok_or_else(|| {
            Error::Invalid(
                "purchase history login response did not contain a valid session ID".to_owned(),
            )
        })?;

        let query = format!(
            "('com.apple.itunes.extended\\-media\\-kind:{OWNED_APPS_MEDIA_KIND}','com.apple.itunes.extended\\-media\\-kind:{OWNED_ARCADE_APPS_MEDIA_KIND}','com.apple.itunes.extended\\-media\\-kind:{OWNED_MAC_APPS_MEDIA_KIND}')"
        );
        let request = self.signed(owned_apps_update_request(
            account,
            self.guid,
            session_id,
            &query,
            (self.now)(),
        ));
        let update = self
            .send(request)
            .await
            .map_err(|error| error.context("failed to update purchase history"))?;
        check_response("purchase history update", &update)?;
        let latest_version = match first_dmap_uint(&update.data, "musr").map_err(|error| {
            parse_error("failed to parse purchase history update response", &error)
        })? {
            Some(version) => u32::try_from(version).ok(),
            None => None,
        }
        .ok_or_else(|| {
            Error::Invalid(
                "purchase history update response did not contain a valid revision".to_owned(),
            )
        })?;

        let request = self.signed(owned_apps_items_request(
            account,
            self.guid,
            session_id,
            latest_version,
            &query,
            (self.now)(),
        ));
        let items = self
            .send(request)
            .await
            .map_err(|error| error.context("failed to retrieve purchase history items"))?;
        check_response("purchase history items", &items)?;
        parse_owned_apps(&items.data)
            .map_err(|error| parse_error("failed to parse purchase history items response", &error))
    }

    async fn send(&self, request: CommerceRequest) -> Result<StoreResult<Vec<u8>>> {
        self.client.store_raw("request", request).await
    }

    /// Go's `ActionSigner` on the request: the signature of its raw body.
    fn signed(&mut self, mut request: CommerceRequest) -> CommerceRequest {
        if let Some(Payload::Raw(body)) = &request.payload {
            request.signature = Some(self.signer.sign(body).map_err(|error| error.to_string()));
        }
        request
    }
}

fn parse_error(context: &str, message: &str) -> Error {
    Error::Invalid(format!("{context}: {message}"))
}

/// Go's `ownedAppsResponseError` and `ownedAppsDMAPStatusError`.
fn check_response(label: &str, result: &StoreResult<Vec<u8>>) -> Result<()> {
    if matches!(result.status, 401 | 403) {
        return Err(token_expired(&format!(
            "{label} request returned HTTP {}",
            result.status
        )));
    }
    if result.status != 200 {
        return Err(Error::Invalid(format!(
            "{label} request returned HTTP {}",
            result.status
        )));
    }
    owned_apps_dmap_status_error(label, &result.data)
}

fn owned_apps_dmap_status_error(label: &str, data: &[u8]) -> Result<()> {
    let status = first_dmap_uint(data, "mstt")
        .map_err(|error| parse_error(&format!("failed to parse {label} response"), &error))?;
    match status {
        Some(status @ (401 | 403)) => Err(token_expired(&format!(
            "{label} response returned DAAP status {status}"
        ))),
        Some(status) if status != 200 => Err(Error::Invalid(format!(
            "{label} response returned DAAP status {status}"
        ))),
        _ => Ok(()),
    }
}

/// Go's `fmt.Errorf("%w: …", ErrPasswordTokenExpired)`.
fn token_expired(detail: &str) -> Error {
    Error::Wrapped {
        message: format!("{}: {detail}", Error::PasswordTokenExpired),
        sources: vec![Error::PasswordTokenExpired],
    }
}

fn owned_apps_login_request(account: &Account, guid: &str, now: OffsetDateTime) -> CommerceRequest {
    CommerceRequest {
        method: "POST".to_owned(),
        url: format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/login"),
        headers: owned_apps_headers(account, guid, now),
        ..CommerceRequest::default()
    }
}

fn owned_apps_update_request(
    account: &Account,
    guid: &str,
    session_id: u32,
    query: &str,
    now: OffsetDateTime,
) -> CommerceRequest {
    let mut headers = owned_apps_headers(account, guid, now);
    headers.insert(
        "Content-Type".to_owned(),
        "application/x-www-form-urlencoded".to_owned(),
    );
    CommerceRequest {
        method: "POST".to_owned(),
        url: format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/update"),
        headers,
        payload: Some(Payload::Raw(
            format!("session-id={session_id}&revision-number=(null)&query={query}").into_bytes(),
        )),
        ..CommerceRequest::default()
    }
}

fn owned_apps_items_request(
    account: &Account,
    guid: &str,
    session_id: u32,
    latest_version: u32,
    query: &str,
    now: OffsetDateTime,
) -> CommerceRequest {
    let mut headers = owned_apps_headers(account, guid, now);
    headers.insert(
        "Content-Type".to_owned(),
        "application/x-dmap-tagged".to_owned(),
    );
    CommerceRequest {
        method: "POST".to_owned(),
        url: format!("{PRIVATE_PURCHASE_DAAP_BASE_URL}/databases/{latest_version}/items"),
        headers,
        payload: Some(Payload::Raw(owned_apps_items_body(
            session_id,
            latest_version,
            query,
            now,
        ))),
        ..CommerceRequest::default()
    }
}

/// Go's `ownedAppsHeaders`. `now` is UTC; the zone fields describe the local zone, as Go's
/// `time.Now()` does.
fn owned_apps_headers(
    account: &Account,
    guid: &str,
    now: OffsetDateTime,
) -> BTreeMap<String, String> {
    let offset = UtcOffset::local_offset_at(now).unwrap_or(UtcOffset::UTC);
    let utc = now.to_offset(UtcOffset::UTC);
    [
        ("Accept", "*/*".to_owned()),
        ("Accept-Language", "en-us".to_owned()),
        ("Client-Cloud-DAAP-Request-Reason", "5".to_owned()),
        (
            "Client-Cloud-Purchase-Daap-Version",
            "1.1/Configurator-2.0".to_owned(),
        ),
        ("Client-DAAP-Version", "3.12".to_owned()),
        ("Date", utc.format(HTTP_DATE).unwrap_or_default()),
        ("User-Agent", STORE_USER_AGENT.to_owned()),
        ("iCloud-DSID", account.directory_services_id.clone()),
        (
            "X-Apple-I-Client-Time",
            utc.format(CLIENT_TIME).unwrap_or_default(),
        ),
        ("X-Apple-I-Locale", "en_US".to_owned()),
        (
            "X-Apple-I-TimeZone",
            local_location_name(std::env::var("TZ").ok().as_deref()),
        ),
        ("X-Apple-Store-Front", account.store_front.clone()),
        ("X-Apple-TZ", (offset.whole_seconds() / 60).to_string()),
        ("X-Dsid", account.directory_services_id.clone()),
        ("X-Guid", guid.to_owned()),
        ("X-Token", account.password_token.clone()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

/// Go's `time.Local.String()` as `initLocal` names it on Unix: `Local` for the system zone,
/// the `TZ` value (without a leading `:`) when it names another zone, `UTC` for an empty one.
fn local_location_name(tz: Option<&str>) -> String {
    match tz {
        None => "Local".to_owned(),
        Some("") => "UTC".to_owned(),
        Some(tz) => match tz.strip_prefix(':').unwrap_or(tz) {
            "" => "UTC".to_owned(),
            "/etc/localtime" => "Local".to_owned(),
            name => name.to_owned(),
        },
    }
}

/// Go's `ownedAppsItemsBody`.
fn owned_apps_items_body(
    session_id: u32,
    latest_version: u32,
    query: &str,
    now: OffsetDateTime,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(73 + query.len());
    // Go's `uint32(now.Unix())` wraps.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Go's uint32 conversion"
    )]
    payload.extend(dmap_uint32("mstc", now.unix_timestamp() as u32));
    payload.extend(dmap_uint32("mlid", session_id));
    payload.extend(dmap_tag("mikd", &[2]));
    payload.extend(dmap_uint32("musr", latest_version));
    payload.extend(dmap_uint32("mder", 0));
    payload.extend(dmap_tag("mque", query.as_bytes()));
    payload.extend(dmap_tag("aetl", &[]));
    dmap_tag("adsr", &payload)
}

pub(crate) fn dmap_tag(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(8 + payload.len());
    let mut tag = [0u8; 4];
    for (slot, byte) in tag.iter_mut().zip(name.bytes()) {
        *slot = byte;
    }
    result.extend_from_slice(&tag);
    // Go's `uint32(len(payload))`; DMAP tags are far below 4 GiB.
    #[allow(clippy::cast_possible_truncation, reason = "Go's uint32 conversion")]
    result.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    result.extend_from_slice(payload);
    result
}

pub(crate) fn dmap_uint32(name: &str, value: u32) -> Vec<u8> {
    dmap_tag(name, &value.to_be_bytes())
}

/// Go's `firstDMAPUint`. Errors are Go's text without a context.
fn first_dmap_uint(data: &[u8], target: &str) -> std::result::Result<Option<u64>, String> {
    let mut result = None;
    walk_dmap(data, 0, &mut |tag, payload| {
        if result.is_some() || tag != target {
            return Ok(());
        }
        result = Some(match payload.len() {
            4 => u64::from(be_u32(payload)),
            8 => be_u64(payload),
            length => {
                return Err(format!("tag {target} has invalid integer length {length}"));
            }
        });
        Ok(())
    })?;
    Ok(result)
}

/// Go's `parseOwnedApps`.
fn parse_owned_apps(data: &[u8]) -> std::result::Result<Vec<App>, String> {
    let mut apps = Vec::new();
    walk_dmap(data, 0, &mut |tag, payload| {
        if tag != "mlit" {
            return Ok(());
        }
        let app = parse_owned_app(payload)?;
        if app.id != 0 {
            apps.push(app);
        }
        Ok(())
    })?;
    Ok(merge_owned_apps(apps))
}

/// Go's `mergeOwnedApps`: one entry per ID with every platform and the latest purchase date.
fn merge_owned_apps(apps: Vec<App>) -> Vec<App> {
    let mut merged: Vec<App> = Vec::with_capacity(apps.len());
    let mut seen = HashMap::new();
    for app in apps {
        if let Some(&index) = seen.get(&app.id) {
            let entry: &mut App = &mut merged[index];
            for platform in app.platforms {
                if !entry.platforms.contains(&platform) {
                    entry.platforms.push(platform);
                }
            }
            if app.purchase_date > entry.purchase_date {
                entry.purchase_date = app.purchase_date;
            }
            continue;
        }
        seen.insert(app.id, merged.len());
        merged.push(app);
    }
    for app in &mut merged {
        if app.platforms.len() > 1 {
            app.platforms
                .retain(|platform| *platform != Platform::Unknown);
        }
    }
    merged
}

/// Go's `parseOwnedApp`. Like Go, a field error still yields the fields read so far.
fn parse_owned_app(data: &[u8]) -> std::result::Result<App, String> {
    let mut app = App::default();
    let (mut media_kind, mut supported_products) = (0, 0);
    let walked = walk_dmap(data, 0, &mut |tag, payload| {
        match tag {
            "aeMk" | "aeSS" => {
                let value = owned_app_uint(tag, payload)?;
                if tag == "aeMk" {
                    media_kind = value;
                } else {
                    supported_products = value;
                }
            }
            "aeSI" => {
                app.id = dmap_int64(payload)
                    .map_err(|error| format!("failed to parse owned app ID: {error}"))?;
            }
            "aeBI" => app.bundle_id = go_string(payload),
            "aeLN" => app.name = go_string(payload),
            "minm" if app.name.is_empty() => app.name = go_string(payload),
            "aePd" => app.version = go_string(payload),
            "asdp" => {
                if payload.len() != 4 {
                    return Err(format!(
                        "owned app purchase date has invalid length {}",
                        payload.len()
                    ));
                }
                app.purchase_date =
                    OffsetDateTime::from_unix_timestamp(i64::from(be_u32(payload))).ok();
            }
            _ => {}
        }
        Ok(())
    });
    // Mac software uses a separate media kind; its supported-products bits
    // do not describe mobile device support.
    match media_kind {
        OWNED_MAC_APPS_MEDIA_KIND => app.platforms.push(Platform::MacOs),
        OWNED_APPS_MEDIA_KIND | OWNED_ARCADE_APPS_MEDIA_KIND => {
            // The purchase-history mask uses 8 for Mac and 16 for visionOS.
            // It does not encode Apple TV support.
            for (mask, platform) in [
                (1, Platform::IPhone),
                (2, Platform::IPad),
                (16, Platform::VisionOs),
                (8, Platform::MacOs),
            ] {
                if supported_products & mask != 0 {
                    app.platforms.push(platform);
                }
            }
        }
        _ => {}
    }
    if app.platforms.is_empty() {
        app.platforms.push(Platform::Unknown);
    }
    walked.map(|()| app)
}

fn owned_app_uint(tag: &str, payload: &[u8]) -> std::result::Result<u64, String> {
    Ok(match payload.len() {
        1 => u64::from(payload[0]),
        2 => u64::from(u16::from_be_bytes([payload[0], payload[1]])),
        4 => u64::from(be_u32(payload)),
        8 => be_u64(payload),
        length => {
            return Err(format!(
                "owned app {tag} has invalid integer length {length}"
            ));
        }
    })
}

/// Go's `ownedAppsSortedByPurchaseDate`: newest first, undated last, ties in response order.
fn owned_apps_sorted_by_purchase_date(mut apps: Vec<App>) -> Vec<App> {
    apps.sort_by(
        |left, right| match (left.purchase_date, right.purchase_date) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        },
    );
    apps
}

/// Go's `ownedAppsPage`.
fn owned_apps_page(apps: &[App], page: i64, limit: i64) -> Vec<App> {
    let (Ok(page_index), Ok(page_size)) = (u64::try_from(page - 1), u64::try_from(limit)) else {
        return Vec::new();
    };
    if page_size == 0 {
        return Vec::new();
    }
    let len = apps.len() as u64;
    if page_index > len / page_size {
        return Vec::new();
    }
    let start = page_index * page_size;
    if start >= len {
        return Vec::new();
    }
    let end = (start + page_size).min(len);
    // Both bounds are at most `apps.len()`.
    #[allow(clippy::cast_possible_truncation, reason = "bounded by apps.len()")]
    apps[start as usize..end as usize].to_vec()
}

fn dmap_int64(payload: &[u8]) -> std::result::Result<i64, String> {
    match payload.len() {
        4 => Ok(i64::from(be_u32(payload))),
        8 => i64::try_from(be_u64(payload)).map_err(|_| "integer exceeds int64".to_owned()),
        length => Err(format!("invalid integer length {length}")),
    }
}

/// Go's `walkDMAP`: visits every tag, descending into the container tags.
fn walk_dmap(
    data: &[u8],
    depth: usize,
    visit: &mut dyn FnMut(&str, &[u8]) -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    if depth > 16 {
        return Err("DMAP nesting is too deep".to_owned());
    }
    let mut offset = 0;
    while offset < data.len() {
        if data.len() - offset < 8 {
            return Err(format!("truncated DMAP tag header at byte {offset}"));
        }
        let tag_bytes = &data[offset..offset + 4];
        if !tag_bytes.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
            return Err(format!("invalid DMAP tag at byte {offset}"));
        }
        let tag = std::str::from_utf8(tag_bytes).unwrap_or_default();
        let length = be_u32(&data[offset + 4..offset + 8]) as usize;
        let remaining = data.len() - offset - 8;
        if length > remaining {
            return Err(format!(
                "DMAP tag {tag} length {length} exceeds remaining response length {remaining}"
            ));
        }
        let end = offset + 8 + length;
        let payload = &data[offset + 8..end];
        visit(tag, payload)?;
        if is_dmap_container(tag) {
            walk_dmap(payload, depth + 1, visit)?;
        }
        offset = end;
    }
    Ok(())
}

fn is_dmap_container(tag: &str) -> bool {
    matches!(
        tag,
        "adbs"
            | "adsr"
            | "aply"
            | "avdb"
            | "mbcl"
            | "mccr"
            | "mcty"
            | "mdcl"
            | "mlcl"
            | "mlit"
            | "mlog"
            | "msrv"
            | "mupd"
    )
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_u64(bytes: &[u8]) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(value)
}

/// Go's `string(payload)`: invalid UTF-8 becomes U+FFFD when printed.
fn go_string(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).into_owned()
}

#[cfg(test)]
mod tests;
