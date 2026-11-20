//! Extraction from the macOS restore image. Port of Go `assets/remote.go` and its readers.

use super::Bundle;
use crate::error::{Error, Result};

/// Returns the frameworks as stored in the restore image (universal binaries intact).
pub(super) fn resolve_remote_image() -> Result<Bundle> {
    Err(Error::msg(
        "macOS restore image extraction is not ported yet",
    ))
}
