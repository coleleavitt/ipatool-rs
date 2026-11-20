use thiserror::Error;

use crate::CookieJarError;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{url} returned HTTP {status}")]
    Status { status: u16, url: String },
    #[error("failed to decode response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("app not found")]
    AppNotFound,
    #[error("invalid platform {0:?}")]
    InvalidPlatform(String),
    #[error("invalid country code {0:?}; expected two letters such as US")]
    InvalidCountry(String),
    #[error("catalog returned a different app")]
    DifferentApp,
    #[error("failed to parse release timestamp {0:?}")]
    InvalidTimestamp(String),
    #[error("{0} is not ported yet")]
    NotPorted(&'static str),
    /// Go's `UnexpectedResponseError`: an HTML or empty body where a plist was expected.
    #[error("{}", unexpected_response(*status, snippet))]
    UnexpectedResponse { status: u16, snippet: String },
    #[error("rate limited by Apple (HTTP {status}): {body}")]
    RateLimited { status: u16, body: String },
    #[error("failed to unmarshal xml: {0}")]
    Plist(#[from] plist::Error),
    #[error("failed to save cookies: {0}")]
    Cookies(#[from] CookieJarError),
    /// Go's `NewErrorWithMetadata`: a Store failure that keeps the HTTP status.
    #[error("{message}")]
    Store { message: String, status: u16 },
    /// Validation failures whose text matches the Go error.
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Context { context: String, source: Box<Error> },
}

impl Error {
    pub(crate) fn context(self, context: impl Into<String>) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }
}

fn unexpected_response(status: u16, snippet: &str) -> String {
    if snippet.is_empty() {
        format!("unexpected response from Apple (HTTP {status}): empty or non-plist body")
    } else {
        format!("unexpected response from Apple (HTTP {status}): {snippet}")
    }
}
