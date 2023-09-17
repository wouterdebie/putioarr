use std::collections::HashMap;

/// Returns a new OOB code.
pub async fn get() -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::get("https://api.put.io/v2/oauth2/oob/code?app_id=6487")
        .await?
        .json::<HashMap<String, String>>()
        .await?;
    let code = resp.get("code").expect("fetching OOB code");
    Ok(code.to_string())
}

/// Returns new OAuth token if the OOB code is linked to the user's account.
pub async fn check(oob_code: String) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::get(format!(
        "https://api.put.io/v2/oauth2/oob/code/{}",
        oob_code
    ))
    .await?
    .json::<HashMap<String, String>>()
    .await?;
    let token = resp.get("oauth_token").expect("deserializing OAuth token");
    Ok(token.to_string())
}
