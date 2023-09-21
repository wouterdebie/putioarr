use actix_web::{web, App, HttpServer};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use log::info;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::sleep;

mod arr;
mod downloader;
mod handlers;
mod oob;
mod putio;
mod routes;
mod transmission;

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
    username: String,
    password: String,
    bind_address: String,
    port: u16,
    loglevel: String,
    download_directory: String,
    uid: u32,
    polling_interval: u64,
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
                .join(Serialized::default("port", 9091))
                .join(Serialized::default("loglevel", "info"))
                .join(Serialized::default("uid", 1000))
                .join(Serialized::default("polling_interval", 10))
                .merge(Toml::file(&args.config_path))
                .extract()?;

            std::env::set_var("RUST_LOG", config.loglevel.as_str());
            env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

            info!("Starting putioarr, version {}", VERSION);

            let app_data = web::Data::new(AppData {
                config: config.clone(),
            });

            let data_for_download_system = app_data.clone();
            downloader::start_download_system(data_for_download_system)
                .await
                .unwrap();

            info!("Starting web server at http://{}:{}", config.bind_address, config.port);
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
            let oob_code = oob::get().await.expect("fetching OOB code");
            println!(
                "Go to https://put.io/link and enter the code: {:#?}",
                oob_code
            );
            println!("Waiting for link...");

            // Every three seconds, check if the OOB code was linked to the user's account
            let three_seconds = Duration::from_secs(3);

            loop {
                sleep(three_seconds).await;

                let get_oauth_token_result = oob::check(oob_code.clone()).await;

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
