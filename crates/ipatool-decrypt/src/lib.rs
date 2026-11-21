//! Offline `FairPlay` decryption of App Store IPAs.
//!
//! This crate is a port of EthanArbuckle/fairplay-decrypt (`core.py` and `decrypt.py`),
//! which is licensed under the GNU General Public License version 3 "or any later version".
//! This port is therefore licensed GPL-3.0-or-later (see the crate's `LICENSE`), unlike the
//! MIT workspace it lives in. It derives per-page keys from an account's 16-byte credential
//! and the app's SINF and SUPF records, decrypts the encrypted arm64 Mach-O pages and clears
//! `cryptid`.
//!
//! Only offline decryption is ported. Credential minting and on-device decryption are out
//! of scope.

mod core;
mod error;
mod ipa;

pub use crate::core::{
    MachoInfo, arm64_encryption_infos, arm64_supf_record_offset, decrypt_binary, derive_work16,
};
pub use crate::error::{Error, Result};
pub use crate::ipa::{Summary, decrypt_ipa};
