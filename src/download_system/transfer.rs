use crate::{
    services::{
        arr::ArrApp,
        putio::{self, PutIOTransfer},
    },
    state::OrphanFile,
    AppData,
};
use actix_web::web::Data;
use anyhow::{Context, Result};
use async_channel::Sender;
use async_recursion::async_recursion;
use colored::*;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt::Display,
    path::Path,
    time::{Duration, Instant},
};
use tokio::time::sleep;

#[derive(Clone)]
pub struct Transfer {
    pub name: String,
    pub file_id: Option<i64>,
    pub hash: Option<String>,
    pub transfer_id: u64,
    pub targets: Option<Vec<DownloadTarget>>,
    pub app_data: Data<AppData>,
    /// True if this came from a watch-folder scan (an orphaned file with no
    /// put.io transfer record) rather than `transfers/list`. Such transfers are
    /// cleaned up by deleting the file directly, since there is no transfer to
    /// remove or seeding to wait on (issue #34).
    pub is_orphan: bool,
}

impl Transfer {
    pub async fn is_imported(&self) -> bool {
        let targets = self.targets.as_ref().unwrap().clone();
        let apps: Vec<ArrApp> = self
            .app_data
            .config
            .all_arrs()
            .into_iter()
            .map(|(name, kind, c)| ArrApp::new(name, kind, c))
            .collect();

        let targets = targets
            .into_iter()
            .filter(|t| t.target_type == TargetType::File)
            .collect::<Vec<DownloadTarget>>();

        let mut results = Vec::<bool>::new();
        for target in targets {
            let mut service_results = vec![];
            for app in &apps {
                // Only ask an *arr about files matching its media type.
                if let Some(mt) = &target.media_type {
                    if *mt != app.kind.media_type() {
                        continue;
                    }
                }
                let service_result = match app.check_imported(&target.to).await {
                    Ok(r) => r,
                    Err(e) => {
                        // A misconfigured/unreachable *arr fails for every
                        // transfer on every poll; throttle the log so it doesn't
                        // fill the disk over time (issue #21). Key on the app's
                        // existing name (no allocation on the suppressed path).
                        if self.app_data.state.should_log_arr_error(&app.name).await {
                            error!(
                                "Error retrieving history from {} (suppressing repeats for {:?}): {}",
                                app,
                                crate::state::StateManager::ARR_ERROR_LOG_INTERVAL,
                                e
                            );
                        }
                        false
                    }
                };
                if service_result {
                    info!(
                        "{}: found imported by {}",
                        &target,
                        app.to_string().bright_blue()
                    );
                }
                service_results.push(service_result)
            }
            // If no service was eligible for this target, treat it as imported
            // (otherwise an audio file with no Lidarr would block forever).
            let any_imported = service_results.is_empty() || service_results.into_iter().any(|x| x);
            results.push(any_imported);
        }
        // Check if all targets have been imported
        results.into_iter().all(|x| x)
    }

    pub async fn get_download_targets(&self) -> Result<Vec<DownloadTarget>> {
        self.generate_targets(None).await
    }

    /// Like [`get_download_targets`] but with an explicit base directory instead
    /// of looking one up from stored transfer state. Used for orphaned files,
    /// which have no persisted state to route them (issue #34).
    pub async fn get_download_targets_in(&self, base_path: &str) -> Result<Vec<DownloadTarget>> {
        self.generate_targets(Some(base_path.to_string())).await
    }

    async fn generate_targets(&self, base_path: Option<String>) -> Result<Vec<DownloadTarget>> {
        info!("{}: generating targets", self);
        let file_id = self.file_id.context("transfer has no file_id")?;
        let default = "0000".to_string();
        let hash = self.hash.as_ref().unwrap_or(&default).as_str();
        recurse_download_targets(&self.app_data, file_id, hash, base_path, true).await
    }

    pub fn get_top_level(&self) -> DownloadTarget {
        self.targets
            .clone()
            .unwrap()
            .into_iter()
            .find(|t| t.top_level)
            .unwrap()
    }

    pub fn from(app_data: Data<AppData>, transfer: &PutIOTransfer) -> Self {
        let default = &"Unknown".to_string();
        let name = transfer.name.as_ref().unwrap_or(default);
        Self {
            transfer_id: transfer.id,
            name: name.clone(),
            file_id: transfer.file_id,
            targets: None,
            hash: transfer.hash.clone(),
            app_data,
            is_orphan: false,
        }
    }

    /// Builds a synthetic transfer for an orphaned watch-folder file (one with
    /// no put.io transfer record). The file_id doubles as the transfer id and is
    /// formatted into a deterministic synthetic hash so the *arr can track it.
    pub fn from_orphan(app_data: Data<AppData>, file_id: i64, name: String) -> Self {
        // put.io file ids are non-negative; the watch-folder scan filters out
        // any that aren't before constructing an orphan, so the `as u64` casts
        // below are exact. Assert it to catch future misuse.
        debug_assert!(file_id >= 0, "orphan file_id must be non-negative");
        Self {
            transfer_id: file_id as u64,
            name,
            file_id: Some(file_id),
            targets: None,
            hash: Some(format!("{:040x}", file_id as u64)),
            app_data,
            is_orphan: true,
        }
    }
}

impl Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let default = "0000".to_string();
        let hash = &self.hash.as_ref().unwrap_or(&default)[..4];
        let s = format!("[{}: {}]", hash, self.name).cyan();
        write!(f, "{s}")
    }
}

#[async_recursion]
async fn recurse_download_targets(
    app_data: &Data<AppData>,
    file_id: i64,
    hash: &str,
    override_base_path: Option<String>,
    top_level: bool,
) -> Result<Vec<DownloadTarget>> {
    // Check if we have stored state for this transfer to get the correct download directory
    let base_path = if let Some(path) = override_base_path {
        path
    } else {
        // Try to get the download directory from state, fallback to default
        app_data.state.get_download_dir_for_transfer(hash, &app_data.config.download_directory).await
    };
    let mut targets = Vec::<DownloadTarget>::new();
    let response = putio::list_files(&app_data.config.putio.api_key, file_id).await?;
    let to = Path::new(&base_path)
        .join(&response.parent.name)
        .to_string_lossy()
        .to_string();

    match response.parent.file_type.as_str() {
        "FOLDER" => {
            if !app_data
                .config
                .skip_directories
                .contains(&response.parent.name.to_lowercase())
            {
                let new_base_path = to.clone();

                targets.push(DownloadTarget {
                    from: None,
                    target_type: TargetType::Directory,
                    to,
                    top_level,
                    transfer_hash: hash.to_string(),
                    media_type: None,
                });

                for file in response.files {
                    targets.append(
                        &mut recurse_download_targets(
                            app_data,
                            file.id,
                            hash,
                            Some(new_base_path.clone()),
                            false,
                        )
                        .await?,
                    );
                }
            }
        }
        "VIDEO" | "AUDIO" => {
            // Get download URL for file
            let url = putio::url(&app_data.config.putio.api_key, response.parent.id).await?;
            targets.push(DownloadTarget {
                from: Some(url),
                target_type: TargetType::File,
                to,
                top_level,
                transfer_hash: hash.to_string(),
                media_type: MediaType::from_putio(response.parent.file_type.as_str()),
            });
        }
        other => {
            debug!(
                "{}: skipping file type {}",
                response.parent.name, other
            );
        }
    }

    Ok(targets)
}

#[derive(Clone)]
pub enum TransferMessage {
    QueuedForDownload(Transfer),
    Downloaded(Transfer),
    Imported(Transfer),
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq, Copy)]
pub enum MediaType {
    Audio,
    Video,
}

impl MediaType {
    pub fn from_putio(file_type: &str) -> Option<Self> {
        match file_type {
            "AUDIO" => Some(Self::Audio),
            "VIDEO" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadTarget {
    pub from: Option<String>,
    pub to: String,
    pub target_type: TargetType,
    pub top_level: bool,
    pub transfer_hash: String,
    pub media_type: Option<MediaType>,
}

impl Display for DownloadTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = &self.transfer_hash.as_str()[..4];
        let s = format!("[{}: {}]", hash, self.to).magenta();
        write!(f, "{s}")
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum TargetType {
    Directory,
    File,
}

// Check for new putio transfers and if they qualify, send them on for download
/// Returns true if this transfer is managed by putioarr, i.e. it has stored
/// category/download-dir state because *it* uploaded it on behalf of an *arr.
/// Transfers added directly on put.io (e.g. a manually-maintained "Watch List")
/// have no state. Unless the user opts in via `download_unmanaged`, those are
/// ignored so putioarr does not try to download the entire put.io account and
/// hang on seeding transfers (see upstream issue #9).
async fn is_managed(app_data: &Data<AppData>, putio_transfer: &PutIOTransfer) -> bool {
    if app_data.config.download_unmanaged {
        return true;
    }
    match &putio_transfer.hash {
        Some(hash) => app_data.state.get_transfer(hash).await.is_some(),
        None => false,
    }
}

pub async fn produce_transfers(app_data: Data<AppData>, tx: Sender<TransferMessage>) -> Result<()> {
    let putio_check_interval = Duration::from_secs(app_data.config.polling_interval);
    let mut seen = Vec::<u64>::new();
    // Watch-folder scans hit put.io once per folder, so run them on their own
    // configurable interval rather than every poll to avoid extra API traffic
    // (issue #34).
    // Clamp to >= 1s so a misconfigured 0 doesn't become a zero Duration that
    // scans on every poll. The effective floor is the polling interval anyway,
    // since the scan only runs inside this loop.
    let orphan_scan_interval =
        Duration::from_secs(app_data.config.watch_folder_interval_secs.max(1));
    let mut last_orphan_scan: Option<Instant> = None;

    info!("Checking unfinished transfers");
    // We only need to check if something has been imported. Just by looking at the filesystem we
    // can't determine if a transfer has been imported and removed or hasn't been downloaded.
    // This avoids downloading a tranfer that has already been imported. In case there is a download,
    // but it wasn't (completely) imported, we will attempt a (partial) download. Files that have
    // been completed downloading will be skipped.
    for putio_transfer in &putio::list_transfers(&app_data.config.putio.api_key)
        .await?
        .transfers
    {
        let name = putio_transfer.name.clone().unwrap_or("??".to_string());
        let mut transfer = Transfer::from(app_data.clone(), putio_transfer);
        if putio_transfer.is_downloadable() && is_managed(&app_data, putio_transfer).await {
            info!("Getting download target for {name}");
            let targets = transfer.get_download_targets().await;
            if targets.is_err() {
                // For example, if the user trashed the file in Putio
                warn!("Could not get target for {name}");
                continue;
            }
            transfer.targets = Some(targets?);
            if transfer.is_imported().await {
                info!("{}: already imported", &transfer);
                seen.push(transfer.transfer_id);
                tx.send(TransferMessage::Imported(transfer)).await?;
            } else {
                info!("{}: not imported yet", &transfer);
            }
        }
    }
    info!("Done checking for unfinished transfers. Starting to monitor transfers.");

    // Set the start time
    let mut start = std::time::Instant::now();

    loop {
        if let Ok(list_transfer_response) =
            putio::list_transfers(&app_data.config.putio.api_key).await
        {
            for putio_transfer in &list_transfer_response.transfers {
                if seen.contains(&putio_transfer.id) || !putio_transfer.is_downloadable() {
                    continue;
                }
                let transfer = Transfer::from(app_data.clone(), putio_transfer);

                // Skip transfers we don't manage (e.g. a manual Watch List) unless
                // `download_unmanaged` is set. This prevents putioarr from trying to
                // download the whole account and hanging on seeding transfers (#9).
                if !is_managed(&app_data, putio_transfer).await {
                    debug!(
                        "{}: not managed by putioarr (no stored category/download-dir), skipping",
                        transfer
                    );
                    seen.push(putio_transfer.id);
                    continue;
                }

                info!("{}: ready for download", transfer);
                tx.send(TransferMessage::QueuedForDownload(transfer))
                    .await?;
                seen.push(putio_transfer.id);
            }

            // Remove any transfers from seen that are not in the active transfers
            let active_ids: Vec<u64> = list_transfer_response
                .transfers
                .iter()
                .map(|t| t.id)
                .collect();
            seen.retain(|t| active_ids.contains(t));

            // Pull orphaned files from the configured watch folders (completed
            // files whose transfer record no longer exists — see issue #34),
            // throttled so it doesn't list every folder on every poll.
            if last_orphan_scan.map_or(true, |t| t.elapsed() >= orphan_scan_interval) {
                scan_watch_folders(&app_data, &tx, &list_transfer_response.transfers).await;
                last_orphan_scan = Some(Instant::now());
            }

            // Log status when 60 seconds have passed since last time
            if start.elapsed().as_secs() >= 60 {
                info!(
                    "Active transfers: {}",
                    list_transfer_response.transfers.len()
                );
                list_transfer_response
                    .transfers
                    .iter()
                    .for_each(|t| info!("  {}", Transfer::from(app_data.clone(), t)));

                start = std::time::Instant::now();
            }

            sleep(putio_check_interval).await;
        } else {
            warn!("List put.io transfers failed. Retrying..");
            continue;
        };
    }
}

/// Heuristic: does this name look like a TV episode (SxxExx, or "Season")?
/// Used to route an orphaned file to the Sonarr vs Radarr category folder.
fn looks_like_episode(name: &str) -> bool {
    let b = name.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i + 3 < n {
        if (b[i] == b'S' || b[i] == b's') && b[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < n && (b[j] == b'E' || b[j] == b'e') && b[j + 1].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    // Case-insensitive "season" check without allocating a lowercased copy.
    name.as_bytes()
        .windows(6)
        .any(|w| w.eq_ignore_ascii_case(b"season"))
}

/// Scans the configured `watch_folders` for orphaned completed files — files
/// with no transfer record (e.g. removed by put.io's "clear completed") that
/// `transfers/list` will never surface — and queues them for download like
/// normal transfers so they don't get stranded on put.io (issue #34).
async fn scan_watch_folders(
    app_data: &Data<AppData>,
    tx: &Sender<TransferMessage>,
    active_transfers: &[PutIOTransfer],
) {
    if app_data.config.watch_folders.is_empty() {
        return;
    }
    let api_key = &app_data.config.putio.api_key;
    let active_file_ids: HashSet<i64> = active_transfers.iter().filter_map(|t| t.file_id).collect();

    for folder_id in &app_data.config.watch_folders {
        let resp = match putio::list_files(api_key, *folder_id).await {
            Ok(r) => r,
            Err(e) => {
                warn!("watch folder {}: listing failed: {}", folder_id, e);
                continue;
            }
        };
        for file in &resp.files {
            // Only media (and folders that may contain media); skip stray
            // images/nfos and anything with an unusable (negative) id.
            if file.id < 0 || !matches!(file.file_type.as_str(), "FOLDER" | "VIDEO" | "AUDIO") {
                continue;
            }
            // Skip the result of an active transfer (handled the normal way) and
            // any orphan we're already pulling. Using `has_orphan` as the
            // "in progress" marker keeps tracking bounded and, since a failed
            // orphan is dropped from it, lets a later poll retry it.
            if active_file_ids.contains(&file.id) || app_data.state.has_orphan(file.id).await {
                continue;
            }

            // Route to the matching *arr category folder based on the name. The
            // base dir is passed explicitly so orphans need no persisted state.
            let category = if looks_like_episode(&file.name) {
                app_data
                    .config
                    .sonarr
                    .as_ref()
                    .and_then(|c| c.category.clone())
            } else {
                app_data
                    .config
                    .radarr
                    .as_ref()
                    .and_then(|c| c.category.clone())
            };
            let download_dir = match &category {
                Some(c) => format!("{}/{}", app_data.config.download_directory, c),
                None => app_data.config.download_directory.clone(),
            };

            let mut transfer = Transfer::from_orphan(app_data.clone(), file.id, file.name.clone());
            let targets = match transfer.get_download_targets_in(&download_dir).await {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => continue, // no downloadable (video) content
                Err(e) => {
                    warn!("{}: orphan target generation failed: {}", transfer, e);
                    continue;
                }
            };
            transfer.targets = Some(targets);

            if transfer.is_imported().await {
                // Already imported by the *arr — just clean it off put.io.
                info!("{}: orphan already imported, deleting from put.io", transfer);
                if let Err(e) = putio::delete_file(api_key, file.id).await {
                    warn!(
                        "{}: failed to delete imported orphan from put.io: {}",
                        transfer, e
                    );
                }
                continue;
            }

            info!("{}: orphan ready for download", transfer);
            // Reuse the hash already derived by `from_orphan` so there's a single
            // source of truth for the synthetic hash.
            let hash = transfer.hash.clone().unwrap_or_default();
            // Only start tracking/reporting the orphan once it's actually been
            // queued, so a failed send can't leave it advertised via torrent-get
            // as a download that never happens.
            if tx
                .send(TransferMessage::QueuedForDownload(transfer))
                .await
                .is_ok()
            {
                app_data
                    .state
                    .add_orphan(OrphanFile {
                        file_id: file.id,
                        name: file.name.clone(),
                        hash,
                        size: file.size,
                        download_dir,
                    })
                    .await;
            }
        }
    }
}
