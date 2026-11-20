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
mod commerce;
#[cfg(test)]
mod commerce_tests;
mod cookies;
mod download;
mod download_file;
mod download_macos;
mod download_package;
mod download_product;
mod download_types;
mod error;
mod login;
mod login_storefront;
pub mod machine;
#[cfg(test)]
mod macos_oracle_tests;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by get-version-metadata, phase 7")
)]
mod macos_preflight_metadata;
mod macos_version_lookup;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by get-version-metadata, phase 7")
)]
mod macos_version_metadata;
mod native_login;
mod platform;
mod purchase;
mod replicate_sinf;
mod session;
mod store;
pub mod storeauth;
mod storefront;
mod version_lookup;
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "the metadata readers are used by get-version-metadata, phase 7"
    )
)]
mod xar;
mod zipfile;

use ipatool_store::plist_body;

pub use account::{Account, keys};
pub use app::App;
pub use bag::{SapConfig, UrlBag};
pub use catalog::{CatalogVersion, CountryCode, LookupQuery};
pub use client::Client;
pub use commerce::{CommerceRequest, Payload, StoreResponse, StoreResult, StoreTransport};
pub use cookies::{CookieJar, CookieJarError};
pub use download_types::{
    DownloadInput, DownloadItem, DownloadOutput, DownloadProduct, DownloadResult, Progress,
    ProgressHandle, Sinf,
};
pub use error::{Error, MachineAction, Result, TransportError, UnexpectedResponseError};
pub use ipatool_store::backend::ResponseFormat;
pub use ipatool_store::emulator::Value;
pub use ipatool_store::http::{Header, HttpRequest, HttpResponse};
pub use login::{LoginInput, LoginOutput};
pub use platform::Platform;
pub use session::StoreSession;
pub use storefront::storefront_identifier;
