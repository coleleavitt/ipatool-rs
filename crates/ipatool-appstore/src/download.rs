//! Go's `Download` (`appstore_download.go`) and `appstore_download_artwork.go`: the iOS
//! package flow from the download response to the patched `.ipa`.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use plist::{Dictionary, Value};

use crate::download_package::download_package_platform;
use crate::download_product::{failure, store_error};
use crate::download_types::{DownloadInput, DownloadItem, DownloadOutput, sprint};
use crate::session::StoreSession;
use crate::version_lookup::{PLATFORM_APP_NOT_FOUND, PLATFORM_OFFERS_NOT_FOUND};
use crate::zipfile::{self, Archive, Writer};
use crate::{Account, App, Client, Error, HttpRequest, Platform, Result, machine};
use ipatool_store::http::Header;

/// Go's `packageFileName`.
pub(crate) fn package_file_name(app: &App, version: &str, platform: Option<Platform>) -> String {
    let mut parts = Vec::new();
    if !app.bundle_id.is_empty() {
        parts.push(app.bundle_id.clone());
    }
    if app.id != 0 {
        parts.push(app.id.to_string());
    }
    if !version.is_empty() {
        parts.push(version.to_owned());
    }
    let extension = if platform == Some(Platform::MacOs) {
        "pkg"
    } else {
        "ipa"
    };
    format!("{}.{extension}", parts.join("_"))
}

/// Go's `resolveDestinationPath`.
pub(crate) fn resolve_destination_path(
    app: &App,
    version: &str,
    path: &str,
    platform: Option<Platform>,
) -> Result<PathBuf> {
    let file = package_file_name(app, version, platform);
    if path.is_empty() {
        let workdir = std::env::current_dir()
            .map_err(|error| Error::io("failed to get current directory", error))?;
        return Ok(PathBuf::from(format!("{}/{file}", workdir.display())));
    }
    let is_dir = match fs::metadata(path) {
        Ok(info) => info.is_dir(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(Error::io("failed to read file metadata", error)
                .context("failed to determine whether path is a directory"));
        }
    };
    Ok(if is_dir {
        Path::new(path).join(file)
    } else {
        PathBuf::from(path)
    })
}

/// Go's `isTopLevelAppInfoPlist`.
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "Go compares case-sensitively"
)]
fn is_top_level_app_info_plist(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() == 3
        && parts[0] == "Payload"
        && parts[1].ends_with(".app")
        && parts[2] == "Info.plist"
}

/// Go's `validatePackagePlatform`.
pub(crate) fn validate_package_platform(path: &Path, platform: Option<Platform>) -> Result<()> {
    let expected = match platform {
        Some(Platform::IPhone | Platform::IPad) => "iPhoneOS",
        Some(Platform::AppleTv) => "AppleTVOS",
        Some(Platform::WatchOs) => "WatchOS",
        Some(Platform::VisionOs) => "XROS",
        _ => return Ok(()),
    };
    let archive =
        Archive::open(path).map_err(|error| Error::io("failed to open zip reader", error))?;
    for file in &archive.files {
        if !is_top_level_app_info_plist(&file.name) {
            continue;
        }
        let data = archive
            .read(file)
            .map_err(|error| Error::io("failed to read info plist", error))?;
        let info: Value = plist::from_bytes(&data)
            .map_err(|error| Error::Invalid(format!("failed to decode info plist: {error}")))?;
        let platforms = info
            .as_dictionary()
            .and_then(|dict| dict.get("CFBundleSupportedPlatforms"))
            .and_then(Value::as_array);
        // Legacy iOS apps may predate CFBundleSupportedPlatforms; only a missing
        // declaration qualifies.
        let Some(platforms) = platforms else {
            if expected == "iPhoneOS" {
                return Ok(());
            }
            continue;
        };
        if platforms
            .iter()
            .any(|value| value.as_string() == Some(expected))
        {
            return Ok(());
        }
    }
    Err(Error::Invalid(format!(
        "downloaded package does not declare {expected} support"
    )))
}

/// Go's `writeMetadata`: the item metadata with the account email, as a binary plist with
/// sorted keys (howett's encoder sorts map keys).
fn metadata_plist(metadata: &Dictionary, account: &Account) -> Result<Vec<u8>> {
    let mut metadata = metadata.clone();
    metadata.insert("apple-id".to_owned(), Value::String(account.email.clone()));
    metadata.insert("userName".to_owned(), Value::String(account.email.clone()));
    let mut data = Vec::new();
    plist::to_writer_binary(&mut data, &sorted(Value::Dictionary(metadata)))
        .map_err(|error| Error::Invalid(format!("failed to marshal data: {error}")))?;
    Ok(data)
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Dictionary(dict) => {
            let mut entries: Vec<(String, Value)> = dict.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Dictionary(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

fn io_context(context: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |error| Error::io(context, error)
}

/// Go's `applyPatches`: the package copied entry by entry, then `iTunesArtwork` and
/// `iTunesMetadata.plist`.
pub(crate) fn apply_patches(
    item: &DownloadItem,
    account: &Account,
    src: &Path,
    dst: &Path,
    artwork: &[u8],
) -> Result<()> {
    let archive = Archive::open(src).map_err(io_context("failed to open zip reader"))?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)
        .map_err(io_context("failed to open file"))?;
    let mut writer = Writer::new(file);
    zipfile::replicate(&archive, &mut writer).map_err(io_context("failed to replicate zip"))?;
    if !artwork.is_empty() {
        writer
            .create("iTunesArtwork")
            .map_err(io_context("failed to create artwork"))?;
        writer
            .write_all(artwork)
            .map_err(io_context("failed to write artwork"))?;
    }
    let metadata = metadata_plist(&item.metadata, account)
        .map_err(|error| error.context("failed to write metadata"))?;
    writer
        .create("iTunesMetadata.plist")
        .and_then(|()| writer.write_all(&metadata))
        .map_err(|error| {
            Error::io("failed to create file", error).context("failed to write metadata")
        })?;
    let mut file = writer
        .finish()
        .map_err(io_context("failed to close zip writer"))?;
    file.flush()
        .map_err(io_context("failed to close patched file"))
}

impl Client {
    /// Go's `downloadArtwork`.
    ///
    /// # Errors
    /// Go's request and response errors.
    pub fn download_artwork(&self, url: &str) -> Result<Vec<u8>> {
        if url.is_empty() {
            return Ok(Vec::new());
        }
        let mut response = self
            .store_http(
                "transfer",
                HttpRequest {
                    method: "GET".to_owned(),
                    url: url.to_owned(),
                    headers: Header::new(),
                    body: Vec::new(),
                },
            )
            .map_err(|error| error.context("request failed"))?;
        if response.status != 200 {
            return Err(Error::Invalid(format!(
                "unexpected artwork response status: {}",
                response.status
            )));
        }
        let mut data = Vec::new();
        response
            .body
            .read_to_end(&mut data)
            .map_err(io_context("failed to read artwork"))?;
        if data.is_empty() {
            return Err(Error::Invalid("artwork response is empty".to_owned()));
        }
        Ok(data)
    }
}

impl StoreSession<'_> {
    /// Go's `Download`: requests the package, downloads it next to the destination
    /// (`<destination>.tmp`, resumable), checks its platform and writes the patched
    /// package with artwork and `iTunesMetadata.plist`. The sinfs are returned for
    /// [`Client::replicate_sinf`].
    ///
    /// # Errors
    /// [`Error::PasswordTokenExpired`], [`Error::LicenseRequired`] and Go's other errors.
    pub async fn download(&self, input: DownloadInput) -> Result<DownloadOutput> {
        let mac_address =
            machine::mac_address().map_err(|error| error.context("failed to get mac address"))?;
        let mut guid = mac_address.to_uppercase().replace(':', "");
        let mut machine_guid = Vec::new();
        if input.platform == Some(Platform::MacOs) {
            (guid, machine_guid) = machine::identity(&mac_address)
                .map_err(|error| error.context("failed to resolve machine identity"))?;
        }
        let mut external_version_id = input.external_version_id.clone();
        if external_version_id.is_empty()
            && let Some(platform @ (Platform::AppleTv | Platform::WatchOs | Platform::VisionOs)) =
                input.platform
        {
            match self
                .client
                .lookup_latest_external_version_id(&input.account, &input.app, platform)
                .await
            {
                Ok(version) => external_version_id = version,
                // Delisted tvOS apps may have no catalog offer but still be downloadable.
                Err(error)
                    if platform == Platform::AppleTv
                        && (error.has_message(PLATFORM_APP_NOT_FOUND)
                            || error.has_message(PLATFORM_OFFERS_NOT_FOUND)) => {}
                Err(error) => return Err(error.context("failed to resolve platform version")),
            }
        }

        let (response, resolved_platform) = self
            .download_product(
                &input.account,
                &input.app,
                &guid,
                &external_version_id,
                input.platform,
            )
            .await?;
        let data = &response.data;
        if data.failure_type == failure::PASSWORD_TOKEN_EXPIRED
            || data.failure_type == failure::SIGN_IN_REQUIRED
            || data.failure_type == failure::DEVICE_VERIFICATION_FAILED
            || data.failure_type == failure::LICENSE_ALREADY_EXISTS
        {
            return Err(Error::PasswordTokenExpired);
        }
        if data.failure_type == failure::LICENSE_NOT_FOUND {
            return Err(Error::LicenseRequired);
        }
        if !data.customer_message.is_empty()
            && (!data.failure_type.is_empty() || data.items.is_empty())
        {
            return Err(store_error(
                format!("received error: {}", data.customer_message),
                &response,
            ));
        }
        if !data.failure_type.is_empty() {
            return Err(store_error(
                format!("received error: {}", data.failure_type),
                &response,
            ));
        }
        let Some(item) = data.items.first() else {
            return Err(store_error("invalid response", &response));
        };
        let version = item
            .metadata
            .get("bundleShortVersionString")
            .map_or_else(|| "unknown".to_owned(), |value| sprint(Some(value)));
        let package_platform = download_package_platform(input.platform, item)?;
        let destination =
            resolve_destination_path(&input.app, &version, &input.output_path, package_platform)
                .map_err(|error| error.context("failed to resolve destination path"))?;
        if package_platform == Some(Platform::MacOs) {
            return self
                .client
                .download_mac_package(item, &destination, &machine_guid, input.progress)
                .await;
        }

        let mut tmp = destination.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        self.client
            .download_file(&item.url, &tmp, input.progress.clone())
            .await
            .map_err(|error| error.context("failed to download file"))?;
        let client = self.client.clone();
        let (item, account) = (item.clone(), input.account.clone());
        let output_destination = destination.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = validate_package_platform(&tmp, resolved_platform) {
                let error = match fs::remove_file(&tmp) {
                    Ok(()) => error,
                    Err(remove) => Error::Joined(vec![
                        error,
                        Error::io("failed to remove invalid package", remove),
                    ]),
                };
                return Err(error.context("failed to validate package platform"));
            }
            let artwork = client
                .download_artwork(&item.artwork_url)
                .map_err(|error| error.context("failed to download artwork"))?;
            apply_patches(&item, &account, &tmp, &destination, &artwork)
                .map_err(|error| error.context("failed to apply patches"))?;
            fs::remove_file(&tmp).map_err(io_context("failed to remove file"))?;
            Ok(DownloadOutput {
                destination_path: output_destination,
                sinfs: item.sinfs,
            })
        })
        .await
        .map_err(|error| Error::Invalid(format!("download task failed: {error}")))?
    }
}
