//! Offline `FairPlay` decryption of App Store IPAs.
//!
//! This crate ports ipadecrypt's `internal/fairplay` (londek/ipadecrypt, MIT), itself a
//! Go port of EthanArbuckle/fairplay-decrypt (`core.py` and `decrypt.py`), which is
//! licensed under the GNU General Public License version 3 "or any later version". The
//! crate began as a port of the Python code, so it is licensed GPL-3.0-or-later (see the
//! crate's `LICENSE`, and `LICENSE-ipadecrypt` for the MIT notice), unlike the MIT
//! workspace it lives in.
//!
//! It derives per-page keys from an account's 16-byte credential (or its Arcade
//! credential, for Apple Arcade licenses) and the app's SINF and SUPF/SUPP records,
//! decrypts one ARM slice of each encrypted Mach-O (arm64, else arm64e, else `arm64_32`,
//! else 32-bit ARM), thins the binary to that slice and clears `cryptid`, then checks that
//! no binary in the output is left encrypted.
//!
//! Credential minting and on-device decryption are not in this module set.

mod core;
mod error;
mod ipa;
mod verify;

pub use crate::core::{
    CREDENTIAL_SIZE, MachoInfo, arm_encryption_info, arm_supf_record_offset, credential_decodes,
    decrypt_binary, derive_work16, is_arcade_sinf, pclass_page_key_offset,
};
pub use crate::error::{Error, Result};
pub use crate::ipa::{Credentials, Options, Summary, decrypt_ipa};
pub use crate::verify::Verification;
