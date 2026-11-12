//! Native device-code sign-in. The wire protocol follows OpenAI Codex's
//! Apache-2.0 device_code_auth.rs; existing OpenAI notices apply (see NOTICE).
//! Device secrets and tokens never enter service status, events or diagnostics.
use super::lib::{get_auth_file, write_auth_json, AuthDotJson, TokenData};
use base64::Engine;
use chrono::Utc;
use reqwest::{Client, Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io, path::Path, time::Duration};
use tokio::time::{timeout_at, Instant};

const ISSUER: &str = "https://auth.openai.com";
const LIFETIME: Duration = Duration::from_secs(15 * 60);
const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Serialize)]
pub struct DeviceCodePrompt {
    pub verification_url: String,
    pub user_code: String,
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Value,
    #[serde(default)]
    expires_in: Value,
}

#[derive(Deserialize)]
struct Authorization {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct Tokens {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

fn seconds(value: &Value, fallback: u64) -> io::Result<u64> {
    if value.is_null() {
        return Ok(fallback);
    }
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|v| *v > 0 && *v <= 900)
        .ok_or_else(|| io::Error::other("Invalid device-code timing response"))
}

async fn response_json<T: DeserializeOwned>(mut response: Response) -> io::Result<T> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(io::Error::other(
                "Device-code response exceeds the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    // A serde error can contain a returned credential value; expose no raw body.
    serde_json::from_slice(&body).map_err(|_| io::Error::other("Invalid device-code response"))
}

fn account_id(jwt: &str) -> Option<String> {
    let body = jwt.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body)
        .ok()?;
    let claims: Value = serde_json::from_slice(&raw).ok()?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .map(str::to_owned)
}

fn persist_tokens(destination: &Path, tokens: Tokens) -> io::Result<()> {
    let account_id = account_id(&tokens.id_token)
        .or_else(|| account_id(&tokens.access_token))
        .ok_or_else(|| io::Error::other("Device login returned no ChatGPT account identity"))?;
    if tokens.access_token.trim().is_empty() || tokens.refresh_token.trim().is_empty() {
        return Err(io::Error::other(
            "Device login returned incomplete subscription credentials",
        ));
    }
    let id_token = super::token_data::parse_id_token(&tokens.id_token)
        .map_err(|_| io::Error::other("Device login returned an invalid ID token"))?;
    let auth = AuthDotJson {
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            account_id: Some(account_id),
        }),
        last_refresh: Some(Utc::now()),
    };
    write_auth_json(&get_auth_file(destination), &auth, Some(&tokens.id_token))
}

pub async fn staged_login(destination: &Path) -> io::Result<tempfile::TempDir> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(io::Error::other)?;
    staged_login_with(
        &client,
        ISSUER,
        destination,
        Duration::from_secs(1),
        |prompt| {
            super::lib::publish_device_code(prompt);
        },
    )
    .await
}

async fn staged_login_with(
    client: &Client,
    issuer: &str,
    destination: &Path,
    minimum_interval: Duration,
    publish: impl FnOnce(DeviceCodePrompt),
) -> io::Result<tempfile::TempDir> {
    std::fs::create_dir_all(destination)?;
    // Same-directory staging also proves writability before asking for consent.
    let staged = tempfile::tempdir_in(destination)?;
    let response = client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": super::lib::CLIENT_ID}))
        .send()
        .await
        .map_err(|_| {
            io::Error::other("Could not request a device code; check the network/proxy")
        })?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(io::Error::other(
            if matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
            ) {
                "Device-code login is unavailable. Enable it in ChatGPT security settings or workspace permissions, or use browser sign-in".to_owned()
            } else {
                format!("Device-code request failed (HTTP {status})")
            },
        ));
    }
    let code: UserCode = response_json(response).await?;
    if code.user_code.is_empty()
        || code.user_code.len() > 64
        || !code
            .user_code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || code.device_auth_id.trim().is_empty()
        || code.device_auth_id.len() > 1024
    {
        return Err(io::Error::other(
            "Invalid device code returned by the provider",
        ));
    }
    let lifetime = Duration::from_secs(seconds(&code.expires_in, LIFETIME.as_secs())?);
    let deadline = Instant::now() + lifetime;
    let mut interval = Duration::from_secs(seconds(&code.interval, 5)?).max(minimum_interval);
    // Test fixtures can poll promptly; production always respects provider seconds.
    if minimum_interval < Duration::from_secs(1) {
        interval = minimum_interval;
    }
    publish(DeviceCodePrompt {
        verification_url: format!("{issuer}/codex/device"),
        user_code: code.user_code.clone(),
        expires_at: Utc::now().timestamp() + lifetime.as_secs() as i64,
    });
    let exchange = async {
        let authorization = loop {
            let response = client
                .post(format!("{issuer}/api/accounts/deviceauth/token"))
                .json(&json!({"device_auth_id": code.device_auth_id, "user_code": code.user_code}))
                .send()
                .await
                .map_err(|_| {
                    io::Error::other("Device-code polling failed; check the network/proxy")
                })?;
            let status = response.status();
            if status.is_success() {
                break response_json::<Authorization>(response).await?;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok());
                interval = interval
                    .saturating_add(Duration::from_secs(5))
                    .max(Duration::from_secs(retry.unwrap_or(0)));
            } else if status != StatusCode::FORBIDDEN && status != StatusCode::NOT_FOUND {
                return Err(io::Error::other(format!("Device-code authorization failed (HTTP {status}); request a new code or use browser sign-in")));
            }
            tokio::time::sleep(interval).await;
        };
        if authorization.authorization_code.trim().is_empty()
            || authorization.code_verifier.trim().is_empty()
        {
            return Err(io::Error::other(
                "Device-code authorization returned incomplete credentials",
            ));
        }
        let response = client
            .post(format!("{issuer}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", super::lib::CLIENT_ID),
                ("code", &authorization.authorization_code),
                ("code_verifier", &authorization.code_verifier),
                ("redirect_uri", &format!("{issuer}/deviceauth/callback")),
            ])
            .send()
            .await
            .map_err(|_| {
                io::Error::other("Device-code token exchange failed; check the network/proxy")
            })?;
        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "Device-code token exchange failed (HTTP {})",
                response.status()
            )));
        }
        let tokens = response_json::<Tokens>(response).await?;
        persist_tokens(staged.path(), tokens)?;
        Ok(())
    };
    timeout_at(deadline, exchange)
        .await
        .map_err(|_| io::Error::other("Device code expired; start sign-in again"))??;
    Ok(staged)
}

#[cfg(test)]
#[path = "device_code_tests.rs"]
mod tests;
