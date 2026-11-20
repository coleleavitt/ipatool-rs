//! The Store AMD headers (Go `StoreAMDHeaders`, `store_amd.go`): `AppleMediaServices`'
//! type-2 anisette flow and its `com.apple.adid` dependency, both run in the pinned Unicorn
//! guests.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use ipatool_sap::machine::ModernAdi;
use ipatool_store::emulator::{self, Runtime};

use super::{AnisetteState, decode_anisette_state};
use crate::{Error, Result};

/// Go's `StoreAMDHeaders`: `X-Apple-AMD` and `X-Apple-AMD-M` for `endpoint`, with the updated
/// anisette state (Go's JSON). Implementations start with [`provisioned_store_amd_state`].
pub trait StoreAmd {
    fn headers(
        &self,
        state: &[u8],
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)>;
}

/// Go's state checks before the Store AMD flow: a valid state with an ADI database.
pub fn provisioned_store_amd_state(encoded: &[u8]) -> Result<AnisetteState> {
    let state = decode_anisette_state(encoded)?;
    if state.provisioning.as_ref().is_none_or(Vec::is_empty) {
        return Err(Error::Invalid(
            "store AMD state is not provisioned; run 'ipatool auth login' to authenticate"
                .to_owned(),
        ));
    }
    Ok(state)
}

/// The Store AMD flow in the emulators: modern ADI answers the framework's XPC messages.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmulatorStoreAmd;

impl StoreAmd for EmulatorStoreAmd {
    fn headers(
        &self,
        state: &[u8],
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
        // The guests are not `Send`, and a cold asset cache reads the restore image with
        // blocking HTTP, which must not run on an async runtime's thread.
        std::thread::scope(|scope| {
            scope
                .spawn(|| store_amd_headers(state, endpoint, hardware_id))
                .join()
                .unwrap_or_else(|_| Err(Error::Invalid("Store AMD guest panicked".to_owned())))
        })
    }
}

fn store_amd_headers(
    encoded: &[u8],
    endpoint: &str,
    hardware_id: &[u8],
) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
    let mut state = provisioned_store_amd_state(encoded)?;
    let bundle = ipatool_sap::assets::load()
        .map_err(|error| Error::from(error).context("load Store AMD assets"))?;
    let provisioning = state.provisioning.clone().unwrap_or_default();
    let adi = ModernAdi::open(&bundle, &state.identifier, &provisioning)
        .map_err(|error| Error::from(error).context("open Store AMD service"))?;
    let adi = Rc::new(RefCell::new(adi));
    let prepared = ipatool_store::profile::load().map_err(|error| {
        Error::Invalid(error.to_string()).context("load Store AMD framework profile")
    })?;
    let mut framework = Runtime::open(&bundle.apple_media_services, prepared)
        .map_err(|error| Error::from(error).context("open Store AMD framework"))?;
    let service = Rc::clone(&adi);
    let headers = framework
        .store_amd_headers(
            endpoint,
            hardware_id,
            Box::new(move |name, request| {
                if name != "com.apple.adid" {
                    return Err(emulator::Error::msg(format!(
                        "unsupported Store AMD XPC service {name}"
                    )));
                }
                Ok(service.borrow_mut().exchange_xpc(request)?)
            }),
        )
        .map_err(|error| Error::from(error).context("run Store AMD framework"))?;
    drop(framework);
    let provisioning = adi
        .borrow_mut()
        .state()
        .map_err(|error| Error::from(error).context("persist Store AMD state"))?;
    state.provisioning = Some(provisioning);
    Ok((headers, state.to_json()))
}
