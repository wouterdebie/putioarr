use crate::{
    // downloader::DownloadStatus,
    putio::{self, PutIOTransfer},
    transmission::{TransmissionRequest, TransmissionTorrent},
    AppData,
};
use actix_web::web;
use base64::Engine;
use log::info;
use serde_json::json;

pub(crate) async fn handle_torrent_add(
    api_token: &str,
    payload: &web::Json<TransmissionRequest>,
) -> Option<serde_json::Value> {
    let arguments = payload.arguments.as_ref().unwrap().as_object().unwrap();
    if arguments.contains_key("metainfo") {
        // .torrent files
        let b64 = arguments["metainfo"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        putio::upload_file(api_token, bytes).await.unwrap();
    } else {
        // Magnet links
        let url = arguments["filename"].as_str().unwrap();
        putio::add_transfer(api_token, url).await.unwrap();
    };
    info!("Torrent uploaded");
    None
}

pub(crate) async fn handle_torrent_remove(
    api_token: &str,
    payload: &web::Json<TransmissionRequest>,
) -> Option<serde_json::Value> {
    // Cleanup all the unwrap stuff
    let arguments = payload.arguments.as_ref().unwrap().as_object().unwrap();
    let ids: Vec<&str> = arguments
        .get("ids")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let delete_local_data = arguments
        .get("delete-local-data")
        .unwrap()
        .as_bool()
        .unwrap();

    let putio_transfers: Vec<PutIOTransfer> = putio::list_transfers(api_token)
        .await
        .unwrap()
        .transfers
        .into_iter()
        .filter(|t| ids.contains(&t.hash.as_str()))
        .collect();

    for t in putio_transfers {
        putio::remove_transfer(api_token, t.id).await.unwrap();

        if t.userfile_exists && delete_local_data {
            putio::delete_file(api_token, t.file_id.unwrap())
                .await
                .unwrap();
        }
    }

    None
}

pub(crate) async fn handle_torrent_get(
    api_token: &str,
    app_data: &web::Data<AppData>,
) -> Option<serde_json::Value> {
    let transfers = putio::list_transfers(api_token).await.unwrap().transfers;

    let transmission_transfers = transfers.into_iter().map(|t| async {
        let mut tt: TransmissionTorrent = t.into();
        tt.download_dir = app_data.config.download_directory.clone();
        tt
    });
    let transmission_transfers: Vec<TransmissionTorrent> =
        futures::future::join_all(transmission_transfers).await;

    let torrents = json!(transmission_transfers);

    let mut arguments = serde_json::Map::new();
    arguments.insert(String::from("torrents"), torrents);

    Some(json!(arguments))
}
