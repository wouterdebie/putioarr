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
use std::time::{Duration, Instant};
use tokio::{fs::metadata, time::sleep};

use super::transfer::{TargetType, TransferMessage};

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
                    match self.handle_queued(t.clone()).await {
                        Ok(true) => {}
                        Ok(false) => self.after_failed_download(t).await,
                        Err(e) => {
                            warn!("{}: download orchestration error (worker continuing): {:#}", t, e);
                            self.after_failed_download(t).await;
                        }
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

    /// After a transfer's download failed: a watch-folder orphan is dropped so
    /// a later scan picks it up again (issue #34). Any other transfer is queued
    /// again after a backoff. It stays on the producer's seen list for as long
    /// as it's on put.io, so without this it sat at 99% until a restart (#51).
    async fn after_failed_download(&self, t: Transfer) {
        if t.is_orphan {
            if let Some(file_id) = t.file_id {
                self.app_data.state.remove_orphan(file_id).await;
            }
            return;
        }
        if self.app_data.state.is_removed(t.hash_key()).await {
            info!("{}: removed by the *arr, not retrying", t);
            return;
        }
        let delay = retry_delay(t.retries);
        warn!("{}: retrying the download in {}", t, format_delay(delay));
        let app_data = self.app_data.clone();
        let tx = self.tx.clone();
        actix_rt::spawn(async move { requeue_after(app_data, tx, t, delay).await });
    }

    /// Downloads all of a transfer's targets and, on full success, marks it
    /// complete and forwards it for import. Returns Ok(false) when some targets
    /// failed, and Err on other failures, so the caller can retry it and log
    /// without ending the worker (see issue #34).
    async fn handle_queued(&self, t: Transfer) -> Result<bool> {
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
            return Ok(true);
        }
        // Only folders and no files: still report it complete so the *arr shows
        // why it can't import it, but it is never treated as imported, so its
        // put.io copy stays.
        if !targets.iter().any(|t| t.target_type == TargetType::File) {
            warn!(
                "{}: no video or audio files to download (archives only? see \
                 download_archives); the *arr won't find anything to import, so it stays \
                 on put.io",
                t
            );
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

        let failed = all_downloaded
            .iter()
            .filter(|d| !matches!(d, DownloadDoneStatus::Success))
            .count();
        if failed == 0 {
            info!("{}: download {}", t, "done".blue());
            // The files now exist locally, so it's safe to report this transfer
            // as complete to the *arr (see issue #16).
            self.app_data.state.mark_local_complete(t.transfer_id).await;
            // Local download finished; the progress counter is no longer read
            // (torrent-get reports completion via local_complete now), so drop
            // it to keep the map bounded (issue #5).
            if let Some(hash) = &t.hash {
                self.app_data.state.clear_local_bytes(hash).await;
            }
            self.tx
                .send(TransferMessage::Downloaded(Transfer {
                    targets: Some(targets),
                    ..t
                }))
                .await?;
            Ok(true)
        } else {
            warn!(
                "{}: not all targets downloaded ({} of {} failed)",
                t,
                failed,
                all_downloaded.len()
            );
            Ok(false)
        }
    }
}

/// How long to wait before downloading a failed transfer again: 1 minute,
/// doubling with each retry, up to 30 minutes.
pub(super) fn retry_delay(retries: u32) -> Duration {
    const FIRST: Duration = Duration::from_secs(60);
    const MAX: Duration = Duration::from_secs(30 * 60);
    FIRST.saturating_mul(2u32.saturating_pow(retries)).min(MAX)
}

pub(super) fn format_delay(d: Duration) -> String {
    match d.as_secs() {
        60 => "1 minute".to_string(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{s}s"),
    }
}

/// Queues a failed transfer for download again after `delay`, as long as it's
/// still on put.io and the *arr hasn't removed it. Files already on disk are
/// skipped, so this finishes what's missing, like a restart does.
async fn requeue_after(
    app_data: Data<AppData>,
    tx: Sender<TransferMessage>,
    t: Transfer,
    mut delay: Duration,
) {
    loop {
        sleep(delay).await;
        if app_data.state.is_removed(t.hash_key()).await {
            info!("{}: removed by the *arr, not retrying", t);
            return;
        }
        match putio::list_transfers(&app_data.config.putio.api_key).await {
            Ok(r) if r.transfers.iter().any(|p| p.id == t.transfer_id) => {
                info!("{}: retrying the download", t);
                // Regenerate the targets, which also fetches fresh download URLs.
                let retry = Transfer {
                    retries: t.retries + 1,
                    targets: None,
                    ..t
                };
                if tx.send(TransferMessage::QueuedForDownload(retry)).await.is_err() {
                    warn!("could not queue a retry: the orchestration workers are gone");
                }
                return;
            }
            Ok(_) => {
                info!("{}: no longer on put.io, not retrying", t);
                return;
            }
            Err(e) => {
                // Don't give up on a put.io hiccup; check again later.
                delay = retry_delay(u32::MAX);
                warn!(
                    "{}: could not check put.io before retrying ({}); trying again in {}",
                    t,
                    e,
                    format_delay(delay)
                );
            }
        }
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
        // The *arr dropped it from its queue, so it will never import it.
        if app_data.state.is_removed(transfer.hash_key()).await {
            info!("{}: removed by the *arr, stopped watching imports", transfer);
            break;
        }
        if transfer.is_imported().await {
            info!("{}: imported", transfer);
            let top_level_target = transfer.get_top_level();

            // Remove the local copy now that it's imported, unless the user
            // opted to keep downloads (e.g. the *arr copies imports and they
            // want to keep the original).
            if app_data.config.keep_downloads {
                info!("{}: keeping local download (keep_downloads)", top_level_target);
            } else {
                // Use async fs and log failures instead of unwrap/panic: a failed
                // delete (permissions, a concurrent removal, the path already gone)
                // shouldn't take the process down, and blocking fs here could stall
                // the runtime under load.
                let result = match metadata(&top_level_target.to).await {
                    Ok(m) if m.is_dir() => {
                        tokio::fs::remove_dir_all(&top_level_target.to).await
                    }
                    Ok(m) if m.is_file() => tokio::fs::remove_file(&top_level_target.to).await,
                    // Neither a file nor a dir (e.g. already removed): nothing to do.
                    Ok(_) | Err(_) => Ok(()),
                };
                match result {
                    Ok(_) => info!("{}: deleted", top_level_target),
                    Err(e) => warn!("{}: failed to delete local copy: {}", top_level_target, e),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_doubles_up_to_half_an_hour() {
        let minutes: Vec<u64> = (0..7).map(|r| retry_delay(r).as_secs() / 60).collect();
        assert_eq!(minutes, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(30 * 60));
    }

    #[test]
    fn delays_read_naturally() {
        assert_eq!(format_delay(Duration::from_secs(60)), "1 minute");
        assert_eq!(format_delay(Duration::from_secs(16 * 60)), "16 minutes");
    }
}
