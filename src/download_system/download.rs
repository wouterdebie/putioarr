use crate::AppData;
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use file_owner::PathExt;
use futures::StreamExt;
use log::info;
use nix::unistd::Uid;
use std::{fs, path::Path};

use super::transfer::{DownloadTarget, TargetType};

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
            dtm.tx.send(done_status).await?;
        }
    }
}

async fn download_target(app_data: &Data<AppData>, target: &DownloadTarget) -> Result<()> {
    match target.target_type {
        TargetType::Directory => {
            if !Path::new(&target.to).exists() {
                info!("Creating dir {}", &target.to);
                fs::create_dir(&target.to)?;
                if Uid::effective().is_root() {
                    target.to.clone().set_owner(app_data.config.uid)?;
                }
            }
        }
        TargetType::File => {
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
