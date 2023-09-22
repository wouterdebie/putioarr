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
use std::time::Duration;
use tokio::time::sleep;

mod download_system;
mod http;
mod services;

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
}

#[derive(Parser)]
struct RunArgs {
    #[arg(short, long = "config", default_value_t = ProjectDirs::from("nl", "evenflow", "putioarr").unwrap().config_dir().join("config.toml").into_os_string().into_string().unwrap(), env("APP_CONFIG_PATH"))]
    pub config_path: String,
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
    port: u16,
    skip_directories: Vec<String>,
    uid: u32,
    username: String,
    putio: PutioConfig,
    sonarr: Option<ArrConfig>,
    radarr: Option<ArrConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PutioConfig {
    api_key: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArrConfig {
    url: String,
    api_key: String,
}

pub struct AppData {
    pub config: Config,
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
                .join(Serialized::default("port", 9091))
                .join(Serialized::default("uid", 1000))
                .join(Serialized::default(
                    "skip_directories",
                    vec!["sample", "extras"],
                ))
                .merge(Toml::file(&args.config_path))
                .extract()?;

            // std::env::set_var("RUST_LOG", config.loglevel.as_str());
            // env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

            let log_timestamp = match nix::unistd::isatty(0) {
                Ok(istty) if istty => Some(TimestampPrecision::Seconds),
                Ok(_) => None,
                Err(_) => Some(TimestampPrecision::Seconds),
            };

            env_logger::Builder::new()
                .default_format()
                .format_module_path(false)
                .format_target(false)
                .format_timestamp(log_timestamp)
                .parse_filters(config.loglevel.as_str())
                .init();

            info!("Starting putioarr, version {}", VERSION);

            let app_data = web::Data::new(AppData {
                config: config.clone(),
            });

            match putio::account_info(&app_data.config.putio.api_key).await {
                Ok(_) => {}
                Err(e) => {
                    error!("{}", e);
                    bail!(e)
                }
            }

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
            // Create new OOB code and prompt user to link
            let oob_code = services::putio::get_oob().await.expect("fetching OOB code");
            println!(
                "Go to https://put.io/link and enter the code: {:#?}",
                oob_code
            );
            println!("Waiting for link...");

            // Every three seconds, check if the OOB code was linked to the user's account
            let three_seconds = Duration::from_secs(3);

            loop {
                sleep(three_seconds).await;

                let get_oauth_token_result = services::putio::check_oob(oob_code.clone()).await;

                match get_oauth_token_result {
                    Ok(token) => {
                        println!("Put.io API token: {token}");
                        break;
                    }
                    Err(_error) => {
                        continue;
                    }
                };
            }
            Ok(())
        }
    }
}
