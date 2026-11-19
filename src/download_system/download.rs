use super::transfer::{DownloadTarget, TargetType};
use crate::{state::StateManager, AppData};
use actix_web::web::Data;
use anyhow::{bail, Context, Result};
use async_channel::{Receiver, Sender};
use colored::*;
use file_owner::PathExt;
use futures::StreamExt;
use log::{error, info, warn};
use nix::unistd::Uid;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{fs, path::Path};

/// How long to wait for a download request to start returning a response (the
/// connect + response-headers phase). Bounds it so a server that accepts the
/// connection but then stalls can't hang the worker; the retry loop resumes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the next chunk of a download stream before treating it
/// as stalled (and erroring so the retry loop resumes).
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often a running download checks whether the *arr removed its transfer.
const REMOVED_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The error a download stops with when the *arr removed its transfer
/// (torrent-remove) while it was queued or running (issue #52).
#[derive(Debug)]
pub struct TransferRemoved;

impl std::fmt::Display for TransferRemoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "transfer was removed")
    }
}

impl std::error::Error for TransferRemoved {}

async fn ensure_not_removed(state: &StateManager, hash: &str) -> Result<()> {
    if state.is_removed(hash).await {
        bail!(TransferRemoved);
    }
    Ok(())
}

#[derive(Clone)]
pub struct Worker {
    _id: usize,
    app_data: Data<AppData>,
    drx: Receiver<DownloadTargetMessage>,
}

impl Worker {
    pub fn start(id: usize, app_data: Data<AppData>, drx: Receiver<DownloadTargetMessage>) {
        let s = Self {
            _id: id,
            app_data,
            drx,
        };

        let _join_handle = actix_rt::spawn(async move { s.work().await });
    }
    async fn work(&self) -> Result<()> {
        loop {
            // Wait for a DownloadTarget
            let dtm = self.drx.recv().await?;

            // Download the target. Log why a target failed here, in one place,
            // so "not all targets downloaded" is never the only clue (#51).
            let target = &dtm.download_target;
            let done_status = match download_target(&self.app_data, target).await {
                Ok(_) => DownloadDoneStatus::Success,
                Err(e) if e.is::<TransferRemoved>() => {
                    info!("{}: stopped, the transfer was removed", target);
                    DownloadDoneStatus::Failed
                }
                Err(e) => {
                    error!("{}: {}: {:#}", target, "failed".red(), e);
                    DownloadDoneStatus::Failed
                }
            };
            // Reporting status can fail if the orchestration worker that queued
            // this target is gone. Don't let that take this worker down too —
            // log and keep serving other downloads (issue #34).
            if let Err(e) = dtm.tx.send(done_status).await {
                warn!("download worker: could not report status (receiver gone): {e}");
            }
        }
    }
}

async fn download_target(app_data: &Data<AppData>, target: &DownloadTarget) -> Result<()> {
    ensure_not_removed(&app_data.state, &target.transfer_hash).await?;
    match target.target_type {
        TargetType::Directory => {
            // create_dir_all, not create_dir: the category folder may not exist
            // yet on a first grab, and a file target of the same transfer may
            // create this folder at the same moment. Either used to fail the
            // folder, and so the transfer, which then sat at 99% (#51).
            let created = create_dir_all_owned(Path::new(&target.to), app_data.config.uid)
                .context("could not create directory")?;
            if created {
                info!("{}: directory created", target);
            }
        }
        TargetType::File => {
            // Shared per-transfer counter of bytes pulled to local disk, so the
            // *arr can show second-stage progress (issue #5).
            let counter = app_data
                .state
                .local_byte_counter(&target.transfer_hash)
                .await;
            // Delete file if already exists
            if !Path::new(&target.to).exists() {
                info!("{}: download {}", target, "started".yellow());
                // A failure is logged by the worker, with its cause.
                fetch(target, app_data, &counter).await?;
                info!("{}: download {}", target, "succeeded".green());
            } else {
                // Already fully downloaded (e.g. a resumed run after the files
                // were pulled but not yet imported): still count its bytes so the
                // aggregate progress across the transfer's files stays accurate.
                if let Ok(m) = fs::metadata(&target.to) {
                    counter.fetch_add(m.len(), Ordering::Relaxed);
                }
                info!("{}: already exists", target);
            }
        }
    }
    Ok(())
}

/// Creates `dir` and any missing parents, like `create_dir_all`, and returns
/// whether `dir` had to be created. When running as root, each folder created
/// here is handed to `uid`, so a category folder made on the way isn't left
/// owned by root.
fn create_dir_all_owned(dir: &Path, uid: u32) -> Result<bool> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.exists()).collect();
    if missing.is_empty() {
        return Ok(false);
    }
    fs::create_dir_all(dir)?;
    if Uid::effective().is_root() {
        for d in missing {
            d.set_owner(uid)?;
        }
    }
    Ok(true)
}

async fn fetch(target: &DownloadTarget, app_data: &AppData, counter: &AtomicU64) -> Result<()> {
    let state = &app_data.state;
    let hash = target.transfer_hash.as_str();
    let tmp_path = format!("{}.downloading", target.to);

    // Bytes of *this file* currently reflected in the shared transfer counter.
    // Tracked across attempts so a resume can account for bytes already on disk
    // exactly once, and a from-scratch restart (a server that ignores our Range
    // and returns 200) can undo this file's contribution before re-counting the
    // re-downloaded bytes (issue #5).
    let mut counted: u64 = 0;

    // Make sure the destination directory exists. A File target can be processed
    // before its parent Directory target, and external cleanup may have removed
    // an emptied folder, so create it here rather than failing with "No such
    // file or directory" (which the retry loop below would otherwise spin on).
    if let Some(parent) = Path::new(&tmp_path).parent() {
        create_dir_all_owned(parent, app_data.config.uid)
            .context("could not create directory")?;
    }

    // A single stream can stall (put.io stops sending mid-download). Rather than
    // restarting the whole file, retry and resume from the bytes already on disk
    // using a Range request. put.io serves `206 Partial Content`, so each retry
    // picks up where the previous one stopped until the file is complete.
    const MAX_ATTEMPTS: u32 = 20;
    let mut attempt = 0;
    loop {
        attempt += 1;
        ensure_not_removed(state, hash).await?;
        match fetch_attempt(target, &tmp_path, app_data, counter, &mut counted).await {
            Ok(()) => break,
            Err(e) if e.is::<TransferRemoved>() => return Err(e),
            Err(e) if attempt < MAX_ATTEMPTS => {
                warn!("{}: download attempt {} failed ({}), resuming", target, attempt, e);
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            Err(e) => bail!("download failed after {} attempts: {}", attempt, e),
        }
    }

    // Don't put the file in place for a transfer removed meanwhile: its local
    // copy may just have been deleted (#52).
    ensure_not_removed(state, hash).await?;

    if Uid::effective().is_root() {
        tmp_path.clone().set_owner(app_data.config.uid)?;
    }

    fs::rename(&tmp_path, &target.to)?;

    Ok(())
}

/// Downloads `target` into `tmp_path`, resuming from whatever is already on disk
/// via a Range request. Returns Ok only when the stream finished cleanly; a
/// stall (no data for 60s) or a non-success status returns an error so the
/// caller can retry and resume.
async fn fetch_attempt(
    target: &DownloadTarget,
    tmp_path: &str,
    app_data: &AppData,
    counter: &AtomicU64,
    counted: &mut u64,
) -> Result<()> {
    let client = &app_data.http;
    let existing = tokio::fs::metadata(tmp_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let url = target.from.clone().context("No URL found")?;
    let mut req = client.get(url);
    if existing > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={}-", existing));
    }
    // Bound the request itself, not just the connect. put.io can accept the
    // connection and then stall before sending response headers; without this
    // timeout `send()` blocks forever, parking the download worker (and, via the
    // blocked done channel, every orchestration worker) until the whole process
    // stops pulling. On timeout we error so the retry loop can resume (issue #32).
    let response =
        match tokio::time::timeout(REQUEST_TIMEOUT, req.send()).await {
            Ok(r) => r?,
            Err(_) => bail!(
                "timed out after {:?} waiting for response headers from {}",
                REQUEST_TIMEOUT,
                target.from.as_deref().unwrap_or("<no url>")
            ),
        };
    let status = response.status();
    if !(status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT) {
        bail!("HTTP {}", status);
    }

    // If the server honored the Range request (206), append to the partial file;
    // otherwise it returned the whole file (200), so start it over.
    let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT && existing > 0;
    let mut tmp_file = if resumed {
        // Count the bytes already on disk exactly once (they aren't re-streamed).
        if *counted < existing {
            counter.fetch_add(existing - *counted, Ordering::Relaxed);
            *counted = existing;
        }
        tokio::fs::OpenOptions::new().append(true).open(tmp_path).await?
    } else {
        // Starting over from scratch: the file is about to be truncated, so drop
        // whatever this file previously added to the counter before re-counting
        // the re-downloaded bytes below.
        if *counted > 0 {
            counter.fetch_sub(*counted, Ordering::Relaxed);
            *counted = 0;
        }
        tokio::fs::File::create(tmp_path).await?
    };

    let mut byte_stream = response.bytes_stream();
    let mut last_removed_check = Instant::now();
    loop {
        // Stop pulling a large file soon after the *arr removes its transfer,
        // rather than finishing a download nobody wants (#52).
        if last_removed_check.elapsed() >= REMOVED_CHECK_INTERVAL {
            ensure_not_removed(&app_data.state, &target.transfer_hash).await?;
            last_removed_check = Instant::now();
        }
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, byte_stream.next()).await {
            Ok(Some(item)) => {
                let chunk = item?;
                tokio::io::copy(&mut chunk.as_ref(), &mut tmp_file).await?;
                // Lock-free running total for progress reporting (issue #5).
                let n = chunk.len() as u64;
                counter.fetch_add(n, Ordering::Relaxed);
                *counted += n;
            }
            Ok(None) => break,
            Err(_) => bail!("stalled: no data received for {:?}", STREAM_IDLE_TIMEOUT),
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DownloadTargetMessage {
    pub download_target: DownloadTarget,
    pub tx: Sender<DownloadDoneStatus>,
}

#[derive(Debug, Clone)]
pub enum DownloadDoneStatus {
    Success,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::app_data_in;
    use std::path::PathBuf;

    fn temp_base(test: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("putioarr-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn folder_target(to: &Path) -> DownloadTarget {
        DownloadTarget {
            from: None,
            to: to.to_string_lossy().into_owned(),
            target_type: TargetType::Directory,
            top_level: true,
            transfer_hash: "abcd".repeat(10),
            media_type: None,
        }
    }

    #[actix_rt::test]
    async fn folder_is_created_when_the_category_folder_does_not_exist_yet() {
        // Issue #51: <download_directory>/radarr didn't exist on the first grab.
        let base = temp_base("missing-category");
        let app_data = app_data_in(base.to_str().unwrap());
        let folder = base.join("radarr").join("Some.Movie.2024");

        download_target(&app_data, &folder_target(&folder)).await.unwrap();
        assert!(folder.is_dir());

        // Already there (e.g. a file target of the same transfer made it
        // first): still a success.
        download_target(&app_data, &folder_target(&folder)).await.unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[actix_rt::test]
    async fn targets_of_a_removed_transfer_are_skipped() {
        let base = temp_base("removed");
        let app_data = app_data_in(base.to_str().unwrap());
        let folder = base.join("radarr").join("Some.Movie.2024");
        let target = folder_target(&folder);
        app_data.state.mark_removed(&target.transfer_hash).await;

        let err = download_target(&app_data, &target).await.unwrap_err();
        assert!(err.is::<TransferRemoved>());
        assert!(!folder.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn create_dir_all_owned_reports_whether_it_created_the_folder() {
        let base = temp_base("create-owned");
        let dir = base.join("a").join("b");
        assert!(create_dir_all_owned(&dir, 1000).unwrap());
        assert!(dir.is_dir());
        assert!(!create_dir_all_owned(&dir, 1000).unwrap());
        fs::remove_dir_all(&base).unwrap();
    }
}
