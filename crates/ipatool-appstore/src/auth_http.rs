//! Signed login requests: Go's `pkg/http` client in authentication mode. Go sends these
//! through the Store framework emulator (phase 5); until then the plist payload is encoded
//! and signed here, as the Go client did before the emulator (`X-Apple-ActionSignature`
//! over the request body).

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ipatool_sap::signer::ActionSigner;
use plist::Dictionary;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::de::DeserializeOwned;

use crate::{Client, Error, Result, TransportError, UnexpectedResponseError, plist_body};

const ACTION_SIGNATURE_HEADER: &str = "X-Apple-ActionSignature";

/// A login POST: Go's `http.Request` with an `XMLPayload`.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AuthRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub payload: Dictionary,
}

/// Go's `http.Result`: header values are joined with `"; "`, and redirects carry no data.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuthResult<T> {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub data: T,
}

impl<T> AuthResult<T> {
    /// Go's `GetHeader`: case-insensitive.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }
}

/// Go's `IsAuthenticationRedirect`: the redirects that can replay a signed POST.
pub(crate) fn is_authentication_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 307 | 308)
}

impl Client {
    /// Go's `client.Send` for the login client.
    pub(crate) async fn send_authentication<T: DeserializeOwned + Default>(
        &self,
        request: &AuthRequest,
        signer: &mut impl ActionSigner,
    ) -> Result<AuthResult<T>> {
        let mut body = Vec::new();
        plist::to_writer_xml(&mut body, &request.payload)
            .map_err(|err| Error::from(err).context("failed to get payload data"))?;
        let signature = signer
            .sign(&body)
            .map_err(|err| Error::from(err).context("failed to sign Apple action"))?;
        let mut builder = self.auth.post(&request.url);
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }
        let response = builder
            .header(ACTION_SIGNATURE_HEADER, STANDARD.encode(signature))
            .body(body)
            .send()
            .await
            .map_err(|err| transport_error("request failed", err, String::new()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let retry_after = header_text(&headers, RETRY_AFTER.as_str());
        let body = response
            .bytes()
            .await
            .map_err(|err| transport_error("failed to read response body", err, retry_after))?;
        if let Some(jar) = &self.jar {
            jar.save()?;
        }
        decode_authentication_response(status, &headers, &body)
    }
}

fn transport_error(context: &'static str, err: reqwest::Error, retry_after: String) -> Error {
    Error::Transport(TransportError {
        context,
        source: Box::new(err),
        retry_after,
    })
}

fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Go's `responseHeaders`.
fn response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .keys()
        .map(|name| {
            let values: Vec<String> = headers
                .get_all(name)
                .iter()
                .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
                .collect();
            (name.as_str().to_owned(), values.join("; "))
        })
        .collect()
}

/// Go's `handleXMLResponse` with authentication diagnostics: failures keep the status,
/// body length and a few headers, never the body, which can hold credentials.
pub(crate) fn decode_authentication_response<T: DeserializeOwned + Default>(
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<AuthResult<T>> {
    let failure = |reason: &str| {
        Error::UnexpectedResponse(UnexpectedResponseError {
            status,
            reason: reason.to_owned(),
            body_length: body.len(),
            content_type: plist_body::snippet(header_text(headers, "Content-Type").as_bytes()),
            correlation_id: plist_body::snippet(
                header_text(headers, "X-Apple-Jingle-Correlation-Key").as_bytes(),
            ),
            retry_after: header_text(headers, RETRY_AFTER.as_str()),
            ..UnexpectedResponseError::default()
        })
    };
    if is_authentication_redirect(status) && header_text(headers, "Location").trim().is_empty() {
        return Err(failure("authentication redirect is missing Location"));
    }
    if (300..400).contains(&status) {
        return Ok(AuthResult {
            status,
            headers: response_headers(headers),
            data: T::default(),
        });
    }
    if !plist_body::looks_like_plist(&plist_body::normalize(body)) {
        return Err(failure(if status == 429 {
            "rate limited by Apple"
        } else {
            "empty or non-plist authentication response"
        }));
    }
    let data = plist_body::decode(body).map_err(|_| failure("malformed authentication plist"))?;
    Ok(AuthResult {
        status,
        headers: response_headers(headers),
        data,
    })
}

/// Go's `retryableAuthenticationError` for transport failures: timeouts, closed
/// connections, resets and broken pipes.
pub(crate) fn retryable_transport_error(error: &TransportError) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error.source.as_ref());
    while let Some(current) = cause {
        if let Some(err) = current.downcast_ref::<reqwest::Error>()
            && err.is_timeout()
        {
            return true;
        }
        if let Some(err) = current.downcast_ref::<hyper::Error>()
            && (err.is_timeout() || err.is_incomplete_message())
        {
            return true;
        }
        if let Some(err) = current.downcast_ref::<std::io::Error>()
            && matches!(
                err.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
        {
            return true;
        }
        cause = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn preserves_a_populated_apple_error_even_with_http_429() {
        let result: AuthResult<HashMap<String, String>> = decode_authentication_response(
            429,
            &headers(&[("Retry-After", "5")]),
            b"<dict><key>failureType</key><string>badCredentials</string></dict>",
        )
        .unwrap();
        assert_eq!(result.data["failureType"], "badCredentials");
        assert_eq!(result.status, 429);
    }

    #[test]
    fn reports_malformed_responses_without_retaining_credentials() {
        for (status, body, reason) in [
            (403, "", "empty or non-plist authentication response"),
            (
                500,
                "<html>secret-password</html>",
                "empty or non-plist authentication response",
            ),
            (
                302,
                "secret-token",
                "authentication redirect is missing Location",
            ),
            (
                301,
                "secret-token",
                "authentication redirect is missing Location",
            ),
            (
                307,
                "secret-token",
                "authentication redirect is missing Location",
            ),
            (
                308,
                "secret-token",
                "authentication redirect is missing Location",
            ),
            (429, "secret-token", "rate limited by Apple"),
            (
                200,
                "<plist><dict>secret-token",
                "malformed authentication plist",
            ),
        ] {
            let error = decode_authentication_response::<HashMap<String, String>>(
                status,
                &headers(&[
                    ("Content-Type", "text/html"),
                    ("X-Apple-Jingle-Correlation-Key", "correlation-123"),
                    ("Set-Cookie", "secret-cookie"),
                    ("Retry-After", "5"),
                ]),
                body.as_bytes(),
            )
            .unwrap_err();
            let Error::UnexpectedResponse(response) = &error else {
                panic!("{status}: {error}");
            };
            assert_eq!(response.status, status);
            assert_eq!(response.body_length, body.len());
            assert_eq!(response.content_type, "text/html");
            assert_eq!(response.correlation_id, "correlation-123");
            assert_eq!(response.retry_after, "5");
            assert_eq!(response.reason, reason);
            assert_eq!(response.snippet, "");
            assert!(!error.to_string().contains("secret"), "{error}");
        }
    }

    #[test]
    fn passes_redirects_through_with_joined_headers() {
        let result: AuthResult<HashMap<String, String>> = decode_authentication_response(
            302,
            &headers(&[
                ("Location", "https://p7-buy.itunes.apple.com/"),
                ("Set-Cookie", "a=1"),
                ("Set-Cookie", "mz_at0-42=x"),
            ]),
            b"",
        )
        .unwrap();
        assert!(result.data.is_empty());
        assert_eq!(
            result.header("location"),
            Some("https://p7-buy.itunes.apple.com/")
        );
        assert_eq!(result.header("SET-COOKIE"), Some("a=1; mz_at0-42=x"));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("failed to make round trip")]
    struct Wrapper(#[source] std::io::Error);

    #[test]
    fn classifies_retryable_transport_failures() {
        let error = |source: std::io::Error| TransportError {
            context: "request failed",
            source: Box::new(source),
            retry_after: String::new(),
        };
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
        ] {
            assert!(retryable_transport_error(&error(kind.into())), "{kind:?}");
        }
        let wrapped = TransportError {
            source: Box::new(Wrapper(std::io::ErrorKind::TimedOut.into())),
            ..error(std::io::ErrorKind::Other.into())
        };
        assert!(retryable_transport_error(&wrapped));
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::Other,
        ] {
            assert!(!retryable_transport_error(&error(kind.into())), "{kind:?}");
        }
    }
}
