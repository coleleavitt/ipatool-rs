//! Port of the Go `internal/store` packages from <https://github.com/majd/ipatool>: the
//! precomputed Store profile and (in progress) the framework emulator. See `PLAN.md`.
//!
//! Every fallible function returns Go's error text; per-function `# Errors` sections would
//! only repeat it.
#![allow(clippy::missing_errors_doc)]

pub mod emulator;

pub(crate) mod gourl;
pub mod profile;
pub mod terms;
