use super::orchestration::{format_delay, retry_delay};
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

/// Stands in for the hash of a transfer put.io reports without one.
const NO_HASH: &str = "0000";

/// Whether the *arr imported a transfer's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    Imported,
    NotImported,
    /// An *arr that could have imported a file didn't answer, so its history
    /// can't tell either way. Counting that as "not imported" made putioarr
    /// download imported transfers again when it started before the *arr
    /// (issue #58), so callers ask again later instead.
    Unknown,
}

impl ImportStatus {
    /// One file's status, from the answers of the *arrs that could have
    /// imported it (None: that *arr didn't answer). It's imported if any of
    /// them says so, and unknown if none did but one didn't answer.
    fn from_answers(answers: &[Option<bool>]) -> Self {
        if answers.contains(&Some(true)) {
            Self::Imported
        } else if answers.contains(&None) {
            Self::Unknown
        } else {
            Self::NotImported
        }
    }

    /// A transfer's status, from those of its files: not imported if any file
    /// isn't, otherwise unknown if any file's status is.
    fn of_all(statuses: impl IntoIterator<Item = Self>) -> Self {
        let mut all = Self::Imported;
        for status in statuses {
            match status {
                Self::NotImported => return Self::NotImported,
                Self::Unknown => all = Self::Unknown,
                Self::Imported => {}
            }
        }
        all
    }
}

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
    /// How often a failed download of this transfer was retried (issue #51).
    pub retries: u32,
}

impl Transfer {
    /// The hash used as the key for per-transfer state, such as the removed
    /// marker. Download targets carry the same value as `transfer_hash`.
    pub fn hash_key(&self) -> &str {
        self.hash.as_deref().unwrap_or(NO_HASH)
    }

    /// True if the *arr imported all of this transfer's files. A file whose
    /// *arr didn't answer counts as not imported yet.
    pub async fn is_imported(&self) -> bool {
        self.import_status().await == ImportStatus::Imported
    }

    pub async fn import_status(&self) -> ImportStatus {
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

        // With no files to check, `all()` below is vacuously true, and the
        // transfer's local folder and put.io copy get deleted as "imported".
        // That happens when a transfer has only files putioarr doesn't download,
        // such as a RAR release (put.io type ARCHIVE) with download_archives
        // off. Nothing was downloaded, so nothing can have been imported.
        if targets.is_empty() {
            return ImportStatus::NotImported;
        }

        // The transfer's own folder, if it is a folder rather than a single file.
        let transfer_dir = self
            .targets
            .as_ref()
            .unwrap()
            .iter()
            .find(|t| t.top_level && t.target_type == TargetType::Directory)
            .map(|t| t.to.clone());

        let mut results = Vec::<ImportStatus>::new();
        // Archive parts in the same transfer folder share one history lookup.
        let mut dir_import_cache = std::collections::HashMap::<String, ImportStatus>::new();
        for target in targets {
            // The *arr never imports archive parts (put.io type ARCHIVE, only
            // downloaded with download_archives), only the file an extractor
            // such as Unpackerr unpacks from them, so they never show up in
            // its history under their own path. An archive counts as imported
            // once it's gone from disk (Unpackerr's delete_orig), or once the
            // *arr imported something from this transfer's folder.
            if target.media_type == Some(MediaType::Archive) {
                if !Path::new(&target.to).exists() {
                    info!("{}: archive gone (extracted)", target);
                    results.push(ImportStatus::Imported);
                    continue;
                }
                // A transfer that is a single archive file has no folder of its
                // own. Its parent is the category or download directory, which
                // nearly every import's path contains, so only its removal can
                // tell us it was extracted.
                let Some(dir) = archive_import_dir(&target, transfer_dir.as_deref()) else {
                    results.push(ImportStatus::NotImported);
                    continue;
                };
                if let Some(hit) = dir_import_cache.get(&dir) {
                    results.push(*hit);
                    continue;
                }
                let mut answers = vec![];
                for app in &apps {
                    let answer = self.answer(app, app.check_imported_dir(&dir).await).await;
                    answers.push(answer);
                    if answer == Some(true) {
                        break;
                    }
                }
                let status = ImportStatus::from_answers(&answers);
                if status == ImportStatus::Imported {
                    info!("{}: archive extracted and imported from {}", target, dir);
                }
                dir_import_cache.insert(dir, status);
                results.push(status);
                continue;
            }
            let mut answers = vec![];
            for app in &apps {
                // Only ask an *arr about files matching its media type.
                if let Some(mt) = &target.media_type {
                    if *mt != app.kind.media_type() {
                        continue;
                    }
                }
                let answer = self.answer(app, app.check_imported(&target.to).await).await;
                if answer == Some(true) {
                    info!(
                        "{}: found imported by {}",
                        target,
                        app.to_string().bright_blue()
                    );
                }
                answers.push(answer);
            }
            // If no service was eligible for this target, treat it as imported
            // (otherwise an audio file with no Lidarr would block forever).
            results.push(if answers.is_empty() {
                ImportStatus::Imported
            } else {
                ImportStatus::from_answers(&answers)
            });
        }
        ImportStatus::of_all(results)
    }

    /// An *arr's answer to a history lookup, or None if it didn't answer.
    async fn answer(&self, app: &ArrApp, result: Result<bool>) -> Option<bool> {
        match result {
            Ok(imported) => Some(imported),
            Err(e) => {
                // A misconfigured/unreachable *arr fails for every transfer on
                // every poll; throttle the log so it doesn't fill the disk over
                // time (issue #21).
                if self.app_data.state.should_log_arr_error(&app.name).await {
                    error!(
                        "Error retrieving history from {} (suppressing repeats for {:?}): {}",
                        app,
                        crate::state::StateManager::ARR_ERROR_LOG_INTERVAL,
                        e
                    );
                }
                None
            }
        }
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
        recurse_download_targets(&self.app_data, file_id, self.hash_key(), base_path, true).await
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
            retries: 0,
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
            retries: 0,
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

#[allow(clippy::double_must_use)] // the #[must_use] comes from #[async_recursion]
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
        // Archives (.rar parts, .zip, .7z) only when download_archives is on.
        // An extractor such as Unpackerr unpacks them for the *arr; see
        // is_imported for how putioarr then decides they're done.
        "ARCHIVE" if app_data.config.download_archives => {
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

/// The folder name to look for in the *arr's import history for an archive
/// target: the name of the transfer's own top-level folder, when the archive is
/// inside it. `None` when the transfer is a single archive file. Only the name
/// is compared, so an archive deeper down (e.g. in `CD1/`) still matches the
/// release folder rather than a generic subfolder name.
fn archive_import_dir(target: &DownloadTarget, transfer_dir: Option<&str>) -> Option<String> {
    let dir = Path::new(transfer_dir?);
    if !Path::new(&target.to).starts_with(dir) {
        return None;
    }
    dir.file_name().map(|n| n.to_string_lossy().into_owned())
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
    /// Multi-volume RAR parts (put.io file_type ARCHIVE).
    /// Tracked separately from Video so is_imported() can use a different
    /// completion check (local file presence, since the *arr never reports
    /// a downloadFolderImported history event for a .rNN/.rar path -- only
    /// for the video file Unpackerr extracts from it).
    Archive,
}

impl MediaType {
    pub fn from_putio(file_type: &str) -> Option<Self> {
        match file_type {
            "AUDIO" => Some(Self::Audio),
            "VIDEO" => Some(Self::Video),
            "ARCHIVE" => Some(Self::Archive),
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

/// Asks the *arrs again whether a transfer found at startup was imported, with
/// the same backoff as a failed download (1, 2, 4 ... up to 30 minutes), until
/// they answer. Then it goes on as the startup check would have sent it: to
/// seeding if imported, to download if not (issue #58). Stops if the transfer
/// leaves put.io or the *arr removes it meanwhile.
async fn recheck_import(app_data: Data<AppData>, tx: Sender<TransferMessage>, transfer: Transfer) {
    let mut attempt = 0;
    loop {
        let delay = retry_delay(attempt);
        warn!(
            "{}: couldn't ask the *arr whether this was imported; not downloading it until it \
             answers, asking again in {}",
            transfer,
            format_delay(delay)
        );
        sleep(delay).await;
        if app_data.state.is_removed(transfer.hash_key()).await {
            info!("{}: removed by the *arr", transfer);
            return;
        }
        // On a put.io error, ask the *arr anyway: a transfer that's gone fails
        // harmlessly later on.
        if let Ok(r) = putio::list_transfers(&app_data.config.putio.api_key).await {
            if !r.transfers.iter().any(|p| p.id == transfer.transfer_id) {
                info!("{}: no longer on put.io", transfer);
                return;
            }
        }
        let message = match transfer.import_status().await {
            ImportStatus::Imported => {
                info!("{}: already imported", transfer);
                TransferMessage::Imported(transfer)
            }
            ImportStatus::NotImported => {
                info!("{}: not imported yet", transfer);
                // Regenerate the targets, which also fetches fresh download URLs.
                TransferMessage::QueuedForDownload(Transfer {
                    targets: None,
                    ..transfer
                })
            }
            ImportStatus::Unknown => {
                attempt += 1;
                continue;
            }
        };
        if tx.send(message).await.is_err() {
            warn!("could not pass on a transfer: the orchestration workers are gone");
        }
        return;
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
            match transfer.import_status().await {
                ImportStatus::Imported => {
                    info!("{}: already imported", transfer);
                    seen.push(transfer.transfer_id);
                    tx.send(TransferMessage::Imported(transfer)).await?;
                }
                ImportStatus::NotImported => info!("{}: not imported yet", transfer),
                ImportStatus::Unknown => {
                    // An *arr didn't answer, as happens when it starts together
                    // with putioarr. The monitor loop below would download the
                    // transfer, imported or not, so keep it out of there and
                    // ask again until the *arr answers (issue #58).
                    seen.push(transfer.transfer_id);
                    let (app_data, tx) = (app_data.clone(), tx.clone());
                    actix_rt::spawn(async move { recheck_import(app_data, tx, transfer).await });
                }
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
            if last_orphan_scan.is_none_or(|t| t.elapsed() >= orphan_scan_interval) {
                scan_watch_folders(&app_data, &tx, &list_transfer_response.transfers).await;
                last_orphan_scan = Some(Instant::now());
            }

            // Log status when 60 seconds have passed since last time
            if start.elapsed().as_secs() >= 60 {
                info!(
                    "Active transfers: {}",
                    list_transfer_response.transfers.len()
                );
                for t in &list_transfer_response.transfers {
                    let transfer = Transfer::from(app_data.clone(), t);
                    // Report combined two-stage progress (cloud 0-50%, local pull
                    // 50-100%) so a stalled download is visible in the log (#5).
                    let pct = combined_progress_percent(&app_data, t).await;
                    info!("  {} ({}%)", transfer, pct);
                }

                start = std::time::Instant::now();
            }

            sleep(putio_check_interval).await;
        } else {
            warn!("List put.io transfers failed. Retrying..");
            continue;
        };
    }
}

/// Combined two-stage download progress (0-100) for a transfer, mirroring what
/// [`crate::http::handlers`] reports to the *arr: put.io's cloud download is the
/// first 50%, pulling the files to local disk is the second 50% (issue #5). Used
/// for the periodic status log so a stuck download is distinguishable from one
/// that's still making progress.
async fn combined_progress_percent(app_data: &Data<AppData>, t: &PutIOTransfer) -> u64 {
    if app_data.state.is_local_complete(t.id).await {
        return 100;
    }
    let size = t.size.unwrap_or(0).max(0) as u64;
    if size == 0 {
        return 0;
    }
    let putio_done = t.finished_at.is_some()
        || matches!(
            t.status.to_uppercase().as_str(),
            "SEEDING" | "COMPLETED" | "STOPPED"
        );
    if putio_done {
        let local = match &t.hash {
            Some(hash) => app_data
                .state
                .local_bytes_downloaded(hash)
                .await
                .unwrap_or(0)
                .min(size),
            None => 0,
        };
        // Cap at 99 until local_complete flips (handled by the early return
        // above): the counter can reach `size` a moment before the transfer is
        // marked complete, and logging 100% then would misreport a still-active
        // pull as finished.
        (50 + 50 * local / size).min(99)
    } else {
        let downloaded = (t.downloaded.unwrap_or(0).max(0) as u64).min(size);
        50 * downloaded / size
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
    // Only files backed by a *managed* active transfer are handled the normal
    // way by the main loop. Unmanaged active transfers (e.g. a manually added
    // put.io Watch List entry, or one whose state was lost) are skipped there,
    // so their completed files must be picked up here as orphans — otherwise
    // both paths ignore them and they are never downloaded/imported.
    let mut managed_active_file_ids: HashSet<i64> = HashSet::new();
    for t in active_transfers {
        if let Some(file_id) = t.file_id {
            if is_managed(app_data, t).await {
                managed_active_file_ids.insert(file_id);
            }
        }
    }

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
            // Skip the result of a *managed* active transfer (handled the normal
            // way) and any orphan we're already pulling. Files from unmanaged
            // active transfers fall through the main loop, so they are handled
            // here. Using `has_orphan` as the "in progress" marker keeps tracking
            // bounded and, since a failed orphan is dropped from it, lets a later
            // poll retry it.
            if managed_active_file_ids.contains(&file.id) || app_data.state.has_orphan(file.id).await {
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
                // Leave anything without a file to download (e.g. only RAR
                // parts) on put.io: it can never be imported.
                Ok(t) if t.iter().any(|t| t.target_type == TargetType::File) => t,
                Ok(_) => continue, // no downloadable (video/audio) files
                Err(e) => {
                    warn!("{}: orphan target generation failed: {}", transfer, e);
                    continue;
                }
            };
            transfer.targets = Some(targets);

            match transfer.import_status().await {
                ImportStatus::Imported => {
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
                ImportStatus::Unknown => {
                    // An *arr didn't answer. The next scan asks again, rather
                    // than pulling a file that may be imported already (#58).
                    debug!(
                        "{}: couldn't ask the *arr whether this orphan was imported, checking \
                         again next scan",
                        transfer
                    );
                    continue;
                }
                ImportStatus::NotImported => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// AppData with no *arr configured, so `is_imported` makes no HTTP calls.
    fn app_data() -> Data<AppData> {
        crate::test_support::app_data_in("/downloads")
    }

    fn target(target_type: TargetType, to: &str) -> DownloadTarget {
        DownloadTarget {
            from: None,
            to: to.to_string(),
            target_type,
            top_level: false,
            transfer_hash: "0".repeat(40),
            media_type: Some(MediaType::Video),
        }
    }

    #[actix_rt::test]
    async fn transfer_with_only_folders_is_not_imported() {
        // e.g. a RAR release: the folder is a target, the ARCHIVE parts aren't.
        let mut t = Transfer::from_orphan(app_data(), 1, "Some.Release".to_string());
        t.targets = Some(vec![target(TargetType::Directory, "/downloads/Some.Release")]);
        assert!(!t.is_imported().await);
    }

    fn archive(to: &str, top_level: bool) -> DownloadTarget {
        DownloadTarget {
            top_level,
            media_type: Some(MediaType::Archive),
            ..target(TargetType::File, to)
        }
    }

    #[test]
    fn archive_in_a_folder_matches_on_the_transfer_folder() {
        let dir = Some("/downloads/movies/Some.Release");
        let release = Some("Some.Release");
        let part = archive("/downloads/movies/Some.Release/release.part01.rar", false);
        assert_eq!(archive_import_dir(&part, dir).as_deref(), release);
        // Deeper down it still matches the release folder, not "CD1".
        let part = archive("/downloads/movies/Some.Release/CD1/cd1.rar", false);
        assert_eq!(archive_import_dir(&part, dir).as_deref(), release);
    }

    #[test]
    fn lone_archive_has_no_folder_to_match() {
        // A single-file transfer: its parent is the category folder ("movies"),
        // which almost every import path contains.
        let lone = archive("/downloads/movies/some.release.zip", true);
        assert_eq!(archive_import_dir(&lone, None), None);
    }

    #[actix_rt::test]
    async fn archive_counts_as_imported_only_once_it_is_gone() {
        let path = std::env::temp_dir().join(format!("putioarr-test-{}.zip", std::process::id()));
        std::fs::write(&path, b"zip").unwrap();
        let mut t = Transfer::from_orphan(app_data(), 1, "some.release.zip".to_string());
        t.targets = Some(vec![archive(path.to_str().unwrap(), true)]);
        assert!(!t.is_imported().await);
        std::fs::remove_file(&path).unwrap();
        assert!(t.is_imported().await);
    }

    #[test]
    fn a_file_is_imported_if_any_arr_says_so_and_unknown_if_one_did_not_answer() {
        use ImportStatus::*;
        assert_eq!(ImportStatus::from_answers(&[Some(false), Some(true)]), Imported);
        assert_eq!(ImportStatus::from_answers(&[None, Some(true)]), Imported);
        assert_eq!(ImportStatus::from_answers(&[Some(false), None]), Unknown);
        assert_eq!(ImportStatus::from_answers(&[Some(false)]), NotImported);
        // A transfer: a file that isn't imported decides it, then an unknown one.
        assert_eq!(ImportStatus::of_all([Imported, Unknown, NotImported]), NotImported);
        assert_eq!(ImportStatus::of_all([Imported, Unknown]), Unknown);
        assert_eq!(ImportStatus::of_all([Imported, Imported]), Imported);
    }

    /// AppData with Radarr at `url`, so it's asked about video files.
    fn with_radarr(url: &str) -> Data<AppData> {
        crate::test_support::app_data_with(
            "/downloads",
            &format!("[radarr]\nurl = \"{url}\"\napi_key = \"x\"\n"),
        )
    }

    /// A stand-in *arr that answers every request with `body`; returns its URL.
    fn serve(body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the request up to the end of its headers.
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        url
    }

    fn movie(app_data: Data<AppData>) -> Transfer {
        let mut t = Transfer::from_orphan(app_data, 1, "Some.Release".to_string());
        t.targets = Some(vec![
            target(TargetType::Directory, "/downloads/Some.Release"),
            target(TargetType::File, "/downloads/Some.Release/movie.mkv"),
        ]);
        t
    }

    #[actix_rt::test]
    async fn import_status_follows_the_arrs_history() {
        let imported = serve(
            r#"{"totalRecords":1,"records":[{"eventType":"downloadFolderImported",
                "data":{"droppedPath":"/downloads/Some.Release/movie.mkv"}}]}"#,
        );
        let t = movie(with_radarr(&imported));
        assert_eq!(t.import_status().await, ImportStatus::Imported);

        let empty = serve(r#"{"totalRecords":0,"records":[]}"#);
        let t = movie(with_radarr(&empty));
        assert_eq!(t.import_status().await, ImportStatus::NotImported);
    }

    #[actix_rt::test]
    async fn import_status_is_unknown_while_the_arr_does_not_answer() {
        // Nothing listens on port 1, as with an *arr that's still starting (#58).
        let t = movie(with_radarr("http://127.0.0.1:1"));
        assert_eq!(t.import_status().await, ImportStatus::Unknown);
        assert!(!t.is_imported().await);
    }

    #[actix_rt::test]
    async fn file_with_no_arr_to_ask_still_counts_as_imported() {
        // Unchanged behaviour: a file no configured *arr handles doesn't block.
        let mut t = Transfer::from_orphan(app_data(), 1, "Some.Release".to_string());
        t.targets = Some(vec![
            target(TargetType::Directory, "/downloads/Some.Release"),
            target(TargetType::File, "/downloads/Some.Release/movie.mkv"),
        ]);
        assert!(t.is_imported().await);
    }
}
