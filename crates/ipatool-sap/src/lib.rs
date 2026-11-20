//! Apple SAP signing. Port of Go `internal/sap`.
//!
//! Every fallible function returns Go's error text; per-function `# Errors` sections would
//! only repeat it.
#![allow(clippy::missing_errors_doc)]

pub mod assets;
pub mod engine;
pub mod error;
pub mod machimage;
pub mod machine;
mod protocol;
pub mod signer;

pub use error::{Error, Result};
