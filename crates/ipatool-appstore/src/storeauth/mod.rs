//! Apple authentication support: local anisette provisioning, the Store-specific ADI
//! provisioning and the Store AMD state. Port of Go `internal/storeauth`.
//!
//! The ADI guest ([`ModernAdi`]) is single-threaded and not `Send`, so the futures here are not
//! `Send` either; the guest calls run synchronously between the HTTP requests, as in Go.
//!
//! Every fallible function returns Go's error text; per-function `# Errors` sections would
//! only repeat it.
#![allow(clippy::missing_errors_doc)]

mod anisette;
mod base64_go;
mod http_client;
mod plist_xml;
mod state;
mod store_amd;
mod store_provisioning;

use ipatool_sap::machine::ModernAdi;

pub use anisette::anisette_headers;
pub use http_client::AuthHttp;
pub use state::{AnisetteState, decode_anisette_state};
pub use store_amd::{EmulatorStoreAmd, StoreAmd, provisioned_store_amd_state};
pub use store_provisioning::provision_store_anisette;

use crate::{Error, Result};

/// Go's `anisetteLimit`: the cap on every provisioning blob and response body.
const ANISETTE_LIMIT: usize = 1 << 20;

/// The guest calls the provisioning flows make, so tests can run them without the emulator.
trait Adi {
    fn start_provisioning(&mut self, dsid: u64, spim: &[u8])
    -> ipatool_sap::Result<(Vec<u8>, u32)>;
    fn end_provisioning(
        &mut self,
        session: u32,
        setting_info: &[u8],
        transport_key: &[u8],
    ) -> ipatool_sap::Result<()>;
    fn request_otp(&mut self, dsid: u64) -> ipatool_sap::Result<(Vec<u8>, Vec<u8>)>;
    fn import_key_bag(&mut self, hardware_id: &[u8], key_bag: &[u8]) -> ipatool_sap::Result<()>;
    fn state(&mut self) -> ipatool_sap::Result<Vec<u8>>;
}

impl Adi for ModernAdi {
    fn start_provisioning(
        &mut self,
        dsid: u64,
        spim: &[u8],
    ) -> ipatool_sap::Result<(Vec<u8>, u32)> {
        ModernAdi::start_provisioning(self, dsid, spim)
    }

    fn end_provisioning(
        &mut self,
        session: u32,
        setting_info: &[u8],
        transport_key: &[u8],
    ) -> ipatool_sap::Result<()> {
        ModernAdi::end_provisioning(self, session, setting_info, transport_key)
    }

    fn request_otp(&mut self, dsid: u64) -> ipatool_sap::Result<(Vec<u8>, Vec<u8>)> {
        ModernAdi::request_otp(self, dsid)
    }

    fn import_key_bag(&mut self, hardware_id: &[u8], key_bag: &[u8]) -> ipatool_sap::Result<()> {
        ModernAdi::import_key_bag(self, hardware_id, key_bag)
    }

    fn state(&mut self) -> ipatool_sap::Result<Vec<u8>> {
        ModernAdi::state(self)
    }
}

/// Go's `assets.Load` + `machine.OpenModernADI` with a fresh ADI database.
async fn open_fresh_adi(
    identifier: &[u8; 16],
    load_context: &str,
    open_context: &str,
) -> Result<ModernAdi> {
    // A cold cache reads the restore image with blocking HTTP, which must not run on the runtime.
    let bundle = tokio::task::spawn_blocking(ipatool_sap::assets::load)
        .await
        .map_err(|error| Error::Invalid(format!("{load_context}: {error}")))?
        .map_err(|error| error.wrap(load_context))?;
    Ok(ModernAdi::open(&bundle, identifier, &[]).map_err(|error| error.wrap(open_context))?)
}

/// `fmt.Errorf("context: %w", err)` for guest errors.
fn guest(context: &'static str) -> impl FnOnce(ipatool_sap::Error) -> Error {
    move |error| Error::Sap(error.wrap(context))
}

/// `fmt.Errorf("context: %w", err)` for transport errors.
fn transport(context: &'static str) -> impl FnOnce(reqwest::Error) -> Error {
    move |error| Error::from(error).context(context)
}

/// Go's `headerValue`: non-empty printable ASCII of at most `limit` bytes.
fn header_value(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && value.chars().all(|c| (' '..='~').contains(&c))
}

#[cfg(test)]
mod tests;
