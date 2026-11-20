//! Bag requests through Go's `bag-direct` executor (`internal/store/backend.go`) and the
//! XML response rules of `pkg/http/client.go` for non-login clients.

use std::time::Duration;

use reqwest::StatusCode;
use std::collections::BTreeMap;

use ipatool_store::backend::ResponseFormat;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;

use crate::commerce::CommerceRequest;
use crate::{Client, Error, Result, UnexpectedResponseError, plist_body};

/// `storeAuthenticationUserAgent` in `pkg/appstore/constants.go`.
pub(crate) const STORE_USER_AGENT: &str =
    "Configurator/2.21 (Macintosh; OS X 27.0.1; 26A434) AppleWebKit/0625.2.5.11.6";
/// `http.DefaultAuthenticationTimeout`, used by the bag clients.
pub(crate) const STORE_TIMEOUT: Duration = Duration::from_secs(30);

/// A Store response. `data` is `None` for redirects, which Go passes through undecoded.
#[derive(Debug)]
pub(crate) struct StoreResult<T> {
    pub status: u16,
    pub data: Option<T>,
}

impl Client {
    /// GET with the Store user agent and `Accept: application/xml`, returning the raw body.
    pub(crate) async fn store_get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(u16, HeaderMap, Vec<u8>)> {
        // Go sends bag requests through the `bag-direct` executor.
        let mut fields = BTreeMap::from([
            ("Accept".to_owned(), "application/xml".to_owned()),
            ("User-Agent".to_owned(), STORE_USER_AGENT.to_owned()),
        ]);
        for (name, value) in &headers {
            fields.insert(
                ipatool_store::http::canonical_header_key(name.as_str()),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            );
        }
        let response = self
            .store_send(
                "bag-direct",
                CommerceRequest {
                    method: "GET".to_owned(),
                    url: url.to_owned(),
                    headers: fields,
                    response_format: ResponseFormat::Xml,
                    timeout: Some(STORE_TIMEOUT),
                    ..CommerceRequest::default()
                },
            )
            .await?;
        let mut headers = HeaderMap::new();
        for (name, values) in response.headers.iter() {
            for value in values {
                if let (Ok(name), Ok(value)) =
                    (HeaderName::try_from(name), HeaderValue::from_str(value))
                {
                    headers.append(name, value);
                }
            }
        }
        Ok((response.status, headers, response.body))
    }

    /// `handleXMLResponse` for clients without login diagnostics.
    pub(crate) async fn store_get_xml<T: DeserializeOwned>(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<StoreResult<T>> {
        let (status, _, body) = self.store_get(url, headers).await?;
        Ok(StoreResult {
            status,
            data: decode_xml_response(status, &body)?,
        })
    }
}

pub(crate) fn decode_xml_response<T: DeserializeOwned>(
    status: u16,
    body: &[u8],
) -> Result<Option<T>> {
    if status == StatusCode::TOO_MANY_REQUESTS.as_u16() {
        return Err(Error::RateLimited {
            status,
            body: String::from_utf8_lossy(body).trim().to_owned(),
        });
    }
    if (300..400).contains(&status) {
        return Ok(None);
    }
    if !plist_body::looks_like_plist(&plist_body::normalize(body)) {
        return Err(Error::UnexpectedResponse(UnexpectedResponseError {
            status,
            snippet: plist_body::snippet(body),
            ..UnexpectedResponseError::default()
        }));
    }
    Ok(Some(plist_body::decode(body)?))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct Foo {
        foo: String,
    }

    #[test]
    fn follows_go_response_rules() {
        let ok: Option<Foo> =
            decode_xml_response(200, b"<dict><key>foo</key><string>bar</string></dict>").unwrap();
        assert_eq!(ok.unwrap().foo, "bar");
        assert!(decode_xml_response::<Foo>(302, b"").unwrap().is_none());
        assert_eq!(
            decode_xml_response::<Foo>(429, b" slow down \n")
                .unwrap_err()
                .to_string(),
            "rate limited by Apple (HTTP 429): slow down"
        );
        assert_eq!(
            decode_xml_response::<Foo>(503, b"<html><h1>Down</h1></html>")
                .unwrap_err()
                .to_string(),
            "unexpected response from Apple (HTTP 503): Down"
        );
        assert_eq!(
            decode_xml_response::<Foo>(200, b"")
                .unwrap_err()
                .to_string(),
            "unexpected response from Apple (HTTP 200): empty or non-plist body"
        );
    }
}
