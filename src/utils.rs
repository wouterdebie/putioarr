use anyhow::Result;
use log::debug;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, hash::Hash, path::Path};
use tokio::{fs::File, io::AsyncWriteExt, sync::Mutex};

#[derive(Debug)]
pub struct MutexMap<K, V> {
    inner: Mutex<HashMap<K, V>>,
    save_path: String,
}

impl<K, V> MutexMap<K, V>
where
    K: Serialize + for<'a> Deserialize<'a>,
    V: Serialize + for<'a> Deserialize<'a>,
{
    pub async fn new(save_path: &str) -> Self {
        let inner = if Path::new(save_path).exists() {
            let data = String::from_utf8(tokio::fs::read(save_path).await.unwrap()).unwrap();
            let des = serde_json::from_str::<HashMap<K, V>>(&data).unwrap();
            Mutex::new(des)
        } else {
            Mutex::new(HashMap::<K, V>::new())
        };

        Self {
            inner,
            save_path: save_path.to_string(),
        }
    }

    pub async fn contains_key(&self, x: &K) -> bool {
        self.inner.lock().await.contains_key(x)
    }

    pub async fn insert(&self, key: K, value: V) {
        self.inner.lock().await.insert(value);
    }

    pub async fn remove(&self, value: &K) {
        let mut download_list = self.inner.lock().await;
        download_list.retain(|x| *x != *value);
    }

    pub async fn save(&self) -> Result<()> {
        let data = json!(&*self.inner.lock().await).to_string();
        debug!("Saving state: {}", data);
        let mut file = File::create(&self.save_path).await.unwrap();
        file.write_all(data.as_bytes()).await.unwrap();
        file.flush().await.unwrap();

        Ok(())
    }
}
