//! Account-scoped Store calls (Go's `appstore` methods that need the keychain): the Store AMD
//! proof (`appstore_amd.go`), the download kbsync cache (`appstore_kbsync_cache.go`) and the
//! storefront correction (`account_storefront.go`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ipatool_keychain::Keychain;
use ipatool_store::gourl;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::storeauth::StoreAmd;
use crate::{Account, Client, Error, Result, keys};

/// `downloadDispatchDomain`.
pub(crate) const DOWNLOAD_DISPATCH_DOMAIN: &str = "downloaddispatch.itunes.apple.com";
pub(crate) const ENT_DOWNLOAD_PATH: &str = "/WebObjects/DownloadDispatch.woa/wa/ent/download";
pub(crate) const REDOWNLOAD_PATH: &str = "/r/redownload";
pub(crate) const UPDATE_PRODUCT_PATH: &str = "/up/updateProduct";
/// `PrivateAppStoreAPIDomain`.
pub(crate) const BUY_DOMAIN: &str = "buy.itunes.apple.com";
pub(crate) const PURCHASE_PATH: &str = "/WebObjects/MZFinance.woa/wa/buyProduct";
pub(crate) const VOLUME_STORE_DOWNLOAD_PATH: &str =
    "/WebObjects/MZFinance.woa/wa/volumeStoreDownloadProduct";

/// Go's `kbsyncCacheEntry`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KbsyncEntry {
    #[serde(default)]
    pub dsid: u64,
    #[serde(default)]
    pub guid: String,
    #[serde(default)]
    pub data: String,
}

/// The Go `appstore` state that outlives one request: the anisette state read once from the
/// keychain and the last kbsync blob that served a download.
#[derive(Default)]
struct SessionState {
    anisette: Vec<u8>,
    kbsync: KbsyncEntry,
}

/// Generates the download kbsync, as Go's `kbsyncGenerator` (tests replace it).
pub(crate) type KbsyncGenerator = Box<dyn Fn(&[u8], u64) -> Result<Vec<u8>> + Send + Sync>;

/// A client with the account services Go's `appstore` keeps: the keychain (account,
/// `anisette-v4`, `kbsync-v1`) and the Store AMD provider. Purchase and download live here.
pub struct StoreSession<'a> {
    pub client: &'a Client,
    pub(crate) keychain: &'a Keychain,
    store_amd: &'a dyn StoreAmd,
    state: Mutex<SessionState>,
    pub(crate) kbsync_generator: Option<KbsyncGenerator>,
    /// Go's `machine.MacAddress` (tests replace it).
    pub(crate) mac_address: fn() -> Result<String>,
}

impl std::fmt::Debug for StoreSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSession").finish_non_exhaustive()
    }
}

impl Client {
    /// The account-scoped Store calls, as Go's `appstore` with its keychain.
    #[must_use]
    pub fn store_session<'a>(
        &'a self,
        keychain: &'a Keychain,
        store_amd: &'a dyn StoreAmd,
    ) -> StoreSession<'a> {
        StoreSession {
            client: self,
            keychain,
            store_amd,
            state: Mutex::default(),
            kbsync_generator: Some(Box::new(default_kbsync_generator)),
            mac_address: crate::machine::mac_address,
        }
    }
}

/// Go's `defaultKBSyncGenerator`. A cold asset cache reads the restore image with blocking
/// HTTP, which must not run on an async runtime's thread.
fn default_kbsync_generator(hardware_id: &[u8], dsid: u64) -> Result<Vec<u8>> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let bundle = ipatool_sap::assets::load()
                    .map_err(|error| Error::from(error).context("load SAP assets for kbsync"))?;
                ipatool_sap::machine::generate_kb_sync(&bundle, hardware_id, dsid)
                    .map_err(|error| Error::from(error).context("generate kbsync"))
            })
            .join()
            .unwrap_or_else(|_| Err(Error::Invalid("kbsync guest panicked".to_owned())))
    })
}

impl<'a> StoreSession<'a> {
    /// The keychain the session reads and writes.
    #[must_use]
    pub fn keychain(&self) -> &'a Keychain {
        self.keychain
    }
}

impl StoreSession<'_> {
    /// Go's `AccountInfo`: the saved account.
    ///
    /// # Errors
    /// `failed to get account: …` or `failed to unmarshal json: …`, as Go.
    pub async fn account_info(&self) -> Result<Account> {
        let data = self
            .keychain
            .get(keys::ACCOUNT)
            .await
            .map_err(|error| Error::from(error).context("failed to get account"))?;
        serde_json::from_slice(&data)
            .map_err(|error| Error::Invalid(format!("failed to unmarshal json: {error}")))
    }

    /// Go's `storeAMDProof`: `X-Apple-AMD` headers for an allowlisted commerce URL. The
    /// state is read from the keychain once and written back only when it changes (fork:
    /// `fix/store-amd-redundant-save`).
    ///
    /// # Errors
    /// Go's validation, generation and save errors.
    pub async fn store_amd_proof(
        &self,
        endpoint: &str,
        hardware_id: &[u8],
    ) -> Result<std::collections::BTreeMap<String, String>> {
        if hardware_id.len() != 6 {
            return Err(Error::Invalid(
                "store AMD proof requires a six-byte device GUID".to_owned(),
            ));
        }
        if !is_store_amd_commerce_url(endpoint) {
            return Err(Error::Invalid(
                "store AMD proof endpoint is not allowlisted".to_owned(),
            ));
        }
        let mut state = self.state.lock().await;
        if state.anisette.is_empty() {
            state.anisette = self.keychain.get(keys::ANISETTE).await.unwrap_or_default();
        }
        let (proof, next) = self
            .store_amd
            .headers(&state.anisette, endpoint, hardware_id)
            .map_err(|error| error.context("generate Store AMD proof"))?;
        if next != state.anisette {
            self.keychain
                .set(keys::ANISETTE, &next)
                .await
                .map_err(|error| Error::from(error).context("save Store AMD state"))?;
            state.anisette = next;
        }
        Ok(proof)
    }

    /// Go's `generateKBSync`.
    pub(crate) fn generate_kbsync(&self, hardware_id: &[u8], dsid: u64) -> Result<String> {
        let Some(generator) = &self.kbsync_generator else {
            return Err(Error::Invalid("kbsync generator is unavailable".to_owned()));
        };
        let data = generator(hardware_id, dsid)
            .map_err(|error| error.context("generate download kbsync"))?;
        if data.is_empty() {
            return Err(Error::Invalid(
                "generated download kbsync is empty".to_owned(),
            ));
        }
        Ok(STANDARD.encode(data))
    }

    /// Go's `storeKBSync`: best effort, a keychain failure does not fail the download.
    pub(crate) async fn store_kbsync(&self, dsid: u64, guid: &str, data: &str) {
        let entry = KbsyncEntry {
            dsid,
            guid: guid.to_owned(),
            data: data.to_owned(),
        };
        let mut state = self.state.lock().await;
        if let Ok(encoded) = serde_json::to_vec(&entry) {
            let _ = self.keychain.set(keys::KBSYNC, &encoded).await;
        }
        state.kbsync = entry;
    }

    /// Go's `cachedKBSync`.
    pub(crate) async fn cached_kbsync(&self, dsid: u64, guid: &str) -> String {
        let mut state = self.state.lock().await;
        let entry = &state.kbsync;
        if entry.dsid == dsid && entry.guid == guid && !entry.data.is_empty() {
            return entry.data.clone();
        }
        let Ok(encoded) = self.keychain.get(keys::KBSYNC).await else {
            return String::new();
        };
        let Ok(entry) = serde_json::from_slice::<KbsyncEntry>(&encoded) else {
            return String::new();
        };
        if entry.dsid != dsid || entry.guid != guid {
            return String::new();
        }
        if STANDARD
            .decode(&entry.data)
            .map_or(true, |data| data.is_empty())
        {
            return String::new();
        }
        let data = entry.data.clone();
        state.kbsync = entry;
        data
    }

    /// Go's `invalidateKBSync`: removes only the exact rejected entry.
    pub(crate) async fn invalidate_kbsync(&self, dsid: u64, guid: &str, data: &str) {
        let rejected = KbsyncEntry {
            dsid,
            guid: guid.to_owned(),
            data: data.to_owned(),
        };
        let mut state = self.state.lock().await;
        if state.kbsync == rejected {
            state.kbsync = KbsyncEntry::default();
        }
        let Ok(encoded) = self.keychain.get(keys::KBSYNC).await else {
            return;
        };
        if serde_json::from_slice::<KbsyncEntry>(&encoded).is_ok_and(|entry| entry == rejected) {
            let _ = self.keychain.remove(keys::KBSYNC).await;
        }
    }

    /// Go's `saveAccountStorefront`. Go serializes it with native login through
    /// `authStateMu`; the session lock does the same within this process.
    pub(crate) async fn save_account_storefront(&self, dsid: &str, storefront: &str) -> Result<()> {
        let _state = self.state.lock().await;
        if dsid.is_empty() {
            return Err(Error::Invalid(
                "saved account is required for storefront recovery".to_owned(),
            ));
        }
        let mut account = self.account_info().await?;
        if account.directory_services_id != dsid {
            return Err(Error::Invalid(
                "saved account changed during storefront recovery".to_owned(),
            ));
        }
        storefront.clone_into(&mut account.store_front);
        let data = serde_json::to_vec(&account).map_err(|error| {
            Error::Invalid(format!("failed to encode corrected account: {error}"))
        })?;
        self.keychain
            .set(keys::ACCOUNT, &data)
            .await
            .map_err(|error| Error::from(error).context("failed to save corrected account"))
    }
}

/// Go's `isStoreAMDCommerceURL`.
pub(crate) fn is_store_amd_commerce_url(endpoint: &str) -> bool {
    if endpoint.contains('#') {
        return false;
    }
    let Ok(parsed) = gourl::parse_request_uri(endpoint) else {
        return false;
    };
    if parsed.scheme != "https"
        || parsed.user.is_some()
        || !parsed.fragment.is_empty()
        || !parsed.raw_path.is_empty()
        || !parsed.opaque.is_empty()
        || (!parsed.port().is_empty() && parsed.port() != "443")
    {
        return false;
    }
    let (query, error) = gourl::parse_query(&parsed.raw_query);
    if error.is_some() || parsed.force_query || query.len() != 1 {
        return false;
    }
    let values = query.values("guid");
    if values.len() != 1 || values[0].is_empty() {
        return false;
    }
    let host = parsed.hostname().to_lowercase();
    if host == DOWNLOAD_DISPATCH_DOMAIN {
        return matches!(
            parsed.path.as_str(),
            ENT_DOWNLOAD_PATH | REDOWNLOAD_PATH | UPDATE_PRODUCT_PATH
        );
    }
    is_store_buy_host(&host) && parsed.path == VOLUME_STORE_DOWNLOAD_PATH
}

/// Go's `isStoreBuyHost`.
pub(crate) fn is_store_buy_host(host: &str) -> bool {
    if host == BUY_DOMAIN {
        return true;
    }
    let Some(pod) = host.strip_suffix("-buy.itunes.apple.com") else {
        return false;
    };
    pod.len() >= 2 && pod.starts_with('p') && pod[1..].bytes().all(|digit| digit.is_ascii_digit())
}
