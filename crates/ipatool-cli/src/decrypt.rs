//! `ipatool decrypt` (behind the non-default `decrypt` feature): download or read a local
//! IPA, then decrypt it offline with a 16-byte credential.

use std::path::{Path, PathBuf};

use clap::Args;
use ipatool_appstore::StoreSession;
use serde_json::json;

use crate::BoxError;
use crate::commerce::{self, DownloadArgs};
use crate::output::Output;

const DEFAULT_CREDENTIAL_FILE_NAME: &str = "credential.bin";

#[derive(Args, Clone)]
pub struct DecryptArgs {
    /// Bundle identifier of the target app, or a path to a local .ipa file.
    #[arg(value_name = "BUNDLE_ID|PATH")]
    target: Option<String>,
    /// Numeric App Store ID of the target app (overrides the bundle identifier).
    #[arg(short = 'i', long, allow_hyphen_values = true, value_parser = crate::cobra::parse_int)]
    app_id: Option<i64>,
    /// Destination path of the decrypted IPA (must not exist). Defaults to the input name
    /// with a `.decrypted.ipa` suffix.
    #[arg(short, long, default_value = "")]
    output: String,
    /// Path to the 16-byte credential. Defaults to `credential.bin` in the ipatool state
    /// directory, then `./credential.bin`.
    #[arg(long)]
    credential: Option<PathBuf>,
    /// Obtain a license for the app if needed (only when downloading).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = crate::cobra::parse_bool)]
    purchase: bool,
    /// External version identifier of the target app (only when downloading; defaults to the
    /// latest version).
    #[arg(long, default_value = "")]
    external_version_id: String,
}

/// What the IPA to decrypt is: a file already on disk, or one to download first.
enum Input {
    Local(PathBuf),
    Download { app_id: i64, bundle_id: String },
}

fn classify(args: &DecryptArgs) -> Result<Input, BoxError> {
    if let Some(app_id) = args.app_id
        && app_id != 0
    {
        return Ok(Input::Download {
            app_id,
            bundle_id: String::new(),
        });
    }
    let Some(target) = args.target.as_deref().filter(|value| !value.is_empty()) else {
        return Err(
            "either a bundle identifier, an app ID, or a path to a local .ipa must be specified"
                .into(),
        );
    };
    if Path::new(target).is_file() {
        return Ok(Input::Local(PathBuf::from(target)));
    }
    Ok(Input::Download {
        app_id: 0,
        bundle_id: target.to_owned(),
    })
}

/// The credential path: `--credential`, else `credential.bin` in the state directory, else
/// `./credential.bin`.
fn credential_path(explicit: Option<&Path>) -> Result<PathBuf, BoxError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let in_state = crate::state_dir()?.join(DEFAULT_CREDENTIAL_FILE_NAME);
    if in_state.is_file() {
        return Ok(in_state);
    }
    Ok(PathBuf::from(DEFAULT_CREDENTIAL_FILE_NAME))
}

/// The default output path for an input: `<stem>.decrypted.ipa` in `dir`.
fn default_output(input: &Path, dir: &Path) -> PathBuf {
    let name = input
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app.ipa");
    let stem = name.strip_suffix(".ipa").unwrap_or(name);
    dir.join(format!("{stem}.decrypted.ipa"))
}

pub(crate) async fn run(
    args: &DecryptArgs,
    session: &StoreSession<'_>,
    interactive: bool,
    out: &Output,
) -> Result<(), BoxError> {
    let credential_path = credential_path(args.credential.as_deref())?;
    let credential = tokio::fs::read(&credential_path).await.map_err(|error| {
        format!(
            "failed to read credential {}: {error}",
            credential_path.display()
        )
    })?;

    let input = classify(args)?;
    // Keep the temporary download directory alive until decryption finishes.
    let download_dir;
    let (input_path, default_dir) = match input {
        Input::Local(path) => {
            let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            (path, dir)
        }
        Input::Download { app_id, bundle_id } => {
            download_dir = tempfile::Builder::new()
                .prefix("ipatool-decrypt-")
                .tempdir()
                .map_err(|error| format!("failed to create temporary directory: {error}"))?;
            let download_args = DownloadArgs::for_decrypt(
                app_id,
                bundle_id,
                download_dir.path().to_string_lossy().into_owned(),
                args.external_version_id.clone(),
                args.purchase,
            );
            let (output, _purchased) =
                commerce::download_run(&download_args, session, interactive, out).await?;
            (
                output.destination_path,
                std::env::current_dir()
                    .map_err(|error| format!("failed to get the current directory: {error}"))?,
            )
        }
    };

    let output_path = if args.output.is_empty() {
        default_output(&input_path, &default_dir)
    } else {
        PathBuf::from(&args.output)
    };

    // Decryption is CPU- and IO-bound; keep it off the async runtime thread.
    let summary = tokio::task::spawn_blocking(move || {
        ipatool_decrypt::decrypt_ipa(&input_path, &output_path, &credential)
    })
    .await
    .map_err(|error| format!("decryption task failed: {error}"))??;
    out.info(json!({
        "output": summary.output.display().to_string(),
        "decrypted": summary.decrypted_binaries,
        "success": true,
    }));
    Ok(())
}
