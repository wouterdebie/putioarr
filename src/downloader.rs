use crate::{putio, AppData};
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_recursion::async_recursion;
use file_owner::PathExt;
use futures::StreamExt;
use log::info;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, metadata},
    os::unix::prelude::MetadataExt,
    path::Path,
};
use tokio::time::sleep;

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) enum DownloadStatus {
    Downloading,
    Downloaded,
    Imported,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Download {
    hash: String,
    pub status: DownloadStatus,
    file_id: u64,
    targets: Option<Vec<DownloadedTarget>>,
}

pub(crate) async fn start_downloader_task(app_data: Data<AppData>) -> Result<()> {
    let api_token = &app_data.api_token;
    let download_dir = &app_data.download_dir;
    let ten_seconds = std::time::Duration::from_secs(10);

    // Never wait when starting up
    app_data.tx.send(true).await?;

    loop {
        info!("Waiting to start..");
        // Wait for anything to happen
        app_data.rx.recv().await?;
        info!("Downloader started");

        loop {
            let transfers = putio::list_transfers(api_token).await?.transfers;
            if transfers.is_empty() {
                info!("Downloader stopped");
                break;
            }

            for transfer in &transfers {
                // Don't even bother with unfinished transfers
                if !transfer.userfile_exists {
                    continue;
                }

                // We know about this download and maybe need to update status
                if app_data.downloads.lock().await.contains_key(&transfer.hash) {
                    // Don't bother with imported transfers that are still seeding, but delete if they're not
                    {
                        let mut m = app_data.downloads.lock().await;
                        let d = m.get_mut(&transfer.hash).unwrap();
                        if d.status == DownloadStatus::Imported && transfer.status == "SEEDING" {
                            continue;
                        } else if d.status == DownloadStatus::Imported {
                            info!("Transfer {} done seeding. Removing..", &transfer.name);
                            putio::remove_transfer(api_token, transfer.id).await?;
                            m.remove(&transfer.hash);
                            drop(m);
                            app_data.save().await?;
                            continue;
                        }
                    }

                    info!("Checking if '{}' has been imported already", &transfer.name);

                    // Get all the targets that are files
                    let targets: Vec<String> = {
                        let m = app_data.downloads.lock().await;
                        let d = m.get(&transfer.hash).unwrap();
                        d.targets
                            .as_ref()
                            .unwrap()
                            .iter()
                            .filter(|t| t.target_type == DownloadType::File)
                            .map(|t| t.path_name.clone())
                            .collect()
                    };

                    // Check if any of the files have more than 1 hardling. If so, this means
                    // that Sonarr has imported the file. Thus, we can delete the import.
                    let imported = targets
                        .iter()
                        .filter_map(|s| {
                            let meta = fs::metadata(s).unwrap();
                            let links = meta.nlink();
                            if links > 1 {
                                Some(true)
                            } else {
                                None
                            }
                        })
                        .any(|x| x);

                    if imported {
                        info!("'{}' has been imported. Deleting files.", &transfer.name);
                        // find top level target

                        let top_level_path = app_data
                            .downloads
                            .lock()
                            .await
                            .get(&transfer.hash)
                            .unwrap()
                            .targets
                            .as_ref()
                            .unwrap()
                            .iter()
                            .find(|d| d.top_level)
                            .unwrap()
                            .path_name
                            .clone();

                        match metadata(&top_level_path) {
                            Ok(m) if m.is_dir() => {
                                info!("Deleting everyting in {}", &top_level_path);
                                fs::remove_dir_all(top_level_path).unwrap();
                            }
                            Ok(m) if m.is_file() => {
                                info!("Deleting {}", &top_level_path);
                                fs::remove_file(&top_level_path).unwrap();
                            }
                            Ok(_) | Err(_) => {
                                panic!("Don't know how to handle {}", &top_level_path)
                            }
                        };

                        {
                            let mut m = app_data.downloads.lock().await;
                            let d = m.get_mut(&transfer.hash).unwrap();
                            d.status = DownloadStatus::Imported;
                        }
                        app_data.save().await?;
                    }
                } else {
                    // We don't know this transfer yet, but we should download it
                    info!("Downloading transfer '{}'", transfer.hash);
                    let d = Download {
                        hash: transfer.hash.clone(),
                        status: DownloadStatus::Downloading,
                        file_id: transfer.file_id.unwrap(),
                        targets: None,
                    };
                    let file_id = d.file_id;
                    app_data
                        .downloads
                        .lock()
                        .await
                        .insert(transfer.hash.clone(), d);
                    let targets = download(api_token, file_id, download_dir).await?;
                    {
                        let mut m = app_data.downloads.lock().await;
                        let d = m.get_mut(&transfer.hash).unwrap();
                        d.targets = Some(targets);
                        d.status = DownloadStatus::Downloaded;
                    }
                    app_data.save().await?;
                }
            }
            // Clean up manually cancelled transfers
            {
                let transfer_keys: Vec<String> = transfers.into_iter().map(|t| t.hash).collect();
                let mut downloads = app_data.downloads.lock().await;
                downloads.retain(|k, _| {
                    if !transfer_keys.contains(k) {
                        info!("Cleaning up {}", k);
                        false
                    } else {
                        true
                    }
                });
            }
            sleep(ten_seconds).await;
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DownloadedTarget {
    pub path_name: String,
    pub target_type: DownloadType,
    pub top_level: bool,
}

/// Downloads all files belonging to a file_id
pub(crate) async fn download(
    api_token: &str,
    file_id: u64,
    download_dir: &str,
) -> Result<Vec<DownloadedTarget>> {
    let listing = get_download_operations(api_token, file_id, download_dir).await?;
    let mut targets = Vec::<DownloadedTarget>::new();
    for (i, op) in listing.into_iter().enumerate() {
        match op.download_type {
            DownloadType::Directory => {
                if !Path::new(&op.target).exists() {
                    fs::create_dir(&op.target)?;
                    op.target.clone().set_owner(1000)?;
                }
            }
            DownloadType::File => {
                // Delete file if already exists
                if Path::new(&op.target).exists() {
                    fs::remove_file(&op.target)?;
                }
                fetch_url(op.url.context("No URL found")?, op.target.clone()).await?
            }
        }
        targets.push(DownloadedTarget {
            path_name: op.target,
            target_type: op.download_type,
            top_level: i == 0, // DownloadOperations are sorted, since we start at the top-level
        })
    }
    // TODO: Cleanup when stuff goes bad
    Ok(targets)
}

async fn fetch_url(url: String, target: String) -> Result<()> {
    // dbg!(url, target);
    info!("Downloading {} started...", &target);
    let tmp_path = format!("{}.downloading", &target);
    let mut tmp_file = tokio::fs::File::create(&tmp_path).await?;
    let mut byte_stream = reqwest::get(&url).await?.bytes_stream();

    while let Some(item) = byte_stream.next().await {
        tokio::io::copy(&mut item?.as_ref(), &mut tmp_file).await?;
    }
    tmp_path.clone().set_owner(1000)?;
    fs::rename(&tmp_path, &target)?;
    info!("Downloading {} finished...", &target);
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum DownloadType {
    Directory,
    File,
}

#[derive(Debug)]
struct DownloadOperation {
    pub url: Option<String>,
    pub download_type: DownloadType,
    pub target: String,
}

#[async_recursion]
async fn get_download_operations(
    api_token: &str,
    file_id: u64,
    download_dir: &str,
) -> Result<Vec<DownloadOperation>> {
    let mut operations = Vec::<DownloadOperation>::new();
    let response = putio::list_files(api_token, file_id).await?;
    if response.parent.content_type == "application/x-directory" {
        let target = Path::new(download_dir)
            .join(&response.parent.name)
            .to_string_lossy()
            .to_string();

        operations.push(DownloadOperation {
            url: None,
            download_type: DownloadType::Directory,
            target: target.clone(),
        });
        let new_base = format!("{}/", &target);
        for file in response.files {
            operations
                .append(&mut get_download_operations(api_token, file.id, new_base.as_str()).await?)
        }
    } else {
        // Get download URL for file
        let url = putio::url(api_token, response.parent.id).await?;

        let target = Path::new(download_dir)
            .join(&response.parent.name)
            .to_string_lossy()
            .to_string();

        operations.push(DownloadOperation {
            url: Some(url),
            download_type: DownloadType::File,
            target,
        })
    }
    Ok(operations)
}
