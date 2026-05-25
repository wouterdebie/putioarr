use anyhow::Result;
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferState {
    pub hash: String,
    pub source_category: String,
    pub download_dir: String,
}

#[derive(Clone)]
pub struct StateManager {
    transfers: Arc<RwLock<HashMap<String, TransferState>>>,
}

impl StateManager {
    pub fn new() -> Self {
        Self {
            transfers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn add_transfer(&self, hash: String, category: String, download_dir: String) -> Result<()> {
        let key = hash.to_lowercase();
        let mut transfers = self.transfers.write().await;
        debug!("state: add_transfer hash={} category={} dir={}", key, category, download_dir);
        transfers.insert(
            key.clone(),
            TransferState {
                hash: key,
                source_category: category,
                download_dir,
            },
        );
        Ok(())
    }

    pub async fn get_transfer(&self, hash: &str) -> Option<TransferState> {
        let transfers = self.transfers.read().await;
        transfers.get(&hash.to_lowercase()).cloned()
    }

    pub async fn remove_transfer(&self, hash: &str) -> Result<()> {
        let mut transfers = self.transfers.write().await;
        transfers.remove(&hash.to_lowercase());
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