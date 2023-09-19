use crate::{
    handlers::{handle_torrent_add, handle_torrent_get, handle_torrent_remove},
    putio,
    transmission::{TransmissionConfig, TransmissionRequest, TransmissionResponse},
    AppData,
};
use actix_web::{
    get,
    http::header::{ContentType, Header},
    post, web, HttpRequest, HttpResponse,
};
use actix_web_httpauth::headers::authorization::{Authorization, Basic};
use anyhow::{Context, Result};
use serde_json::json;

#[post("/transmission/rpc")]
pub(crate) async fn rpc_post(
    payload: web::Json<TransmissionRequest>,
    req: HttpRequest,
    app_data: web::Data<AppData>,
) -> HttpResponse {
    let api_token = &app_data.api_token;

    // Not sure if necessary since we might just look at the session id.
    if validate_user(req).await.is_err() {
        return HttpResponse::Conflict()
            .content_type(ContentType::json())
            .insert_header(("X-Transmission-Session-Id", "useless-session-id"))
            .body("");
    }

    let arguments = match payload.method.as_str() {
        "session-get" => Some(json!(TransmissionConfig {
            download_dir: app_data.download_dir.clone(),
            ..Default::default()
        })),
        "torrent-get" => handle_torrent_get(api_token, &app_data).await,
        "torrent-set" => None, // Nothing to do here
        "queue-move-top" => None,
        "torrent-remove" => handle_torrent_remove(api_token, &payload).await,
        "torrent-add" => handle_torrent_add(api_token, &payload).await,
        _ => panic!("Unknwon method {}", payload.method),
    };

    let response = TransmissionResponse {
        result: String::from("success"),
        arguments,
    };

    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(response)
}

/// Pretty much only used for authentication.
#[get("/transmission/rpc")]
async fn rpc_get(req: HttpRequest) -> HttpResponse {
    if validate_user(req).await.is_err() {
        return HttpResponse::Forbidden().body("forbidden");
    }

    HttpResponse::Conflict()
        .content_type(ContentType::json())
        .insert_header(("X-Transmission-Session-Id", "useless-session-id"))
        .body("")
    // HttpResponse::Ok().body("Hello world!")
}
async fn validate_user(req: HttpRequest) -> Result<()> {
    let auth = Authorization::<Basic>::parse(&req)?;
    let api_key = auth.as_ref().password().context("No password given")?;
    putio::account_info(api_key).await?;
    Ok(())
}
