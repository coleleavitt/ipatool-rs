//! `ipatool decrypt` (behind the non-default `decrypt` feature): download or read a local
//! IPA, then decrypt it offline with the account's 16-byte `FairPlay` credentials.

use std::path::{Path, PathBuf};

use clap::Args;
use ipatool_appstore::StoreSession;
use ipatool_decrypt::{Credentials, Options, Summary};
use serde_json::json;

use crate::BoxError;
use crate::commerce::{self, DownloadArgs};
use crate::output::Output;

const DEFAULT_CREDENTIAL_FILE_NAME: &str = "credential.bin";
/// The Arcade credential's default file name, next to the standard credential (ipadecrypt
/// stores `<dsid>.arcade.bin` next to `<dsid>.bin`).
const DEFAULT_ARCADE_CREDENTIAL_FILE_NAME: &str = "credential.arcade.bin";

#[derive(Args, Clone)]
pub struct DecryptArgs {
    /// Bundle identifier, numeric App Store ID, App Store URL, or path to a local .ipa
    /// file (as ipadecrypt takes it).
    #[arg(value_name = "BUNDLE_ID|APP_ID|URL|PATH")]
    target: Option<String>,
    /// The bundle identifier of the target app.
    #[arg(short, long, default_value = "")]
    bundle_identifier: String,
    /// Numeric App Store ID of the target app.
    #[arg(short = 'i', long, default_value_t = 0, allow_hyphen_values = true, value_parser = crate::cobra::parse_int)]
    app_id: i64,
    /// Destination path of the decrypted IPA (must not exist), or a directory for it.
    /// Defaults to the input name with a `.decrypted.ipa` suffix.
    #[arg(short, long, default_value = "")]
    output: String,
    /// Path to the 16-byte credential. Defaults to `credential.bin` in the ipatool state
    /// directory, then `./credential.bin`.
    #[arg(long)]
    credential: Option<PathBuf>,
    /// Path to the account's 16-byte Apple Arcade credential, needed only for Arcade apps.
    /// Defaults to `credential.arcade.bin` next to the credential, when that file exists.
    #[arg(long)]
    arcade_credential: Option<PathBuf>,
    /// Skip app extensions (*.appex); they stay encrypted in the output IPA.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = crate::cobra::parse_bool)]
    skip_appex: bool,
    /// Keep and decrypt the Watch app instead of removing it from the output IPA.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = crate::cobra::parse_bool)]
    keep_watch: bool,
    /// Obtain a license for the app if needed (only when downloading).
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", default_value = "false", value_parser = crate::cobra::parse_bool)]
    purchase: bool,
    /// External version identifier of the target app (only when downloading; defaults to the
    /// latest version).
    #[arg(long, default_value = "")]
    external_version_id: String,
}

/// What the IPA to decrypt is: a file already on disk, or one to download first.
#[derive(Debug, PartialEq, Eq)]
enum Input {
    Local(PathBuf),
    Download { app_id: i64, bundle_id: String },
}

/// What the positional argument names (ipadecrypt's `parseDecryptArg`).
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Local(PathBuf),
    AppId(i64),
    BundleId(String),
}

/// The digits after the first `/id` in an App Store URL's path, as ipadecrypt's
/// `/id(\d+)` match.
fn app_id_from_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest
        .find(['/', '?', '#'])
        .filter(|&at| rest[at..].starts_with('/'))
        .map_or("", |slash| &rest[slash..]);
    let path = path.split(['?', '#']).next().unwrap_or_default();
    path.match_indices("/id").find_map(|(at, _)| {
        let digits = &path[at + 3..];
        let end = digits
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(digits.len());
        (end > 0).then(|| &digits[..end])
    })
}

fn parse_app_id(digits: &str) -> Result<i64, BoxError> {
    digits
        .parse()
        .map_err(|error| format!("invalid app ID {digits}: {error}").into())
}

/// ipadecrypt's `parseDecryptArg`: an App Store URL, a local `.ipa` path, a numeric App
/// Store ID, or else a bundle identifier. Any existing file is also taken as a local IPA.
fn parse_target(raw: &str) -> Result<Target, BoxError> {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let digits = app_id_from_url(raw).ok_or_else(|| format!("no /id<digits> in url {raw}"))?;
        return Ok(Target::AppId(parse_app_id(digits)?));
    }
    let path = Path::new(raw);
    if raw.to_lowercase().ends_with(".ipa") || path.is_file() {
        let metadata =
            std::fs::metadata(path).map_err(|error| format!("local IPA {raw}: {error}"))?;
        if metadata.is_dir() {
            return Err(format!("local IPA {raw} is a directory").into());
        }
        return Ok(Target::Local(path.to_path_buf()));
    }
    if !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(Target::AppId(parse_app_id(raw)?));
    }
    Ok(Target::BundleId(raw.to_owned()))
}

/// Combines the positional argument with `-b` and `-i`; a value given both ways must agree.
fn classify(args: &DecryptArgs) -> Result<Input, BoxError> {
    let mut app_id = args.app_id;
    let mut bundle_id = args.bundle_identifier.clone();
    match args.target.as_deref().filter(|value| !value.is_empty()) {
        None => {}
        Some(raw) => match parse_target(raw)? {
            Target::Local(path) => {
                if app_id != 0 || !bundle_id.is_empty() {
                    return Err(
                        "a local .ipa cannot be combined with --bundle-identifier or --app-id"
                            .into(),
                    );
                }
                return Ok(Input::Local(path));
            }
            Target::AppId(id) => {
                if app_id != 0 && app_id != id {
                    return Err(
                        format!("the app ID argument {id} and --app-id {app_id} disagree").into(),
                    );
                }
                app_id = id;
            }
            Target::BundleId(id) => {
                if !bundle_id.is_empty() && bundle_id != id {
                    return Err(format!(
                        "the bundle identifier argument {id} and --bundle-identifier {bundle_id} disagree"
                    )
                    .into());
                }
                bundle_id = id;
            }
        },
    }
    if app_id == 0 && bundle_id.is_empty() {
        return Err(
            "either a bundle identifier, an app ID, or a path to a local .ipa must be specified"
                .into(),
        );
    }
    Ok(Input::Download { app_id, bundle_id })
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

async fn read_credential(path: &Path, label: &str) -> Result<Vec<u8>, BoxError> {
    tokio::fs::read(path)
        .await
        .map_err(|error| format!("failed to read {label} {}: {error}", path.display()).into())
}

/// The standard credential and, for Arcade apps, the Arcade one: `--arcade-credential`,
/// else `credential.arcade.bin` next to the standard credential when it exists.
async fn load_credentials(args: &DecryptArgs) -> Result<Credentials, BoxError> {
    let standard_path = credential_path(args.credential.as_deref())?;
    let standard = read_credential(&standard_path, "credential").await?;
    let arcade_path = match &args.arcade_credential {
        Some(path) => Some(path.clone()),
        None => Some(
            standard_path
                .parent()
                .unwrap_or(Path::new(""))
                .join(DEFAULT_ARCADE_CREDENTIAL_FILE_NAME),
        )
        .filter(|path| path.is_file()),
    };
    let arcade = match arcade_path {
        Some(path) => Some(read_credential(&path, "Arcade credential").await?),
        None => None,
    };
    Ok(Credentials { standard, arcade })
}

/// The default output file name for an input: `<stem>.decrypted.ipa`.
fn default_output_name(input: &Path) -> String {
    let name = input
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app.ipa");
    let stem = name.strip_suffix(".ipa").unwrap_or(name);
    format!("{stem}.decrypted.ipa")
}

/// `-o` as given, or the default name inside `-o` when it is a directory, else the
/// default name in `default_dir` (ipadecrypt's `localOutputPath`).
fn output_path(output: &str, input: &Path, default_dir: &Path) -> PathBuf {
    if output.is_empty() {
        return default_dir.join(default_output_name(input));
    }
    let output = PathBuf::from(output);
    if output.is_dir() {
        return output.join(default_output_name(input));
    }
    output
}

/// Decrypts `input` into `output`. The one place the decryption runs, so another path
/// (an on-device fallback) can hook in here.
fn decrypt(
    input: &Path,
    output: &Path,
    credentials: &Credentials,
    options: Options,
) -> ipatool_decrypt::Result<Summary> {
    ipatool_decrypt::decrypt_ipa(input, output, credentials, options)
}

pub(crate) async fn run(
    args: &DecryptArgs,
    session: &StoreSession<'_>,
    interactive: bool,
    out: &Output,
) -> Result<(), BoxError> {
    let input = classify(args)?;
    let credentials = load_credentials(args).await?;
    let options = Options {
        skip_appex: args.skip_appex,
        keep_watch: args.keep_watch,
    };
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
    let output_path = output_path(&args.output, &input_path, &default_dir);

    // Decryption is CPU- and IO-bound; keep it off the async runtime thread.
    let summary = tokio::task::spawn_blocking(move || {
        decrypt(&input_path, &output_path, &credentials, options)
    })
    .await
    .map_err(|error| format!("decryption task failed: {error}"))??;
    out.info(json!({
        "output": summary.output.display().to_string(),
        "decrypted": summary.binaries,
        "pages": summary.pages,
        "verified": summary.verified,
        "success": true,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(target: Option<&str>, bundle: &str, app_id: i64) -> DecryptArgs {
        DecryptArgs {
            target: target.map(str::to_owned),
            bundle_identifier: bundle.to_owned(),
            app_id,
            output: String::new(),
            credential: None,
            arcade_credential: None,
            skip_appex: false,
            keep_watch: false,
            purchase: false,
            external_version_id: String::new(),
        }
    }

    fn download(app_id: i64, bundle_id: &str) -> Input {
        Input::Download {
            app_id,
            bundle_id: bundle_id.to_owned(),
        }
    }

    #[test]
    fn app_store_urls() {
        assert_eq!(
            app_id_from_url("https://apps.apple.com/us/app/google-home/id680819774?l=en"),
            Some("680819774")
        );
        assert_eq!(
            app_id_from_url("https://apps.apple.com/app/idx/id42"),
            Some("42")
        );
        assert_eq!(app_id_from_url("https://apps.apple.com/us/app/x"), None);
        assert_eq!(app_id_from_url("https://example.com?q=/id5"), None);
        assert_eq!(
            parse_target("https://apps.apple.com/us/app/x/id680819774").expect("url"),
            Target::AppId(680_819_774)
        );
        assert_eq!(
            parse_target("https://apps.apple.com/us/app/x")
                .unwrap_err()
                .to_string(),
            "no /id<digits> in url https://apps.apple.com/us/app/x"
        );
    }

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("680819774").expect("id"),
            Target::AppId(680_819_774)
        );
        assert_eq!(
            parse_target("com.google.Chromecast").expect("bundle"),
            Target::BundleId("com.google.Chromecast".into())
        );
        let err = parse_target("missing-file.IPA").unwrap_err().to_string();
        assert!(err.starts_with("local IPA missing-file.IPA: "), "{err}");
        let dir = tempfile::tempdir().expect("temp dir");
        let ipa_dir = dir.path().join("x.ipa");
        std::fs::create_dir(&ipa_dir).expect("mkdir");
        assert_eq!(
            parse_target(ipa_dir.to_str().expect("utf-8"))
                .unwrap_err()
                .to_string(),
            format!("local IPA {} is a directory", ipa_dir.display())
        );
        let file = dir.path().join("app.zip");
        std::fs::write(&file, b"").expect("write");
        assert_eq!(
            parse_target(file.to_str().expect("utf-8")).expect("file"),
            Target::Local(file.clone())
        );
    }

    #[test]
    fn positional_and_flags_must_agree() {
        assert_eq!(
            classify(&args(Some("a.b"), "", 0)).expect("bundle"),
            download(0, "a.b")
        );
        assert_eq!(
            classify(&args(None, "a.b", 0)).expect("flag"),
            download(0, "a.b")
        );
        assert_eq!(
            classify(&args(Some("a.b"), "a.b", 0)).expect("same"),
            download(0, "a.b")
        );
        assert_eq!(
            classify(&args(Some("a.b"), "c.d", 0))
                .unwrap_err()
                .to_string(),
            "the bundle identifier argument a.b and --bundle-identifier c.d disagree"
        );
        assert_eq!(
            classify(&args(Some("42"), "", 7)).unwrap_err().to_string(),
            "the app ID argument 42 and --app-id 7 disagree"
        );
        assert_eq!(
            classify(&args(Some("42"), "a.b", 0)).expect("both"),
            download(42, "a.b")
        );
        assert_eq!(
            classify(&args(Some("a.b"), "", 42)).expect("both"),
            download(42, "a.b")
        );
        assert_eq!(
            classify(&args(None, "", 0)).unwrap_err().to_string(),
            "either a bundle identifier, an app ID, or a path to a local .ipa must be specified"
        );
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("app.ipa");
        std::fs::write(&file, b"").expect("write");
        let local = file.to_str().expect("utf-8");
        assert_eq!(
            classify(&args(Some(local), "", 0)).expect("local"),
            Input::Local(file.clone())
        );
        assert_eq!(
            classify(&args(Some(local), "a.b", 0))
                .unwrap_err()
                .to_string(),
            "a local .ipa cannot be combined with --bundle-identifier or --app-id"
        );
    }

    #[test]
    fn output_paths() {
        let dir = tempfile::tempdir().expect("temp dir");
        let input = Path::new("/in/App_1.0.ipa");
        assert_eq!(
            output_path("", input, Path::new("/cwd")),
            Path::new("/cwd/App_1.0.decrypted.ipa")
        );
        assert_eq!(
            output_path(
                dir.path().to_str().expect("utf-8"),
                input,
                Path::new("/cwd")
            ),
            dir.path().join("App_1.0.decrypted.ipa")
        );
        assert_eq!(
            output_path("/out/x.ipa", input, Path::new("/cwd")),
            Path::new("/out/x.ipa")
        );
    }

    #[tokio::test]
    async fn arcade_credential_defaults_next_to_the_credential() {
        let dir = tempfile::tempdir().expect("temp dir");
        let standard = dir.path().join("credential.bin");
        tokio::fs::write(&standard, [1; 16]).await.expect("write");
        let mut decrypt_args = args(Some("a.b"), "", 0);
        decrypt_args.credential = Some(standard);
        let credentials = load_credentials(&decrypt_args).await.expect("load");
        assert_eq!(credentials.arcade, None);
        tokio::fs::write(dir.path().join("credential.arcade.bin"), [2; 16])
            .await
            .expect("write");
        let credentials = load_credentials(&decrypt_args).await.expect("load");
        assert_eq!(credentials.standard, [1; 16]);
        assert_eq!(credentials.arcade, Some(vec![2; 16]));
        decrypt_args.arcade_credential = Some(dir.path().join("missing.bin"));
        let err = load_credentials(&decrypt_args)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("failed to read Arcade credential "),
            "{err}"
        );
    }
}
