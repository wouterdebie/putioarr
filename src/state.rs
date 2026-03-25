use anyhow::Result;
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
        let mut transfers = self.transfers.write().await;
        transfers.insert(
            hash.clone(),
            TransferState {
                hash,
                source_category: category,
                download_dir,
            },
        );
        Ok(())
    }

    pub async fn get_transfer(&self, hash: &str) -> Option<TransferState> {
        let transfers = self.transfers.read().await;
        transfers.get(hash).cloned()
    }

    pub async fn remove_transfer(&self, hash: &str) -> Result<()> {
        let mut transfers = self.transfers.write().await;
        transfers.remove(hash);
        Ok(())
    }

    pub async fn get_download_dir_for_transfer(&self, hash: &str, default_dir: &str) -> String {
        if let Some(state) = self.get_transfer(hash).await {
            state.download_dir
        } else {
            default_dir.to_string()
        }
    }
}