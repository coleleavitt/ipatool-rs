//! App Store client used by the `ipatool` CLI.
//!
//! This is a port of the Go `pkg/appstore` package from
//! <https://github.com/majd/ipatool>. See `PLAN.md` for what is ported so far.

mod account;
mod app;
mod catalog;
mod client;
mod cookies;
mod error;
mod platform;

pub use account::{Account, keys};
pub use app::App;
pub use catalog::{CatalogVersion, CountryCode, LookupQuery};
pub use client::Client;
pub use cookies::{CookieJar, CookieJarError};
pub use error::{Error, Result};
pub use platform::Platform;
