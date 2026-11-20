use thiserror::Error;

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
}
