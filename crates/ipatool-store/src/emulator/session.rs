//! Go `session.go`: `AMSURLSession` requests and promise resolution.

use std::collections::BTreeMap;

use super::bridge::import_value;
use super::runtime::invoke;
use super::{Error, FrameworkError, Guest, Payload, Result, Uc, Value};

/// Go's `resolveAddress`. Apple's `resultWithError:` starts lazy promises and drives their
/// wait; the `NSCondition` bridge runs queued callbacks until that operation completes.
/// Draining every unrelated background task first can fail after success.
pub(crate) fn resolve_address(uc: &mut Uc, promise: u64) -> Result<u64> {
    let failure = uc.alloc(8)?;
    let result = invoke(uc, promise, "resultWithError:", &[failure])?;
    let error = uc.read64(failure)?;
    if error != 0 {
        let unknown = || Error::msg("framework returned an error");
        let domain = invoke(uc, error, "domain", &[]).map_err(|_| unknown())?;
        let code = invoke(uc, error, "code", &[]).map_err(|_| unknown())?;
        #[allow(clippy::cast_possible_wrap, reason = "Go's int64(uint64)")]
        let mut failure = FrameworkError {
            domain: uc.string(domain)?,
            message: String::new(),
            code: code as i64,
        };
        if let Ok(message) = invoke(uc, error, "localizedDescription", &[])
            && message != 0
        {
            failure.message = uc.string(message)?;
        }
        return Err(Error::Framework(failure));
    }
    if result == 0 {
        return Err(Error::msg("framework returned no result"));
    }
    Ok(result)
}

/// Go's `ephemeralAccount`.
pub(crate) fn ephemeral_account(
    uc: &mut Uc,
    saved: Option<&BTreeMap<String, Value>>,
) -> Result<u64> {
    let class = uc.class_address("ACAccount");
    let account = invoke(uc, class, "ams_createEphemeralAccount", &[])?;
    let Some(saved) = saved else {
        return Ok(account);
    };
    let field = |key: &str| saved.get(key).cloned().unwrap_or(Value::Nil);
    let dsid: i64 = field("directoryServicesIdentifier")
        .sprint()
        .parse()
        .map_err(|_| Error::msg("invalid saved account identifier"))?;
    let username = import_value(uc, &field("email"))?;
    let dsid = uc.new_object("NSNumber", Payload::Int(dsid))?;
    let password = import_value(uc, &field("passwordToken"))?;
    let storefront = import_value(uc, &field("storeFront"))?;
    for (selector, value) in [
        ("setUsername:", username),
        ("ams_setDSID:", dsid),
        ("ams_setPassword:", password),
        ("ams_setStorefront:", storefront),
    ] {
        invoke(uc, account, selector, &[value])?;
    }
    Ok(account)
}

/// Go's `configureRequest`.
pub(crate) fn configure_request(
    uc: &mut Uc,
    request: u64,
    account: u64,
    bag: u64,
    raw: bool,
) -> Result<()> {
    let properties = invoke(uc, request, "properties", &[])?;
    for (selector, value) in [
        ("setAccount:", account),
        ("setBag:", bag),
        ("setKnownToBeTrusted:", 1),
        ("setShouldSetCookiesFromResponse:", 0),
        ("setShouldSetStorefrontFromResponse:", 0),
        ("setDisableLoadURLMetrics:", 1),
        ("setDisableBiometricsResponseHandling:", 1),
        ("setMaxRetryCount:", 0),
        ("setDisableResponseDecoding:", u64::from(raw)),
    ] {
        invoke(uc, properties, selector, &[value])?;
    }
    Ok(())
}

/// Go's `newSession`.
pub(crate) fn new_session(uc: &mut Uc) -> Result<u64> {
    let session = uc.new_object("AMSURLSession", Payload::Nil)?;
    let session = invoke(
        uc,
        session,
        "initWithConfiguration:delegate:delegateQueue:",
        &[0, 0, 0],
    )?;
    invoke(uc, session, "setUseFallbackBag:", &[0])?;
    Ok(session)
}

/// Go's `sendRequest`.
pub(crate) fn send_request(uc: &mut Uc, session: u64, request: u64) -> Result<u64> {
    let promise = invoke(uc, session, "dataTaskPromiseWithRequest:", &[request])?;
    resolve_address(uc, promise)
}
