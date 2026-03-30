use crate::{
    // downloader::DownloadStatus,
    services::putio::{self, PutIOTransfer},
    services::transmission::{TransmissionRequest, TransmissionTorrent},
    AppData, Config,
};
use actix_web::web;
use anyhow::Result;
use base64::Engine;
use colored::Colorize;
use lava_torrent::torrent::v1::Torrent;
use log::info;
use magnet_url::Magnet;
use serde_json::json;

fn determine_category(download_dir: &str, config: &Config) -> String {
    // Check if the download_dir matches any configured category
    if let Some(sonarr) = &config.sonarr {
        if let Some(category) = &sonarr.category {
            if download_dir.contains(category) {
                return category.clone();
            }
        }
    }
    if let Some(radarr) = &config.radarr {
        if let Some(category) = &radarr.category {
            if download_dir.contains(category) {
                return category.clone();
            }
        }
    }
    if let Some(whisparr) = &config.whisparr {
        if let Some(category) = &whisparr.category {
            if download_dir.contains(category) {
                return category.clone();
            }
        }
    }
    // Default category if none matched
    "default".to_string()
}

pub(crate) async fn handle_torrent_add(
    api_token: &str,
    payload: &web::Json<TransmissionRequest>,
    app_data: &web::Data<AppData>,
) -> Result<Option<serde_json::Value>> {
    let arguments = payload.arguments.as_ref().unwrap().as_object().unwrap();
    
    // Extract download-dir from the request to determine the category
    let download_dir = arguments.get("download-dir")
        .and_then(|v| v.as_str())
        .unwrap_or(&app_data.config.download_directory)
        .to_string();
    
    // Determine which service sent this based on the download-dir
    let category = determine_category(&download_dir, &app_data.config);
    if arguments.contains_key("metainfo") {
        // .torrent files
        let b64 = arguments["metainfo"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        putio::upload_file(api_token, &bytes).await?;

        match Torrent::read_from_bytes(bytes) {
            Ok(t) => {
                // Store transfer state with the torrent hash
                // When put.io processes this torrent, it will have the same hash
                let hash = t.info_hash();
                let full_download_dir = if category != "default" {
                    format!("{}/{}", app_data.config.download_directory, category)
                } else {
                    app_data.config.download_directory.clone()
                };
                app_data.state.add_transfer(
                    hash.to_lowercase(),
                    category.clone(),
                    full_download_dir
                ).await?;
                info!(
                    "{}: torrent uploaded (category: {})",
                    format!("[ffff: {}]", t.name).magenta(),
                    category
                );
            }
            Err(_) => info!("New torrent uploaded (category: {})", category),
        };
    } else {
        // Magnet links
        let magnet_url = arguments["filename"].as_str().unwrap();
        putio::add_transfer(api_token, magnet_url).await?;
        match Magnet::new(magnet_url) {
            Ok(m) => {
                // Store transfer state with hash from magnet
                if let Some(xt) = m.xt {
                    // Extract hash from xt (usually in format "urn:btih:HASH")
                    // unless magnet_url::Magnet has already extracted it
                    let hash = xt.strip_prefix("urn:btih:").unwrap_or(&xt);
                    let full_download_dir = if category != "default" {
                        format!("{}/{}", app_data.config.download_directory, category)
                    } else {
                        app_data.config.download_directory.clone()
                    };
                    app_data.state.add_transfer(
                        hash.to_lowercase(),
                        category.clone(),
                        full_download_dir
                    ).await?;
                }
                if let Some(dn) = m.dn {
                    info!(
                        "{}: magnet link uploaded (category: {})",
                        format!("[ffff: {}]", urldecode::decode(dn)).magenta(),
                        category
                    );
                } else {
                    info!("magnet link uploaded (category: {})", category);
                }
            }
            _ => {
                info!("unknown magnet link uploaded (category: {})", category);
            }
        }
    };
    Ok(None)
}

pub(crate) async fn handle_torrent_remove(
    api_token: &str,
    payload: &web::Json<TransmissionRequest>,
) -> Option<serde_json::Value> {
    // TODO: leanup all the unwrap stuff
    let arguments = payload.arguments.as_ref().unwrap().as_object().unwrap();
    let ids: Vec<&str> = arguments
        .get("ids")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let delete_local_data = arguments
        .get("delete-local-data")
        .unwrap()
        .as_bool()
        .unwrap();

    let putio_transfers: Vec<PutIOTransfer> = putio::list_transfers(api_token)
        .await
        .unwrap()
        .transfers
        .into_iter()
        .filter(|t| ids.contains(&t.hash.clone().unwrap_or(String::from("no_hash")).as_str()))
        .collect();

    for t in putio_transfers {
        putio::remove_transfer(api_token, t.id).await.unwrap();

        if t.userfile_exists && delete_local_data {
            putio::delete_file(api_token, t.file_id.unwrap())
                .await
                .unwrap();
        }
    }

    None
}

pub(crate) async fn handle_torrent_get(
    api_token: &str,
    app_data: &web::Data<AppData>,
) -> Option<serde_json::Value> {
    let transfers = putio::list_transfers(api_token).await.unwrap().transfers;

    let transmission_transfers = transfers.into_iter().map(|t| {
        let app_data = app_data.clone();
        async move {
            let mut tt: TransmissionTorrent = t.clone().into();
            // Get the correct download directory from state if available
            if let Some(hash) = &t.hash {
                tt.download_dir = app_data.state.get_download_dir_for_transfer(
                    hash, 
                    &app_data.config.download_directory
                ).await;
            } else {
                tt.download_dir = app_data.config.download_directory.clone();
            }
            tt
        }
    });
    let transmission_transfers: Vec<TransmissionTorrent> =
        futures::future::join_all(transmission_transfers).await;

    let torrents = json!(transmission_transfers);

    let mut arguments = serde_json::Map::new();
    arguments.insert(String::from("torrents"), torrents);

    Some(json!(arguments))
}
