use std::{fs, os::unix::prelude::MetadataExt, path::Path, time::Duration};

use crate::{appdata::AppData, putio};
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use async_recursion::async_recursion;
use file_owner::PathExt;
use futures::StreamExt;
use log::info;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    fs::{metadata, File},
    io::{AsyncReadExt, AsyncWriteExt},
    time::sleep,
};

pub async fn start_download_system(app_data: Data<AppData>) -> Result<()> {
    let (sender, receiver) = async_channel::unbounded();
    let data = app_data.clone();
    let tx = sender.clone();
    actix_rt::spawn(async { produce_transfers(data, tx).await });

    let size = 10;

    for id in 0..size {
        let data = app_data.clone();
        let tx = sender.clone();
        let rx = receiver.clone();

        Worker::start(id, data, tx, rx);
    }

    Ok(())
}

// Check for new putio transfers and if they qualify, send them on for download
async fn produce_transfers(app_data: Data<AppData>, tx: Sender<MessageType>) -> Result<()> {
    // Load state
    let mut state = read_state(&app_data.state_file).await?;

    let ten_seconds = std::time::Duration::from_secs(10);
    loop {
        let transfers = putio::list_transfers(&app_data.api_token).await?.transfers;

        if !transfers.is_empty() {
            info!("Active transfers: {}", transfers.len());
        }
        for transfer in &transfers {
            if state.contains(&transfer.id) || transfer.file_id.is_none() {
                continue;
            }

            let msg = Message {
                transfer_id: transfer.id,
                name: transfer.name.clone(),
                file_id: transfer.file_id.unwrap(),
                targets: None,
            };

            info!("Queueing {} for download", msg.name);
            tx.send(MessageType::QueuedForDownload(msg)).await?;
            state.push(transfer.id);
            save_state(&app_data.state_file, &state).await?;
        }

        // Remove any transfers from seen that are not in the active transfers
        let active_ids: Vec<u64> = transfers.iter().map(|t| t.id).collect();
        let before = state.len();
        state.retain(|t| active_ids.contains(t));
        if before != state.len() {
            save_state(&app_data.state_file, &state).await?;
        }

        sleep(ten_seconds).await;
    }
}

async fn read_state(state_file: &String) -> Result<Vec<u64>, anyhow::Error> {
    let state = if Path::new(state_file).exists() {
        info!("Restoring state from {}..", state_file);
        let mut file = File::open(state_file).await?;
        let mut data = String::new();
        file.read_to_string(&mut data).await?;
        serde_json::from_str(&data).unwrap()
    } else {
        Vec::<u64>::new()
    };
    Ok(state)
}

async fn save_state(state_file: &String, state: impl Serialize) -> Result<()> {
    info!("Saving state to {}..", state_file);
    let mut file = File::create(state_file).await.unwrap();
    file.write_all(json!(state).to_string().as_bytes())
        .await
        .unwrap();
    file.flush().await.unwrap();
    Ok(())
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
    targets: Option<Vec<DownloadedTarget>>,
}

#[derive(Debug, Clone)]
enum MessageType {
    QueuedForDownload(Message),
    Downloaded(Message),
    Imported(Message),
}

async fn watch_for_import(tx: Sender<MessageType>, message: Message) -> Result<()> {
    loop {
        if all_imported(&message.targets.as_ref().unwrap().clone()) {
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

fn all_imported(targets: &[DownloadedTarget]) -> bool {
    let results = targets
        .iter()
        .filter(|t| t.target_type == DownloadType::File)
        .map(|t| t.path_name.clone())
        .map(|s| {
            let meta = fs::metadata(s).unwrap();
            let links = meta.nlink();
            links > 1
        })
        .collect::<Vec<bool>>();
    results.into_iter().all(|x| x)
}

fn get_top_level(targets: &[DownloadedTarget]) -> &str {
    targets
        .iter()
        .find(|t| t.top_level)
        .unwrap()
        .path_name
        .as_str()
}

async fn watch_seeding(app_data: Data<AppData>, message: Message) -> Result<()> {
    loop {
        let transfer = putio::get_transfer(&app_data.api_token, message.transfer_id)
            .await?
            .transfer;
        if transfer.status != "SEEDING" {
            info!(
                "Transfer {} is no longer seeding. Removing..",
                transfer.name
            );
            putio::remove_transfer(&app_data.api_token, message.transfer_id).await?;
            break;
        }
    }
    sleep(Duration::from_secs(10)).await;

    info!("{} removed. Stop watching.", message.name);
    Ok(())
}

/// Downloads all files belonging to a file_id
async fn download(app_data: &Data<AppData>, file_id: u64) -> Result<Vec<DownloadedTarget>> {
    let operations = get_download_operations(app_data, file_id, &app_data.download_dir).await?;
    let mut targets = Vec::<DownloadedTarget>::new();
    for (i, op) in operations.into_iter().enumerate() {
        match op.download_type {
            DownloadType::Directory => {
                if !Path::new(&op.target).exists() {
                    fs::create_dir(&op.target)?;
                    op.target.clone().set_owner(app_data.uid)?;
                }
            }
            DownloadType::File => {
                // Delete file if already exists
                if Path::new(&op.target).exists() {
                    fs::remove_file(&op.target)?;
                }
                fetch_url(
                    op.url.context("No URL found")?,
                    op.target.clone(),
                    app_data.uid,
                )
                .await?
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

#[async_recursion]
async fn get_download_operations(
    app_data: &Data<AppData>,
    file_id: u64,
    base_path: &str,
) -> Result<Vec<DownloadOperation>> {
    let mut operations = Vec::<DownloadOperation>::new();
    let response = putio::list_files(&app_data.api_token, file_id).await?;

    match response.parent.file_type.as_str() {
        "FOLDER" => {
            let target = Path::new(&app_data.download_dir)
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
                    .append(&mut get_download_operations(app_data, file.id, &new_base).await?);
            }
        }
        "VIDEO" => {
            // Get download URL for file
            let url = putio::url(&app_data.api_token, response.parent.id).await?;

            let target = Path::new(&base_path)
                .join(&response.parent.name)
                .to_string_lossy()
                .to_string();

            operations.push(DownloadOperation {
                url: Some(url),
                download_type: DownloadType::File,
                target,
            });
        }
        _ => {}
    }

    Ok(operations)
}

async fn fetch_url(url: String, target: String, uid: u32) -> Result<()> {
    info!("Downloading {} started...", &target);
    let tmp_path = format!("{}.downloading", &target);
    let mut tmp_file = tokio::fs::File::create(&tmp_path).await?;
    let mut byte_stream = reqwest::get(&url).await?.bytes_stream();

    while let Some(item) = byte_stream.next().await {
        tokio::io::copy(&mut item?.as_ref(), &mut tmp_file).await?;
    }
    tmp_path.clone().set_owner(uid)?;
    fs::rename(&tmp_path, &target)?;
    info!("Downloading {} finished...", &target);
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
    targets: Option<Vec<DownloadedTarget>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DownloadedTarget {
    pub path_name: String,
    pub target_type: DownloadType,
    pub top_level: bool,
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
