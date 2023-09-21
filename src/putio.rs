use anyhow::Result;
use reqwest::multipart;
use serde::{Deserialize, Serialize};

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
    pub percent_done: u16,
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
