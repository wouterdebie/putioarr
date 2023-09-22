use crate::{
    download_system::{
        download::{DownloadDoneStatus, DownloadTargetMessage},
        transfer::Transfer,
    },
    AppData, services::putio,
};
use actix_web::web::Data;
use anyhow::Result;
use async_channel::{Receiver, Sender};
use log::info;
use std::{fs, time::Duration};
use tokio::{fs::metadata, time::sleep};

use super::transfer::TransferMessage;

#[derive(Clone)]
pub struct Worker {
    _id: usize,
    app_data: Data<AppData>,
    tx: Sender<TransferMessage>,
    rx: Receiver<TransferMessage>,
    dtx: Sender<DownloadTargetMessage>,
}

impl Worker {
    pub fn start(
        id: usize,
        app_data: Data<AppData>,
        tx: Sender<TransferMessage>,
        rx: Receiver<TransferMessage>,
        dtx: Sender<DownloadTargetMessage>,
    ) {
        let s = Self {
            _id: id,
            app_data,
            tx,
            rx,
            dtx,
        };
        let _join_handle = actix_rt::spawn(async move { s.work().await });
    }

    async fn work(&self) -> Result<()> {
        loop {
            let msg = self.rx.recv().await?;
            let app_data = self.app_data.clone();
            match msg {
                TransferMessage::QueuedForDownload(t) => {
                    info!("Downloading {}", t.name);
                    let targets = t.get_download_targets().await?;
                    // Create a communications channel for the download worker to communicate status back.
                    let done_channels: &Vec<(
                        Sender<DownloadDoneStatus>,
                        Receiver<DownloadDoneStatus>,
                    )> = &targets.iter().map(|_| async_channel::unbounded()).collect();

                    for (i, target) in targets.iter().enumerate() {
                        let (done_tx, _) = done_channels[i].clone();
                        self.dtx
                            .send(DownloadTargetMessage {
                                download_target: target.clone(),
                                tx: done_tx,
                            })
                            .await?;
                    }

                    // Wait for all the workers having sent back their status.
                    for (_, done_rx) in done_channels {
                        done_rx.recv().await?;
                    }

                    info!("Downloading {} done", t.name);
                    self.tx
                        .send(TransferMessage::Downloaded(Transfer {
                            targets: Some(targets),
                            ..t
                        }))
                        .await?;
                }
                TransferMessage::Downloaded(m) => {
                    info!("Watching imports {}", m.name);
                    let tx = self.tx.clone();
                    actix_rt::spawn(async { watch_for_import(app_data, tx, m).await });
                }
                TransferMessage::Imported(m) => {
                    info!("Watching seeding {}", m.name);

                    actix_rt::spawn(async { watch_seeding(app_data, m).await });
                }
            }
        }
    }
}

async fn watch_for_import(
    app_data: Data<AppData>,
    tx: Sender<TransferMessage>,
    transfer: Transfer,
) -> Result<()> {
    loop {
        if transfer.is_imported().await {
            info!("{} has been imported. Deleting files.", transfer.name);
            let top_level_path = transfer.get_top_level();

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
            let m = transfer.clone();
            tx.send(TransferMessage::Imported(m)).await?;

            break;
        }
        sleep(Duration::from_secs(app_data.config.polling_interval)).await;
    }
    info!("{} deleted. Stop watching.", transfer.name);
    Ok(())
}

async fn watch_seeding(app_data: Data<AppData>, transfer: Transfer) -> Result<()> {
    loop {
        let putio_transfer =
            putio::get_transfer(&app_data.config.putio.api_key, transfer.transfer_id)
                .await?
                .transfer;
        if putio_transfer.status != "SEEDING" {
            info!(
                "put.io transfer {} is no longer seeding. Removing..",
                putio_transfer.name
            );

            putio::remove_transfer(&app_data.config.putio.api_key, transfer.transfer_id).await?;
            match putio::remove_files(&app_data.config.putio.api_key, transfer.file_id.unwrap())
                .await
            {
                Ok(_) => {
                    info!("Removed remote files for {}", transfer.name);
                }
                Err(_) => {
                    info!(
                        "Unable to remove remove files for {}. Ignoring.",
                        transfer.name
                    );
                }
            };
            break;
        }
    }
    sleep(Duration::from_secs(app_data.config.polling_interval)).await;

    info!("{} removed. Stop watching.", transfer.name);
    Ok(())
}
