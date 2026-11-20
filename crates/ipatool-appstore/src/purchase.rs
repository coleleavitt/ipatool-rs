//! Go's `appstore_purchase.go`, `appstore_terms.go` and `purchaseStorefrontCorrection`
//! (`account_storefront.go`): free-app purchases through the Store `purchase` executor.

use std::collections::BTreeMap;
use std::time::Duration;

use ipatool_store::{gourl, terms};
use plist::Dictionary;
use serde::Deserialize;

use crate::commerce::{CommerceRequest, Payload, StoreResult};
use crate::download_product::{failure, pod_prefix};
use crate::download_types::{integer, string};
use crate::session::{PURCHASE_PATH, StoreSession};
use crate::store::STORE_USER_AGENT;
use crate::storefront::storefront_identifier;
use crate::{Account, App, Error, Platform, Result, Value, machine};

/// `PricingParameterAppStore` and `PricingParameterAppleArcade`.
pub(crate) const PRICING_APP_STORE: &str = "STDQ";
pub(crate) const PRICING_APPLE_ARCADE: &str = "GAME";
/// The terms client's `http.DefaultAuthenticationTimeout`.
const TERMS_TIMEOUT: Duration = Duration::from_secs(30);

/// Go's `purchaseResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PurchaseResult {
    pub failure_type: String,
    pub customer_message: String,
    pub jingle_doc_type: String,
    pub status: i64,
    pub dialog_id: String,
    pub dialog_ok_kind: String,
    pub dialog_ok_url: String,
    pub action_kind: String,
    pub action_url: String,
}

fn dictionary<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a Dictionary> {
    dict.get(key).and_then(plist::Value::as_dictionary)
}

impl PurchaseResult {
    pub(crate) fn from_plist(dict: &Dictionary) -> Self {
        let empty = Dictionary::new();
        let metrics = dictionary(dict, "metrics").unwrap_or(&empty);
        let dialog = dictionary(dict, "dialog").unwrap_or(&empty);
        let ok = dictionary(dialog, "okButtonAction").unwrap_or(&empty);
        let action = dictionary(dict, "action").unwrap_or(&empty);
        Self {
            failure_type: string(dict, "failureType"),
            customer_message: string(dict, "customerMessage"),
            jingle_doc_type: string(dict, "jingleDocType"),
            status: integer(dict, "status"),
            dialog_id: string(metrics, "dialogId"),
            dialog_ok_kind: string(ok, "kind"),
            dialog_ok_url: string(ok, "url"),
            action_kind: string(action, "kind"),
            action_url: string(action, "url"),
        }
    }
}

/// Go's `purchaseStorefrontCorrection`: the storefront of Apple's country-mismatch dialog.
pub(crate) fn purchase_storefront_correction(result: &PurchaseResult, current: &str) -> String {
    if result.failure_type != "-128"
        || result.dialog_id != "MZCommerce.CountryMismatch"
        || result.dialog_ok_kind != "Goto"
    {
        return String::new();
    }
    // Only Apple's known store-switch action; never a response-supplied URL.
    let Ok(u) = gourl::parse(&result.dialog_ok_url) else {
        return String::new();
    };
    if u.scheme != "https"
        || u.hostname() != "itunes.apple.com"
        || (!u.port().is_empty() && u.port() != "443")
        || u.user.is_some()
        || !u.fragment.is_empty()
        || !u.raw_path.is_empty()
        || u.path != "/WebObjects/MZStore.woa/wa/resetAndRedirect"
    {
        return String::new();
    }
    let (query, error) = gourl::parse_query(&u.raw_query);
    if error.is_some() || query.values("dsf").len() != 1 {
        return String::new();
    }
    let identifier = query.get("dsf");
    let canonical = identifier
        .parse::<u32>()
        .is_ok_and(|number| number != 0 && number.to_string() == identifier);
    if !canonical {
        return String::new();
    }
    let current = current.trim();
    let previous = storefront_identifier(current);
    if identifier == previous {
        return String::new();
    }
    // Keep the client's language/platform suffix, changing only the country.
    format!(
        "{identifier}{}",
        current.strip_prefix(previous).unwrap_or(current)
    )
}

/// Go's `purchaseRequest`.
pub(crate) fn purchase_request(
    account: &Account,
    app: &App,
    store_front: &str,
    guid: &str,
    pricing: &str,
) -> CommerceRequest {
    let text = |value: &str| Value::String(value.to_owned());
    let payload = BTreeMap::from([
        ("appExtVrsId".to_owned(), text("0")),
        ("hasAskedToFulfillPreorder".to_owned(), text("true")),
        ("buyWithoutAuthorization".to_owned(), text("true")),
        ("hasDoneAgeCheck".to_owned(), text("true")),
        ("guid".to_owned(), text(guid)),
        ("needDiv".to_owned(), text("0")),
        ("origPage".to_owned(), text(&format!("Software-{}", app.id))),
        ("origPageLocation".to_owned(), text("Buy")),
        ("price".to_owned(), text("0")),
        ("pricingParameters".to_owned(), text(pricing)),
        ("productType".to_owned(), text("C")),
        ("salableAdamId".to_owned(), Value::Integer(app.id)),
    ]);
    let dsid = &account.directory_services_id;
    CommerceRequest {
        method: "POST".to_owned(),
        url: format!(
            "https://{}buy.itunes.apple.com{PURCHASE_PATH}",
            pod_prefix(account)
        ),
        headers: BTreeMap::from([
            (
                "Content-Type".to_owned(),
                "application/x-apple-plist".to_owned(),
            ),
            ("User-Agent".to_owned(), STORE_USER_AGENT.to_owned()),
            ("iCloud-DSID".to_owned(), dsid.clone()),
            ("X-Dsid".to_owned(), dsid.clone()),
            ("X-Apple-Store-Front".to_owned(), store_front.to_owned()),
            ("X-Token".to_owned(), account.password_token.clone()),
        ]),
        payload: Some(Payload::Xml(payload)),
        ..CommerceRequest::default()
    }
}

/// Go's `termsResult`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TermsResult {
    #[serde(default)]
    status: Option<i64>,
    #[serde(default)]
    terms_version_id: Option<serde_json::Value>,
    #[serde(default)]
    latest_terms_version_id: Option<serde_json::Value>,
    #[serde(default)]
    is_current: bool,
    #[serde(default)]
    error_message_key: String,
}

/// Go's `termsVersion`: the version as Apple sent it (a number, or a string holding one).
pub(crate) fn terms_version(result: &TermsResult) -> Result<serde_json::Value> {
    let invalid = || Error::Invalid("apple returned an invalid terms version".to_owned());
    let version = match &result.latest_terms_version_id {
        None | Some(serde_json::Value::Null) => result.terms_version_id.clone(),
        Some(version) => Some(version.clone()),
    }
    .ok_or_else(invalid)?;
    let number = match &version {
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text)
            if serde_json::from_str::<serde_json::Number>(text).is_ok() =>
        {
            text.clone()
        }
        _ => return Err(invalid()),
    };
    if number
        .parse::<u64>()
        .is_ok_and(|value| value != 0 && value.to_string() == number)
    {
        Ok(version)
    } else {
        Err(invalid())
    }
}

/// Go's `checkTermsResponse`.
pub(crate) fn check_terms_response(response: &StoreResult<TermsResult>) -> Result<()> {
    if response.data.error_message_key == "MZFinance.TermsCheckLoginRequired" {
        return Err(Error::PasswordTokenExpired);
    }
    if response.status != 200 {
        return Err(Error::Invalid(format!(
            "unexpected terms response status: {}",
            response.status
        )));
    }
    match response.data.status {
        None => Err(Error::Invalid(
            "apple returned a terms response without a status".to_owned(),
        )),
        Some(0) => Ok(()),
        Some(status) => Err(Error::Invalid(format!(
            "apple terms request failed with status {status}"
        ))),
    }
}

/// Go's `termsRequest`.
fn terms_request(
    account: &Account,
    url: String,
    method: &str,
    body: Option<Vec<u8>>,
) -> CommerceRequest {
    let dsid = &account.directory_services_id;
    CommerceRequest {
        method: method.to_owned(),
        url,
        headers: BTreeMap::from([
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("User-Agent".to_owned(), STORE_USER_AGENT.to_owned()),
            ("X-Dsid".to_owned(), dsid.clone()),
            ("iCloud-DSID".to_owned(), dsid.clone()),
            ("X-Token".to_owned(), account.password_token.clone()),
            (
                "X-Apple-Store-Front".to_owned(),
                account.store_front.clone(),
            ),
        ]),
        payload: body.map(Payload::Raw),
        timeout: Some(TERMS_TIMEOUT),
        ..CommerceRequest::default()
    }
}

/// Go's `termsEndpoint`.
pub(crate) fn terms_endpoint(raw: &str, pod: &str, acceptance: bool) -> Result<gourl::Url> {
    let invalid = |text: &str| Error::Invalid(text.to_owned());
    let mut u = gourl::parse(raw)
        .ok()
        .filter(|u| {
            u.scheme == "https"
                && u.user.is_none()
                && u.fragment.is_empty()
                && u.raw_path.is_empty()
                && (u.port().is_empty() || u.port() == "443")
        })
        .ok_or_else(|| invalid("invalid terms endpoint in bag"))?;
    if gourl::parse_query(&u.raw_query).1.is_some() {
        return Err(invalid("invalid terms endpoint query in bag"));
    }
    if !acceptance {
        if !u
            .hostname()
            .eq_ignore_ascii_case("amp-account.itunes.apple.com")
            || u.path != "/account/terms/info"
        {
            return Err(invalid("unsupported terms information endpoint in bag"));
        }
        return Ok(u);
    }
    if !terms::is_buy_host(u.hostname())
        || (u.path != "/account/terms" && u.path != "/WebObjects/MZFinance.woa/wa/termsCheckSrv")
    {
        return Err(invalid("unsupported terms acceptance endpoint in bag"));
    }
    if u.hostname().eq_ignore_ascii_case("buy.itunes.apple.com") && !pod.is_empty() {
        let host = format!("p{pod}-buy.itunes.apple.com");
        if !terms::is_buy_host(&host) {
            return Err(invalid("invalid account pod for terms acceptance"));
        }
        u.host = host;
    }
    Ok(u)
}

impl StoreSession<'_> {
    /// Go's `Purchase`: obtains a license for a free app. A `GAME` retry follows a
    /// temporarily-unavailable `STDQ` purchase outside macOS. The account's storefront may
    /// be corrected (and saved) from Apple's country-mismatch answer.
    ///
    /// # Errors
    /// [`Error::LicenseAlreadyExists`] when the account owns the app; Go's other errors.
    pub async fn purchase(
        &self,
        account: &mut Account,
        app: &App,
        platform: Option<Platform>,
    ) -> Result<()> {
        let mac_address =
            machine::mac_address().map_err(|error| error.context("failed to get mac address"))?;
        let guid = mac_address.to_uppercase().replace(':', "");
        if app.price > 0.0 {
            return Err(Error::Invalid(
                "purchasing paid apps is not supported".to_owned(),
            ));
        }
        match self
            .purchase_with_params(account, app, &guid, PRICING_APP_STORE)
            .await
        {
            Ok(()) => Ok(()),
            Err(Error::TemporarilyUnavailable) if platform != Some(Platform::MacOs) => self
                .purchase_with_params(account, app, &guid, PRICING_APPLE_ARCADE)
                .await
                .map_err(|error| {
                    error.context(format!(
                        "failed to purchase item with param '{PRICING_APPLE_ARCADE}'"
                    ))
                }),
            Err(error) => Err(error.context(format!(
                "failed to purchase item with param '{PRICING_APP_STORE}'"
            ))),
        }
    }

    /// Go's `purchaseWithParams`.
    pub(crate) async fn purchase_with_params(
        &self,
        account: &mut Account,
        app: &App,
        guid: &str,
        pricing: &str,
    ) -> Result<()> {
        let (mut storefront_recovered, mut terms_accepted) = (false, false);
        let (status, data) = loop {
            let request = purchase_request(account, app, &account.store_front, guid, pricing);
            let response = self
                .client
                .store_xml::<Dictionary>("purchase", request)
                .await
                .map_err(|error| error.context("request failed"))?;
            let data = PurchaseResult::from_plist(&response.data);
            if response.status != 200 {
                break (response.status, data);
            }
            let storefront = purchase_storefront_correction(&data, &account.store_front);
            if !storefront.is_empty() && !storefront_recovered {
                self.save_account_storefront(&account.directory_services_id, &storefront)
                    .await
                    .map_err(|error| error.context("failed to correct account storefront"))?;
                account.store_front = storefront;
                storefront_recovered = true;
                continue;
            }
            if data.jingle_doc_type == "buyProductFailure"
                && data.action_kind == "Goto"
                && terms::is_page_url(&data.action_url)
            {
                if terms_accepted {
                    return Err(Error::Invalid(
                        "apple still requires terms acceptance after confirming the current terms"
                            .to_owned(),
                    ));
                }
                self.accept_purchase_terms(account, guid)
                    .await
                    .map_err(|error| error.context("failed to accept App Store terms"))?;
                terms_accepted = true;
                continue;
            }
            break (response.status, data);
        };
        purchase_outcome(status, &data)
    }

    /// Go's `acceptPurchaseTerms`.
    pub(crate) async fn accept_purchase_terms(&self, account: &Account, guid: &str) -> Result<()> {
        if account
            .directory_services_id
            .parse::<u64>()
            .map_or(true, |dsid| dsid == 0)
        {
            return Err(Error::Invalid(
                "account identifier is required for terms acceptance".to_owned(),
            ));
        }
        let country = self
            .client
            .country_code_from_store_front(&account.store_front)
            .await?;
        let bag = self.client.url_bag(guid).await?;
        let mut info_url = terms_endpoint(&bag.terms_info_endpoint, &account.pod, false)?;
        let check_url = terms_endpoint(&bag.terms_check_endpoint, &account.pod, true)?;
        let mut query = info_url.query();
        query.set("cc", country);
        query.set("termsType", "MZStore.NewTermsAndConditionsStatement");
        info_url.raw_query = query.encode();

        let info = self
            .client
            .store_json::<TermsResult>(
                "request",
                terms_request(account, info_url.to_string(), "GET", None),
            )
            .await
            .map_err(|error| error.context("failed to retrieve current terms"))?;
        check_terms_response(&info)?;
        let version = terms_version(&info.data)?;
        // Apple's Commerce UI posts the DSID and the version from getTermsByCountryAndTypeSrv.
        // Go's json.Marshal sorts the map keys.
        let body = format!(
            "{{\"accepted\":{version},\"id\":{}}}",
            serde_json::Value::String(account.directory_services_id.clone())
        );
        let accepted = self
            .client
            .store_json::<TermsResult>(
                "request",
                terms_request(
                    account,
                    check_url.to_string(),
                    "POST",
                    Some(body.into_bytes()),
                ),
            )
            .await
            .map_err(|error| error.context("failed to submit terms acceptance"))?;
        check_terms_response(&accepted)?;
        if !accepted.data.is_current {
            return Err(Error::Invalid(
                "apple did not confirm acceptance of the current terms".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The end of Go's `purchaseWithParams`: the final response's failure mapping.
pub(crate) fn purchase_outcome(status: u16, data: &PurchaseResult) -> Result<()> {
    let store = |message: &str| Error::Store {
        message: message.to_owned(),
        status,
    };
    if data.failure_type == failure::TEMPORARILY_UNAVAILABLE {
        return Err(Error::TemporarilyUnavailable);
    }
    if data.customer_message == failure::SUBSCRIPTION_REQUIRED {
        return Err(Error::SubscriptionRequired);
    }
    if data.failure_type == failure::PASSWORD_TOKEN_EXPIRED
        || data.failure_type == failure::SIGN_IN_REQUIRED
        || data.failure_type == failure::DEVICE_VERIFICATION_FAILED
        || data.customer_message == failure::PASSWORD_CHANGED
    {
        return Err(Error::PasswordTokenExpired);
    }
    if data.failure_type == failure::LICENSE_ALREADY_EXISTS {
        return Err(Error::LicenseAlreadyExists);
    }
    if !data.failure_type.is_empty() && !data.customer_message.is_empty() {
        return Err(store(&data.customer_message));
    }
    if !data.failure_type.is_empty() {
        return Err(store("something went wrong"));
    }
    if status == 500 {
        return Err(Error::LicenseAlreadyExists);
    }
    if data.jingle_doc_type != "purchaseSuccess" || data.status != 0 {
        return Err(store("failed to purchase app"));
    }
    Ok(())
}
