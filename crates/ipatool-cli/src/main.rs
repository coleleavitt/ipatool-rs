//! `ipatool` command line interface (Rust port of the Go `cmd` package).

mod output;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use ipatool_appstore::{Client, CountryCode, LookupQuery, Platform};
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
}

#[derive(Args)]
struct StoreArgs {
    /// Platform: iphone, ipad, appletv, watchos, visionos, or macos.
    #[arg(long)]
    platform: Option<Platform>,
    /// Storefront country code. Used until login and account storefronts are ported.
    #[arg(long, default_value = "US")]
    country: CountryCode,
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

async fn run(cli: Cli, out: &Output) -> ipatool_appstore::Result<()> {
    let client = Client::new()?;
    match cli.command {
        Command::Search { term, limit, store } => {
            let apps = client
                .search(&term, &store.country, limit, store.platform)
                .await?;
            out.info(json!({ "count": apps.len(), "apps": apps }));
        }
        Command::CatalogVersions { app, store } => {
            let app = client
                .lookup(&app.query(), &store.country, store.platform)
                .await?;
            let versions = client
                .catalog_version_history(app.id, &store.country, store.platform)
                .await?;
            out.info(json!({ "app": app, "count": versions.len(), "versions": versions }));
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let out = Output::new(cli.format, cli.verbose);
    match run(cli, &out).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            out.error(&error);
            ExitCode::FAILURE
        }
    }
}
