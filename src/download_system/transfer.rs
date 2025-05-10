use crate::{
    services::{
        arr::ArrApp,
        putio::{self, PutIOTransfer},
    },
    AppData,
};
use actix_web::web::Data;
use anyhow::Result;
use async_channel::Sender;
use async_recursion::async_recursion;
use colored::*;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::{fmt::Display, path::Path};
use tokio::time::sleep;

#[derive(Clone)]
pub struct Transfer {
    pub name: String,
    pub file_id: Option<i64>,
    pub hash: Option<String>,
    pub transfer_id: u64,
    pub targets: Option<Vec<DownloadTarget>>,
    pub app_data: Data<AppData>,
}

impl Transfer {
    pub async fn is_imported(&self) -> bool {
        let targets = self.targets.as_ref().unwrap().clone();
        let apps = ArrApp::from_config(&self.app_data.config);

        let targets = targets
            .into_iter()
            .filter(|t| t.target_type == TargetType::File)
            .collect::<Vec<DownloadTarget>>();
        // .map(|t| t.to.clone())
        // .collect::<Vec<String>>();

        let mut results = Vec::<bool>::new();
        for target in targets {
            let mut service_results = vec![];
            for app in &apps {
                let service_result = match app.check_imported(&target).await {
                    Ok(r) => r,
                    Err(e) => {
                        error!("Error retrieving history from {}: {}", app, e);
                        false
                    }
                };
                if service_result {
                    info!("{}: found imported by {}", &target, app);
                }
                service_results.push(service_result)
            }
            results.push(service_results.into_iter().any(|x| x));
        }
        // Check if all targets have been imported
        results.into_iter().all(|x| x)
    }

    pub async fn get_download_targets(&self) -> Result<Vec<DownloadTarget>> {
        info!("{}: generating targets", self);
        let default = "0000".to_string();
        let hash = self.hash.as_ref().unwrap_or(&default).as_str();
        recurse_download_targets(&self.app_data, self.file_id.unwrap(), hash, None, true).await
    }

    pub fn get_top_level(&self) -> DownloadTarget {
        self.targets
            .clone()
            .unwrap()
            .into_iter()
            .find(|t| t.top_level)
            .unwrap()
    }

    pub fn from(app_data: Data<AppData>, transfer: &PutIOTransfer) -> Self {
        let default = &"Unknown".to_string();
        let name = transfer.name.as_ref().unwrap_or(default);
        Self {
            transfer_id: transfer.id,
            name: name.clone(),
            file_id: transfer.file_id,
            targets: None,
            hash: transfer.hash.clone(),
            app_data,
        }
    }
}

impl Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let default = "0000".to_string();
        let hash = &self.hash.as_ref().unwrap_or(&default)[..4];
        let s = format!("[{}: {}]", hash, self.name).cyan();
        write!(f, "{s}")
    }
}

#[async_recursion]
async fn recurse_download_targets(
    app_data: &Data<AppData>,
    file_id: i64,
    hash: &str,
    override_base_path: Option<String>,
    top_level: bool,
) -> Result<Vec<DownloadTarget>> {
    let base_path = override_base_path.unwrap_or(app_data.config.download_directory.clone());
    let mut targets = Vec::<DownloadTarget>::new();
    let response = putio::list_files(&app_data.config.putio.api_key, file_id).await?;
    let to = Path::new(&base_path)
        .join(&response.parent.name)
        .to_string_lossy()
        .to_string();

    match response.parent.file_type.as_str() {
        "FOLDER" => {
            if !app_data
                .config
                .skip_directories
                .contains(&response.parent.name.to_lowercase())
            {
                let new_base_path = to.clone();

                targets.push(DownloadTarget {
                    from: None,
                    target_type: TargetType::Directory,
                    to,
                    top_level,
                    transfer_hash: hash.to_string(),
                    media_type: None,
                });

                for file in response.files {
                    targets.append(
                        &mut recurse_download_targets(
                            app_data,
                            file.id,
                            hash,
                            Some(new_base_path.clone()),
                            false,
                        )
                        .await?,
                    );
                }
            }
        }
        "VIDEO" | "AUDIO" => {
            // Get download URL for file
            let url = putio::url(&app_data.config.putio.api_key, response.parent.id).await?;
            targets.push(DownloadTarget {
                from: Some(url),
                target_type: TargetType::File,
                to,
                top_level,
                transfer_hash: hash.to_string(),
                media_type: MediaType::from_file_type_str(response.parent.file_type.as_str()),
            });
        }
        _ => {
            debug!(
                "{}: skipping filetype {}",
                response.parent.name,
                response.parent.file_type.as_str()
            );
        }
    }

    Ok(targets)
}

#[derive(Clone)]
pub enum TransferMessage {
    QueuedForDownload(Transfer),
    Downloaded(Transfer),
    Imported(Transfer),
}

#[derive(PartialEq, Debug, Serialize, Deserialize, Clone)]
pub enum MediaType {
    Audio,
    Video,
}

impl MediaType {
    pub fn from_file_type_str(file_type: &str) -> Option<Self> {
        match file_type {
            "AUDIO" => Some(Self::Audio),
            "VIDEO" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadTarget {
    pub from: Option<String>,
    pub to: String,
    pub target_type: TargetType,
    pub top_level: bool,
    pub transfer_hash: String,
    pub media_type: Option<MediaType>,
}

impl Display for DownloadTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = &self.transfer_hash.as_str()[..4];
        let s = format!("[{}: {}]", hash, self.to).magenta();
        write!(f, "{s}")
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum TargetType {
    Directory,
    File,
}

// Check for new putio transfers and if they qualify, send them on for download
pub async fn produce_transfers(app_data: Data<AppData>, tx: Sender<TransferMessage>) -> Result<()> {
    let putio_check_interval = std::time::Duration::from_secs(app_data.config.polling_interval);
    let mut seen = Vec::<u64>::new();

    info!("Checking unfinished transfers");
    // We only need to check if something has been imported. Just by looking at the filesystem we
    // can't determine if a transfer has been imported and removed or hasn't been downloaded.
    // This avoids downloading a tranfer that has already been imported. In case there is a download,
    // but it wasn't (completely) imported, we will attempt a (partial) download. Files that have
    // been completed downloading will be skipped.
    for putio_transfer in &putio::list_transfers(&app_data.config.putio.api_key)
        .await?
        .transfers
    {
        let name = putio_transfer.name.clone().unwrap_or("??".to_string());
        let mut transfer = Transfer::from(app_data.clone(), putio_transfer);
        if putio_transfer.is_downloadable() {
            info!("Getting download target for {name}");
            let targets = transfer.get_download_targets().await;
            if targets.is_err() {
                // For example, if the user trashed the file in Putio
                warn!("Could not get target for {name}");
                continue;
            }
            transfer.targets = Some(targets?);
            if transfer.is_imported().await {
                info!("{}: already imported", &transfer);
                seen.push(transfer.transfer_id);
                tx.send(TransferMessage::Imported(transfer)).await?;
            } else {
                info!("{}: not imported yet", &transfer);
            }
        }
    }
    info!("Done checking for unfinished transfers. Starting to monitor transfers.");

    // Set the start time
    let mut start = std::time::Instant::now();

    loop {
        if let Ok(list_transfer_response) =
            putio::list_transfers(&app_data.config.putio.api_key).await
        {
            for putio_transfer in &list_transfer_response.transfers {
                if seen.contains(&putio_transfer.id) || !putio_transfer.is_downloadable() {
                    continue;
                }
                let transfer = Transfer::from(app_data.clone(), putio_transfer);

                info!("{}: ready for download", transfer);
                tx.send(TransferMessage::QueuedForDownload(transfer))
                    .await?;
                seen.push(putio_transfer.id);
            }

            // Remove any transfers from seen that are not in the active transfers
            let active_ids: Vec<u64> = list_transfer_response
                .transfers
                .iter()
                .map(|t| t.id)
                .collect();
            seen.retain(|t| active_ids.contains(t));

            // Log status when 60 seconds have passed since last time
            if start.elapsed().as_secs() >= 60 {
                info!(
                    "Active transfers: {}",
                    list_transfer_response.transfers.len()
                );
                list_transfer_response
                    .transfers
                    .iter()
                    .for_each(|t| info!("  {}", Transfer::from(app_data.clone(), t)));

                start = std::time::Instant::now();
            }

            sleep(putio_check_interval).await;
        } else {
            warn!("List put.io transfers failed. Retrying..");
            continue;
        };
    }
}
