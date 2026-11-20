//! Mach-O image mapping for the SAP guest. Port of Go `internal/sap/machimage`.

mod fat;
mod fixups;
mod image;
mod macho;
mod preparation_cache;
mod shared;
#[cfg(test)]
mod tests;

pub use fat::{arm64_range, arm64_slice, read_arm64};
pub use image::{Image, Resolve, without_pointer_authentication};
pub use macho::{Bind, Rebase, Symbol};

use crate::error::Result;

const PAGE_SIZE: u64 = 0x1000;
const POINTER_SIZE: u64 = 8;

/// Guest memory an image is loaded into.
pub trait Memory {
    fn mem_map(&mut self, address: u64, size: u64) -> Result<()>;
    fn mem_write(&mut self, address: u64, data: &[u8]) -> Result<()>;
}

const fn add(left: u64, right: u64) -> Option<u64> {
    left.checked_add(right)
}

const fn align(value: u64, alignment: u64) -> u64 {
    value.wrapping_add(alignment - 1) & !(alignment - 1)
}
