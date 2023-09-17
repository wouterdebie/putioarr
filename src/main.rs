use std::time::Duration;

use actix_web::{web, App, HttpServer};
use appdata::AppData;
use clap::{Args, Parser, Subcommand};
use tokio::time::sleep;

mod appdata;
mod downloader;
mod handlers;
mod oob;
mod putio;
mod routes;
mod transmission;

/// put.io to sonarr/radarr proxy
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the proxy
    Run(RunArgs),
    /// Generate a put.io API token
    GetToken,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// put.io API token
    #[arg(short, long)]
    api_token: String,

    /// File path where state is saved
    #[arg(short, long)]
    state_file: String,

    /// Directory where downloads are saved
    #[arg(short, long)]
    download_directory: String,

    #[arg(short, long, default_value_t = 7070)]
    port: u16,

    #[arg(short, long, default_value_t = String::from("0.0.0.0"))]
    bind_address: String,

    #[arg(short, long, default_value_t = String::from("info"))]
    loglevel: String,

    /// UID of the user owning the donwloads
    #[arg(short, long, default_value_t = 1000)]
    uid: u32,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Run(args) => {
            std::env::set_var("RUST_LOG", args.loglevel.as_str());
            env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

            let app_data = web::Data::new(
                AppData::new(
                    args.api_token.clone(),
                    args.state_file.clone(),
                    args.download_directory.clone(),
                    args.uid,
                )
                .await
                .unwrap(),
            );

            // Actix background jobs
            let data_for_background = app_data.clone();
            actix_rt::spawn(async { downloader::start_downloader_task(data_for_background).await });

            // let http_server_data = data.clone();
            HttpServer::new(move || {
                App::new()
                    // .wrap(Logger::new(
                    //     "%a \"%r\" %s %b \"%{Referer}i\" \"%{User-Agent}i\" %T",
                    // ))
                    .app_data(app_data.clone())
                    .service(routes::rpc_post)
                    .service(routes::rpc_get)
            })
            .bind(("0.0.0.0", args.port))?
            .run()
            .await
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
            // If linked, update the config file
            // Stops after 10 tries (30 seconds)
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
