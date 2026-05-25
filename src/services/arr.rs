use crate::{download_system::transfer::MediaType, ArrConfig};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrKind {
    Sonarr,
    Radarr,
    Whisparr,
    Lidarr,
}

impl ArrKind {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "sonarr" => Some(Self::Sonarr),
            "radarr" => Some(Self::Radarr),
            "whisparr" => Some(Self::Whisparr),
            "lidarr" => Some(Self::Lidarr),
            _ => None,
        }
    }

    pub fn media_type(&self) -> MediaType {
        match self {
            Self::Lidarr => MediaType::Audio,
            _ => MediaType::Video,
        }
    }
}

impl fmt::Display for ArrKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
            Self::Whisparr => "whisparr",
            Self::Lidarr => "lidarr",
        };
        write!(f, "{}", s)
    }
}

pub struct ArrApp {
    pub name: String,
    pub kind: ArrKind,
    base_url: String,
    api_key: String,
}

impl ArrApp {
    pub fn new(name: String, kind: ArrKind, config: &ArrConfig) -> Self {
        Self {
            name,
            kind,
            base_url: config.url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
        }
    }

    fn history_url(&self, page: u32) -> String {
        match self.kind {
            ArrKind::Lidarr => format!(
                "{}/api/v1/history?includeArtist=false&includeAlbum=false&includeTrack=false&page={}&pageSize=1000",
                self.base_url, page
            ),
            _ => format!(
                "{}/api/v3/history?includeSeries=false&includeEpisode=false&page={}&pageSize=1000",
                self.base_url, page
            ),
        }
    }

    fn import_event(&self) -> &'static str {
        match self.kind {
            ArrKind::Lidarr => "trackFileImported",
            _ => "downloadFolderImported",
        }
    }

    pub async fn check_imported(&self, target: &str) -> Result<bool> {
        let client = reqwest::Client::new();
        let mut inspected = 0;
        let mut page = 0;
        let import_event = self.import_event();
        loop {
            let url = self.history_url(page);
            let response = client
                .get(&url)
                .header("X-Api-Key", &self.api_key)
                .send()
                .await?;
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
                if record.event_type == import_event
                    && record
                        .data
                        .get("droppedPath")
                        .and_then(|v| v.as_ref())
                        .map(|p| p == target)
                        .unwrap_or(false)
                {
                    return Ok(true);
                }
                inspected += 1;
            }

            if history_response.total_records < inspected {
                page += 1;
            } else {
                return Ok(false);
            }
        }
    }
}

impl fmt::Display for ArrApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.kind)
    }
}
