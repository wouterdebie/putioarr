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
    pub rpc_version: u32,
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
            rpc_version: 18,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash_string: Option<String>,
    pub name: String,
    pub download_dir: String,
    pub total_size: i64,
    pub left_until_done: i64,
    pub is_finished: bool,
    pub eta: i64,
    pub status: TransmissionTorrentStatus,
    pub seconds_downloading: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_string: Option<String>,
    pub downloaded_ever: i64,
    pub percent_done: f64,
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
        let is_finished = t.finished_at.is_some();
        Self {
            id: t.id,
            hash_string: t.hash,
            name: name.clone(),
            download_dir: String::from(""),
            total_size: t.size.unwrap_or(0),
            left_until_done: max(t.size.unwrap_or(0) - t.downloaded.unwrap_or(0), 0),
            is_finished,
            eta: t.estimated_time.unwrap_or(0),
            status: TransmissionTorrentStatus::from(t.status),
            seconds_downloading,
            // put.io can leave error_message set from a transient problem it
            // recovered from on its own (e.g. "not enough space" during
            // allocation, retried, then completed) without ever clearing the
            // field. Forwarding it verbatim on a transfer that went on to
            // finish makes the *arr's Transmission client treat an otherwise
            // complete download as still erroring, which blocks Completed
            // Download Handling from ever importing it. Only surface the
            // error while the transfer hasn't finished, when it still
            // reflects a real, current problem.
            error_string: if is_finished { None } else { t.error_message },
            downloaded_ever: t.downloaded.unwrap_or(0),
            percent_done: 0.0,
            seed_ratio_limit: 0.0,
            seed_ratio_mode: 0,
            seed_idle_limit: 0,
            seed_idle_mode: 0,
            file_count: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransmissionTorrentStatus {
    Stopped = 0,
    CheckWait = 1,
    Check = 2,
    Queued = 3,
    Downloading = 4,
    SeedingWait = 5,
    Seeding = 6,
}

impl Serialize for TransmissionTorrentStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl From<String> for TransmissionTorrentStatus {
    fn from(value: String) -> Self {
        match value.to_uppercase().as_str() {
            "STOPPED" | "COMPLETED" | "ERROR" => Self::Stopped,
            "CHECKWAIT" | "PREPARING_DOWNLOAD" | "WAITING" => Self::CheckWait,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A transfer that finished cleanly, with no `error_message` and no
    /// `finished_at`/`started_at` set, for tests to override as needed.
    fn base_transfer() -> PutIOTransfer {
        PutIOTransfer {
            id: 1,
            hash: Some("abc123".to_string()),
            name: Some("Some.Movie.2024".to_string()),
            size: Some(1000),
            downloaded: Some(1000),
            finished_at: None,
            estimated_time: None,
            status: "COMPLETED".to_string(),
            started_at: None,
            error_message: None,
            file_id: Some(42),
            userfile_exists: true,
        }
    }

    #[test]
    fn stale_error_on_a_finished_transfer_is_dropped() {
        let t = PutIOTransfer {
            finished_at: Some("2024-01-01T00:00:00".to_string()),
            error_message: Some("You need 4.5 G free space to start this transfer.".to_string()),
            ..base_transfer()
        };

        let torrent: TransmissionTorrent = t.into();

        assert!(torrent.is_finished);
        assert_eq!(torrent.error_string, None);
    }

    #[test]
    fn error_on_an_unfinished_transfer_is_kept() {
        let t = PutIOTransfer {
            finished_at: None,
            error_message: Some("Not enough space on put.io account.".to_string()),
            ..base_transfer()
        };

        let torrent: TransmissionTorrent = t.into();

        assert!(!torrent.is_finished);
        assert_eq!(
            torrent.error_string,
            Some("Not enough space on put.io account.".to_string())
        );
    }

    #[test]
    fn finished_transfer_with_no_error_is_unaffected() {
        let t = PutIOTransfer {
            finished_at: Some("2024-01-01T00:00:00".to_string()),
            error_message: None,
            ..base_transfer()
        };

        let torrent: TransmissionTorrent = t.into();

        assert!(torrent.is_finished);
        assert_eq!(torrent.error_string, None);
    }
}
