use crate::{
    arr,
    putio::{self, PutIOTransfer},
    AppData,
};
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use async_recursion::async_recursion;
use file_owner::PathExt;
use futures::{stream, StreamExt};
use log::{debug, info};
use nix::unistd::Uid;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::Duration};
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
    let ten_seconds = std::time::Duration::from_secs(app_data.config.polling_interval);
    let mut seen = Vec::<u64>::new();

    info!("Checking if there are unfinished transfers.");
    // We only need to check if something has been imported. Just by looking at the filesystem we
    // can't determine if a transfer has been imported and removed or hasn't been downloaded.
    // This avoids downloading a tranfer that has already been imported. In case there is a download,
    // but it wasn't (completely) imported, we will attempt a (partial) download. Files that have
    // been completed downloading will be skipped.
    for transfer in &putio::list_transfers(&app_data.config.putio.api_key)
        .await?
        .transfers
    {
        let mut message: Message = transfer.into();
        if transfer.is_downloadable() {
            let targets = get_download_targets(&app_data, message.file_id.unwrap()).await?;
            message.targets = Some(targets);
            if is_imported(&app_data, &message).await {
                info!("{} already imported. Notifying of import.", &message.name);
                seen.push(message.transfer_id);
                tx.send(MessageType::Imported(message)).await?;
            } else {
                info!("{} not imported yet. Continuing as normal.", &message.name);
            }
        }
    }
    info!("Done checking for unfinished transfers.");
    loop {
        let transfers = putio::list_transfers(&app_data.config.putio.api_key)
            .await?
            .transfers;

        if !transfers.is_empty() {
            debug!("Active transfers: {:?}", transfers);
        }
        for transfer in &transfers {
            if seen.contains(&transfer.id) || !transfer.is_downloadable() {
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
            let app_data = self.app_data.clone();
            match msg {
                MessageType::QueuedForDownload(m) => {
                    info!("Downloading {}", m.name);
                    let targets = Some(download(&self.app_data, m.file_id.unwrap()).await?);
                    info!("Downloading {} done", m.name);
                    self.tx
                        .send(MessageType::Downloaded(Message { targets, ..m }))
                        .await?;
                }
                MessageType::Downloaded(m) => {
                    info!("Watching imports {}", m.name);
                    let tx = self.tx.clone();
                    actix_rt::spawn(async { watch_for_import(app_data, tx, m).await });
                }
                MessageType::Imported(m) => {
                    info!("Watching seeding {}", m.name);

                    actix_rt::spawn(async { watch_seeding(app_data, m).await });
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Message {
    name: String,
    file_id: Option<u64>,
    transfer_id: u64,
    targets: Option<Vec<DownloadTarget>>,
}

impl From<&PutIOTransfer> for Message {
    fn from(transfer: &PutIOTransfer) -> Self {
        Self {
            transfer_id: transfer.id,
            name: transfer.name.clone(),
            file_id: transfer.file_id,
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

async fn watch_for_import(
    app_data: Data<AppData>,
    tx: Sender<MessageType>,
    message: Message,
) -> Result<()> {
    loop {
        if is_imported(&app_data, &message).await {
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
        sleep(Duration::from_secs(app_data.config.polling_interval)).await;
    }
    info!("{} deleted. Stop watching.", message.name);
    Ok(())
}

async fn is_imported(app_data: &Data<AppData>, message: &Message) -> bool {
    let targets = &message.targets.as_ref().unwrap().clone();
    // all_targets_hardlinked(targets);
    all_targets_imported(app_data, targets).await
}

async fn all_targets_imported(app_data: &Data<AppData>, targets: &[DownloadTarget]) -> bool {
    let mut check_services = Vec::<(&str, String, String)>::new();
    if let Some(a) = &app_data.config.sonarr {
        check_services.push(("Sonarr", a.url.clone(), a.api_key.clone()))
    }
    if let Some(a) = &app_data.config.radarr {
        check_services.push(("Radarr", a.url.clone(), a.api_key.clone()))
    }

    let paths = targets
        .iter()
        .filter(|t| t.target_type == DownloadType::File)
        .map(|t| t.to.clone())
        .collect::<Vec<String>>();

    let mut results = Vec::<bool>::new();
    for path in paths {
        info!("Checking if {} has been imported yet", &path);
        let mut service_results = vec![];
        for (service_name, url, key) in &check_services {
            let service_result = arr::is_imported(&path, key, url).await.unwrap();
            if service_result {
                info!("Found {} imported by {}", &path, service_name);
            }
            service_results.push(service_result)
        }
        // Check if ANY of the service_results are true and put the outcome in results
        results.push(service_results.into_iter().any(|x| x));
    }
    // Check if all targets have been imported
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
            match putio::remove_files(&app_data.config.putio.api_key, message.file_id.unwrap())
                .await
            {
                Ok(_) => {
                    info!("Removed remote files for {}", message.name);
                }
                Err(_) => {
                    info!(
                        "Unable to remove remove files for {}. Ignoring.",
                        message.name
                    );
                }
            };
            break;
        }
    }
    sleep(Duration::from_secs(app_data.config.polling_interval)).await;

    info!("{} removed. Stop watching.", message.name);
    Ok(())
}

/// Downloads all files belonging to a file_id
async fn download(app_data: &Data<AppData>, file_id: u64) -> Result<Vec<DownloadTarget>> {
    let targets = get_download_targets(app_data, file_id).await?;
    download_targets(&targets, app_data).await?;

    // TODO: Cleanup when stuff goes bad
    Ok(targets)
}

async fn download_targets(targets: &Vec<DownloadTarget>, app_data: &Data<AppData>) -> Result<()> {
    stream::iter(targets)
        .for_each_concurrent(4, |target| async move {
            download_target(app_data, target).await.unwrap();
        })
        .await;
    Ok(())
}

async fn download_target(app_data: &Data<AppData>, target: &DownloadTarget) -> Result<()> {
    match target.target_type {
        DownloadType::Directory => {
            if !Path::new(&target.to).exists() {
                info!("Creating dir {}", &target.to);
                fs::create_dir(&target.to)?;
                if Uid::effective().is_root() {
                    target.to.clone().set_owner(app_data.config.uid)?;
                }
            }
        }
        DownloadType::File => {
            // Delete file if already exists
            if !Path::new(&target.to).exists() {
                let url = target.from.clone().context("No URL found")?;
                fetch(&url, &target.to, app_data.config.uid).await?
            } else {
                info!("{} already exists. Skipping download.", &target.to);
                // fs::remove_file(&target.to)?;
            }
        }
    }
    Ok(())
}

async fn get_download_targets(
    app_data: &Data<AppData>,
    file_id: u64,
) -> Result<Vec<DownloadTarget>> {
    recurse_download_targets(app_data, file_id, None, true).await
}

#[async_recursion]
async fn recurse_download_targets(
    app_data: &Data<AppData>,
    file_id: u64,
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
                    target_type: DownloadType::Directory,
                    to,
                    top_level,
                });

                for file in response.files {
                    targets.append(
                        &mut recurse_download_targets(
                            app_data,
                            file.id,
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
    if Uid::effective().is_root() {
        tmp_path.clone().set_owner(uid)?;
    }

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
