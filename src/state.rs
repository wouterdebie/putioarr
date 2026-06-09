use crate::services::putio;
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Key under which putioarr stores its transfer state in put.io's per-user
/// key-value config store.
const CONFIG_KEY: &str = "putioarr_transfers";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferState {
    pub hash: String,
    pub source_category: String,
    pub download_dir: String,
}

/// Tracks the category/download-dir chosen for each transfer.
///
/// Reads are served from an in-memory cache for speed, while mutations are
/// written through to put.io's per-user key-value config store so the mapping
/// survives putioarr restarts.
#[derive(Clone)]
pub struct StateManager {
    api_token: String,
    transfers: Arc<RwLock<HashMap<String, TransferState>>>,
    /// Transfer ids whose files putioarr has finished downloading to local
    /// disk. Used to avoid telling the *arr a download is complete before the
    /// files actually exist locally (see issue #16).
    local_complete: Arc<RwLock<HashSet<u64>>>,
    /// Last time a connection error was logged for each *arr, used to throttle
    /// the log. A misconfigured Sonarr/Radarr fails on every poll for every
    /// transfer, and logging each one filled users' disks over time (issue #21).
    arr_error_logged: Arc<RwLock<HashMap<String, Instant>>>,
}

impl StateManager {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
            transfers: Arc::new(RwLock::new(HashMap::new())),
            local_complete: Arc::new(RwLock::new(HashSet::new())),
            arr_error_logged: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Minimum time between logging the same *arr's connection error.
    pub const ARR_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(300);

    /// Returns true if an error for `app` should be logged now, throttling
    /// repeats to at most one per [`Self::ARR_ERROR_LOG_INTERVAL`]. Keeps a
    /// persistently unreachable/misconfigured *arr from filling the disk with
    /// identical error lines on every poll (issue #21).
    pub async fn should_log_arr_error(&self, app: &str) -> bool {
        let mut map = self.arr_error_logged.write().await;
        let now = Instant::now();
        match map.get(app) {
            Some(at) if now.saturating_duration_since(*at) < Self::ARR_ERROR_LOG_INTERVAL => false,
            _ => {
                map.insert(app.to_string(), now);
                true
            }
        }
    }

    /// Marks a transfer's local download as fully finished (pulled home).
    pub async fn mark_local_complete(&self, id: u64) {
        self.local_complete.write().await.insert(id);
    }

    /// Returns true once putioarr has finished downloading the transfer locally.
    pub async fn is_local_complete(&self, id: u64) -> bool {
        self.local_complete.read().await.contains(&id)
    }

    /// Forgets a transfer's local-complete marker (e.g. after cleanup).
    pub async fn clear_local_complete(&self, id: u64) {
        self.local_complete.write().await.remove(&id);
    }

    /// Loads persisted state from put.io into the in-memory cache. Should be
    /// called once at startup, before any transfers are processed.
    pub async fn load(&self) -> Result<()> {
        match putio::get_config_value::<HashMap<String, TransferState>>(&self.api_token, CONFIG_KEY)
            .await
        {
            Ok(Some(map)) => {
                let count = map.len();
                *self.transfers.write().await = map;
                info!("state: loaded {} transfer(s) from put.io config", count);
            }
            Ok(None) => debug!("state: no persisted state found in put.io config"),
            Err(e) => warn!("state: failed to load persisted state from put.io: {}", e),
        }
        Ok(())
    }

    /// Persists the current in-memory cache to put.io's config store.
    async fn persist(&self) {
        let map = self.transfers.read().await.clone();
        if let Err(e) = putio::set_config_value(&self.api_token, CONFIG_KEY, &map).await {
            error!("state: failed to persist state to put.io: {}", e);
        }
    }

    pub async fn add_transfer(
        &self,
        hash: String,
        category: String,
        download_dir: String,
    ) -> Result<()> {
        let key = hash.to_lowercase();
        debug!(
            "state: add_transfer hash={} category={} dir={}",
            key, category, download_dir
        );
        {
            let mut transfers = self.transfers.write().await;
            transfers.insert(
                key.clone(),
                TransferState {
                    hash: key,
                    source_category: category,
                    download_dir,
                },
            );
        }
        self.persist().await;
        Ok(())
    }

    pub async fn get_transfer(&self, hash: &str) -> Option<TransferState> {
        let transfers = self.transfers.read().await;
        transfers.get(&hash.to_lowercase()).cloned()
    }

    pub async fn remove_transfer(&self, hash: &str) -> Result<()> {
        {
            let mut transfers = self.transfers.write().await;
            transfers.remove(&hash.to_lowercase());
        }
        self.persist().await;
        Ok(())
    }

    pub async fn get_download_dir_for_transfer(&self, hash: &str, default_dir: &str) -> String {
        if let Some(state) = self.get_transfer(hash).await {
            state.download_dir
        } else {
            debug!(
                "state: no entry for hash={} (using default dir {})",
                hash, default_dir
            );
            default_dir.to_string()
        }
    }
}