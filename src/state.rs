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

/// A completed file found in a `watch_folders` folder that has no transfer
/// record (e.g. put.io cleared the transfer but left the file). It's reported
/// to the *arr like a normal download so it can still be imported (issue #34).
#[derive(Debug, Clone)]
pub struct OrphanFile {
    pub file_id: i64,
    pub name: String,
    pub hash: String,
    pub size: i64,
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
    /// Caches the actual put.io file/folder name for a transfer's `file_id`.
    /// put.io's transfer name often differs from the downloaded file/folder name
    /// (e.g. an indexer prefix), and we report the latter to the *arr so it can
    /// locate the download (see issue #20). A file_id's name never changes once
    /// downloaded, so caching it avoids an API call on every torrent-get.
    file_names: Arc<RwLock<HashMap<i64, String>>>,
    /// Negative cache of `file_id`s whose name lookup failed, with the time of
    /// the last attempt. A file may have been removed from put.io (a persistent
    /// 404); without this, every torrent-get would re-hit the API and re-log a
    /// warning. Retries are suppressed until [`Self::NAME_FAILURE_TTL`] passes.
    failed_names: Arc<RwLock<HashMap<i64, Instant>>>,
    /// Orphaned watch-folder files (no transfer record) currently being pulled,
    /// keyed by file_id. Reported to the *arr via torrent-get so they import
    /// like normal downloads (issue #34).
    orphans: Arc<RwLock<HashMap<i64, OrphanFile>>>,
}

impl StateManager {
    pub fn new(api_token: String) -> Self {
        Self {
            api_token,
            transfers: Arc::new(RwLock::new(HashMap::new())),
            local_complete: Arc::new(RwLock::new(HashSet::new())),
            file_names: Arc::new(RwLock::new(HashMap::new())),
            failed_names: Arc::new(RwLock::new(HashMap::new())),
            orphans: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Records an orphaned watch-folder file that is being pulled.
    pub async fn add_orphan(&self, orphan: OrphanFile) {
        self.orphans.write().await.insert(orphan.file_id, orphan);
    }

    /// True if `file_id` is already tracked as an orphan being pulled.
    pub async fn has_orphan(&self, file_id: i64) -> bool {
        self.orphans.read().await.contains_key(&file_id)
    }

    /// Stops tracking an orphan (e.g. once it has been imported and removed).
    pub async fn remove_orphan(&self, file_id: i64) {
        self.orphans.write().await.remove(&file_id);
    }

    /// All orphaned files currently being pulled, for reporting to the *arr.
    pub async fn orphans(&self) -> Vec<OrphanFile> {
        self.orphans.read().await.values().cloned().collect()
    }

    /// How long to suppress retrying a failed file-name lookup.
    const NAME_FAILURE_TTL: Duration = Duration::from_secs(600);

    /// Returns the cached put.io file/folder name for a `file_id`, if known.
    pub async fn get_file_name(&self, file_id: i64) -> Option<String> {
        self.file_names.read().await.get(&file_id).cloned()
    }

    /// Caches the put.io file/folder name for a `file_id` and clears any prior
    /// failure recorded for it.
    pub async fn set_file_name(&self, file_id: i64, name: String) {
        self.file_names.write().await.insert(file_id, name);
        self.failed_names.write().await.remove(&file_id);
    }

    /// Records that resolving the name for `file_id` just failed.
    pub async fn mark_name_failed(&self, file_id: i64) {
        self.failed_names.write().await.insert(file_id, Instant::now());
    }

    /// Returns true if `file_id`'s name lookup failed recently and shouldn't be
    /// retried yet, avoiding repeated API calls/warnings for persistent errors.
    pub async fn name_lookup_suppressed(&self, file_id: i64) -> bool {
        match self.failed_names.read().await.get(&file_id) {
            Some(at) => at.elapsed() < Self::NAME_FAILURE_TTL,
            None => false,
        }
    }

    /// Drops cached file names (and failure markers) for any `file_id` not in
    /// `keep`, so the caches stay bounded to the transfers currently on the
    /// account instead of growing without limit over the lifetime of the process.
    pub async fn retain_file_names(&self, keep: &HashSet<i64>) {
        self.file_names.write().await.retain(|id, _| keep.contains(id));
        self.failed_names.write().await.retain(|id, _| keep.contains(id));
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