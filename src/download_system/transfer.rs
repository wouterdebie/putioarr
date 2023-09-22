use crate::{
    services::{
        arr,
        putio::{self, PutIOTransfer},
    },
    AppData,
};
use actix_web::web::Data;
use anyhow::Result;
use async_recursion::async_recursion;
use log::info;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone)]
pub struct Transfer {
    pub name: String,
    pub file_id: Option<u64>,
    pub transfer_id: u64,
    pub targets: Option<Vec<DownloadTarget>>,
    pub app_data: Data<AppData>,
}

impl Transfer {
    pub async fn is_imported(&self) -> bool {
        let targets = self.targets.as_ref().unwrap().clone();
        let mut check_services = Vec::<(&str, String, String)>::new();
        if let Some(a) = &self.app_data.config.sonarr {
            check_services.push(("Sonarr", a.url.clone(), a.api_key.clone()))
        }
        if let Some(a) = &self.app_data.config.radarr {
            check_services.push(("Radarr", a.url.clone(), a.api_key.clone()))
        }

        let paths = targets
            .iter()
            .filter(|t| t.target_type == TargetType::File)
            .map(|t| t.to.clone())
            .collect::<Vec<String>>();

        let mut results = Vec::<bool>::new();
        for path in paths {
            let mut service_results = vec![];
            for (service_name, url, key) in &check_services {
                let service_result = arr::check_imported(&path, key, url).await.unwrap();
                if service_result {
                    info!("Found {} imported by {}", &path, service_name);
                }
                service_results.push(service_result)
            }
            // Check if ANY of the service_results are true and put the outcome in results
            results.push(service_results.into_iter().any(|x| x));
        }
        // Check if all targets have been imported
        results.into_iter().all(|x| x)
    }

    pub async fn get_download_targets(&self) -> Result<Vec<DownloadTarget>> {
        info!("Generating targets for {}", self.name);
        recurse_download_targets(&self.app_data, self.file_id.unwrap(), None, true).await
    }

    pub fn get_top_level(&self) -> String {
        self.targets
            .clone()
            .unwrap()
            .into_iter()
            .find(|t| t.top_level)
            .unwrap()
            .to
    }

    pub fn from(app_data: Data<AppData>, transfer: &PutIOTransfer) -> Self {
        Self {
            transfer_id: transfer.id,
            name: transfer.name.clone(),
            file_id: transfer.file_id,
            targets: None,
            app_data,
        }
    }
}

#[async_recursion]
async fn recurse_download_targets(
    app_data: &Data<AppData>,
    file_id: u64,
    override_base_path: Option<String>,
    top_level: bool,
) -> Result<Vec<DownloadTarget>> {
    let base_path = override_base_path.unwrap_or(app_data.config.download_directory.clone());
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
                });

                for file in response.files {
                    targets.append(
                        &mut recurse_download_targets(
                            app_data,
                            file.id,
                            Some(new_base_path.clone()),
                            false,
                        )
                        .await?,
                    );
                }
            }
        }
        "VIDEO" => {
            // Get download URL for file
            let url = putio::url(&app_data.config.putio.api_key, response.parent.id).await?;

            targets.push(DownloadTarget {
                from: Some(url),
                target_type: TargetType::File,
                to,
                top_level,
            });
        }
        _ => {}
    }

    Ok(targets)
}

#[derive(Clone)]
pub enum TransferMessage {
    QueuedForDownload(Transfer),
    Downloaded(Transfer),
    Imported(Transfer),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadTarget {
    pub from: Option<String>,
    pub to: String,
    pub target_type: TargetType,
    pub top_level: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum TargetType {
    Directory,
    File,
}
