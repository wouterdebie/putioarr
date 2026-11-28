use crate::services::putio;
use anyhow::Result;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// A transfer's local download rate, estimated from its byte counter each time
/// the *arr polls, so torrent-get can report a real `eta` while putioarr pulls
/// the files (issue #56).
#[derive(Debug, Clone, Copy)]
pub struct RateSample {
    at: Instant,
    bytes: u64,
    /// Smoothed bytes per second, or None until two samples are far enough apart.
    rate: Option<f64>,
}

impl RateSample {
    /// Samples closer together than this are ignored: over a very short window
    /// the counter moves in bursts and the rate would jump around.
    const MIN_INTERVAL: Duration = Duration::from_secs(5);
    /// How quickly the smoothed rate follows a change in speed. A sample a
    /// minute after the last one (the *arr's usual poll) moves the rate about
    /// 40% of the way to the speed over that minute.
    const SMOOTHING: Duration = Duration::from_secs(120);

    /// The sample after seeing `bytes` downloaded at `now`.
    fn next(prev: Option<RateSample>, now: Instant, bytes: u64) -> RateSample {
        let Some(prev) = prev else {
            return RateSample {
                at: now,
                bytes,
                rate: None,
            };
        };
        if bytes < prev.bytes {
            // A file started over (the server ignored the Range request), so its
            // bytes were taken off the counter. The speed hasn't changed: keep
            // the rate and measure from here.
            return RateSample {
                at: now,
                bytes,
                rate: prev.rate,
            };
        }
        let elapsed = now.saturating_duration_since(prev.at);
        if elapsed < Self::MIN_INTERVAL {
            return prev;
        }
        let current = (bytes - prev.bytes) as f64 / elapsed.as_secs_f64();
        let rate = match prev.rate {
            None => current,
            Some(rate) => {
                let weight = 1.0 - (-elapsed.as_secs_f64() / Self::SMOOTHING.as_secs_f64()).exp();
                rate + weight * (current - rate)
            }
        };
        RateSample {
            at: now,
            bytes,
            rate: Some(rate),
        }
    }

    /// Seconds until `remaining` bytes are downloaded at the current rate, or
    /// None without a usable rate.
    fn eta(&self, remaining: u64) -> Option<i64> {
        let rate = self.rate.filter(|r| *r >= 1.0)?;
        Some((remaining as f64 / rate).ceil() as i64)
    }
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
    /// Last time a connection error was logged for each *arr, used to throttle
    /// the log. A misconfigured Sonarr/Radarr fails on every poll for every
    /// transfer, and logging each one filled users' disks over time (issue #21).
    arr_error_logged: Arc<RwLock<HashMap<String, Instant>>>,
    /// Bytes pulled to local disk so far, keyed by transfer hash. put.io's own
    /// percentage only covers the cloud download; this tracks the second stage
    /// (put.io -> local disk) so the *arr can show real progress instead of a
    /// bar frozen at 0% while the files are being pulled (issue #5). Written
    /// lock-free per chunk by the download workers via an AtomicU64, and read
    /// only when the *arr polls or the status line is logged, so it adds no
    /// measurable cost to the download hot path.
    local_bytes: Arc<RwLock<HashMap<String, Arc<AtomicU64>>>>,
    /// The last rate sample of each transfer's local download, keyed by hash
    /// like `local_bytes` and pruned with it (issue #56).
    local_rates: Arc<RwLock<HashMap<String, RateSample>>>,
    /// Hashes of transfers the *arr removed with torrent-remove, and when.
    /// Download workers stop pulling their files, and a failed download isn't
    /// retried, so a removed transfer's files don't come back after its local
    /// copy was deleted (issues #51, #52). Entries expire after
    /// [`Self::REMOVED_TTL`], and a new torrent-add of the same hash clears one.
    removed: Arc<RwLock<HashMap<String, Instant>>>,
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
            arr_error_logged: Arc::new(RwLock::new(HashMap::new())),
            local_bytes: Arc::new(RwLock::new(HashMap::new())),
            local_rates: Arc::new(RwLock::new(HashMap::new())),
            removed: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// How long a removed marker is kept. Far longer than any download retry
    /// interval, so a removed transfer can't be picked up again meanwhile.
    const REMOVED_TTL: Duration = Duration::from_secs(24 * 60 * 60);

    /// Records that the *arr removed this transfer (torrent-remove).
    pub async fn mark_removed(&self, hash: &str) {
        let mut removed = self.removed.write().await;
        removed.retain(|_, at| at.elapsed() < Self::REMOVED_TTL);
        removed.insert(hash.to_lowercase(), Instant::now());
    }

    /// True if the *arr removed this transfer, so its downloads should stop.
    pub async fn is_removed(&self, hash: &str) -> bool {
        self.removed
            .read()
            .await
            .get(&hash.to_lowercase())
            .is_some_and(|at| at.elapsed() < Self::REMOVED_TTL)
    }

    /// Forgets a removed marker, for when the *arr adds the same torrent again.
    pub async fn clear_removed(&self, hash: &str) {
        self.removed.write().await.remove(&hash.to_lowercase());
    }

    /// Returns the shared byte counter for a transfer's local download, creating
    /// it on first use. Download workers hold this and add to it per chunk, so
    /// progress accumulates lock-free across the (possibly several) files of a
    /// transfer (issue #5).
    pub async fn local_byte_counter(&self, hash: &str) -> Arc<AtomicU64> {
        // Fast path: the counter almost always already exists (a transfer's
        // several files all resolve the same one), so take only a read lock and
        // avoid contending the write lock across download workers.
        if let Some(counter) = self.local_bytes.read().await.get(hash) {
            return counter.clone();
        }
        self.local_bytes
            .write()
            .await
            .entry(hash.to_string())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// Bytes pulled to local disk so far for a transfer, or None if it isn't
    /// being downloaded (no counter registered yet).
    pub async fn local_bytes_downloaded(&self, hash: &str) -> Option<u64> {
        self.local_bytes
            .read()
            .await
            .get(hash)
            .map(|c| c.load(Ordering::Relaxed))
    }

    /// Forgets a transfer's local byte counter once it's done or removed, so the
    /// map doesn't grow without bound over the process lifetime.
    pub async fn clear_local_bytes(&self, hash: &str) {
        self.local_bytes.write().await.remove(hash);
        self.local_rates.write().await.remove(hash);
    }

    /// Drops local byte counters for transfers no longer present, keeping the
    /// map bounded regardless of which cleanup path a transfer took (mirrors
    /// [`Self::retain_file_names`]).
    pub async fn retain_local_bytes(&self, keep: &HashSet<String>) {
        self.local_bytes.write().await.retain(|h, _| keep.contains(h));
        self.local_rates.write().await.retain(|h, _| keep.contains(h));
    }

    /// Estimated seconds until a transfer's local download is done, with
    /// `remaining` bytes left (issue #56). Each call records a rate sample, and
    /// torrent-get calls it on every poll, so an estimate appears from the
    /// second poll after the download starts. None until then, or while
    /// nothing is being downloaded.
    pub async fn local_eta(&self, hash: &str, remaining: u64) -> Option<i64> {
        let bytes = self.local_bytes_downloaded(hash).await?;
        let mut rates = self.local_rates.write().await;
        let sample = RateSample::next(rates.get(hash).copied(), Instant::now(), bytes);
        rates.insert(hash.to_string(), sample);
        sample.eta(remaining)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[actix_rt::test]
    async fn removed_marker_is_case_insensitive_and_can_be_cleared() {
        let state = StateManager::new("k".to_string());
        assert!(!state.is_removed("abcd").await);
        state.mark_removed("ABCD").await;
        assert!(state.is_removed("abcd").await);
        state.clear_removed("AbCd").await;
        assert!(!state.is_removed("abcd").await);
    }

    const MB: u64 = 1_000_000;

    #[test]
    fn rate_comes_from_two_samples_and_follows_a_change_in_speed() {
        let t0 = Instant::now();
        let s = RateSample::next(None, t0, 0);
        assert_eq!(s.eta(100 * MB), None);

        // 60 MB in a minute: 1 MB/s, so 100 MB takes 100 seconds.
        let s = RateSample::next(Some(s), t0 + Duration::from_secs(60), 60 * MB);
        assert_eq!(s.eta(100 * MB), Some(100));

        // Twice as fast for the next minute: the rate moves part of the way.
        let s = RateSample::next(Some(s), t0 + Duration::from_secs(120), 180 * MB);
        let eta = s.eta(100 * MB).unwrap();
        assert!((50..100).contains(&eta), "eta {eta}");

        // A stall makes the estimate grow instead of jumping to nothing.
        let stalled = RateSample::next(Some(s), t0 + Duration::from_secs(180), 180 * MB);
        assert!(stalled.eta(100 * MB).unwrap() > eta);
    }

    #[test]
    fn close_samples_are_ignored_and_a_restarted_file_keeps_the_rate() {
        let t0 = Instant::now();
        let s = RateSample::next(None, t0, 0);
        let s = RateSample::next(Some(s), t0 + Duration::from_secs(60), 60 * MB);

        // A second poll a moment later doesn't change anything.
        let close = RateSample::next(Some(s), t0 + Duration::from_secs(61), 70 * MB);
        assert_eq!(close.bytes, 60 * MB);
        assert_eq!(close.eta(100 * MB), Some(100));

        // The counter going back (a file started over) keeps the rate.
        let back = RateSample::next(Some(s), t0 + Duration::from_secs(90), 10 * MB);
        assert_eq!(back.bytes, 10 * MB);
        assert_eq!(back.eta(100 * MB), Some(100));
    }

    #[actix_rt::test]
    async fn no_local_eta_before_the_download_starts() {
        let state = StateManager::new("k".to_string());
        assert_eq!(state.local_eta("abcd", 100).await, None);
        state.local_byte_counter("abcd").await;
        // The first poll only takes a sample.
        assert_eq!(state.local_eta("abcd", 100).await, None);
        state.clear_local_bytes("abcd").await;
        assert!(state.local_rates.read().await.is_empty());
    }
}
