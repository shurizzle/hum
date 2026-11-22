use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use librespot_core::{Session, SessionConfig, authentication::Credentials, cache::Cache};
use librespot_oauth::{OAuthClient, OAuthClientBuilder, OAuthToken};
use serde::{Deserialize, Serialize};

const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

// librespot's keymaster app: its session tokens unlock the internal (pathfinder) APIs, and are the
// subject token of the Soloist pairing token-exchange (`soloist::mint_code`).
pub const SESSION_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const SESSION_SCOPES: &[&str] = &[
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "user-library-read",
    "user-read-email",
    "user-read-private",
];

// hum's own developer app, ECS only: Web Playback SDK and Web API. The keymaster id gets 429 there.
const WEB_CLIENT_ID: &str = "7bd5b13670b74564b1de554c858d469e";
const WEB_SCOPES: &[&str] = &[
    "streaming",
    "user-read-email",
    "user-read-private",
    "user-read-playback-state",
    "user-modify-playback-state",
    // Saving playlists to Your Library (`Api::set_saved`).
    "playlist-modify-public",
    "playlist-modify-private",
];

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("hum")
}

/// Installed data (castLabs Electron): `$XDG_DATA_HOME/hum`, `~/Library/Application Support/hum`.
#[cfg(feature = "ecs")]
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("hum")
}

/// Daemon log: `$XDG_STATE_HOME/hum`; macOS has no state dir, so the data dir.
pub fn state_dir() -> PathBuf {
    dirs::state_dir().or_else(dirs::data_dir).unwrap_or_else(std::env::temp_dir).join("hum")
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

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn oauth(client_id: &str, scopes: &[&str]) -> Result<OAuthClient> {
    Ok(OAuthClientBuilder::new(client_id, REDIRECT_URI, scopes.to_vec())
        .open_in_browser()
        .with_custom_message("<h1>hum: logged in, return to the terminal.</h1>")
        .build()?)
}

/// One app to authorize in the browser.
struct Flow {
    client_id: &'static str,
    scopes: &'static [&'static str],
    verifier: oauth2::PkceCodeVerifier,
    url: String,
}

impl Flow {
    fn new(client_id: &'static str, scopes: &'static [&'static str]) -> Self {
        let (challenge, verifier) = oauth2::PkceCodeChallenge::new_random_sha256();
        let query = [
            ("client_id", client_id),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", &scopes.join(" ")),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
        ];
        let url = reqwest::Url::parse_with_params("https://accounts.spotify.com/authorize", query).expect("static url");
        Self { client_id, scopes, verifier, url: url.into() }
    }

    /// Exchanges the authorization code for tokens (PKCE, no client secret).
    async fn exchange(self, code: &str) -> Result<WebToken> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", self.client_id),
            ("code_verifier", self.verifier.secret()),
        ];
        let res = reqwest::Client::new().post("https://accounts.spotify.com/api/token").form(&form).send().await?;
        let v: serde_json::Value = res.error_for_status()?.json().await?;
        let scopes = v["scope"].as_str().map(|s| s.split(' ').map(str::to_string).collect()).unwrap_or_else(|| self.scopes.iter().map(|s| s.to_string()).collect());
        Ok(WebToken {
            access_token: v["access_token"].as_str().context("no access token")?.to_string(),
            refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_string(),
            expires_at: now() + v["expires_in"].as_u64().unwrap_or(3600),
            scopes,
        })
    }
}

/// Authorizes every flow in a single browser tab, then sends the tab to `finish` if given.
/// Returns the codes in order.
fn authorize(urls: Vec<String>, finish: Option<&str>) -> Result<Vec<String>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:8898").context("listen on 127.0.0.1:8898")?;
    eprintln!("hum: log in to Spotify in the browser (or open {})", urls[0]);
    open::that_in_background(&urls[0]);
    collect_codes(&listener, &urls, finish)
}

/// Serves the OAuth redirects: each code but the last is answered with a redirect to the next
/// app's consent page, so the user stays in one tab; the last one redirects to `finish`, if given.
fn collect_codes(listener: &std::net::TcpListener, urls: &[String], finish: Option<&str>) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader, Write};
    let mut codes = vec![];
    while codes.len() < urls.len() {
        let (mut stream, _) = listener.accept()?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let path = line.split(' ').nth(1).unwrap_or_default();
        let Some(query) = path.strip_prefix("/login?") else {
            // favicon and the like
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
            continue;
        };
        let params: std::collections::HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{query}"))?.query_pairs().into_owned().collect();
        let Some(code) = params.get("code") else {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
            bail!("authorization refused: {}", params.get("error").map_or("unknown", |e| e.as_str()));
        };
        codes.push(code.clone());
        let reply = match urls.get(codes.len()).map(String::as_str).or(finish) {
            Some(next) => format!("HTTP/1.1 302 Found\r\nlocation: {next}\r\ncontent-length: 0\r\n\r\n"),
            None => {
                let body = "<h1>hum: logged in, return to the terminal.</h1>";
                format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{body}", body.len())
            }
        };
        let _ = stream.write_all(reply.as_bytes());
    }
    Ok(codes)
}

/// Logs in through the browser when needed: session credentials (keymaster app, for the access
/// point, login5, pathfinder and spclient) and, with ECS only, the web token (hum's app, for the
/// Web API and the SDK). The apps are authorized in one tab, which then opens the Soloist dashboard
/// when pairing comes next (`setup::ensure_engine`), so the API key is fetched in the same tab.
/// Must run in the foreground process: the daemon has no terminal or browser.
pub async fn ensure_login() -> Result<()> {
    let cache = cache()?;
    let need_session = cache.credentials().is_none();
    let granted: Option<WebToken> = std::fs::read(web_token_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
    // Soloist runs on the session token alone; only ECS (SDK and Web API) needs hum's app.
    let need_web = !crate::setup::soloist_next() && !granted.is_some_and(|t| WEB_SCOPES.iter().all(|s| t.scopes.iter().any(|g| g == s)));
    let flows: Vec<Flow> = [(need_session, SESSION_CLIENT_ID, SESSION_SCOPES), (need_web, WEB_CLIENT_ID, WEB_SCOPES)]
        .into_iter()
        .filter(|(needed, ..)| *needed)
        .map(|(_, id, scopes)| Flow::new(id, scopes))
        .collect();
    if flows.is_empty() {
        return Ok(());
    }
    let urls = flows.iter().map(|f| f.url.clone()).collect();
    let pairing = crate::setup::engine().is_none() && crate::soloist::binary().is_some();
    let codes = tokio::task::spawn_blocking(move || authorize(urls, pairing.then_some(crate::soloist::DASHBOARD))).await??;
    if pairing {
        crate::soloist::DASHBOARD_SHOWN.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    for (flow, code) in flows.into_iter().zip(codes) {
        if flow.client_id == SESSION_CLIENT_ID {
            let token = flow.exchange(&code).await.context("session login")?;
            // Connecting with store=true swaps the short-lived token for stored credentials.
            let session = Session::new(SessionConfig::default(), Some(cache.clone()));
            session.connect(Credentials::with_access_token(token.access_token), true).await.context("spotify login")?;
            session.shutdown();
        } else {
            save(&flow.exchange(&code).await.context("web login")?)?;
        }
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    fn get(port: u16, path: &str) -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        reply
    }

    #[test]
    fn codes_are_chained_in_one_tab() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let urls = vec!["https://a/auth".to_string(), "https://b/auth".to_string()];
        let server = std::thread::spawn(move || collect_codes(&listener, &urls, None));
        assert!(get(port, "/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(get(port, "/login?code=one&state=x").contains("location: https://b/auth"));
        assert!(get(port, "/login?code=two").contains("logged in"));
        assert_eq!(server.join().unwrap().unwrap(), ["one", "two"]);
    }

    #[test]
    fn last_code_redirects_to_finish() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let urls = vec!["https://a/auth".to_string()];
        let server = std::thread::spawn(move || collect_codes(&listener, &urls, Some("https://dash")));
        assert!(get(port, "/login?code=one").contains("location: https://dash"));
        assert_eq!(server.join().unwrap().unwrap(), ["one"]);
    }
}
