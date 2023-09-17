use std::{collections::HashMap, path::Path};

use async_channel::{Receiver, Sender};
use log::info;
use serde_json::json;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

use crate::downloader::Download;

pub(crate) struct AppData {
    pub api_token: String,
    pub state_file: String,
    pub downloads: Mutex<HashMap<String, Download>>,
    pub tx: Sender<bool>,
    pub rx: Receiver<bool>,
    pub download_dir: String,
    pub uid: u32
}

impl AppData {
    pub async fn new(
        api_token: String,
        state_file: String,
        download_directory: String,
        uid: u32
    ) -> anyhow::Result<Self> {
        // Load state if file exists
        info!("Loading state..");
        let downloads = if Path::new(&state_file).exists() {
            let mut file = File::open(&state_file).await?;
            let mut data = String::new();
            file.read_to_string(&mut data).await?;
            serde_json::from_str(&data).unwrap()
        } else {
            HashMap::<String, Download>::new()
        };

        let (tx, rx) = async_channel::bounded(10);

        Ok(AppData {
            api_token,
            state_file,
            downloads: Mutex::new(downloads),
            tx,
            rx,
            download_dir: download_directory,
            uid
        })
    }

    pub async fn save(&self) -> anyhow::Result<()> {
        info!("Saving state at {}", &self.state_file);
        let data = &self.downloads;
        let s = json!(&*data.lock().await).to_string();
        let mut file = File::create(&self.state_file).await.unwrap();
        file.write_all(s.as_bytes()).await.unwrap();
        file.flush().await.unwrap();

        Ok(())
    }
}
