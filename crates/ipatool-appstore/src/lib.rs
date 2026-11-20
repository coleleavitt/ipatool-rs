//! App Store client used by the `ipatool` CLI.
//!
//! This is a port of the Go `pkg/appstore` package from
//! <https://github.com/majd/ipatool>. See `PLAN.md` for what is ported so far.

mod account;
mod app;
mod auth_http;
mod bag;
mod catalog;
mod client;
mod cookies;
#[allow(dead_code, reason = "placeholder until the phase6-ios merge")]
mod download;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod download_macos;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod download_package;
mod error;
mod login;
mod login_storefront;
pub mod machine;
#[cfg(test)]
mod macos_oracle_tests;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod macos_preflight_metadata;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod macos_version_lookup;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod macos_version_metadata;
mod native_login;
mod platform;
mod store;
pub mod storeauth;
mod storefront;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by Client::download after the phase6-ios merge"
    )
)]
mod xar;

use ipatool_store::plist_body;

pub use account::{Account, keys};
pub use app::App;
pub use bag::{SapConfig, UrlBag};
pub use catalog::{CatalogVersion, CountryCode, LookupQuery};
pub use client::Client;
pub use cookies::{CookieJar, CookieJarError};
pub use error::{Error, MachineAction, Result, TransportError, UnexpectedResponseError};
pub use login::{LoginInput, LoginOutput};
pub use platform::Platform;
pub use storefront::storefront_identifier;
