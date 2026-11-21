//! Go's download types (`appstore_download.go`, `appstore_replicate_sinf.go`) and the
//! `fmt.Sprint` view of plist values that Go's checks use.

use std::path::PathBuf;
use std::sync::Arc;

use plist::{Dictionary, Value};

/// Go's `Sinf`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sinf {
    pub id: i64,
    pub data: Vec<u8>,
    pub dp_info: Vec<u8>,
}

/// Go's `downloadItemResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DownloadItem {
    pub preflight_package_url: String,
    pub artwork_url: String,
    pub hash_md5: String,
    pub url: String,
    pub sinfs: Vec<Sinf>,
    pub metadata: Dictionary,
}

/// Go's `downloadResult`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DownloadResult {
    pub failure_type: String,
    pub customer_message: String,
    pub dialog: Dictionary,
    pub metrics: Dictionary,
    pub items: Vec<DownloadItem>,
}

/// Go's `http.Result[downloadResult]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DownloadProduct {
    pub status: u16,
    pub data: DownloadResult,
}

/// Go's `DownloadOutput`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadOutput {
    pub destination_path: PathBuf,
    pub sinfs: Vec<Sinf>,
}

/// A stage of a download, reported through [`Progress::phase`] (the fork's progress
/// modes, majd/ipatool#649). It may repeat the current phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Resolving,
    Purchasing,
    Downloading,
    Decrypting,
    Patching,
}

impl Phase {
    /// The name the progress output prints.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::Purchasing => "purchasing",
            Self::Downloading => "downloading",
            Self::Decrypting => "decrypting",
            Self::Patching => "patching",
        }
    }
}

/// Go's `progressbar.ProgressBar` as the downloader drives it, plus the phases of the
/// fork's `appstore.Progress`.
pub trait Progress: Send + Sync {
    fn phase(&self, phase: Phase);
    /// `ChangeMax64`.
    fn set_total(&self, total: i64);
    /// `Set64`.
    fn set(&self, position: i64);
    /// `Write`/`Add64` of downloaded bytes.
    fn add(&self, bytes: u64);
}

/// A shared progress bar; the parallel ranges write to it from their own threads.
pub type ProgressHandle = Arc<dyn Progress>;

/// The fork's `reportPhase`: a missing progress disables reporting.
pub(crate) fn report_phase(progress: Option<&ProgressHandle>, phase: Phase) {
    if let Some(progress) = progress {
        progress.phase(phase);
    }
}

/// Go's `DownloadInput`.
#[derive(Clone, Default)]
pub struct DownloadInput {
    pub account: crate::Account,
    pub app: crate::App,
    pub output_path: String,
    pub progress: Option<ProgressHandle>,
    pub external_version_id: String,
    pub platform: Option<crate::Platform>,
    /// Keeps a large package on one connection, so all bytes land in the destination's
    /// `.tmp` file as they arrive (the fork's `--no-parallel`, majd/ipatool#669).
    pub disable_parallel_download: bool,
}

impl std::fmt::Debug for DownloadInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadInput")
            .field("app", &self.app)
            .field("output_path", &self.output_path)
            .field("external_version_id", &self.external_version_id)
            .field("platform", &self.platform)
            .field("disable_parallel_download", &self.disable_parallel_download)
            .finish_non_exhaustive()
    }
}

/// A string field, as Go's plist decoder fills a `string` (other types stay empty).
pub(crate) fn string(dict: &Dictionary, key: &str) -> String {
    match dict.get(key) {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn data(dict: &Dictionary, key: &str) -> Vec<u8> {
    match dict.get(key) {
        Some(Value::Data(bytes)) => bytes.clone(),
        _ => Vec::new(),
    }
}

fn dictionary(dict: &Dictionary, key: &str) -> Dictionary {
    match dict.get(key) {
        Some(Value::Dictionary(value)) => value.clone(),
        _ => Dictionary::new(),
    }
}

pub(crate) fn integer(dict: &Dictionary, key: &str) -> i64 {
    match dict.get(key) {
        Some(Value::Integer(number)) => number
            .as_signed()
            .or_else(|| number.as_unsigned().and_then(|n| i64::try_from(n).ok()))
            .unwrap_or_default(),
        _ => 0,
    }
}

impl Sinf {
    fn from_plist(dict: &Dictionary) -> Self {
        Self {
            id: integer(dict, "id"),
            data: data(dict, "sinf"),
            dp_info: data(dict, "dpInfo"),
        }
    }
}

impl DownloadItem {
    fn from_plist(dict: &Dictionary) -> Self {
        let sinfs = match dict.get("sinfs") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_dictionary)
                .map(Sinf::from_plist)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            preflight_package_url: string(dict, "preflightPackageURL"),
            artwork_url: string(dict, "artworkURL"),
            hash_md5: string(dict, "md5"),
            url: string(dict, "URL"),
            sinfs,
            metadata: dictionary(dict, "metadata"),
        }
    }
}

impl DownloadResult {
    pub(crate) fn from_plist(dict: &Dictionary) -> Self {
        let items = match dict.get("songList") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_dictionary)
                .map(DownloadItem::from_plist)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            failure_type: string(dict, "failureType"),
            customer_message: string(dict, "customerMessage"),
            dialog: dictionary(dict, "dialog"),
            metrics: dictionary(dict, "metrics"),
            items,
        }
    }
}

/// Go's `fmt.Sprint` of a decoded plist value (`nil` for a missing key).
pub(crate) fn sprint(value: Option<&Value>) -> String {
    match value {
        None => "<nil>".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Real(number)) => ipatool_store::emulator::go_float(*number),
        Some(Value::Boolean(flag)) => flag.to_string(),
        Some(Value::Data(bytes)) => format!(
            "[{}]",
            bytes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(Value::Array(items)) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| sprint(Some(item)))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Some(Value::Dictionary(map)) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            format!(
                "map[{}]",
                entries
                    .iter()
                    .map(|(key, item)| format!("{key}:{}", sprint(Some(item))))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Some(other) => format!("{other:?}"),
    }
}
