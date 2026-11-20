use std::fmt;

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
    #[error(transparent)]
    UnexpectedResponse(#[from] UnexpectedResponseError),
    #[error(transparent)]
    Transport(#[from] TransportError),
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
    /// Go's `errors.Join`.
    #[error("{}", join_messages(.0))]
    Joined(Vec<Error>),
    /// Go's `ErrAuthCodeRequired`.
    #[error("auth code is required")]
    AuthCodeRequired,
    /// Go's `ErrInvalidAuthCode` (fork), optionally with Apple's reason appended.
    #[error("invalid or expired 2FA code{}", .0.map_or_else(String::new, |detail| format!(": {detail}")))]
    InvalidAuthCode(Option<&'static str>),
    #[error(transparent)]
    MachineAction(#[from] MachineAction),
    /// Go's `authenticationRedirectError`: the destination keeps only scheme, host and path.
    #[error("{source} (HTTP {status})")]
    AuthRedirect {
        source: Box<Error>,
        status: u16,
        destination: String,
    },
    #[error(transparent)]
    Sap(#[from] ipatool_sap::Error),
    #[error(transparent)]
    Keychain(#[from] ipatool_keychain::Error),
}

impl Error {
    pub(crate) fn context(self, context: impl Into<String>) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }

    /// This error and the errors it wraps, as Go's `errors.Is` walks them.
    fn any(&self, matches: &impl Fn(&Self) -> bool) -> bool {
        matches(self)
            || match self {
                Self::Context { source, .. } | Self::AuthRedirect { source, .. } => {
                    source.any(matches)
                }
                Self::Joined(errors) => errors.iter().any(|error| error.any(matches)),
                _ => false,
            }
    }

    /// `errors.Is(err, ErrAuthCodeRequired)`.
    #[must_use]
    pub fn is_auth_code_required(&self) -> bool {
        self.any(&|error| matches!(error, Self::AuthCodeRequired))
    }

    /// `errors.Is(err, ErrInvalidAuthCode)`.
    #[must_use]
    pub fn is_invalid_auth_code(&self) -> bool {
        self.any(&|error| matches!(error, Self::InvalidAuthCode(_)))
    }

    /// `errors.As(err, &authenticationMachineActionError)`.
    #[must_use]
    pub fn machine_action(&self) -> Option<&MachineAction> {
        match self {
            Self::MachineAction(action) => Some(action),
            Self::Context { source, .. } | Self::AuthRedirect { source, .. } => {
                source.machine_action()
            }
            Self::Joined(errors) => errors.iter().find_map(Self::machine_action),
            _ => None,
        }
    }
}

fn join_messages(errors: &[Error]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Go's `authenticationMachineActionError`: Apple asks the client to provision
/// (`X-Apple-AMD-Action`, for example `RP` or `SP`) with `X-Apple-AMD-Data`.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("apple requested an authentication machine action: {action}")]
pub struct MachineAction {
    pub action: String,
    pub data: String,
}

/// Go's `UnexpectedResponseError`: an HTML or empty body where a plist was expected.
/// Login responses set `reason` and keep no body text, since they can carry credentials.
#[derive(Clone, Debug, Default, PartialEq, Eq, Error)]
pub struct UnexpectedResponseError {
    pub status: u16,
    pub snippet: String,
    pub reason: String,
    pub body_length: usize,
    pub content_type: String,
    pub correlation_id: String,
    pub retry_after: String,
}

impl fmt::Display for UnexpectedResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = self.status;
        if !self.reason.is_empty() {
            return write!(
                f,
                "unexpected response from Apple (HTTP {status}): {} (body length={}, content type={:?}, correlation ID={:?})",
                self.reason, self.body_length, self.content_type, self.correlation_id
            );
        }
        if self.snippet.is_empty() {
            return write!(
                f,
                "unexpected response from Apple (HTTP {status}): empty or non-plist body"
            );
        }
        write!(
            f,
            "unexpected response from Apple (HTTP {status}): {}",
            self.snippet
        )
    }
}

/// Go's `http.TransportError`: sending the request or reading the response failed.
#[derive(Debug, Error)]
#[error("{context}: {source}")]
pub struct TransportError {
    pub context: &'static str,
    pub source: Box<dyn std::error::Error + Send + Sync>,
    /// Apple's backoff, kept even when reading the response body fails.
    pub retry_after: String,
}
