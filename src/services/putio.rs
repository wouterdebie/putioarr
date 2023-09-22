use std::collections::HashMap;

use actix_web::web::Data;
use anyhow::Result;
use async_channel::Sender;
use log::{info, debug};
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::{AppData, download_system::transfer::{TransferMessage, Transfer}};

#[derive(Debug, Serialize, Deserialize)]
pub struct PutIOAccountInfo {
    pub username: String,
    pub mail: String,
    pub account_active: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PutIOAccountResponse {
    pub info: PutIOAccountInfo,
}

#[derive(Debug, Deserialize)]
pub struct PutIOTransfer {
    pub id: u64,
    pub hash: Option<String>,
    pub name: String,
    pub size: Option<i64>,
    pub downloaded: Option<i64>,
    pub finished_at: Option<String>,
    pub estimated_time: Option<u64>,
    pub status: String,
    pub started_at: String,
    pub error_message: Option<String>,
    pub file_id: Option<u64>,
    pub percent_done: Option<u16>,
    pub userfile_exists: bool,
}

impl PutIOTransfer {
    pub fn is_downloadable(&self) -> bool {
        self.file_id.is_some()
    }
}

#[derive(Debug, Deserialize)]
pub struct AccountInfoResponse {
    pub info: Info,
}

#[derive(Debug, Deserialize)]
pub struct Info {
    pub user_id: u32,
    pub username: String,
    pub mail: String,
    pub monthly_bandwidth_usage: u64,
}

#[derive(Debug, Deserialize)]
pub struct ListTransferResponse {
    pub transfers: Vec<PutIOTransfer>,
}

#[derive(Debug, Deserialize)]
pub struct GetTransferResponse {
    pub transfer: PutIOTransfer,
}

/// Returns the user's transfers.
pub async fn list_transfers(api_token: &str) -> Result<ListTransferResponse> {
    let client = reqwest::Client::new();
    let response: ListTransferResponse = client
        .get("https://api.put.io/v2/transfers/list")
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?
        .json()
        .await?;
    Ok(response)
}

pub async fn get_transfer(api_token: &str, id: u64) -> Result<GetTransferResponse> {
    let client = reqwest::Client::new();
    let response: GetTransferResponse = client
        .get(format!("https://api.put.io/v2/transfers/{}", id))
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?
        .json()
        .await?;
    Ok(response)
}

pub async fn remove_transfer(api_token: &str, id: u64) -> Result<()> {
    let client = reqwest::Client::new();
    let form = multipart::Form::new().text("transfer_ids", id.to_string());
    client
        .post("https://api.put.io/v2/transfers/remove")
        .multipart(form)
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?;

    Ok(())
}

pub async fn remove_files(api_token: &str, id: u64) -> Result<()> {
    let client = reqwest::Client::new();
    let form = multipart::Form::new().text("file_ids", id.to_string());
    client
        .post("https://api.put.io/v2/files/delete")
        .multipart(form)
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?;

    Ok(())
}

pub async fn add_transfer(api_token: &str, url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let form = multipart::Form::new().text("url", url.to_string());
    client
        .post("https://api.put.io/v2/transfers/add")
        .multipart(form)
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?;

    Ok(())
}

pub async fn delete_file(api_token: &str, file_id: u64) -> Result<()> {
    let client = reqwest::Client::new();
    let form = multipart::Form::new().text("file_ids", file_id.to_string());
    client
        .post("https://api.put.io/v2/files/delete")
        .multipart(form)
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?;

    Ok(())
}

pub async fn upload_file(api_token: &str, bytes: Vec<u8>) -> Result<()> {
    let client = reqwest::Client::new();
    let file_part = multipart::Part::bytes(bytes).file_name("foo.torrent");

    let form = reqwest::multipart::Form::new()
        .part("file", file_part)
        .text("filename", "foo.torrent");
    let _response = client
        .post("https://upload.put.io/v2/files/upload")
        .header("authorization", format!("Bearer {}", api_token))
        .multipart(form)
        .send()
        .await?;

    // Todo: error if invalid request
    Ok(())
}
#[derive(Debug, Serialize, Deserialize)]
pub struct UrlResponse {
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListFileResponse {
    pub files: Vec<FileResponse>,
    pub parent: FileResponse,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileResponse {
    pub content_type: String,
    pub id: u64,
    pub name: String,
    pub file_type: String,
}

pub async fn list_files(api_token: &str, file_id: u64) -> Result<ListFileResponse> {
    let client = reqwest::Client::new();
    let response: ListFileResponse = client
        .get(format!(
            "https://api.put.io/v2/files/list?parent_id={}",
            file_id
        ))
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?
        .json()
        .await?;
    Ok(response)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct URLResponse {
    pub url: String,
}

pub async fn url(api_token: &str, file_id: u64) -> Result<String> {
    let client = reqwest::Client::new();
    let response: URLResponse = client
        .get(format!("https://api.put.io/v2/files/{}/url", file_id))
        .header("authorization", format!("Bearer {}", api_token))
        .send()
        .await?
        .json()
        .await?;
    Ok(response.url)
}


/// Returns a new OOB code.
pub async fn get_oob() -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::get("https://api.put.io/v2/oauth2/oob/code?app_id=6487")
        .await?
        .json::<HashMap<String, String>>()
        .await?;
    let code = resp.get("code").expect("fetching OOB code");
    Ok(code.to_string())
}

/// Returns new OAuth token if the OOB code is linked to the user's account.
pub async fn check_oob(oob_code: String) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::get(format!(
        "https://api.put.io/v2/oauth2/oob/code/{}",
        oob_code
    ))
    .await?
    .json::<HashMap<String, String>>()
    .await?;
    let token = resp.get("oauth_token").expect("deserializing OAuth token");
    Ok(token.to_string())
}

// Check for new putio transfers and if they qualify, send them on for download
pub async fn produce_transfers(app_data: Data<AppData>, tx: Sender<TransferMessage>) -> Result<()> {
    let ten_seconds = std::time::Duration::from_secs(app_data.config.polling_interval);
    let mut seen = Vec::<u64>::new();

    info!("Checking if there are unfinished transfers.");
    // We only need to check if something has been imported. Just by looking at the filesystem we
    // can't determine if a transfer has been imported and removed or hasn't been downloaded.
    // This avoids downloading a tranfer that has already been imported. In case there is a download,
    // but it wasn't (completely) imported, we will attempt a (partial) download. Files that have
    // been completed downloading will be skipped.
    for putio_transfer in &list_transfers(&app_data.config.putio.api_key)
        .await?
        .transfers
    {
        let mut transfer = Transfer::from(app_data.clone(), putio_transfer);
        if putio_transfer.is_downloadable() {
            let targets = transfer.get_download_targets().await?;
            transfer.targets = Some(targets);
            if transfer.is_imported().await {
                info!("{} already imported. Notifying of import.", &transfer.name);
                seen.push(transfer.transfer_id);
                tx.send(TransferMessage::Imported(transfer)).await?;
            } else {
                info!(
                    "{} is not imported yet. Continuing as normal.",
                    &transfer.name
                );
            }
        }
    }
    info!("Done checking for unfinished transfers.");
    loop {
        let putio_transfers = list_transfers(&app_data.config.putio.api_key)
            .await?
            .transfers;

        if !putio_transfers.is_empty() {
            debug!("Active transfers: {:?}", putio_transfers);
        }
        for putio_transfer in &putio_transfers {
            if seen.contains(&putio_transfer.id) || !putio_transfer.is_downloadable() {
                continue;
            }

            let transfer = Transfer::from(app_data.clone(), putio_transfer);

            info!("Queueing {} for download", transfer.name);
            tx.send(TransferMessage::QueuedForDownload(transfer))
                .await?;
            seen.push(putio_transfer.id);
        }

        // Remove any transfers from seen that are not in the active transfers
        let active_ids: Vec<u64> = putio_transfers.iter().map(|t| t.id).collect();
        seen.retain(|t| active_ids.contains(t));

        sleep(ten_seconds).await;
    }
}

