pub struct AppData {
    pub api_token: String,
    pub state_file: String,
    pub download_dir: String,
    pub uid: u32,
}

impl AppData {
    pub async fn new(
        api_token: String,
        state_file: String,
        download_directory: String,
        uid: u32,
    ) -> anyhow::Result<Self> {
        Ok(AppData {
            api_token,
            state_file,
            download_dir: download_directory,
            uid,
        })
    }
}
