use crate::{
    download_system::{
        download::{DownloadDoneStatus, DownloadTargetMessage},
        transfer::Transfer,
    },
    services::putio,
    AppData,
};
use actix_web::web::Data;
use anyhow::Result;
use async_channel::{Receiver, Sender};
use colored::*;
use log::{info, warn};
use std::{
    fs,
    time::{Duration, Instant},
};
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
                    // Handle each transfer in a way that can't take the worker
                    // down: a `?` here used to bubble up and end work(), which
                    // (silently) killed the worker and, via the dropped done
                    // channels, cascaded to the others until everything stalled
                    // (issue #34). Log and carry on instead.
                    if let Err(e) = self.handle_queued(t).await {
                        warn!("download orchestration error (worker continuing): {}", e);
                    }
                }
                TransferMessage::Downloaded(t) => {
                    let tx = self.tx.clone();
                    actix_rt::spawn(async { watch_for_import(app_data, tx, t).await });
                }
                TransferMessage::Imported(t) => {
                    actix_rt::spawn(async { watch_seeding(app_data, t).await });
                }
            }
        }
    }

    /// Downloads all of a transfer's targets and, on full success, marks it
    /// complete and forwards it for import. Returns Err on any failure so the
    /// caller can log it without ending the worker (see issue #34).
    async fn handle_queued(&self, t: Transfer) -> Result<()> {
        info!("{}: download {}", t, "started".yellow());
        // Reuse targets computed when the transfer was discovered if present —
        // watch-folder orphans precompute them with the correct base dir
        // (get_download_targets_in), which a plain get_download_targets() here
        // would not reproduce (no persisted state) and would mis-route. Only
        // regenerate for normal transfers, which don't carry them.
        let targets = match t.targets.clone() {
            Some(ts) => ts,
            None => t.get_download_targets().await?,
        };
        // No targets means nothing to download; don't let the `all(...)` check
        // below pass vacuously and mark the transfer complete.
        if targets.is_empty() {
            warn!("{}: no downloadable targets, skipping", t);
            return Ok(());
        }
        // A status channel per target for the download workers to report back.
        let done_channels: Vec<(Sender<DownloadDoneStatus>, Receiver<DownloadDoneStatus>)> =
            targets.iter().map(|_| async_channel::unbounded()).collect();

        for (i, target) in targets.iter().enumerate() {
            let done_tx = done_channels[i].0.clone();
            self.dtx
                .send(DownloadTargetMessage {
                    download_target: target.clone(),
                    tx: done_tx,
                })
                .await?;
        }

        // Wait for all the workers having sent back their status.
        let mut all_downloaded = vec![];
        for (_, done_rx) in &done_channels {
            all_downloaded.push(done_rx.recv().await?);
        }

        if all_downloaded
            .iter()
            .all(|d| matches!(d, DownloadDoneStatus::Success))
        {
            info!("{}: download {}", t, "done".blue());
            // The files now exist locally, so it's safe to report this transfer
            // as complete to the *arr (see issue #16).
            self.app_data.state.mark_local_complete(t.transfer_id).await;
            self.tx
                .send(TransferMessage::Downloaded(Transfer {
                    targets: Some(targets),
                    ..t
                }))
                .await?;
        } else {
            warn!("{}: not all targets downloaded", t);
            // Drop a failed orphan from tracking so a later watch-folder scan
            // can retry it instead of it being suppressed forever (issue #34).
            if t.is_orphan {
                if let Some(file_id) = t.file_id {
                    self.app_data.state.remove_orphan(file_id).await;
                }
            }
        }
        Ok(())
    }
}

async fn watch_for_import(
    app_data: Data<AppData>,
    tx: Sender<TransferMessage>,
    transfer: Transfer,
) -> Result<()> {
    info!("{}: watching imports", transfer);
    // Give up watching after this long so a transfer that never fully imports
    // (e.g. one containing a sample the *arr won't import) can't loop forever
    // and accumulate until downloads stall (see issue #30). Genuine imports are
    // detected within a poll or two, so the default is generous.
    let import_timeout = Duration::from_secs(app_data.config.import_timeout_secs);
    let started = Instant::now();
    loop {
        if transfer.is_imported().await {
            info!("{}: imported", transfer);
            let top_level_target = transfer.get_top_level();

            match metadata(&top_level_target.to).await {
                Ok(m) if m.is_dir() => {
                    fs::remove_dir_all(&top_level_target.to).unwrap();
                    info!("{}: deleted", &top_level_target);
                }
                Ok(m) if m.is_file() => {
                    fs::remove_file(&top_level_target.to).unwrap();
                    info!("{}: deleted", &top_level_target);
                }
                Ok(_) | Err(_) => {
                    panic!("{}: no idea how to handle", &top_level_target)
                }
            };
            // An orphan has no put.io transfer to remove or seed, so finish it
            // here directly instead of routing an Imported message through a
            // worker (which may be busy downloading and never pick it up),
            // deleting the now-imported file from put.io (issue #34).
            if transfer.is_orphan {
                if let Some(file_id) = transfer.file_id {
                    match putio::delete_file(&app_data.config.putio.api_key, file_id).await {
                        Ok(_) => info!("{}: deleted orphan from put.io", transfer),
                        Err(e) => {
                            warn!("{}: failed to delete orphan from put.io: {}", transfer, e)
                        }
                    }
                    app_data.state.remove_orphan(file_id).await;
                }
            } else {
                let m = transfer.clone();
                tx.send(TransferMessage::Imported(m)).await?;
            }

            info!("{}: removed", transfer);
            break;
        }
        if app_data.config.import_timeout_secs > 0 && started.elapsed() > import_timeout {
            warn!(
                "{}: still not imported after {:?}; giving up watching. It likely contains a \
                 file the *arr won't import (e.g. a sample outside a skip_directories folder), \
                 so its local/put.io copies won't be cleaned up automatically.",
                transfer, import_timeout
            );
            break;
        }
        sleep(Duration::from_secs(app_data.config.polling_interval)).await;
    }
    Ok(())
}

async fn watch_seeding(app_data: Data<AppData>, transfer: Transfer) -> Result<()> {
    info!("{}: watching seeding", transfer);
    loop {
        let putio_transfer =
            putio::get_transfer(&app_data.config.putio.api_key, transfer.transfer_id)
                .await?
                .transfer;
        if putio_transfer.status != "SEEDING" {
            info!("{}: stopped seeding", transfer);
            putio::remove_transfer(&app_data.config.putio.api_key, transfer.transfer_id).await?;
            info!("{}: removed from put.io", transfer);
            match putio::delete_file(&app_data.config.putio.api_key, transfer.file_id.unwrap())
                .await
            {
                Ok(_) => {
                    info!("{}: deleted remote files", transfer);
                }
                Err(_) => {
                    warn!("{}: unable to delete remote files", transfer);
                }
            };
            break;
        }
        sleep(Duration::from_secs(app_data.config.polling_interval)).await;
    }

    info!("{}: done seeding", transfer);
    Ok(())
}
