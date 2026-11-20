//! `purchase`, `download` and `list-purchases` (Go `cmd/purchase.go`, `cmd/download.go`,
//! `cmd/purchases.go`).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::Args;
use ipatool_appstore::storeauth::EmulatorStoreAmd;
use ipatool_appstore::{
    Account, App, DEFAULT_OWNED_APPS_LIMIT, DownloadInput, DownloadOutput, LoginInput, LookupQuery,
    MAX_OWNED_APPS_LIMIT, OwnedAppsInput, OwnedAppsOutput, Platform, Progress, ProgressHandle,
    Sinf, StoreSession,
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

#[derive(Args, Clone)]
pub struct ListPurchasesArgs {
    /// maximum number of apps to return per page
    #[arg(short = 'l', long, default_value_t = DEFAULT_OWNED_APPS_LIMIT, allow_negative_numbers = true)]
    max_results: i64,
    /// page of owned apps to return
    #[arg(short, long, default_value_t = 1, allow_negative_numbers = true)]
    page: i64,
    /// Filter by platform: iphone (iOS), ipad (iPadOS), appletv (tvOS), visionos, or macos
    #[arg(long, default_value = "")]
    platform: String,
}

const MISSING_APP: &str = "either the app ID or the bundle identifier must be specified";

type AppResult<T> = ipatool_appstore::Result<T>;

/// The App Store calls the commands make (Go's `appstore.AppStore`), so tests can replace them.
pub(crate) trait Store {
    async fn account_info(&self) -> AppResult<Account>;
    /// Go's `Login` with the saved credentials, after an expired password token.
    async fn login(&self, account: &Account) -> AppResult<Account>;
    /// Go's `Lookup` in the account's storefront.
    async fn lookup(
        &self,
        account: &Account,
        bundle_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<App>;
    async fn purchase(
        &self,
        account: &mut Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<()>;
    async fn download(&self, input: DownloadInput) -> AppResult<DownloadOutput>;
    async fn replicate_sinf(&self, sinfs: Vec<Sinf>, package: PathBuf) -> AppResult<()>;
    async fn owned_apps(&self, input: OwnedAppsInput) -> AppResult<OwnedAppsOutput>;
}

impl Store for StoreSession<'_> {
    async fn account_info(&self) -> AppResult<Account> {
        StoreSession::account_info(self).await
    }

    async fn login(&self, account: &Account) -> AppResult<Account> {
        let input = LoginInput {
            email: account.email.clone(),
            password: account.password.clone(),
            auth_code: String::new(),
        };
        Ok(self
            .client
            .native_login(&input, self.keychain(), &EmulatorStoreAmd)
            .await?
            .account)
    }

    async fn lookup(
        &self,
        account: &Account,
        bundle_id: &str,
        platform: Option<Platform>,
    ) -> AppResult<App> {
        let country = self
            .client
            .country_code_from_store_front(&account.store_front)
            .await
            .map_err(|error| {
                ipatool_appstore::Error::Invalid(format!(
                    "failed to resolve the country code: {error}"
                ))
            })?
            .parse()?;
        self.client
            .lookup(
                &LookupQuery::BundleId(bundle_id.to_owned()),
                &country,
                platform,
            )
            .await
    }

    async fn purchase(
        &self,
        account: &mut Account,
        app: &App,
        platform: Option<Platform>,
    ) -> AppResult<()> {
        StoreSession::purchase(self, account, app, platform).await
    }

    async fn download(&self, input: DownloadInput) -> AppResult<DownloadOutput> {
        StoreSession::download(self, input).await
    }

    async fn replicate_sinf(&self, sinfs: Vec<Sinf>, package: PathBuf) -> AppResult<()> {
        self.client.replicate_sinf(sinfs, package).await
    }

    async fn owned_apps(&self, input: OwnedAppsInput) -> AppResult<OwnedAppsOutput> {
        self.client.owned_apps(input).await
    }
}

/// Go's `purchaseCmd`: two attempts; any step failing with an expired password token
/// logs in again on the second. The account is read once and the refreshed one reused
/// (fork: `fix/reuse-account-across-retries`).
pub(crate) async fn purchase(
    args: &PurchaseArgs,
    store: &impl Store,
    out: &Output,
) -> Result<(), BoxError> {
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let mut account = store.account_info().await?;
    let mut token_expired = false;
    for attempt in 0..2 {
        match purchase_attempt(args, store, &mut account, token_expired).await {
            Ok(already_owned) => {
                out.info(json!({ "alreadyOwned": already_owned, "success": true }));
                return Ok(());
            }
            Err(error) if error.is_password_token_expired() && attempt == 0 => {
                token_expired = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the second attempt returns")
}

/// One run of Go's `purchaseCmd` retry callback; `Ok(true)` when the app is already owned.
async fn purchase_attempt(
    args: &PurchaseArgs,
    store: &impl Store,
    account: &mut Account,
    login: bool,
) -> AppResult<bool> {
    if login {
        *account = store.login(account).await?;
    }
    let mut app = App {
        id: args.app_id,
        ..App::default()
    };
    if !args.bundle_identifier.is_empty() {
        app = store
            .lookup(account, &args.bundle_identifier, args.platform)
            .await?;
    }
    match store.purchase(account, &app, args.platform).await {
        Ok(()) => Ok(false),
        Err(error) if error.is_license_already_exists() => Ok(true),
        Err(error) => Err(error),
    }
}

/// Go's `listPurchasesCmd`: the flags are checked before the account is read; two attempts,
/// the second after logging in again when the token expired. The account is read once
/// (fork: `fix/reuse-account-across-retries`).
pub(crate) async fn list_purchases(
    args: &ListPurchasesArgs,
    store: &impl Store,
    out: &Output,
) -> Result<(), BoxError> {
    if args.page < 1 {
        return Err("page must be greater than 0".into());
    }
    if args.max_results < 1 {
        return Err("max results must be greater than 0".into());
    }
    if args.max_results > MAX_OWNED_APPS_LIMIT {
        return Err(format!("max results must not exceed {MAX_OWNED_APPS_LIMIT}").into());
    }
    let platform = parse_platform_filter(&args.platform)?;
    let mut account = store.account_info().await?;
    for attempt in 0..2 {
        if attempt == 1 {
            account = store.login(&account).await?;
        }
        let input = OwnedAppsInput {
            account: account.clone(),
            page: args.page,
            limit: args.max_results,
            platform,
        };
        match store.owned_apps(input).await {
            Ok(output) => {
                out.info(json!({
                    "count": output.count,
                    "totalCount": output.total_count,
                    "page": output.page,
                    "apps": output.results,
                }));
                return Ok(());
            }
            Err(error) if error.is_password_token_expired() && attempt == 0 => {}
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("the second attempt returns")
}

/// Go's `ParsePlatform`, where an empty value means every platform.
fn parse_platform_filter(value: &str) -> AppResult<Option<Platform>> {
    if value.is_empty() {
        Ok(None)
    } else {
        value.parse().map(Some)
    }
}

/// What Go's `downloadCmd` carries from one retry to the next.
struct DownloadState {
    account: Account,
    token_expired: bool,
    purchase_required: bool,
    purchased: bool,
}

/// Go's `downloadCmd`: up to three attempts; any step failing with an expired token logs
/// in again, and with `--purchase` a missing license is obtained first.
pub(crate) async fn download(
    args: &DownloadArgs,
    store: &impl Store,
    interactive: bool,
    out: &Output,
) -> Result<(), BoxError> {
    if args.app_id == 0 && args.bundle_identifier.is_empty() {
        return Err(MISSING_APP.into());
    }
    let mut state = DownloadState {
        account: store.account_info().await?,
        token_expired: false,
        purchase_required: false,
        purchased: false,
    };
    for attempt in 0..3 {
        let error = match download_attempt(args, store, interactive, out, &mut state).await {
            Ok(output) => {
                out.info(json!({
                    "output": output.destination_path.display().to_string(),
                    "purchased": state.purchased,
                    "success": true,
                }));
                return Ok(());
            }
            Err(error) => error,
        };
        state.token_expired = error.is_password_token_expired();
        let license_required = error.is_license_required();
        // Like Go, a pending purchase stays pending until one succeeds.
        state.purchase_required |= license_required;
        let retry = state.token_expired || (license_required && args.purchase);
        if attempt == 2 || !retry {
            return Err(error.into());
        }
    }
    unreachable!("the third attempt returns")
}

/// One run of Go's `downloadCmd` retry callback.
async fn download_attempt(
    args: &DownloadArgs,
    store: &impl Store,
    interactive: bool,
    out: &Output,
    state: &mut DownloadState,
) -> AppResult<DownloadOutput> {
    if state.token_expired {
        state.account = store.login(&state.account).await?;
    }
    let mut app = App {
        id: args.app_id,
        bundle_id: args.bundle_identifier.clone(),
        ..App::default()
    };
    if !args.bundle_identifier.is_empty() {
        match store
            .lookup(&state.account, &args.bundle_identifier, args.platform)
            .await
        {
            Ok(found) => app = found,
            // Delisted apps may still be downloadable by their numeric ID.
            Err(ipatool_appstore::Error::AppNotFound) if args.app_id != 0 => {}
            Err(error) => return Err(error),
        }
    }
    if state.purchase_required {
        if let Err(error) = store
            .purchase(&mut state.account, &app, args.platform)
            .await
            && !error.is_license_already_exists()
        {
            return Err(error);
        }
        // Purchasing may persist a server-corrected storefront.
        state.account = store.account_info().await?;
        state.purchase_required = false;
        state.purchased = true;
        out.verbose(json!({ "success": true }), "purchase");
    }
    let progress = interactive.then(|| Arc::new(ProgressBar::new("downloading")) as ProgressHandle);
    let output = store
        .download(DownloadInput {
            account: state.account.clone(),
            app,
            output_path: args.output.clone(),
            progress,
            external_version_id: args.external_version_id.clone(),
            platform: args.platform,
        })
        .await?;
    replicate_download_sinf(store, args.platform, &output).await?;
    Ok(output)
}

/// Go's `replicateDownloadSinf`.
async fn replicate_download_sinf(
    store: &impl Store,
    platform: Option<Platform>,
    output: &DownloadOutput,
) -> AppResult<()> {
    if platform == Some(Platform::MacOs) && output.sinfs.is_empty() {
        return Ok(());
    }
    store
        .replicate_sinf(output.sinfs.clone(), output.destination_path.clone())
        .await
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

#[cfg(test)]
mod tests;
