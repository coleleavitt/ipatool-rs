//! `ipatool` command line interface (Rust port of the Go `cmd` package).

mod commerce;
mod login;
mod output;
mod prompt;
mod state;
mod versions;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use ipatool_appstore::storeauth::EmulatorStoreAmd;
use ipatool_appstore::{Account, Client, CookieJar, CountryCode, LookupQuery, Platform, keys};
use ipatool_keychain::{Config, Keychain};
use serde_json::json;

use crate::output::{Format, Output};

#[derive(Parser)]
#[command(
    name = "ipatool",
    version,
    about = "A cli tool for interacting with Apple's ipa files"
)]
struct Cli {
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Enables verbose logs.
    #[arg(long, global = true)]
    verbose: bool,
    /// Run in a non-interactive session.
    #[arg(long, global = true)]
    non_interactive: bool,
    /// Passphrase for unlocking the file keychain.
    #[arg(long, global = true, default_value = "")]
    keychain_passphrase: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authenticate with the App Store.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Search for apps available on the App Store.
    Search {
        term: String,
        /// Maximum amount of search results to retrieve.
        #[arg(short, long, default_value_t = 5)]
        limit: u32,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// List the newest versions (up to 25) with display names from the public
    /// catalog. Needs no login.
    CatalogVersions {
        #[command(flatten)]
        app: AppArgs,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Obtain a license for the app from the App Store.
    Purchase(commerce::PurchaseArgs),
    /// Download iOS, iPadOS, tvOS, watchOS, visionOS, and macOS app packages from the App
    /// Store.
    Download(commerce::DownloadArgs),
    /// List the available versions of an App Store app.
    ListVersions(versions::ListVersionsArgs),
    /// Retrieves the metadata for a specific version of an app.
    GetVersionMetadata(versions::GetVersionMetadataArgs),
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Login to the App Store.
    Login(login::LoginArgs),
    /// Show current account info.
    Info,
    /// Revoke your App Store credentials.
    Revoke,
}

#[derive(Args)]
struct StoreArgs {
    /// Platform: iphone, ipad, appletv, watchos, visionos, or macos.
    #[arg(long)]
    platform: Option<Platform>,
    /// Storefront country code. Defaults to the logged-in account's storefront.
    #[arg(long)]
    country: Option<CountryCode>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct AppArgs {
    /// Bundle identifier of the target app.
    #[arg(short, long)]
    bundle_identifier: Option<String>,
    /// Numeric App Store ID of the target app.
    #[arg(short = 'i', long)]
    app_id: Option<i64>,
}

impl AppArgs {
    fn query(&self) -> LookupQuery {
        match (&self.bundle_identifier, self.app_id) {
            (Some(bundle), _) => LookupQuery::BundleId(bundle.clone()),
            (None, Some(id)) => LookupQuery::AppId(id),
            (None, None) => unreachable!("clap requires one of the group"),
        }
    }
}

type BoxError = Box<dyn std::error::Error>;

const KEYCHAIN_SERVICE_NAME: &str = "ipatool-auth.service";

const COOKIE_JAR_FILE_NAME: &str = "cookies";

fn state_dir() -> Result<PathBuf, BoxError> {
    let home = std::env::home_dir().ok_or("could not find the home directory")?;
    Ok(state::prepare(&home, |name| std::env::var(name).ok())?)
}

/// A client sharing the Go tool's cookie jar (`cmd/common.go` `newCookieJar`).
fn store_client() -> Result<Client, BoxError> {
    let jar = CookieJar::open(state_dir()?.join(COOKIE_JAR_FILE_NAME))?;
    Ok(Client::with_cookie_jar(Arc::new(jar))?)
}

/// `--country`, or the country of the logged-in account's storefront as Go uses.
async fn country(cli: &Cli, client: &Client, store: &StoreArgs) -> Result<CountryCode, BoxError> {
    if let Some(country) = &store.country {
        return Ok(country.clone());
    }
    let account = account_info(&open_keychain(cli).await?).await?;
    let code = client
        .country_code_from_store_front(&account.store_front)
        .await
        .map_err(|err| format!("country code is invalid: {err}"))?;
    Ok(code.parse()?)
}

/// The Go keychain setup (`cmd/common.go` `newKeychain`).
async fn open_keychain(cli: &Cli) -> Result<Keychain, BoxError> {
    let state_dir = state_dir()?;
    let passphrase = cli.keychain_passphrase.clone();
    let interactive = !cli.non_interactive;
    Ok(Keychain::open(Config {
        service_name: KEYCHAIN_SERVICE_NAME.to_owned(),
        file_dir: state_dir,
        passphrase: Box::new(move |dir| {
            if !passphrase.is_empty() {
                return Ok(passphrase.clone());
            }
            if !interactive {
                return Err("keychain passphrase is required when not running in interactive mode; use the \"--keychain-passphrase\" flag".to_owned());
            }
            prompt::read_prompt(
                &format!(
                    "enter passphrase to unlock {} (this is separate from your Apple ID password): ",
                    dir.display()
                ),
                true,
            )
            .map_err(|err| format!("failed to read password: {err}"))
        }),
    })
    .await)
}

async fn account_info(keychain: &Keychain) -> Result<Account, BoxError> {
    let data = keychain
        .get(keys::ACCOUNT)
        .await
        .map_err(|err| format!("failed to get account: {err}"))?;
    Ok(serde_json::from_slice(&data).map_err(|err| format!("failed to unmarshal json: {err}"))?)
}

/// Go's `Revoke`: removes the account and the kbsync cache, reporting both errors.
async fn revoke(keychain: &Keychain) -> Result<(), BoxError> {
    let mut errors = Vec::new();
    if let Err(err) = keychain.remove(keys::ACCOUNT).await {
        errors.push(format!("failed to remove account from keychain: {err}"));
    }
    match keychain.remove(keys::KBSYNC).await {
        Ok(()) | Err(ipatool_keychain::Error::NotFound) => {}
        Err(err) => errors.push(format!(
            "failed to remove kbsync cache from keychain: {err}"
        )),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

async fn run_auth(cli: &Cli, command: &AuthCommand, out: &Output) -> Result<(), BoxError> {
    let keychain = open_keychain(cli).await?;
    match command {
        AuthCommand::Login(args) => {
            let client = store_client()?;
            login::run(
                args.clone(),
                !cli.non_interactive,
                out,
                prompt::read_prompt,
                async |input| {
                    client
                        .native_login(&input, &keychain, &EmulatorStoreAmd)
                        .await
                },
            )
            .await?;
        }
        AuthCommand::Info => {
            let account = account_info(&keychain).await?;
            out.info(json!({ "name": account.name, "email": account.email, "success": true }));
        }
        AuthCommand::Revoke => {
            revoke(&keychain).await?;
            out.info(json!({ "success": true }));
        }
    }
    Ok(())
}

async fn run(cli: &Cli, out: &Output) -> Result<(), BoxError> {
    match &cli.command {
        Command::Auth(command) => run_auth(cli, command, out).await?,
        Command::Search { term, limit, store } => {
            let client = store_client()?;
            let country = country(cli, &client, store).await?;
            let apps = client
                .search(term, &country, *limit, store.platform)
                .await?;
            out.info(json!({ "count": apps.len(), "apps": apps }));
        }
        Command::CatalogVersions { app, store } => {
            let client = store_client()?;
            let country = country(cli, &client, store).await?;
            let app = client
                .lookup(&app.query(), &country, store.platform)
                .await?;
            let versions = client
                .catalog_version_history(app.id, &country, store.platform)
                .await?;
            out.info(json!({ "app": app, "count": versions.len(), "versions": versions }));
        }
        Command::Purchase(args) => {
            let (client, keychain) = (store_client()?, open_keychain(cli).await?);
            let session = client.store_session(&keychain, &EmulatorStoreAmd);
            commerce::purchase(args, &session, out).await?;
        }
        Command::Download(args) => {
            let (client, keychain) = (store_client()?, open_keychain(cli).await?);
            let session = client.store_session(&keychain, &EmulatorStoreAmd);
            commerce::download(args, &session, !cli.non_interactive, out).await?;
        }
        Command::ListVersions(args) => {
            let (client, keychain) = (store_client()?, open_keychain(cli).await?);
            let session = client.store_session(&keychain, &EmulatorStoreAmd);
            let cache = state_dir()?.join(versions::VERSION_METADATA_CACHE_FILE_NAME);
            versions::list_versions(args, &session, Some(&cache), out).await?;
        }
        Command::GetVersionMetadata(args) => {
            let (client, keychain) = (store_client()?, open_keychain(cli).await?);
            let session = client.store_session(&keychain, &EmulatorStoreAmd);
            versions::get_version_metadata(args, &session, out).await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let out = Output::new(cli.format, cli.verbose);
    match run(&cli, &out).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            out.error(error.as_ref());
            ExitCode::FAILURE
        }
    }
}
