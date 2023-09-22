use crate::{
    services::{
        arr,
        putio::{self, PutIOTransfer},
    },
    AppData,
};
use actix_web::web::Data;
use anyhow::Result;
use async_channel::Sender;
use async_recursion::async_recursion;
use colored::*;
use log::info;
use serde::{Deserialize, Serialize};
use std::{fmt::Display, path::Path};
use tokio::time::sleep;

#[derive(Clone)]
pub struct Transfer {
    pub name: String,
    pub file_id: Option<u64>,
    pub hash: Option<String>,
    pub transfer_id: u64,
    pub targets: Option<Vec<DownloadTarget>>,
    pub app_data: Data<AppData>,
}

impl Transfer {
    pub async fn is_imported(&self) -> bool {
        let targets = self.targets.as_ref().unwrap().clone();
        let mut check_services = Vec::<(&str, String, String)>::new();
        if let Some(a) = &self.app_data.config.sonarr {
            check_services.push(("Sonarr", a.url.clone(), a.api_key.clone()))
        }
        if let Some(a) = &self.app_data.config.radarr {
            check_services.push(("Radarr", a.url.clone(), a.api_key.clone()))
        }

        let targets = targets
            .into_iter()
            .filter(|t| t.target_type == TargetType::File)
            .collect::<Vec<DownloadTarget>>();
        // .map(|t| t.to.clone())
        // .collect::<Vec<String>>();

        let mut results = Vec::<bool>::new();
        for target in targets {
            let mut service_results = vec![];
            for (service_name, url, key) in &check_services {
                let service_result = arr::check_imported(&target.to, key, url).await.unwrap();
                if service_result {
                    info!(
                        "{}: found imported by {}",
                        &target,
                        service_name.bright_blue()
                    );
                }
                service_results.push(service_result)
            }
            // Check if ANY of the service_results are true and put the outcome in results
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
        let hash = &self.hash.as_ref().unwrap()[..4];
        let s = format!("[{}: {}]", hash, self.name).cyan();
        write!(f, "{s}")
    }
}

#[async_recursion]
async fn recurse_download_targets(
    app_data: &Data<AppData>,
    file_id: u64,
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
        "VIDEO" => {
            // Get download URL for file
            let url = putio::url(&app_data.config.putio.api_key, response.parent.id).await?;
            targets.push(DownloadTarget {
                from: Some(url),
                target_type: TargetType::File,
                to,
                top_level,
                transfer_hash: hash.to_string(),
            });
        }
        _ => {}
    }

    Ok(targets)
}

#[derive(Clone)]
pub enum TransferMessage {
    QueuedForDownload(Transfer),
    Downloaded(Transfer),
    Imported(Transfer),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadTarget {
    pub from: Option<String>,
    pub to: String,
    pub target_type: TargetType,
    pub top_level: bool,
    pub transfer_hash: String,
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
    let ten_seconds = std::time::Duration::from_secs(app_data.config.polling_interval);
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
        let mut transfer = Transfer::from(app_data.clone(), putio_transfer);
        if putio_transfer.is_downloadable() {
            let targets = transfer.get_download_targets().await?;
            transfer.targets = Some(targets);
            if transfer.is_imported().await {
                info!("{}: already imported", &transfer);
                seen.push(transfer.transfer_id);
                tx.send(TransferMessage::Imported(transfer)).await?;
            } else {
                info!("{}: not imported yet", &transfer);
            }
        }
    }
    info!("Done checking for unfinished transfers");
    loop {
        let putio_transfers = putio::list_transfers(&app_data.config.putio.api_key)
            .await?
            .transfers;

        for putio_transfer in &putio_transfers {
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
        let active_ids: Vec<u64> = putio_transfers.iter().map(|t| t.id).collect();
        seen.retain(|t| active_ids.contains(t));

        sleep(ten_seconds).await;
    }
}
