//! Go's `appstore_macos_version_lookup.go`. Placeholder until the macOS port (branch
//! `phase6-macos`) replaces this file.

use crate::{Account, App, Client, Error, Result};

impl Client {
    /// Go's `lookupLatestMacOSExternalVersionID`.
    ///
    /// # Errors
    /// Not ported in this branch.
    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the macOS port fills this in"
    )]
    pub(crate) async fn lookup_latest_macos_external_version_id(
        &self,
        _account: &Account,
        _app: &App,
    ) -> Result<String> {
        Err(Error::NotPorted("macOS version lookup"))
    }
}
