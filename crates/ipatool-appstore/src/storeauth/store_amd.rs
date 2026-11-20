//! The Store AMD headers contract (Go `StoreAMDHeaders`, `store_amd.go`). Producing them
//! runs `AppleMediaServices` in the Store framework emulator, which arrives in phase 5.

use std::collections::BTreeMap;

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

/// Stands in until the Store framework emulator is ported: checks the state as Go does, then
/// fails.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableStoreAmd;

impl StoreAmd for UnavailableStoreAmd {
    fn headers(
        &self,
        state: &[u8],
        _endpoint: &str,
        _hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
        provisioned_store_amd_state(state)?;
        Err(Error::NotPorted(
            "the Store AMD framework (the phase 5 Store emulator)",
        ))
    }
}
