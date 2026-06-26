use super::transfer::{DownloadTarget, TargetType};
use crate::AppData;
use actix_web::web::Data;
use anyhow::{bail, Context, Result};
use async_channel::{Receiver, Sender};
use colored::*;
use file_owner::PathExt;
use futures::StreamExt;
use log::{error, info, warn};
use nix::unistd::Uid;
use std::time::Duration;
use std::{fs, path::Path};

/// How long to wait for a download request to start returning a response (the
/// connect + response-headers phase). Bounds it so a server that accepts the
/// connection but then stalls can't hang the worker; the retry loop resumes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the next chunk of a download stream before treating it
/// as stalled (and erroring so the retry loop resumes).
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

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

            // Download the target
            let done_status = match download_target(&self.app_data, &dtm.download_target).await {
                Ok(_) => DownloadDoneStatus::Success,
                Err(_) => DownloadDoneStatus::Failed,
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
    match target.target_type {
        TargetType::Directory => {
            if !Path::new(&target.to).exists() {
                fs::create_dir(&target.to)?;
                if Uid::effective().is_root() {
                    target.to.clone().set_owner(app_data.config.uid)?;
                }
                info!("{}: directory created", &target);
            }
        }
        TargetType::File => {
            // Delete file if already exists
            if !Path::new(&target.to).exists() {
                info!("{}: download {}", &target, "started".yellow());
                match fetch(target, app_data.config.uid, &app_data.http).await {
                    Ok(_) => info!("{}: download {}", &target, "succeeded".green()),
                    Err(e) => {
                        error!("{}: download {}: {}", &target, "failed".red(), e);
                        bail!(e)
                    }
                };
            } else {
                info!("{}: already exists", &target);
            }
        }
    }
    Ok(())
}

async fn fetch(target: &DownloadTarget, uid: u32, client: &reqwest::Client) -> Result<()> {
    let tmp_path = format!("{}.downloading", &target.to);

    // Make sure the destination directory exists. A File target can be processed
    // before its parent Directory target, and external cleanup may have removed
    // an emptied folder, so create it here rather than failing with "No such
    // file or directory" (which the retry loop below would otherwise spin on).
    if let Some(parent) = Path::new(&tmp_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // A single stream can stall (put.io stops sending mid-download). Rather than
    // restarting the whole file, retry and resume from the bytes already on disk
    // using a Range request. put.io serves `206 Partial Content`, so each retry
    // picks up where the previous one stopped until the file is complete.
    const MAX_ATTEMPTS: u32 = 20;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match fetch_attempt(target, &tmp_path, client).await {
            Ok(()) => break,
            Err(e) if attempt < MAX_ATTEMPTS => {
                warn!("{}: download attempt {} failed ({}), resuming", target, attempt, e);
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            Err(e) => bail!("download failed after {} attempts: {}", attempt, e),
        }
    }

    if Uid::effective().is_root() {
        tmp_path.clone().set_owner(uid)?;
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
    client: &reqwest::Client,
) -> Result<()> {
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
        tokio::fs::OpenOptions::new().append(true).open(tmp_path).await?
    } else {
        tokio::fs::File::create(tmp_path).await?
    };

    let mut byte_stream = response.bytes_stream();
    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, byte_stream.next()).await {
            Ok(Some(item)) => {
                tokio::io::copy(&mut item?.as_ref(), &mut tmp_file).await?;
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
