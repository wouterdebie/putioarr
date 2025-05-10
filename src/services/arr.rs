use crate::{download_system::transfer, Config};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArrConfig {
    url: String,
    api_key: String,
}

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

#[derive(Debug)]
pub enum ArrAppType {
    Lidarr,
    Radarr,
    Sonarr,
    Whisparr,
}

pub struct ArrApp {
    app_type: ArrAppType,
    config: ArrConfig,
    client: reqwest::Client,
    media_type: transfer::MediaType,
}

impl ArrApp {
    pub fn new(app_type: ArrAppType, config: &ArrConfig, media_type: transfer::MediaType) -> Self {
        Self {
            app_type: app_type,
            config: config.clone(),
            client: reqwest::Client::new(),
            media_type: media_type,
        }
    }

    pub fn from_config(config: &Config) -> Vec<Self> {
        let mut apps = vec![];
        if let Some(c) = &config.lidarr {
            apps.push(Self::new(ArrAppType::Lidarr, c, transfer::MediaType::Audio));
        }
        if let Some(c) = &config.radarr {
            apps.push(Self::new(ArrAppType::Radarr, c, transfer::MediaType::Video));
        }
        if let Some(c) = &config.sonarr {
            apps.push(Self::new(ArrAppType::Sonarr, c, transfer::MediaType::Video));
        }
        if let Some(c) = &config.whisparr {
            apps.push(Self::new(
                ArrAppType::Whisparr,
                c,
                transfer::MediaType::Video,
            ));
        }
        apps
    }

    fn url(&self, page: u32) -> String {
        match &self.app_type {
            ArrAppType::Radarr | ArrAppType::Sonarr | ArrAppType::Whisparr => {
                format!(
            "{0}/api/v3/history?includeSeries=false&includeEpisode=false&page={page}&pageSize=1000", self.config.url)
            }
            ArrAppType::Lidarr => {
                format!(
                "{0}/api/v1/history?includeArtist=false&includeAlbum=false&includeTrack=false&page={page}&pageSize=1000", self.config.url)
            }
        }
    }

    async fn get(&self, page: u32) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .get(self.url(page))
            .header("X-Api-Key", &self.config.api_key)
            .send()
            .await
    }

    fn should_handle(&self, target: &transfer::DownloadTarget) -> Result<bool> {
        match &target.media_type {
            Some(mt) => {
                if *mt != self.media_type {
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            None => {
                bail!("Cannot check files with no media type: {}", target);
            }
        }
    }

    pub async fn check_imported(&self, target: &transfer::DownloadTarget) -> Result<bool> {
        if !self.should_handle(target)? {
            return Ok(false);
        }
        let mut inspected = 0;
        let mut page = 0;
        loop {
            let response = self.get(page).await?;
            let status = response.status();

            if !status.is_success() {
                bail!("url: {}, status: {}", self.url(page), status);
            }

            let bytes = response.bytes().await?;
            let json: serde_json::Result<ArrHistoryResponse> = serde_json::from_slice(&bytes);
            if json.is_err() {
                bail!(
                    "url: {}, status: {}, body: {:?}",
                    self.url(page),
                    status,
                    bytes
                );
            }
            let history_response: ArrHistoryResponse = json?;

            for record in history_response.records {
                if (record.event_type == "downloadFolderImported"
                    || record.event_type == "trackFileImported")
                    && record.data["droppedPath"].as_ref().unwrap() == &target.to
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
}

impl fmt::Display for ArrApp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.app_type)
    }
}
