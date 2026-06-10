use crate::{
    // downloader::DownloadStatus,
    services::putio::{self, PutIOTransfer},
    services::transmission::{TransmissionRequest, TransmissionTorrent, TransmissionTorrentStatus},
    AppData, Config,
};
use actix_web::web;
use anyhow::Result;
use base64::Engine;
use colored::Colorize;
use lava_torrent::torrent::v1::Torrent;
use log::{error, info, warn};
use magnet_url::Magnet;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

fn determine_category(download_dir: &str, config: &Config) -> String {
    let arrs = config.all_arrs();
    if arrs.is_empty() {
        warn!("category check: no *arr instances configured");
    }

    for (name, _kind, arr) in &arrs {
        match &arr.category {
            Some(c) if download_dir.contains(c) => {
                info!(
                    "category match: download_dir={:?} contains {} category {:?}",
                    download_dir, name, c
                );
                return c.clone();
            }
            Some(c) => info!(
                "category check: {} category {:?} not found in download_dir={:?}",
                name, c, download_dir
            ),
            None => info!("category check: {} has no category configured", name),
        }
    }

    warn!(
        "category match: no configured *arr category matched download_dir={:?}, falling back to 'default'",
        download_dir
    );
    "default".to_string()
}

pub(crate) async fn handle_torrent_add(
    api_token: &str,
    payload: &web::Json<TransmissionRequest>,
    app_data: &web::Data<AppData>,
) -> Result<Option<serde_json::Value>> {
    let arguments = payload.arguments.as_ref().unwrap().as_object().unwrap();

    let raw_download_dir = arguments.get("download-dir").and_then(|v| v.as_str());
    info!(
        "torrent-add: received download-dir={:?} (configured default: {:?})",
        raw_download_dir, app_data.config.download_directory
    );
    let download_dir = raw_download_dir
        .unwrap_or(&app_data.config.download_directory)
        .to_string();

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
                let hash = t.info_hash();
                let full_download_dir = if category != "default" {
                    format!("{}/{}", app_data.config.download_directory, category)
                } else {
                    app_data.config.download_directory.clone()
                };
                info!(
                    "torrent-add: storing state for hash={} category={} dir={}",
                    hash.to_lowercase(), category, full_download_dir
                );
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
                if let Some(xt) = &m.xt {
                    let hash = xt.strip_prefix("urn:btih:").unwrap_or(xt);
                    let full_download_dir = if category != "default" {
                        format!("{}/{}", app_data.config.download_directory, category)
                    } else {
                        app_data.config.download_directory.clone()
                    };
                    info!(
                        "torrent-add (magnet): storing state for hash={} category={} dir={}",
                        hash.to_lowercase(), category, full_download_dir
                    );
                    app_data.state.add_transfer(
                        hash.to_lowercase(),
                        category.clone(),
                        full_download_dir
                    ).await?;
                } else {
                    warn!(
                        "torrent-add (magnet): no xt field in magnet url, cannot store category/dir state (category={})",
                        category
                    );
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

    let putio_transfers: Vec<PutIOTransfer> = match putio::list_transfers(api_token).await {
        Ok(r) => r
            .transfers
            .into_iter()
            .filter(|t| ids.contains(&t.hash.clone().unwrap_or(String::from("no_hash")).as_str()))
            .collect(),
        Err(e) => {
            error!("Failed to list put.io transfers for removal: {}", e);
            return None;
        }
    };

    for t in putio_transfers {
        if let Err(e) = putio::remove_transfer(api_token, t.id).await {
            error!("Failed to remove put.io transfer {}: {}", t.id, e);
            continue;
        }

        if t.userfile_exists && delete_local_data {
            if let Some(file_id) = t.file_id {
                if let Err(e) = putio::delete_file(api_token, file_id).await {
                    error!("Failed to delete put.io file {}: {}", file_id, e);
                }
            }
        }
    }

    None
}

pub(crate) async fn handle_torrent_get(
    api_token: &str,
    app_data: &web::Data<AppData>,
) -> Option<serde_json::Value> {
    let transfers = match putio::list_transfers(api_token).await {
        Ok(r) => r.transfers,
        Err(e) => {
            error!("Failed to list put.io transfers: {}", e);
            Vec::new()
        }
    };

    // Keep the file-name cache bounded to the transfers currently on the
    // account, dropping cached entries for transfers that no longer exist so it
    // doesn't grow without limit over the lifetime of the process.
    let active_file_ids: HashSet<i64> = transfers.iter().filter_map(|t| t.file_id).collect();
    app_data.state.retain_file_names(&active_file_ids).await;

    // Allocate the token once and share it cheaply (refcount bump) with each
    // per-transfer task, rather than allocating a new String per transfer.
    let api_token: Arc<str> = Arc::from(api_token);
    let transmission_transfers = transfers.into_iter().map(|t| {
        let app_data = app_data.clone();
        let api_token = Arc::clone(&api_token);
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
            // put.io's transfer name often differs from the actual downloaded
            // file/folder name (e.g. an indexer prefix like "www.foo.org - "),
            // but the *arr locates the download at <download_dir>/<name>. Report
            // the real put.io file/folder name (the one we download into) so the
            // import doesn't fail with "No files found eligible for import" (#20).
            if let Some(file_id) = t.file_id {
                let resolved = match app_data.state.get_file_name(file_id).await {
                    Some(name) => Some(name),
                    // Don't re-hit the API for a lookup that recently failed
                    // (e.g. a file removed from put.io); retry only after a TTL.
                    None if app_data.state.name_lookup_suppressed(file_id).await => None,
                    None => match putio::list_files(&api_token, file_id).await {
                        Ok(r) => {
                            app_data.state.set_file_name(file_id, r.parent.name.clone()).await;
                            Some(r.parent.name)
                        }
                        Err(e) => {
                            app_data.state.mark_name_failed(file_id).await;
                            warn!("Could not resolve put.io file name for {}: {}", file_id, e);
                            None
                        }
                    },
                };
                if let Some(name) = resolved {
                    tt.name = name;
                }
            }
            // put.io marks a transfer complete as soon as *its own* (cloud)
            // download finishes, but the files don't exist on local disk until
            // putioarr has pulled them down. Reporting completion to the *arr
            // too early makes it try to import missing files ("No files found
            // eligible for import"). Keep the torrent in a downloading state
            // until putioarr has actually finished the local download (#16).
            let putio_done = tt.is_finished
                || matches!(
                    tt.status,
                    TransmissionTorrentStatus::Seeding | TransmissionTorrentStatus::Stopped
                );
            if putio_done && !app_data.state.is_local_complete(t.id).await {
                tt.is_finished = false;
                tt.left_until_done = std::cmp::max(tt.total_size, 1);
                tt.status = TransmissionTorrentStatus::Downloading;
            }
            tt
        }
    });
    let mut transmission_transfers: Vec<TransmissionTorrent> =
        futures::future::join_all(transmission_transfers).await;

    // Also report orphaned watch-folder files (which have no put.io transfer) as
    // downloads, so the *arr imports them like any other completed download once
    // putioarr has pulled them locally (issue #34).
    for orphan in app_data.state.orphans().await {
        // put.io file ids are non-negative; guard the i64->u64 conversion so a
        // bad value can't wrap into a wrong id / local-complete key.
        let id = match u64::try_from(orphan.file_id) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let complete = app_data.state.is_local_complete(id).await;
        // Keep size/left consistent: never report left_until_done > total_size
        // (size can be 0 if put.io omitted it).
        let size = orphan.size.max(0);
        transmission_transfers.push(TransmissionTorrent {
            id,
            hash_string: Some(orphan.hash),
            name: orphan.name,
            download_dir: orphan.download_dir,
            total_size: size,
            left_until_done: if complete { 0 } else { size },
            is_finished: complete,
            eta: 0,
            status: if complete {
                TransmissionTorrentStatus::Seeding
            } else {
                TransmissionTorrentStatus::Downloading
            },
            seconds_downloading: 0,
            error_string: None,
            downloaded_ever: if complete { size } else { 0 },
            seed_ratio_limit: 0.0,
            seed_ratio_mode: 0,
            seed_idle_limit: 0,
            seed_idle_mode: 0,
            file_count: 1,
        });
    }

    let torrents = json!(transmission_transfers);

    let mut arguments = serde_json::Map::new();
    arguments.insert(String::from("torrents"), torrents);

    Some(json!(arguments))
}
