//! Native App Store login (Go `appstore_native_login.go`): anisette or Store AMD headers in
//! front of the legacy login, and Apple's `RP` and `SP` provisioning requests.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ipatool_keychain::Keychain;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::store::STORE_USER_AGENT;
use crate::storeauth::{self, AuthHttp, StoreAmd};
use crate::{Client, Error, LoginInput, LoginOutput, Result, keys, machine};

/// Go's `privateAppStoreProvisioningURL`.
const PROVISIONING_URL: &str = "https://auth.itunes.apple.com/auth/v1/otp/1.5/finish";
/// Go passes `^uint64(1)` as the DSID of the provisioning kbsync.
const PROVISIONING_DSID: u64 = !1;
/// The anisette headers Go removes before retrying after an `RP` action.
const MACHINE_HEADERS: [&str; 3] = ["X-Apple-I-MD", "X-Apple-I-MD-M", "X-Apple-I-MD-RINFO"];

/// The steps of the native flow, so tests can run it without Apple or the emulator.
pub(crate) trait NativeBackend {
    fn mac_address(&self) -> Result<String>;
    async fn auth_endpoint(&self, guid: &str) -> Result<String>;
    fn store_amd(
        &self,
        state: &[u8],
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)>;
    async fn anisette(&self, state: &[u8]) -> Result<(HeaderMap, Vec<u8>)>;
    async fn kbsync(&self, hardware_id: &[u8], dsid: u64) -> Result<Vec<u8>>;
    async fn provision(
        &self,
        state: &[u8],
        challenge: &str,
        kbsync: &str,
        hardware_id: &[u8],
        identity: &HeaderMap,
    ) -> Result<Vec<u8>>;
    async fn legacy_login(&self, input: &LoginInput, headers: &HeaderMap) -> Result<LoginOutput>;
}

struct Live<'a, A> {
    client: &'a Client,
    keychain: &'a Keychain,
    http: AuthHttp,
    store_amd: &'a A,
}

impl<A: StoreAmd> NativeBackend for Live<'_, A> {
    fn mac_address(&self) -> Result<String> {
        machine::mac_address()
    }

    async fn auth_endpoint(&self, guid: &str) -> Result<String> {
        Ok(self.client.sap_config(guid).await?.auth_endpoint)
    }

    fn store_amd(
        &self,
        state: &[u8],
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
        self.store_amd.headers(state, endpoint, hardware_id)
    }

    async fn anisette(&self, state: &[u8]) -> Result<(HeaderMap, Vec<u8>)> {
        storeauth::anisette_headers(&self.http, state).await
    }

    /// Go's `defaultKBSyncGenerator`.
    async fn kbsync(&self, hardware_id: &[u8], dsid: u64) -> Result<Vec<u8>> {
        // A cold cache reads the restore image with blocking HTTP.
        let bundle = tokio::task::spawn_blocking(ipatool_sap::assets::load)
            .await
            .map_err(|error| Error::Invalid(format!("load SAP assets for kbsync: {error}")))?
            .map_err(|error| Error::from(error).context("load SAP assets for kbsync"))?;
        ipatool_sap::machine::generate_kb_sync(&bundle, hardware_id, dsid)
            .map_err(|error| Error::from(error).context("generate kbsync"))
    }

    async fn provision(
        &self,
        state: &[u8],
        challenge: &str,
        kbsync: &str,
        hardware_id: &[u8],
        identity: &HeaderMap,
    ) -> Result<Vec<u8>> {
        storeauth::provision_store_anisette(
            &self.http,
            state,
            challenge,
            PROVISIONING_URL,
            STORE_USER_AGENT,
            kbsync,
            hardware_id,
            identity,
        )
        .await
    }

    async fn legacy_login(&self, input: &LoginInput, headers: &HeaderMap) -> Result<LoginOutput> {
        self.client.login(input, self.keychain, Some(headers)).await
    }
}

impl Client {
    /// Go's `Login` with the Store backend (`nativeLogin`): reuses a provisioned Store AMD
    /// state, else provisions anisette with `gsa.apple.com`, and answers Apple's `RP` and
    /// `SP` machine actions before the legacy login. The anisette state is saved under
    /// `anisette-v4` whenever it changes.
    ///
    /// # Errors
    /// The errors of [`Client::login`], and the provisioning errors with Go's text.
    pub async fn native_login(
        &self,
        input: &LoginInput,
        keychain: &Keychain,
        store_amd: &impl StoreAmd,
    ) -> Result<LoginOutput> {
        let backend = Live {
            client: self,
            keychain,
            http: AuthHttp::new(self.jar.clone())?,
            store_amd,
        };
        native_login(&backend, keychain, input).await
    }
}

/// The anisette state in memory and in the keychain. Unchanged states are not written again:
/// repeated identical writes made gnome-keyring drop its D-Bus connection.
struct State<'a> {
    keychain: &'a Keychain,
    data: Vec<u8>,
}

impl State<'_> {
    async fn save(&mut self, data: Vec<u8>, context: &'static str) -> Result<()> {
        if data != self.data {
            self.keychain
                .set(keys::ANISETTE, &data)
                .await
                .map_err(|error| Error::from(error).context(context))?;
            self.data = data;
        }
        Ok(())
    }
}

pub(crate) async fn native_login(
    backend: &impl NativeBackend,
    keychain: &Keychain,
    input: &LoginInput,
) -> Result<LoginOutput> {
    let mut state = State {
        keychain,
        // Like Go, a missing or unreadable state starts a new identity.
        data: keychain.get(keys::ANISETTE).await.unwrap_or_default(),
    };
    let mac_address = backend
        .mac_address()
        .map_err(|error| error.context("get Store machine identity"))?;
    let (guid, hardware_id) = machine::identity(&mac_address)?;
    let endpoint = backend
        .auth_endpoint(&guid)
        .await
        .map_err(|error| error.context("get Store authentication bag"))?;

    if !state.data.is_empty()
        && let Ok((proof, next)) = backend.store_amd(&state.data, &endpoint, &hardware_id)
    {
        state.save(next, "save Apple authentication state").await?;
        return backend.legacy_login(input, &header_map(&proof)?).await;
    }

    let (identity, next) = backend
        .anisette(&state.data)
        .await
        .map_err(|error| error.context("prepare Apple authentication"))?;
    state.save(next, "save Apple authentication state").await?;
    let mut headers = identity.clone();

    let result = backend.legacy_login(input, &headers).await;
    if !is_action(&result, "RP") {
        return result;
    }
    for name in MACHINE_HEADERS {
        headers.remove(name);
    }

    let result = backend.legacy_login(input, &headers).await;
    let challenge = match &result {
        Err(error) => match error.machine_action() {
            Some(action) if action.action == "SP" => action.data.clone(),
            _ => return result,
        },
        Ok(_) => return result,
    };

    let kbsync = backend
        .kbsync(&hardware_id, PROVISIONING_DSID)
        .await
        .map_err(|error| error.context("generate download kbsync"))
        .and_then(|data| {
            if data.is_empty() {
                Err(Error::Invalid(
                    "generated download kbsync is empty".to_owned(),
                ))
            } else {
                Ok(STANDARD.encode(data))
            }
        })
        .map_err(|error| error.context("generate Apple Store provisioning kbsync"))?;
    let next = backend
        .provision(&state.data, &challenge, &kbsync, &hardware_id, &identity)
        .await
        .map_err(|error| error.context("complete Apple Store provisioning"))?;
    state.save(next, "save Apple authentication state").await?;
    let (proof, next) = backend
        .store_amd(&state.data, &endpoint, &hardware_id)
        .map_err(|error| error.context("prepare Store AMD proof"))?;
    state.save(next, "save Store AMD state").await?;
    backend.legacy_login(input, &header_map(&proof)?).await
}

fn is_action(result: &Result<LoginOutput>, name: &str) -> bool {
    result
        .as_ref()
        .err()
        .and_then(Error::machine_action)
        .is_some_and(|action| action.action == name)
}

fn header_map(proof: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in proof {
        let name = HeaderName::try_from(name.as_str())
            .map_err(|_| Error::Invalid(format!("invalid Store AMD header name {name:?}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| Error::Invalid(format!("invalid Store AMD header value for {name}")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests;
