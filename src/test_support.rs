//! Helpers shared by the unit tests.

use crate::AppData;
use actix_web::web::Data;
use figment::{
    providers::{Format, Toml},
    Figment,
};

/// AppData downloading into `download_directory`, with no *arr configured so
/// nothing calls one.
pub fn app_data_in(download_directory: &str) -> Data<AppData> {
    app_data_with(download_directory, "")
}

/// Like [`app_data_in`], with `extra` appended to the config, such as an
/// *arr's section.
pub fn app_data_with(download_directory: &str, extra: &str) -> Data<AppData> {
    let config: crate::Config = Figment::new()
        .merge(Toml::string(&format!(
            r#"
            bind_address = "0.0.0.0"
            download_directory = "{download_directory}"
            download_workers = 1
            loglevel = "info"
            orchestration_workers = 1
            password = "p"
            polling_interval = 10
            port = 9091
            skip_directories = ["sample"]
            uid = 1000
            username = "u"
            [putio]
            api_key = "k"
            {extra}
            "#
        )))
        .extract()
        .unwrap();
    Data::new(AppData {
        config,
        state: crate::state::StateManager::new("k".to_string()),
        http: reqwest::Client::new(),
    })
}
