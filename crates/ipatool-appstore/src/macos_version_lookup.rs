//! Port of Go `appstore_macos_version_lookup.go`: pin the native Mac offer of an app
//! before asking for its package, plus `serializedServerData` from `appstore_storefront.go`.

use ipatool_store::gourl;
use serde_json::Value;

use crate::{App, Error, Result};

const SERIALIZED_SERVER_DATA_ID: &str = "serialized-server-data";

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Go's `serializedServerData`: the JSON inside the product page's
/// `<script id="serialized-server-data">`.
pub(crate) fn serialized_server_data(body: &[u8]) -> Result<&[u8]> {
    let marker = find(
        body,
        format!("id=\"{SERIALIZED_SERVER_DATA_ID}\"").as_bytes(),
    )
    .or_else(|| find(body, format!("id='{SERIALIZED_SERVER_DATA_ID}'").as_bytes()))
    .ok_or_else(|| invalid("serialized server data was not found"))?;
    body[..marker]
        .windows(b"<script".len())
        .rposition(|window| window == b"<script")
        .ok_or_else(|| invalid("serialized server data script was not found"))?;
    let content_start = marker
        + body[marker..]
            .iter()
            .position(|b| *b == b'>')
            .ok_or_else(|| invalid("serialized server data script is malformed"))?
        + 1;
    let content_end = find(&body[content_start..], b"</script>")
        .ok_or_else(|| invalid("serialized server data script is not closed"))?;
    Ok(go_trim_space(
        &body[content_start..content_start + content_end],
    ))
}

/// `bytes.TrimSpace` for UTF-8 text.
fn go_trim_space(data: &[u8]) -> &[u8] {
    match std::str::from_utf8(data) {
        Ok(text) => text.trim().as_bytes(),
        Err(_) => data.trim_ascii(),
    }
}

/// Go's `macOSExternalVersionID`: the one `appExtVrsId` among the page's Mac purchase
/// configurations for this app.
pub(crate) fn macos_external_version_id(body: &[u8], app: &App) -> Result<String> {
    let data = serialized_server_data(body)?;
    let value: Value = serde_json::from_slice(data)
        .map_err(|err| Error::Invalid(format!("failed to decode Mac product page: {err}")))?;
    let mut versions = Vec::new();
    collect_versions(&value, app, &mut versions);
    match versions.as_slice() {
        [] => Err(invalid(
            "macOS purchase configuration has no external version id for the requested app",
        )),
        [version] => Ok(version.clone()),
        _ => Err(invalid(
            "macOS purchase configurations contain conflicting external version ids",
        )),
    }
}

/// Go's `collectMacOSExternalVersions`; `versions` holds distinct values.
fn collect_versions(value: &Value, app: &App, versions: &mut Vec<String>) {
    match value {
        Value::Array(children) => {
            for child in children {
                collect_versions(child, app, versions);
            }
        }
        Value::Object(object) => {
            if let Some(Value::Object(configuration)) = object.get("purchaseConfiguration")
                && let Some(version) = mac_offer_version(configuration, app)
                && !versions.contains(&version)
            {
                versions.push(version);
            }
            for child in object.values() {
                collect_versions(child, app, versions);
            }
        }
        _ => {}
    }
}

fn mac_offer_version(configuration: &serde_json::Map<String, Value>, app: &App) -> Option<String> {
    let mac = configuration
        .get("appPlatforms")
        .and_then(Value::as_array)
        .is_some_and(|platforms| platforms.iter().any(|platform| platform == "mac"));
    let bundle_id = configuration
        .get("bundleId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let buy_params = configuration
        .get("buyParams")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (params, err) = gourl::parse_query(buy_params);
    let get = |key: &str| {
        params
            .get(key)
            .and_then(|values| values.first())
            .map_or("", String::as_str)
    };
    if err.is_some()
        || !mac
        || get("salableAdamId") != app.id.to_string()
        || !(app.bundle_id.is_empty() || app.bundle_id == bundle_id)
    {
        return None;
    }
    let version = get("appExtVrsId");
    // Go's `strconv.ParseUint(version, 10, 64)`: ASCII digits only, nonzero, in range.
    let valid = !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
        && version.parse::<u64>().is_ok_and(|id| id != 0);
    valid.then(|| version.to_owned())
}

#[cfg(test)]
mod tests;
