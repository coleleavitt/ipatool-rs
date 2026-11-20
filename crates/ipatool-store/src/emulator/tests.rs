//! Tests against the pinned frameworks. Opt-in: `IPATOOL_TEST_SAP_CACHE=1` (needs the SAP
//! asset cache filled).

use super::runtime::{HEAP_BASE, open};
use super::{Guest, Runtime};
use crate::profile;

fn enabled() -> bool {
    std::env::var_os("IPATOOL_TEST_SAP_CACHE").is_some()
}

fn image() -> Vec<u8> {
    ipatool_sap::assets::load().unwrap().apple_media_services
}

#[test]
fn opens_the_pinned_frameworks() {
    if !enabled() {
        return;
    }
    let runtime = Runtime::open(&image(), profile::load().unwrap()).unwrap();
    assert_ne!(runtime.class_address("AMSAnisette"), 0);
}

#[test]
fn rejects_an_omitted_page_even_though_its_neighbors_share_a_grouped_mapping() {
    if !enabled() {
        return;
    }
    let prepared = profile::load().unwrap();
    let mut metadata = prepared.profile.clone();
    // Bootstrap heap pages are outside both Apple images and the guest heap. Drop an interior
    // page so its neighbors still map the omitted address.
    let index = (1..metadata.pages.len() - 1)
        .find(|&index| {
            let address = metadata.pages[index].address;
            address > 1 << 40
                && address < HEAP_BASE
                && metadata.pages[index - 1].address + 4096 == address
                && metadata.pages[index + 1].address == address + 4096
        })
        .unwrap();
    let missing = metadata.pages.remove(index).address;
    let changed = profile::Data {
        metadata: serde_json::to_vec(&metadata).unwrap(),
        profile: metadata,
        pages: prepared.pages.clone(),
        plugin: prepared.plugin.clone(),
    };
    let runtime = open(&image(), &changed, true).unwrap();
    assert!(runtime.uc.sti().pages.contains(&missing));
    let error = runtime.uc.read_mem(missing, 8).unwrap_err().to_string();
    assert!(error.contains("missing Store profile page"), "{error}");
}

#[test]
fn runs_the_store_amd_provider_up_to_the_adid_exchange() {
    if !enabled() {
        return;
    }
    let mut runtime = Runtime::open(&image(), profile::load().unwrap()).unwrap();
    let error = runtime
        .store_amd_headers(
            "https://buy.itunes.apple.com/WebObjects/MZFinance.woa/wa/authenticate",
            &[0, 1, 2, 3, 4, 5],
            Box::new(|service, request| {
                assert_eq!(service, "com.apple.adid");
                assert_ne!(request, b"");
                Err(super::Error::msg("test adid unavailable"))
            }),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("test adid unavailable"), "{error}");
}

/// Byte-for-byte comparison with the Go emulator's Store AMD flow, recorded by
/// `scripts/store-amd-oracle.sh` from a copy of a real anisette-v4 state: the same fixed time
/// and counter randomness, the recorded `com.apple.adid` replies, and the same headers, call
/// trace and heap size. Opt-in: `IPATOOL_STORE_AMD_ORACLE=<record>` and the SAP asset cache.
#[test]
fn store_amd_oracle_matches_go() {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::rc::Rc;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::Deserialize;

    use super::value::{Date, TEST_NOW, TEST_RANDOM};

    #[derive(Deserialize)]
    struct Exchange {
        service: String,
        request: String,
        reply: String,
    }

    #[derive(Deserialize)]
    struct Record {
        endpoint: String,
        hardware_id: String,
        exchanges: Vec<Exchange>,
        headers: Option<BTreeMap<String, String>>,
        error: String,
        trace: Vec<String>,
        cursor: u64,
    }

    let Some(path) = std::env::var_os("IPATOOL_STORE_AMD_ORACLE") else {
        return;
    };
    let record: Record = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let decode = |text: &str| STANDARD.decode(text).unwrap();
    TEST_NOW.set(Some(Date::unix(1_795_200_000, 123_456_789)));
    TEST_RANDOM.set(Some(0));
    let mut runtime = Runtime::open(&image(), profile::load().unwrap()).unwrap();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&trace);
    runtime
        .uc
        .add_code_hook(0, u64::MAX, move |uc, address, _| {
            if let Some(call) = uc.get_data().meta.calls.get(&address) {
                recorded
                    .borrow_mut()
                    .push(format!("{}{}", call.symbol, call.selector));
            }
        })
        .unwrap();
    let mut exchanges: VecDeque<_> = record.exchanges.into_iter().collect();
    let result = runtime.store_amd_headers(
        &record.endpoint,
        &decode(&record.hardware_id),
        Box::new(move |service, request| {
            let exchange = exchanges.pop_front().expect("an unrecorded XPC exchange");
            assert_eq!(service, exchange.service);
            assert_eq!(request, decode(&exchange.request));
            Ok(decode(&exchange.reply))
        }),
    );
    match result {
        Ok(headers) => assert_eq!(Some(headers), record.headers),
        Err(error) => assert_eq!(error.to_string(), record.error),
    }
    assert_eq!(*trace.borrow(), record.trace);
    assert_eq!(runtime.uc.sti().cursor, record.cursor);
}
