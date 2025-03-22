use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ArrHistoryResponse {
    pub total_records: u32,
    pub records: Vec<ArrHistoryRecord>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ArrHistoryRecord {
    pub event_type: String,
    pub data: HashMap<String, Option<String>>,
}

pub async fn check_imported(target: &str, api_key: &str, base_url: &str) -> Result<bool> {
    let client = reqwest::Client::new();
    let mut inspected = 0;
    let mut page = 0;
    loop {
        let url = format!(
            "{base_url}/api/v3/history?includeSeries=false&includeEpisode=false&page={page}&pageSize=1000");

        let response = client.get(&url).header("X-Api-Key", api_key).send().await?;

        let status = response.status();

        if !status.is_success() {
            bail!("url: {}, status: {}", url, status);
        }

        let bytes = response.bytes().await?;
        let json: serde_json::Result<ArrHistoryResponse> = serde_json::from_slice(&bytes);
        if json.is_err() {
            bail!("url: {url}, status: {status}, body: {bytes:?}");
        }
        let history_response: ArrHistoryResponse = json?;

        for record in history_response.records {
            if record.event_type == "downloadFolderImported"
                && record.data["droppedPath"].as_ref().unwrap() == target
            {
                return Ok(true);
            } else {
                inspected += 1;
                continue;
            }
        }

        if history_response.total_records < inspected {
            page += 1;
        } else {
            return Ok(false);
        }
    }
}
