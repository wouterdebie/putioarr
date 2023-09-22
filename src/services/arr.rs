use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ArrHistoryResponse {
    pub page: u32,
    pub page_size: u32,
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

        let response: ArrHistoryResponse = client
            .get(&url)
            .header("X-Api-Key", api_key)
            .send()
            .await?
            .json()
            .await?;

        for record in response.records {
            if record.event_type == "downloadFolderImported"
                && record.data["droppedPath"].as_ref().unwrap() == target
            {
                return Ok(true);
            } else {
                inspected += 1;
                continue;
            }
        }

        if response.total_records < inspected {
            page += 1;
        } else {
            return Ok(false);
        }
    }
}
