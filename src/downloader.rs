use crate::{
    putio::{self, PutIOTransfer},
    AppData,
};
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use async_recursion::async_recursion;
use file_owner::PathExt;
use futures::StreamExt;
use log::info;
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::prelude::MetadataExt, path::Path, time::Duration};
use tokio::{fs::metadata, time::sleep};

static NUM_WORKERS: usize = 10;

pub async fn start_download_system(app_data: Data<AppData>) -> Result<()> {
    let (sender, receiver) = async_channel::unbounded();
    let data = app_data.clone();
    let tx = sender.clone();
    actix_rt::spawn(async { produce_transfers(data, tx).await });

    for id in 0..NUM_WORKERS {
        let data = app_data.clone();
        let tx = sender.clone();
        let rx = receiver.clone();
        Worker::start(id, data, tx, rx);
    }
    Ok(())
}

// Check for new putio transfers and if they qualify, send them on for download
async fn produce_transfers(app_data: Data<AppData>, tx: Sender<MessageType>) -> Result<()> {
    let ten_seconds = std::time::Duration::from_secs(10);
    let mut seen = Vec::<u64>::new();

    // Restore state
    // if seeding or completed
    //     if is_imported => send Imported
    //     else if is_downloaded => send Downloaded
    //     else => send QueueForDownload
    //     add to seen
    info!("Restoring state");
    for transfer in &putio::list_transfers(&app_data.config.putio.api_key)
        .await?
        .transfers
    {
        let msg: Message = transfer.into();
        if transfer.is_downloadable() {
            // Get download targets
        }
    }

    loop {
        let transfers = putio::list_transfers(&app_data.config.putio.api_key)
            .await?
            .transfers;

        if !transfers.is_empty() {
            info!("Active transfers: {}", transfers.len());
        }
        for transfer in &transfers {
            if seen.contains(&transfer.id) || transfer.is_downloadable() {
                continue;
            }

            let msg: Message = transfer.into();

            info!("Queueing {} for download", msg.name);
            tx.send(MessageType::QueuedForDownload(msg)).await?;
            seen.push(transfer.id);
        }

        // Remove any transfers from seen that are not in the active transfers
        let active_ids: Vec<u64> = transfers.iter().map(|t| t.id).collect();
        seen.retain(|t| active_ids.contains(t));

        sleep(ten_seconds).await;
    }
}

#[derive(Clone)]
struct Worker {
    _id: usize,
    app_data: Data<AppData>,
    tx: Sender<MessageType>,
    rx: Receiver<MessageType>,
}

impl Worker {
    pub fn start(
        id: usize,
        app_data: Data<AppData>,
        tx: Sender<MessageType>,
        rx: Receiver<MessageType>,
    ) {
        let s = Self {
            _id: id,
            app_data,
            tx,
            rx,
        };
        let _join_handle = actix_rt::spawn(async move { s.work().await });
    }

    async fn work(&self) -> Result<()> {
        loop {
            let msg = self.rx.recv().await?;
            match msg {
                MessageType::QueuedForDownload(m) => {
                    info!("Downloading {}", m.name);
                    let targets = Some(download(&self.app_data, m.file_id).await?);
                    info!("Downloading {} done", m.name);
                    self.tx
                        .send(MessageType::Downloaded(Message { targets, ..m }))
                        .await?;
                }
                MessageType::Downloaded(m) => {
                    info!("Watching imports {}", m.name);
                    let tx = self.tx.clone();
                    actix_rt::spawn(async { watch_for_import(tx, m).await });
                }
                MessageType::Imported(m) => {
                    info!("Watching seeding {}", m.name);
                    let data = self.app_data.clone();
                    actix_rt::spawn(async { watch_seeding(data, m).await });
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Message {
    name: String,
    file_id: u64,
    transfer_id: u64,
    targets: Option<Vec<DownloadTarget>>,
}

impl From<&PutIOTransfer> for Message {
    fn from(transfer: &PutIOTransfer) -> Self {
        Self {
            transfer_id: transfer.id,
            name: transfer.name.clone(),
            file_id: transfer.file_id.unwrap(),
            targets: None,
        }
    }
}

#[derive(Debug, Clone)]
enum MessageType {
    QueuedForDownload(Message),
    Downloaded(Message),
    Imported(Message),
}

async fn watch_for_import(tx: Sender<MessageType>, message: Message) -> Result<()> {
    loop {
        if is_imported(&message) {
            info!("{} has been imported. Deleting files.", message.name);
            let targets = &message.targets.clone().unwrap();
            let top_level_path = get_top_level(targets);

            match metadata(&top_level_path).await {
                Ok(m) if m.is_dir() => {
                    info!("Deleting everyting in {}", &top_level_path);
                    fs::remove_dir_all(top_level_path).unwrap();
                }
                Ok(m) if m.is_file() => {
                    info!("Deleting {}", &top_level_path);
                    fs::remove_file(top_level_path).unwrap();
                }
                Ok(_) | Err(_) => {
                    panic!("Don't know how to handle {}", &top_level_path)
                }
            };
            let m = message.clone();
            tx.send(MessageType::Imported(m)).await?;

            break;
        }
        sleep(Duration::from_secs(10)).await;
    }
    info!("{} deleted. Stop watching.", message.name);
    Ok(())
}

fn is_imported(message: &Message) -> bool {
    all_targets_hardlinked(&message.targets.as_ref().unwrap().clone())
}

fn all_targets_hardlinked(targets: &[DownloadTarget]) -> bool {
    let results = targets
        .iter()
        .filter(|t| t.target_type == DownloadType::File)
        .map(|t| t.to.clone())
        .map(|s| {
            let meta = fs::metadata(s).unwrap();
            let links = meta.nlink();
            links > 1
        })
        .collect::<Vec<bool>>();
    results.into_iter().all(|x| x)
}

fn get_top_level(targets: &[DownloadTarget]) -> &str {
    targets.iter().find(|t| t.top_level).unwrap().to.as_str()
}

async fn watch_seeding(app_data: Data<AppData>, message: Message) -> Result<()> {
    loop {
        let transfer = putio::get_transfer(&app_data.config.putio.api_key, message.transfer_id)
            .await?
            .transfer;
        if transfer.status != "SEEDING" {
            info!(
                "Transfer {} is no longer seeding. Removing..",
                transfer.name
            );
            putio::remove_transfer(&app_data.config.putio.api_key, message.transfer_id).await?;
            break;
        }
    }
    sleep(Duration::from_secs(10)).await;

    info!("{} removed. Stop watching.", message.name);
    Ok(())
}

/// Downloads all files belonging to a file_id
async fn download(app_data: &Data<AppData>, file_id: u64) -> Result<Vec<DownloadTarget>> {
    let targets = get_download_targets(app_data, file_id, None, true).await?;
    download_targets(&targets, app_data).await?;

    // TODO: Cleanup when stuff goes bad
    Ok(targets)
}

async fn download_targets(targets: &Vec<DownloadTarget>, app_data: &Data<AppData>) -> Result<()> {
    for target in targets {
        match target.target_type {
            DownloadType::Directory => {
                if !Path::new(&target.to).exists() {
                    fs::create_dir(&target.to)?;
                    target.to.clone().set_owner(app_data.config.uid)?;
                }
            }
            DownloadType::File => {
                // Delete file if already exists
                if Path::new(&target.to).exists() {
                    fs::remove_file(&target.to)?;
                }
                let url = target.from.clone().context("No URL found")?;
                fetch(&url, &target.to, app_data.config.uid).await?
            }
        }
    }
    Ok(())
}

#[async_recursion]
async fn get_download_targets(
    app_data: &Data<AppData>,
    file_id: u64,
    override_base_path: Option<String>,
    top_level: bool,
) -> Result<Vec<DownloadTarget>> {
    let base_path = override_base_path.unwrap_or(app_data.config.download_directory.clone());
    let mut targets = Vec::<DownloadTarget>::new();
    let response = putio::list_files(&app_data.config.putio.api_key, file_id).await?;

    match response.parent.file_type.as_str() {
        "FOLDER" => {
            let target = Path::new(&app_data.config.download_directory)
                .join(&response.parent.name)
                .to_string_lossy()
                .to_string();

            targets.push(DownloadTarget {
                from: None,
                target_type: DownloadType::Directory,
                to: target.clone(),
                top_level,
            });
            let new_base = format!("{}/", &target);
            for file in response.files {
                targets.append(
                    &mut get_download_targets(app_data, file.id, Some(new_base.clone()), false)
                        .await?,
                );
            }
        }
        "VIDEO" => {
            // Get download URL for file
            let url = putio::url(&app_data.config.putio.api_key, response.parent.id).await?;

            let to = Path::new(&base_path)
                .join(&response.parent.name)
                .to_string_lossy()
                .to_string();

            targets.push(DownloadTarget {
                from: Some(url),
                target_type: DownloadType::File,
                to,
                top_level,
            });
        }
        _ => {}
    }

    Ok(targets)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DownloadTarget {
    pub from: Option<String>,
    pub to: String,
    pub target_type: DownloadType,
    pub top_level: bool,
}

async fn fetch(url: &str, to: &str, uid: u32) -> Result<()> {
    info!("Downloading {} started...", &to);
    let tmp_path = format!("{}.downloading", &to);
    let mut tmp_file = tokio::fs::File::create(&tmp_path).await?;
    let mut byte_stream = reqwest::get(url).await?.bytes_stream();

    while let Some(item) = byte_stream.next().await {
        tokio::io::copy(&mut item?.as_ref(), &mut tmp_file).await?;
    }
    tmp_path.clone().set_owner(uid)?;
    fs::rename(&tmp_path, to)?;
    info!("Downloading {} finished...", &to);
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub(crate) enum DownloadStatus {
    New,
    Downloading,
    Downloaded,
    Imported,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Download {
    hash: String,
    pub status: DownloadStatus,
    file_id: Option<u64>,
    targets: Option<Vec<DownloadTarget>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum DownloadType {
    Directory,
    File,
}
