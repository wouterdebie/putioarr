use chrono::prelude::*;
use log::warn;
use serde::{Deserialize, Serialize};
use std::cmp::max;

use super::putio::PutIOTransfer;

#[derive(Serialize, Debug)]
pub struct TransmissionResponse {
    pub result: String,
    pub arguments: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
pub struct TransmissionRequest {
    pub method: String,
    pub arguments: Option<serde_json::Value>,
}

#[derive(Serialize, Debug)]
pub struct TransmissionConfig {
    #[serde(rename(serialize = "rpc-version"))]
    pub rpc_version: String,
    #[serde(default)]
    pub version: String,
    #[serde(rename(serialize = "download-dir"))]
    pub download_dir: String,
    #[serde(rename(serialize = "seedRatioLimit"))]
    pub seed_ratio_limit: f32,
    #[serde(rename(serialize = "seedRatioLimited"))]
    pub seed_ratio_limited: bool,
    #[serde(rename(serialize = "idle-seeding-limit"))]
    pub idle_seeding_limit: u64,
    #[serde(rename(serialize = "idle-seeding-limit-enabled"))]
    pub idle_seeding_limit_enabled: bool,
}

impl Default for TransmissionConfig {
    fn default() -> Self {
        TransmissionConfig {
            rpc_version: String::from("18"),
            version: String::from("14.0.0"),
            download_dir: String::from("/"),
            seed_ratio_limit: 1.0,
            seed_ratio_limited: true,
            idle_seeding_limit: 100,
            idle_seeding_limit_enabled: false,
        }
    }
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TransmissionTorrent {
    pub id: u64,
    pub hash_string: Option<String>,
    pub name: String,
    pub download_dir: String,
    pub total_size: i64,
    pub left_until_done: i64,
    pub is_finished: bool,
    pub eta: u64,
    pub status: TransmissionTorrentStatus,
    pub seconds_downloading: i64,
    pub error_string: Option<String>,
    pub downloaded_ever: i64,
    pub seed_ratio_limit: f32,
    pub seed_ratio_mode: u32,
    pub seed_idle_limit: u64,
    pub seed_idle_mode: u32,
    pub file_count: u32,
}

impl From<PutIOTransfer> for TransmissionTorrent {
    fn from(t: PutIOTransfer) -> Self {
        let s = match t.started_at {
            Some(t) => t,
            None => Utc::now().format("%FT%T").to_string(),
        };

        let started_at = Utc
            .from_local_datetime(&NaiveDateTime::parse_from_str(&s, "%FT%T").unwrap())
            .unwrap();
        let now = Utc::now();
        let seconds_downloading = (now - started_at).num_seconds();
        let default = &"Unknown".to_string();
        let name = t.name.as_ref().unwrap_or(default);
        Self {
            id: t.id,
            hash_string: t.hash,
            name: name.clone(),
            download_dir: String::from(""),
            total_size: t.size.unwrap_or(0),
            left_until_done: max(t.size.unwrap_or(0) - t.downloaded.unwrap_or(0), 0),
            is_finished: t.finished_at.is_some(),
            eta: t.estimated_time.unwrap_or(0),
            status: TransmissionTorrentStatus::from(t.status),
            seconds_downloading,
            error_string: t.error_message,
            downloaded_ever: t.downloaded.unwrap_or(0),
            seed_ratio_limit: 0.0,
            seed_ratio_mode: 0,
            seed_idle_limit: 0,
            seed_idle_mode: 0,
            file_count: 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub enum TransmissionTorrentStatus {
    Stopped = 0,
    CheckWait = 1,
    Check = 2,
    Queued = 3,
    Downloading = 4,
    SeedingWait = 5,
    Seeding = 6,
}

impl From<String> for TransmissionTorrentStatus {
    fn from(value: String) -> Self {
        match value.to_uppercase().as_str() {
            "STOPPED" | "COMPLETED" | "ERROR" => Self::Stopped,
            "CHECKWAIT" | "PREPARING_DOWNLOAD" => Self::CheckWait,
            "CHECK" | "COMPLETING" => Self::Check,
            "QUEUED" | "IN_QUEUE" => Self::Queued,
            "DOWNLOADING" => Self::Downloading,
            "SEEDINGWAIT" => Self::SeedingWait,
            "SEEDING" => Self::Seeding,
            _ => {
                warn!("Status {} unknown. Treating as CheckWait.", &value);
                Self::CheckWait
            }
        }
    }
}
