//! Go `internal/store/emulator`: the Objective-C/Swift runtime bridge that runs the pinned
//! `AppleMediaServices` image in Unicorn. Guest state lives in the engine's data
//! ([`runtime::State`]); every bridge service takes the engine (`&mut Uc`).

mod accounts;
mod anisette;
mod async_tasks;
#[allow(
    dead_code,
    reason = "the Store client (network.rs, client.rs) calls these"
)]
mod auth;
#[allow(
    dead_code,
    reason = "the Store client (network.rs, client.rs) calls these"
)]
mod bag;
mod bridge;
mod callbacks;
pub mod client;
pub mod codec;
mod compression;
mod delegate;
mod dictionary;
mod hashing;
mod memory;
mod messages;
mod metadata;
mod network;
mod notifications;
mod profile_pages;
mod purchase_adapter;
mod runtime;
#[allow(
    dead_code,
    reason = "the Store client (network.rs, client.rs) calls these"
)]
mod session;
mod swift;
mod terms_adapter;
mod value;

use std::fmt::Display;

pub use client::{Request, Signer};
pub(crate) use network::valid_method;
pub use runtime::Runtime;
pub(crate) use runtime::{After, Guest, Object, Uc, method_key};
pub use value::{Date, Payload, Value, go_float};

/// Go's `FrameworkError`: an `NSError` the Store framework finished a request with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameworkError {
    pub domain: String,
    pub message: String,
    pub code: i64,
}

impl Display for FrameworkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(
                formatter,
                "Store framework error {}/{}",
                self.domain, self.code
            )
        } else {
            write!(
                formatter,
                "{} ({}/{})",
                self.message, self.domain, self.code
            )
        }
    }
}

impl std::error::Error for FrameworkError {}

/// Emulator errors with Go's text: Go's panics inside the bridge become [`Error::Message`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Message(String),
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<Error>,
    },
    #[error(transparent)]
    Framework(#[from] FrameworkError),
    #[error(transparent)]
    Sap(#[from] ipatool_sap::Error),
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
    Joined(Vec<Error>),
    /// The Store transport's own error, which `Execute` returns unwrapped.
    #[error(transparent)]
    Transport(crate::http::BoxError),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// Go's `fmt.Errorf("context: %w", err)`.
    #[must_use]
    pub fn wrap(self, context: impl Display) -> Self {
        Self::Context {
            context: context.to_string(),
            source: Box::new(self),
        }
    }

    /// Go's `errors.As(err, &*FrameworkError)`.
    #[must_use]
    pub fn framework(&self) -> Option<&FrameworkError> {
        match self {
            Self::Framework(error) => Some(error),
            Self::Context { source, .. } => source.framework(),
            Self::Joined(errors) => errors.iter().find_map(Self::framework),
            Self::Message(_) | Self::Sap(_) | Self::Transport(_) => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `fmt.Errorf("context: %w", err)` for results.
pub(crate) trait Context<T> {
    fn context(self, context: impl Display) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, context: impl Display) -> Result<T> {
        self.map_err(|source| source.into().wrap(context))
    }
}

/// Go's `panic(fmt.Sprintf(...))` inside the bridge.
macro_rules! fail {
    ($($arg:tt)*) => {
        return Err($crate::emulator::Error::msg(format!($($arg)*)))
    };
}
pub(crate) use fail;
