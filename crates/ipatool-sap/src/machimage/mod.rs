//! Mach-O image mapping for the SAP guest. Port of Go `internal/sap/machimage`.

mod fat;

pub use fat::{arm64_range, arm64_slice, read_arm64};
