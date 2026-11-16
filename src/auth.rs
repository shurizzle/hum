use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use librespot_core::{Session, SessionConfig, authentication::Credentials, cache::Cache};
use librespot_oauth::{OAuthClient, OAuthClientBuilder, OAuthToken};
use serde::{Deserialize, Serialize};

const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

// librespot's keymaster app: its session tokens unlock the internal (pathfinder) APIs.
const SESSION_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const SESSION_SCOPES: &[&str] = &[
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "user-library-read",
    "user-read-email",
    "user-read-private",
];

// hum's own developer app: Web Playback SDK and Web API. The keymaster id is rate limited there (429).
const WEB_CLIENT_ID: &str = "7bd5b13670b74564b1de554c858d469e";
const WEB_SCOPES: &[&str] = &[
    "streaming",
    "user-read-email",
    "user-read-private",
    "user-read-playback-state",
    "user-modify-playback-state",
];

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("hum")
}

pub fn cache() -> Result<Cache> {
    let cfg = config_dir();
    Ok(Cache::new(Some(&cfg), Some(&cfg), None, None)?)
}

fn web_token_path() -> PathBuf {
    config_dir().join("web-token.json")
}

#[derive(Serialize, Deserialize)]
struct WebToken {
    access_token: String,
    refresh_token: String,
    /// Unix seconds.
    expires_at: u64,
    /// Granted scopes; a login is redone when `WEB_SCOPES` grows.
    #[serde(default)]
    scopes: Vec<String>,
}

impl From<OAuthToken> for WebToken {
    fn from(t: OAuthToken) -> Self {
        let left = t.expires_at.saturating_duration_since(std::time::Instant::now());
        Self { access_token: t.access_token, refresh_token: t.refresh_token, expires_at: now() + left.as_secs(), scopes: t.scopes }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn oauth(client_id: &str, scopes: &[&str]) -> Result<OAuthClient> {
    Ok(OAuthClientBuilder::new(client_id, REDIRECT_URI, scopes.to_vec())
        .open_in_browser()
        .with_custom_message("<h1>hum: logged in, return to the terminal.</h1>")
        .build()?)
}

/// Runs the browser OAuth flows once: session credentials and the web token.
/// Must run in the foreground process: the daemon has no terminal or browser.
pub async fn ensure_login() -> Result<()> {
    let cache = cache()?;
    if cache.credentials().is_none() {
        let token = oauth(SESSION_CLIENT_ID, SESSION_SCOPES)?.get_access_token_async().await.context("oauth login")?;
        // Connecting with store=true swaps the short-lived token for stored credentials.
        let session = Session::new(SessionConfig::default(), Some(cache));
        session
            .connect(Credentials::with_access_token(token.access_token), true)
            .await
            .context("spotify login")?;
        session.shutdown();
    }
    let granted: Option<WebToken> = std::fs::read(web_token_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if !granted.is_some_and(|t| WEB_SCOPES.iter().all(|s| t.scopes.iter().any(|g| g == s))) {
        let token = oauth(WEB_CLIENT_ID, WEB_SCOPES)?.get_access_token_async().await.context("web login")?;
        save(&token.into())?;
    }
    Ok(())
}

fn save(t: &WebToken) -> Result<()> {
    std::fs::write(web_token_path(), serde_json::to_vec(t)?)?;
    Ok(())
}

/// Access token of hum's developer app, refreshed when it expires within a minute.
pub async fn web_token() -> Result<String> {
    let path = web_token_path();
    let mut t: WebToken = serde_json::from_slice(&std::fs::read(&path).context("not logged in: run `hum` first")?)?;
    if t.expires_at > now() + Duration::from_secs(60).as_secs() {
        return Ok(t.access_token);
    }
    let fresh: WebToken = oauth(WEB_CLIENT_ID, WEB_SCOPES)?.refresh_token_async(&t.refresh_token).await.context("refresh web token")?.into();
    // Spotify may omit the refresh token on refresh; the old one stays valid then.
    let refresh_token = if fresh.refresh_token.is_empty() { t.refresh_token } else { fresh.refresh_token };
    let scopes = if fresh.scopes.is_empty() { t.scopes } else { fresh.scopes };
    t = WebToken { refresh_token, scopes, ..fresh };
    save(&t)?;
    Ok(t.access_token)
}
