//! `purchase` and `download` (Go `cmd/purchase.go`, `cmd/download.go`).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::Args;
use ipatool_appstore::storeauth::EmulatorStoreAmd;
use ipatool_appstore::{
    Account, App, Client, DownloadInput, DownloadOutput, LoginInput, Platform, Progress,
    ProgressHandle, StoreSession,
};
use serde_json::json;

use crate::BoxError;
use crate::output::Output;

#[derive(Args, Clone)]
pub struct PurchaseArgs {
    /// ID of the target app.
    #[arg(short = 'i', long, default_value_t = 0)]
    app_id: i64,
    /// The bundle identifier of the target app (overrides the app ID).
    #[arg(short, long, default_value = "")]
    bundle_identifier: String,
    /// Platform to purchase for: iphone (iOS), ipad (iPadOS), appletv (tvOS), watchos,
    /// visionos, or macos.
    #[arg(long)]
    platform: Option<Platform>,
}

#[derive(Args, Clone)]
pub struct DownloadArgs {
    /// ID of the target app (required).
    #[arg(short = 'i', long, default_value_t = 0)]
    app_id: i64,
    /// The bundle identifier of the target app (overrides the app ID when found).
    #[arg(short, long, default_value = "")]
    bundle_identifier: String,
    /// The destination path of the downloaded app package.
    #[arg(short, long, default_value = "")]
    output: String,
    /// External version identifier of the target app (defaults to latest version when not
    /// specified).
    #[arg(long, default_value = "")]
    external_version_id: String,
    /// Platform to download for: iphone (iOS), ipad (iPadOS), appletv (tvOS), watchos,
    /// visionos, or macos.
    #[arg(long)]
    platform: Option<Platform>,
    /// Obtain a license for the app if needed.
    #[arg(long)]
    purchase: bool,
}

const MISSING_APP: &str = "either the app ID or the bundle identifier must be specified";

/// Go's `Login` with the saved credentials, after an expired password token.
async fn relogin(
    client: &Client,
    session: &StoreSession<'_>,
    account: &Account,
) -> Result<Account, BoxError> {
    let input = LoginInput {
        email: account.email.clone(),
        password: account.password.clone(),
        auth_code: String::new(),
    };
    Ok(client
        .native_login(&input, session.keychain(), &EmulatorStoreAmd)
        .await?
        .account)
}

/// Go's `store.Lookup` with the account's storefront.
async fn lookup(
    client: &Client,
    account: &Account,
    bundle_id: &str,
    platform: Option<Platform>,
) -> ipatool_appstore::Result<App> {
    let country = client
        .country_code_from_store_front(&account.store_front)
        .await
        .map_err(|error| {
            ipatool_appstore::Error::Invalid(format!("failed to resolve the country code: {error}"))
        })?;
    let country = country
        .parse()
        .map_err(|error: ipatool_appstore::Error| error)?;
    client
        .lookup(
            &ipatool_appstore::LookupQuery::BundleId(bundle_id.to_owned()),
            &country,
            platform,
        )
        .await
}

/// Go's `purchaseCmd`: two attempts, logging in again after an expired token.
pub async fn purchase(
    args: &PurchaseArgs,
    session: &StoreSession<'_>,
    out: &Output,
) -> Result<(), BoxError> {
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let client = session.client;
    let mut token_expired = false;
    for attempt in 0..2 {
        let mut account = session.account_info().await?;
        if token_expired {
            account = relogin(client, session, &account).await?;
        }
        let mut app = App {
            id: args.app_id,
            ..App::default()
        };
        if !args.bundle_identifier.is_empty() {
            app = lookup(client, &account, &args.bundle_identifier, args.platform).await?;
        }
        match session.purchase(&mut account, &app, args.platform).await {
            Err(error) if error.is_password_token_expired() && attempt == 0 => {
                token_expired = true;
            }
            Err(error) if !error.is_license_already_exists() => return Err(error.into()),
            result => {
                out.info(json!({ "alreadyOwned": result.is_err(), "success": true }));
                return Ok(());
            }
        }
    }
    unreachable!("the second attempt returns")
}

/// Go's `downloadCmd`: up to three attempts; an expired token logs in again, and with
/// `--purchase` a missing license is obtained first.
pub async fn download(
    args: &DownloadArgs,
    session: &StoreSession<'_>,
    interactive: bool,
    out: &Output,
) -> Result<(), BoxError> {
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let client = session.client;
    let mut account = session.account_info().await?;
    let (mut token_expired, mut purchase_required, mut purchased) = (false, false, false);
    for attempt in 0..3 {
        if token_expired {
            account = relogin(client, session, &account).await?;
        }
        let mut app = App {
            id: args.app_id,
            bundle_id: args.bundle_identifier.clone(),
            ..App::default()
        };
        if !args.bundle_identifier.is_empty() {
            match lookup(client, &account, &args.bundle_identifier, args.platform).await {
                Ok(found) => app = found,
                // Delisted apps may still be downloadable by their numeric ID.
                Err(ipatool_appstore::Error::AppNotFound) if args.app_id != 0 => {}
                Err(error) => return Err(error.into()),
            }
        }
        if purchase_required {
            if let Err(error) = session.purchase(&mut account, &app, args.platform).await
                && !error.is_license_already_exists()
            {
                return Err(error.into());
            }
            // Purchasing may persist a server-corrected storefront.
            account = session.account_info().await?;
            purchased = true;
            out.verbose(json!({ "success": true }), "purchase");
        }
        let progress =
            interactive.then(|| Arc::new(ProgressBar::new("downloading")) as ProgressHandle);
        let result = session
            .download(DownloadInput {
                account: account.clone(),
                app,
                output_path: args.output.clone(),
                progress: progress.clone(),
                external_version_id: args.external_version_id.clone(),
                platform: args.platform,
            })
            .await;
        let error = match result {
            Ok(output) => {
                replicate_download_sinf(client, args.platform, &output).await?;
                out.info(json!({
                    "output": output.destination_path.display().to_string(),
                    "purchased": purchased,
                    "success": true,
                }));
                return Ok(());
            }
            Err(error) => error,
        };
        let last = attempt == 2;
        token_expired = error.is_password_token_expired();
        purchase_required = error.is_license_required();
        if last || !(token_expired || (purchase_required && args.purchase)) {
            return Err(error.into());
        }
    }
    unreachable!("the third attempt returns")
}

/// Go's `replicateDownloadSinf`.
async fn replicate_download_sinf(
    client: &Client,
    platform: Option<Platform>,
    output: &DownloadOutput,
) -> Result<(), BoxError> {
    if platform == Some(Platform::MacOs) && output.sinfs.is_empty() {
        return Ok(());
    }
    client
        .replicate_sinf(
            output.sinfs.clone(),
            PathBuf::from(&output.destination_path),
        )
        .await?;
    Ok(())
}

/// A terminal progress bar in the spirit of Go's `progressbar` options: description,
/// percentage, bar, byte counts and rate, redrawn at most every 65 ms and cleared when
/// done.
struct ProgressBar {
    description: &'static str,
    state: Mutex<BarState>,
}

struct BarState {
    total: i64,
    position: i64,
    started: Instant,
    drawn: Option<Instant>,
    visible: bool,
}

const THROTTLE: Duration = Duration::from_millis(65);
const BAR_WIDTH: usize = 20;

impl ProgressBar {
    fn new(description: &'static str) -> Self {
        Self {
            description,
            state: Mutex::new(BarState {
                total: 1,
                position: 0,
                started: Instant::now(),
                drawn: None,
                visible: false,
            }),
        }
    }

    fn draw(&self, state: &mut BarState, force: bool) {
        let now = Instant::now();
        if !force && state.drawn.is_some_and(|drawn| now - drawn < THROTTLE) {
            return;
        }
        state.drawn = Some(now);
        let mut stdout = std::io::stdout().lock();
        if state.total > 0 && state.position >= state.total {
            // OptionClearOnFinish.
            if state.visible {
                let _ = write!(stdout, "\r\x1b[K");
                let _ = stdout.flush();
                state.visible = false;
            }
            return;
        }
        #[allow(clippy::cast_precision_loss, reason = "display only")]
        let fraction = if state.total > 0 {
            (state.position as f64 / state.total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "0..=width"
        )]
        let filled = (fraction * BAR_WIDTH as f64) as usize;
        let elapsed = now.duration_since(state.started).as_secs_f64().max(0.001);
        #[allow(clippy::cast_precision_loss, reason = "display only")]
        let rate = state.position as f64 / elapsed;
        let _ = write!(
            stdout,
            "\r{} {:3.0}% |{}{}| ({}/{}, {}/s)\x1b[K",
            self.description,
            fraction * 100.0,
            "█".repeat(filled),
            " ".repeat(BAR_WIDTH - filled),
            bytes(state.position),
            bytes(state.total),
            bytes_f(rate),
        );
        let _ = stdout.flush();
        state.visible = true;
    }
}

#[allow(clippy::cast_precision_loss, reason = "display only")]
fn bytes(value: i64) -> String {
    bytes_f(value.max(0) as f64)
}

fn bytes_f(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = value;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl Progress for ProgressBar {
    fn set_total(&self, total: i64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.total = total;
        self.draw(&mut state, true);
    }

    fn set(&self, position: i64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.position = position;
        state.started = Instant::now();
        self.draw(&mut state, true);
    }

    fn add(&self, bytes: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.position += i64::try_from(bytes).unwrap_or(i64::MAX);
        let done = state.position >= state.total;
        self.draw(&mut state, done);
    }
}
