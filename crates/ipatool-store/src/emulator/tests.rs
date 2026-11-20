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
