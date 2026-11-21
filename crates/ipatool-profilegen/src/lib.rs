//! Port of Go `internal/store/profilegen` from <https://github.com/majd/ipatool>: bootstraps
//! the Store runtime profile from the pinned, on-disk Apple images under Unicorn. Only the
//! `profilegen` binary runs it; normal builds embed its committed output.

mod classes;
mod collect;
mod discover;
mod emit;
mod error;
mod features;
mod objc;
mod realize;
mod references;
mod source;
mod swift_metadata;
mod symbols;
mod validate;
mod vm;

use ipatool_sap::assets::StoreCache;
use ipatool_store::profile::Profile;
use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

pub use emit::{Artifacts, compress};
pub use error::{Error, Result};

use error::Wrap as _;

/// Go's `FrameworkUUID` and `PluginUUID`: the Apple images the bootstrap consumes.
pub const FRAMEWORK_UUID: &str = "5DFC37A5-4689-3F9D-9655-D4774E1D8D08";
pub const PLUGIN_UUID: &str = "B8C07F94-4868-3CD7-A59A-F01E8C24C1B7";

/// Go's `bootstrapRevision`: incremented whenever generation or the profile format changes.
const BOOTSTRAP_REVISION: &str = "3";

/// Go's `Version`: identifies the pinned inputs and the bootstrap implementation.
#[must_use]
pub fn version() -> String {
    format!(
        "macos-27-26A434-unicorn-{BOOTSTRAP_REVISION}-{}",
        &digest(format!("{FRAMEWORK_UUID}{PLUGIN_UUID}").as_bytes())[..16]
    )
}

pub(crate) fn digest(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Lowercase hexadecimal, Go's `%x` of a byte slice.
#[must_use]
pub fn hex(data: &[u8]) -> String {
    data.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Go's `Result`: a self-contained metadata profile and its reconstructed pages. No native
/// reference data is accepted during generation.
pub struct Generated {
    pub profile: Profile,
    pub pages: Vec<u8>,
}

/// Go's `Generate`: executes the pinned Swift runtime under Unicorn and inspects the Mach-O
/// metadata read from `cache`. The host never loads an Apple framework.
///
/// # Errors
///
/// Fails when an input does not match the pinned images or the guest faults.
pub fn generate(cache: &StoreCache<'_>) -> Result<Generated> {
    run(cache).map_err(|error| error.recover("generate Store profile"))
}

fn run(cache: &StoreCache<'_>) -> Result<Generated> {
    const ASSETS: &str = "generate Store assets";
    let mut source = source::Source::new(cache).wrap(ASSETS)?;
    let requests = source
        .discover_swift_requests()
        .wrap("discover Swift metadata")?;
    let mut classes = source.objc_classes()?;
    let mut vm = vm::new_vm(source).wrap(ASSETS)?;
    realize::realize_store_classes(&mut vm, &mut classes).wrap(ASSETS)?;
    let mut profile = swift_metadata::swift_metadata(&mut vm, &requests).wrap(ASSETS)?;
    profile.features = vm.get_data().source.features().wrap(ASSETS)?;
    let pages = collect::collect(&mut vm, &mut profile, &classes)?;
    Ok(Generated { profile, pages })
}

#[cfg(test)]
mod tests;
