use crate::{http::routes, services::putio};
use actix_web::{web, App, HttpServer};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use env_logger::TimestampPrecision;
use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utils::{generate_config, get_token};

mod download_system;
mod http;
mod services;
mod state;
mod utils;

/// put.io to sonarr/radarr proxy
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the proxy
    Run(RunArgs),
    /// Generate a put.io API token
    GetToken,
    /// Generate config
    GenerateConfig(RunArgs),
}

#[derive(Parser)]
struct RunArgs {
    #[arg(short, long = "config", default_value_t = ProjectDirs::from("nl", "evenflow", "putioarr").unwrap().config_dir().join("config.toml").into_os_string().into_string().unwrap(), env("APP_CONFIG_PATH"))]
    pub config_path: String,
}

/// Default for [`Config::import_timeout_secs`] (2h), enforced at the type level
/// so the documented default holds even without the Figment default layer.
fn default_import_timeout_secs() -> u64 {
    7200
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    bind_address: String,
    download_directory: String,
    download_workers: usize,
    loglevel: String,
    orchestration_workers: usize,
    password: String,
    polling_interval: u64,
    /// How long (seconds) to keep polling for a transfer to be imported before
    /// giving up watching it. Bounds the per-transfer import-watch loop so a
    /// transfer that never fully imports (e.g. one with a sample the *arr won't
    /// import) can't accumulate and stall downloads. Default 7200 (2h); 0
    /// disables the bound (watch indefinitely).
    #[serde(default = "default_import_timeout_secs")]
    import_timeout_secs: u64,
    port: u16,
    skip_directories: Vec<String>,
    uid: u32,
    username: String,
    /// When true, download every transfer found on the put.io account, including
    /// ones not added by putioarr (e.g. a manually-maintained Watch List). When
    /// false (default), only download transfers putioarr added on behalf of an
    /// *arr. Keeping this false avoids hanging on seeding transfers (issue #9) and
    /// lets the put.io account be shared with manual downloads.
    #[serde(default)]
    download_unmanaged: bool,
    /// put.io folder ids to additionally scan for *orphaned* completed files:
    /// files that were downloaded but whose transfer record no longer exists
    /// (e.g. put.io's "clear completed transfers" removes the transfer while
    /// leaving the file in the folder). Since putioarr normally only discovers
    /// work from `transfers/list`, such files would never be pulled. Any video
    /// file here with no active transfer that isn't already imported is pulled
    /// like a normal download (see issue #34). Empty (default) disables this.
    #[serde(default)]
    watch_folders: Vec<i64>,
    /// How often, in seconds, to scan `watch_folders` for orphaned files.
    /// Defaults to 60. Each scan lists every configured folder on put.io, so
    /// raise this if you have many folders and want to keep API traffic low;
    /// it's independent of `polling_interval`. Only used when `watch_folders`
    /// is non-empty.
    #[serde(default = "default_watch_folder_interval_secs")]
    watch_folder_interval_secs: u64,
    putio: PutioConfig,
    sonarr: Option<ArrConfig>,
    radarr: Option<ArrConfig>,
    whisparr: Option<ArrConfig>,
    lidarr: Option<ArrConfig>,
    /// Arbitrarily-named *arr instances, configured under `[arrs.<name>]`
    /// in config.toml. Each entry must set `type = "sonarr"|"radarr"|"whisparr"|"lidarr"`
    /// so we know which API flavor and media type to use.
    #[serde(default)]
    arrs: HashMap<String, ArrConfig>,
}

impl Config {
    /// Iterate over every configured *arr instance as `(name, kind, &ArrConfig)`.
    /// Combines the named `[sonarr]`, `[radarr]`, `[whisparr]`, `[lidarr]`
    /// sections with anything under `[arrs.*]`.
    pub fn all_arrs(&self) -> Vec<(String, services::arr::ArrKind, &ArrConfig)> {
        let mut out: Vec<(String, services::arr::ArrKind, &ArrConfig)> = Vec::new();
        if let Some(c) = &self.sonarr {
            out.push(("sonarr".to_string(), services::arr::ArrKind::Sonarr, c));
        }
        if let Some(c) = &self.radarr {
            out.push(("radarr".to_string(), services::arr::ArrKind::Radarr, c));
        }
        if let Some(c) = &self.whisparr {
            out.push(("whisparr".to_string(), services::arr::ArrKind::Whisparr, c));
        }
        if let Some(c) = &self.lidarr {
            out.push(("lidarr".to_string(), services::arr::ArrKind::Lidarr, c));
        }
        for (name, c) in &self.arrs {
            let kind = c
                .r#type
                .as_deref()
                .and_then(services::arr::ArrKind::from_str)
                .or_else(|| services::arr::ArrKind::from_str(name))
                .unwrap_or(services::arr::ArrKind::Sonarr);
            out.push((name.clone(), kind, c));
        }
        out
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PutioConfig {
    api_key: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArrConfig {
    pub url: String,
    pub api_key: String,
    pub category: Option<String>,
    /// For [arrs.<name>] entries: explicitly choose the *arr flavor.
    /// One of "sonarr", "radarr", "whisparr", "lidarr". Defaults to inferring
    /// from the section name, or Sonarr if that fails.
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
}

pub struct AppData {
    pub config: Config,
    pub state: state::StateManager,
    /// Shared HTTP client, reused across all downloads so connections are
    /// pooled instead of building a new client per fetch.
    pub http: reqwest::Client,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[actix_web::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Run(args) => {
            let config: Config = Figment::new()
                .join(Serialized::default("bind_address", "0.0.0.0"))
                .join(Serialized::default("download_workers", 4))
                .join(Serialized::default("orchestration_workers", 10))
                .join(Serialized::default("loglevel", "info"))
                .join(Serialized::default("polling_interval", 10))
                .join(Serialized::default("import_timeout_secs", 7200u64))
                .join(Serialized::default("port", 9091))
                .join(Serialized::default("uid", 1000))
                .join(Serialized::default("download_unmanaged", false))
                .join(Serialized::default(
                    "skip_directories",
                    vec!["sample", "extras"],
                ))
                .merge(Toml::file(&args.config_path))
                .extract()?;

            let log_timestamp = if in_container::in_container() {
                Some(TimestampPrecision::Seconds)
            } else if let Ok(istty) = nix::unistd::isatty(0) {
                if istty {
                    Some(TimestampPrecision::Seconds)
                } else {
                    None
                }
            } else {
                None
            };

            // User-provided level applies to our crate; noisy HTTP/runtime
            // crates are pinned to `info` so debug logs stay readable.
            let log_filter = format!(
                "{level},actix_web=info,actix_server=info,actix_http=info,mio=info,reqwest=info,hyper=info,hyper_util=info,h2=info,rustls=info,want=info,tokio_util=info",
                level = config.loglevel
            );

            env_logger::Builder::new()
                .default_format()
                .format_module_path(false)
                .format_target(false)
                .format_timestamp(log_timestamp)
                .parse_filters(&log_filter)
                .init();

            info!("Starting putioarr, version {}", VERSION);

            let http = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("building shared reqwest client");
            let app_data = web::Data::new(AppData {
                config: config.clone(),
                state: state::StateManager::new(config.putio.api_key.clone()),
                http,
            });

            match putio::account_info(&app_data.config.putio.api_key).await {
                Ok(_) => {}
                Err(e) => {
                    error!("{}", e);
                    bail!(e)
                }
            }

            // Restore transfer state (category/download-dir mappings) that was
            // persisted to put.io's per-user config store, so restarts keep
            // routing transfers to the correct directories.
            app_data.state.load().await?;

            let data_for_download_system = app_data.clone();
            download_system::start(data_for_download_system)
                .await
                .unwrap();

            info!(
                "Starting web server at http://{}:{}",
                config.bind_address, config.port
            );
            HttpServer::new(move || {
                App::new()
                    // .wrap(Logger::new(
                    //     "%a \"%r\" %s %b \"%{Referer}i\" \"%{User-Agent}i\" %T",
                    // ))
                    .app_data(app_data.clone())
                    .service(routes::rpc_post)
                    .service(routes::rpc_get)
            })
            .bind((config.bind_address, config.port))?
            .run()
            .await
            .context("Unable to start http server")
        }
        Commands::GetToken => {
            get_token().await?;
            Ok(())
        }
        Commands::GenerateConfig(args) => {
            generate_config(&args.config_path).await?;
            Ok(())
        }
    }
}
