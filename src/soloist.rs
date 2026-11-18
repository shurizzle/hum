//! Spotify Soloist backend: the official headless Connect player, an alternative to the ECS engine.
//!
//! Pairing (`setup`, client side) mints a DPoP-bound authorization code for Soloist's client on
//! hum's librespot session via an RFC 8693 token-exchange, then hands it to Soloist over its
//! zeroconf `addUser`. No browser, no device picker. The runtime (`run`, daemon side) spawns Soloist
//! with its WebSocket API and bridges it to the player's command/state channels. See docs/playback.md.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use librespot_core::{Session, SessionConfig};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpStream,
    process::Command,
    sync::{mpsc, watch},
};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{
    auth,
    cache::Cache,
    proto::{self, NowPlaying},
};

/// Device name Soloist registers as, and that `run` looks up on the Web API.
const DEVICE_NAME: &str = "hum";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const VOLUME_KEY: &str = "volume";
/// Soloist exits 10 when its build has expired (90 days after its build date).
const EXIT_EXPIRED: i32 = 10;

/// Soloist binary: `$HUM_SOLOIST`, else `soloist` found on `PATH`.
pub fn binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HUM_SOLOIST") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("soloist")).find(|p| p.is_file())
}

/// Soloist session + state directory (`-D`): `<config>/soloist`.
pub fn data_dir() -> PathBuf {
    auth::config_dir().join("soloist")
}

fn key_path() -> PathBuf {
    auth::config_dir().join("soloist-key")
}

/// The stored developer API key, written only after a successful pairing.
pub fn key() -> Option<String> {
    std::fs::read_to_string(key_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Pairing (client side).
// ---------------------------------------------------------------------------------------------

/// Pairs Soloist automatically: prompts for the API key, runs `soloist --pair`, mints a DPoP-bound
/// auth code on the librespot session and delivers it over zeroconf. On success writes the key so
/// the daemon picks Soloist as its engine. Runs in the foreground (needs stdin and the session).
pub async fn setup(bin: &Path) -> Result<()> {
    let _ = open::that_in_background("https://developer.spotify.com/dashboard/soloist");
    eprintln!("hum: paste your Spotify Soloist API key (https://developer.spotify.com/dashboard/soloist):");
    let api_key = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        anyhow::Ok(s.trim().to_string())
    })
    .await??;
    if api_key.is_empty() {
        bail!("no API key entered");
    }

    // Session access token (subject_token) and canonical user id (addUser userName).
    let cache = auth::cache()?;
    let creds = cache.credentials().context("not logged in: run `hum` first")?;
    let session = Session::new(SessionConfig::default(), Some(cache));
    session.connect(creds, true).await.context("spotify login")?;
    let subject = session.login5().auth_token().await.context("session token")?.access_token;
    let user = session.username();
    session.shutdown();

    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    // A stale session would make Soloist skip the login we are about to drive.
    let _ = std::fs::remove_dir_all(&dir);
    let mut child = Command::new(bin)
        .args(["-n", DEVICE_NAME, "-k", &api_key, "-v", "--pair"])
        .arg("-D")
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;

    let port = zeroconf_port(&mut child).await?;
    let http = reqwest::Client::new();
    let info = get_info(&http, port).await?;
    let code = mint_code(&http, &subject, &info).await.context("mint auth code")?;
    eprintln!("hum: pairing Soloist...");
    add_user(&http, port, &user, &info.auth_info_id, &code).await.context("deliver auth code")?;

    let status = child.wait().await?;
    if !status.success() {
        bail!("Soloist pairing failed (exit {:?}); check the API key", status.code());
    }
    std::fs::write(key_path(), &api_key)?;
    set_mode_0600(&key_path());
    eprintln!("hum: Soloist paired.");
    Ok(())
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) {}

/// Reads Soloist's `-v` log (stderr) until it announces its zeroconf HTTP port.
async fn zeroconf_port(child: &mut tokio::process::Child) -> Result<u16> {
    let out = child.stderr.take().context("no soloist stderr")?;
    let mut lines = BufReader::new(out).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .context("timed out waiting for Soloist")??
            .context("Soloist exited before advertising")?;
        if let Some(rest) = line.split("ZeroConf service on port ").nth(1)
            && let Ok(port) = rest.split_whitespace().next().unwrap_or("").parse()
        {
            return Ok(port);
        }
    }
}

/// Fields hum needs from Soloist's zeroconf `getInfo`.
struct Info {
    auth_info_id: String,
    client_id: String,
    jkt: String,
    challenge: String,
}

async fn get_info(http: &reqwest::Client, port: u16) -> Result<Info> {
    let v: Value = http.get(zc_url(port)).query(&[("action", "getInfo")]).send().await?.error_for_status()?.json().await?;
    let auth_info = v["authInfo"].as_str().context("no authInfo")?;
    let ai = b64_decode(auth_info).context("bad authInfo")?;
    // AuthInfo { f1 auth_info_id, f2 client_id, f3 Info { f1 DpopBoundAuthCodeInfo { f1 jkt, f2 pkce_code_challenge } } }
    let auth_info_id = pb_str(&ai, 1).context("no auth_info_id")?;
    let client_id = pb_str(&ai, 2).context("no client_id")?;
    let info = pb_field(&ai, 3).context("no Info")?;
    let dpop = pb_field(info, 1).context("no DpopBoundAuthCodeInfo")?;
    let jkt = pb_str(dpop, 1).context("no jkt")?;
    let challenge = pb_str(dpop, 2).context("no pkce_code_challenge")?;
    Ok(Info { auth_info_id, client_id, jkt, challenge })
}

/// Mints the DPoP-bound authorization code via RFC 8693 token-exchange (bound to Soloist's jkt and
/// pkce challenge, no redirect_uri). The issued `access_token` is the code Soloist redeems.
async fn mint_code(http: &reqwest::Client, subject: &str, info: &Info) -> Result<String> {
    let dpop = Dpop::new()?;
    let form = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
        ("client_id", auth::SESSION_CLIENT_ID),
        ("subject_token", subject),
        ("subject_token_type", "urn:ietf:params:oauth:token-type:access_token"),
        ("requested_token_type", "urn:spotify:params:oauth:authorization_code"),
        ("audience", &info.client_id),
        ("resource", "urn:spotify:resources:connect"),
        ("scope", "streaming"),
        ("code_challenge", &info.challenge),
        ("code_challenge_method", "S256"),
        ("dpop_jkt", &info.jkt),
    ];
    // First attempt primes the required DPoP nonce.
    let send = |proof: String| http.post(TOKEN_URL).header("DPoP", proof).form(&form).send();
    let res = send(dpop.proof(None)?).await?;
    let (status, nonce, v) = read_token(res).await?;
    let v = if v.get("error").and_then(Value::as_str) == Some("use_dpop_nonce") {
        let nonce = nonce.context("server asked for a DPoP nonce but sent none")?;
        let res = send(dpop.proof(Some(&nonce))?).await?;
        read_token(res).await?.2
    } else if !status.is_success() {
        bail!("token-exchange failed: {}", v.get("error").and_then(Value::as_str).unwrap_or("unknown"));
    } else {
        v
    };
    v["access_token"].as_str().map(str::to_string).context("no code in token-exchange response")
}

async fn read_token(res: reqwest::Response) -> Result<(reqwest::StatusCode, Option<String>, Value)> {
    let status = res.status();
    let nonce = res.headers().get("DPoP-Nonce").and_then(|h| h.to_str().ok()).map(str::to_string);
    let v: Value = res.json().await?;
    Ok((status, nonce, v))
}

/// Delivers the auth code to Soloist as a plaintext `AuthToken` protobuf blob (no DH encryption for
/// `tokenType=auth_token`).
async fn add_user(http: &reqwest::Client, port: u16, user: &str, auth_info_id: &str, code: &str) -> Result<()> {
    // AuthToken { f1 auth_info_id, f10 DpopBoundAuthCode { f1 value } }
    let mut blob = pb_string(1, auth_info_id);
    blob.extend(pb_len(10, &pb_string(1, code)));
    let form = [
        ("action", "addUser".to_string()),
        ("userName", user.to_string()),
        ("blob", b64_encode(&blob, false)),
        ("tokenType", "auth_token".to_string()),
        ("loginId", hex(&rand16())),
        ("deviceName", DEVICE_NAME.to_string()),
        ("deviceId", hex(&rand20())),
        ("version", "2.12.0".to_string()),
    ];
    let v: Value = http.post(zc_url(port)).form(&form).send().await?.json().await?;
    if v["status"].as_i64() != Some(101) {
        bail!("Soloist addUser: {}", v);
    }
    Ok(())
}

fn zc_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/zc")
}

// ---------------------------------------------------------------------------------------------
// Runtime (daemon side).
// ---------------------------------------------------------------------------------------------

/// Supervised Soloist process bridged to the player's channels, mirroring `player::run_once`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rx: &mut mpsc::UnboundedReceiver<Value>,
    device: &watch::Sender<Option<String>>,
    state: &Arc<watch::Sender<NowPlaying>>,
    loaded: &AtomicBool,
    remote: &AtomicBool,
    bin: &Path,
    api_key: &str,
    session: &Session,
) -> Result<()> {
    let dir = data_dir();
    let saved = Cache::global().read_any(VOLUME_KEY).and_then(|b| Some(u16::from_le_bytes(b.try_into().ok()?)));
    // Soloist takes whole percents: `restoring` must be the value it will report back, or saving
    // never resumes.
    let pct = saved.map(|v| (v as f64 / u16::MAX as f64 * 100.0).round() as u32);
    let saved = pct.map(|p| (p as f64 / 100.0 * u16::MAX as f64) as u16);
    let vol = pct.map_or_else(|| "40".into(), |p| p.to_string());
    kill_stale(&dir).await;
    let port_file = dir.join("ws.port");
    let _ = std::fs::remove_file(&port_file);
    let mut child = Command::new(bin)
        .args(["-n", DEVICE_NAME, "-k", api_key, "--ws", "127.0.0.1:0", "-i", &vol])
        .arg("-D")
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;

    let port = read_ws_port(&port_file, &mut child).await?;
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (ws, _) = tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/"), stream).await.context("connect Soloist ws")?;
    let (mut sink, mut ws) = ws.split();
    sink.send(WsMessage::Text(json!({"type": "command", "command": "get_state"}).to_string().into())).await?;
    log::info!("Soloist started on ws port {port}");

    // Soloist's Web API device id, for the daemon's device-targeted calls.
    {
        let device = device.clone();
        tokio::spawn(async move {
            for _ in 0..30 {
                if let Ok(Some(id)) = lookup_device().await {
                    device.send_replace(Some(id));
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            log::warn!("Soloist device not found on the Web API");
        });
    }

    let mut last_playing = false;
    let mut restoring = saved;
    let mut filling = String::new();
    // Soloist has no mute: muting sets volume 0 and keeps here the level to restore.
    let mut muted_from: Option<u16> = None;
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { bail!("Soloist ws closed") };
                let WsMessage::Text(txt) = msg? else { continue };
                let Ok(ev) = serde_json::from_str::<Value>(&txt) else { continue };
                handle_event(&ev, state, loaded, remote, &mut last_playing, &mut restoring, muted_from.is_some());
                fill_missing(state, session, &mut filling);
            }
            cmd = rx.recv() => {
                let Some(mut cmd) = cmd else { return Ok(()) };
                if cmd["cmd"] == "mute" {
                    let on = cmd["arg"].as_bool().unwrap_or(false);
                    let vol = match (on, muted_from) {
                        (true, None) => {
                            muted_from = Some(state.borrow().volume);
                            0
                        }
                        (false, Some(v)) => {
                            muted_from = None;
                            v
                        }
                        _ => continue,
                    };
                    state.send_if_modified(|np| std::mem::replace(&mut np.muted, on) != on);
                    cmd = json!({"cmd": "volume", "arg": vol as f64 / u16::MAX as f64});
                }
                if let Some(ws_cmd) = translate(&cmd, last_playing) {
                    sink.send(WsMessage::Text(ws_cmd.to_string().into())).await?;
                }
            }
            status = child.wait() => {
                let status = status?;
                if status.code() == Some(EXIT_EXPIRED) {
                    bail!("Soloist build expired; download a new one from https://developer.spotify.com/documentation/soloist");
                }
                bail!("Soloist exited: {status:?}");
            }
        }
    }
}

/// Applies a Soloist WebSocket event to the shared state.
fn handle_event(
    ev: &Value,
    state: &watch::Sender<NowPlaying>,
    loaded: &AtomicBool,
    remote: &AtomicBool,
    last_playing: &mut bool,
    restoring: &mut Option<u16>,
    // Muted: the reported volume is the 0 hum set, not the level to show and save.
    muted: bool,
) {
    match ev["type"].as_str().unwrap_or_default() {
        "playback_state" => {
            let Some(mut s) = playback_value(ev) else { return };
            if muted && let Some(o) = s.as_object_mut() {
                o.remove("volume");
            }
            if !ev["item"]["uri"].as_str().unwrap_or_default().is_empty() {
                remote.store(false, Ordering::SeqCst);
                loaded.store(true, Ordering::SeqCst);
            }
            *last_playing = ev["status"].as_str() == Some("playing");
            let before = state.borrow().volume;
            state.send_if_modified(|np| {
                let prev = np.clone();
                crate::player::apply(np, &s);
                if np.uri == prev.uri {
                    merge(np, &prev);
                }
                *np != prev
            });
            let volume = state.borrow().volume;
            match *restoring {
                Some(v) if volume.abs_diff(v) <= 1 => *restoring = None,
                Some(_) => {}
                None if volume != before => Cache::global().write(VOLUME_KEY, &volume.to_le_bytes()),
                None => {}
            }
        }
        "volume_changed" if muted => {}
        "volume_changed" => {
            if let Some(vol) = ev["volume"].as_f64() {
                let v = (vol.clamp(0.0, 100.0) / 100.0 * u16::MAX as f64) as u16;
                state.send_if_modified(|np| {
                    let changed = np.volume != v;
                    np.volume = v;
                    changed
                });
                if restoring.is_none() {
                    Cache::global().write(VOLUME_KEY, &v.to_le_bytes());
                }
            }
        }
        _ => {}
    }
}

/// Soloist reports a restored, not yet played track with partial decorations (no artist names,
/// zero duration): keeps what the state already had for that uri and fetches the rest once.
fn fill_missing(state: &Arc<watch::Sender<NowPlaying>>, session: &Session, filling: &mut String) {
    let np = state.borrow().clone();
    if np.uri.is_empty() || (!np.artists.is_empty() && np.duration_ms > 0) || *filling == np.uri {
        return;
    }
    *filling = np.uri.clone();
    let (state, session) = (Arc::clone(state), session.clone());
    tokio::spawn(async move {
        let track = match crate::meta::tracks(&session, std::slice::from_ref(&np.uri)).await {
            Ok(mut t) => t.pop(),
            Err(e) => return log::warn!("soloist metadata: {e:#}"),
        };
        let Some(t) = track else { return };
        state.send_if_modified(|s| s.uri == t.uri && merge(s, &t.now_playing()));
    });
}

/// Fills `np`'s empty track fields from `from` (same uri); true if anything changed.
fn merge(np: &mut NowPlaying, from: &NowPlaying) -> bool {
    let before = np.clone();
    for (dst, src) in [(&mut np.name, &from.name), (&mut np.artists, &from.artists), (&mut np.album, &from.album), (&mut np.cover, &from.cover)] {
        if dst.is_empty() {
            dst.clone_from(src);
        }
    }
    if np.duration_ms == 0 {
        np.duration_ms = from.duration_ms;
    }
    *np != before
}

/// Builds the `player::apply` value from a Soloist `playback_state` event.
fn playback_value(ev: &Value) -> Option<Value> {
    let item = &ev["item"];
    let uri = item["uri"].as_str()?;
    let dec = &item["decorations"];
    let name = dec["identity"]["name"].as_str().unwrap_or_default();
    let artists: Vec<&str> = dec["creators"].as_array().into_iter().flatten().filter_map(|c| c["entity"]["decorations"]["identity"]["name"].as_str()).collect();
    let album = dec["parent"]["entity"]["decorations"]["identity"]["name"].as_str().unwrap_or_default();
    let cover = dec["visual_identity"]["cover"].as_array().and_then(|c| c.last()).and_then(|c| c["url"].as_str()).unwrap_or_default();
    let duration = dec["playback"]["duration_ms"].as_f64().unwrap_or(0.0);
    let playing = ev["status"].as_str() == Some("playing");
    // Extrapolate the reported position to now.
    let pos = &ev["position"];
    let mut position = pos["position_ms"].as_f64().unwrap_or(0.0);
    let speed = pos["speed"].as_f64().unwrap_or(0.0);
    if playing && speed > 0.0 {
        let ts = pos["timestamp_ms"].as_f64().unwrap_or(0.0);
        position += (proto::now_ms() as f64 - ts).max(0.0) * speed;
    }
    Some(json!({
        "uri": uri,
        "name": name,
        "artists": artists.join(", "),
        "album": album,
        "cover": cover,
        "context": ev["context"]["uri"].as_str().unwrap_or_default(),
        "duration_ms": duration,
        "position_ms": position.max(0.0),
        "playing": playing,
        "volume": ev["volume"].as_f64().unwrap_or(0.0) / 100.0,
    }))
}

/// Translates a daemon `{cmd, arg}` into a Soloist WebSocket command.
fn translate(cmd: &Value, last_playing: bool) -> Option<Value> {
    let c = |command: &str| json!({"type": "command", "command": command});
    match cmd["cmd"].as_str()? {
        "toggle" => Some(c(if last_playing { "pause" } else { "play" })),
        "next" => Some(c("skip_next")),
        "prev" => Some(c("skip_prev")),
        "seek" => Some(json!({"type": "command", "command": "seek", "position_ms": cmd["arg"].as_f64()? as u64})),
        "volume" => Some(json!({"type": "command", "command": "set_volume", "volume": (cmd["arg"].as_f64()? * 100.0).round() as u64})),
        _ => None,
    }
}

/// Soloist's device id on the Web API, matched by name.
async fn lookup_device() -> Result<Option<String>> {
    let token = auth::web_token().await?;
    let v: Value = reqwest::Client::new().get("https://api.spotify.com/v1/me/player/devices").bearer_auth(token).send().await?.error_for_status()?.json().await?;
    Ok(v["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["name"].as_str() == Some(DEVICE_NAME))
        .and_then(|d| d["id"].as_str())
        .map(str::to_string))
}

/// Kills a Soloist left by a daemon that died without reaping it: it keeps `<dir>/soloist.pid`, so
/// a new instance on the same directory never serves, and hum would show no state (volume 0).
async fn kill_stale(dir: &Path) {
    let Ok(pid) = std::fs::read_to_string(dir.join("soloist.pid")) else { return };
    let pid = pid.trim();
    let alive = |out: std::io::Result<std::process::Output>| out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("soloist"));
    if !alive(Command::new("ps").args(["-p", pid, "-o", "comm="]).output().await) {
        return;
    }
    log::warn!("killing stale Soloist {pid}");
    let _ = Command::new("kill").args(["-9", pid]).status().await;
    for _ in 0..20 {
        if !alive(Command::new("ps").args(["-p", pid, "-o", "comm="]).output().await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn read_ws_port(port_file: &Path, child: &mut tokio::process::Child) -> Result<u16> {
    for _ in 0..100 {
        if let Some(status) = child.try_wait()? {
            if status.code() == Some(EXIT_EXPIRED) {
                bail!("Soloist build expired; download a new one from https://developer.spotify.com/documentation/soloist");
            }
            bail!("Soloist exited before serving: {status:?}");
        }
        if let Ok(s) = std::fs::read_to_string(port_file)
            && let Ok(port) = s.trim().parse()
        {
            return Ok(port);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("Soloist did not open its ws port")
}

// ---------------------------------------------------------------------------------------------
// DPoP (ES256) and encoding helpers.
// ---------------------------------------------------------------------------------------------

/// A throwaway P-256 key that signs the DPoP proof authenticating the token-exchange request.
struct Dpop {
    key: ring::signature::EcdsaKeyPair,
    rng: ring::rand::SystemRandom,
    jwk: Value,
}

impl Dpop {
    fn new() -> Result<Self> {
        let rng = ring::rand::SystemRandom::new();
        let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng).map_err(|_| anyhow::anyhow!("dpop keygen"))?;
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).map_err(|_| anyhow::anyhow!("dpop key"))?;
        let pk = ring::signature::KeyPair::public_key(&key).as_ref().to_vec();
        // Uncompressed point: 0x04 || x(32) || y(32).
        anyhow::ensure!(pk.len() == 65 && pk[0] == 4, "unexpected dpop public key");
        let jwk = json!({"kty": "EC", "crv": "P-256", "x": b64_encode(&pk[1..33], true), "y": b64_encode(&pk[33..65], true)});
        Ok(Self { key, rng, jwk })
    }

    /// A compact ES256 JWT DPoP proof for `POST <TOKEN_URL>`, with the server nonce once known.
    fn proof(&self, nonce: Option<&str>) -> Result<String> {
        let header = json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": self.jwk});
        let mut payload = json!({
            "jti": b64_encode(&rand16(), true),
            "htm": "POST",
            "htu": TOKEN_URL,
            "iat": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
        });
        if let Some(n) = nonce {
            payload["nonce"] = json!(n);
        }
        let signing_input = format!("{}.{}", b64_encode(header.to_string().as_bytes(), true), b64_encode(payload.to_string().as_bytes(), true));
        let sig = self.key.sign(&self.rng, signing_input.as_bytes()).map_err(|_| anyhow::anyhow!("dpop sign"))?;
        Ok(format!("{signing_input}.{}", b64_encode(sig.as_ref(), true)))
    }
}

fn rand16() -> [u8; 16] {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b
}

fn rand20() -> [u8; 20] {
    let mut b = [0u8; 20];
    fill_random(&mut b);
    b
}

fn fill_random(buf: &mut [u8]) {
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new().fill(buf).expect("system rng");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// --- protobuf (hand-rolled, two wire types) ---

/// First length-delimited field `fnum` (wire type 2) in `buf`.
fn pb_field(buf: &[u8], fnum: u8) -> Option<&[u8]> {
    let mut i = 0;
    while i < buf.len() {
        let tag = buf[i];
        i += 1;
        let (field, wire) = (tag >> 3, tag & 7);
        match wire {
            2 => {
                let (len, n) = pb_varint(&buf[i..])?;
                i += n;
                let v = buf.get(i..i + len as usize)?;
                i += len as usize;
                if field == fnum {
                    return Some(v);
                }
            }
            0 => i += pb_varint(&buf[i..])?.1,
            _ => return None,
        }
    }
    None
}

fn pb_str(buf: &[u8], fnum: u8) -> Option<String> {
    Some(String::from_utf8_lossy(pb_field(buf, fnum)?).into_owned())
}

fn pb_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let (mut n, mut shift, mut i) = (0u64, 0, 0);
    loop {
        let b = *buf.get(i)?;
        n |= ((b & 0x7f) as u64) << shift;
        i += 1;
        if b & 0x80 == 0 {
            return Some((n, i));
        }
        shift += 7;
    }
}

fn pb_put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        out.push(if n != 0 { b | 0x80 } else { b });
        if n == 0 {
            return;
        }
    }
}

/// A length-delimited field (wire type 2).
fn pb_len(fnum: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![fnum << 3 | 2];
    pb_put_varint(&mut out, data.len() as u64);
    out.extend_from_slice(data);
    out
}

fn pb_string(fnum: u8, s: &str) -> Vec<u8> {
    pb_len(fnum, s.as_bytes())
}

// --- base64 ---

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64 encode; `url` picks the URL-safe alphabet with no padding.
fn b64_encode(data: &[u8], url: bool) -> String {
    let abc = if url { URL } else { STD };
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize;
        out.push(abc[n >> 18 & 63] as char);
        out.push(abc[n >> 12 & 63] as char);
        if c.len() > 1 {
            out.push(abc[n >> 6 & 63] as char);
        } else if !url {
            out.push('=');
        }
        if c.len() > 2 {
            out.push(abc[n & 63] as char);
        } else if !url {
            out.push('=');
        }
    }
    out
}

/// Base64 decode, standard alphabet, padding optional.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=' && !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_and_known() {
        assert_eq!(b64_encode(b"hum", false), "aHVt");
        assert_eq!(b64_encode(b"hi", false), "aGk=");
        assert_eq!(b64_encode(&[0xfb, 0xff], true), "-_8");
        assert_eq!(b64_decode("aHVt").unwrap(), b"hum");
        assert_eq!(b64_decode("aGk=").unwrap(), b"hi");
    }

    #[test]
    fn protobuf_authtoken_shape() {
        // AuthToken { f1 = "id", f10 { f1 = "code" } }
        let mut blob = pb_string(1, "id");
        blob.extend(pb_len(10, &pb_string(1, "code")));
        assert_eq!(pb_str(&blob, 1).as_deref(), Some("id"));
        let inner = pb_field(&blob, 10).unwrap();
        assert_eq!(pb_str(inner, 1).as_deref(), Some("code"));
    }

    #[test]
    fn dpop_proof_is_three_part_jwt() {
        let d = Dpop::new().unwrap();
        let proof = d.proof(Some("nonce")).unwrap();
        assert_eq!(proof.split('.').count(), 3);
    }

    #[test]
    fn translate_commands() {
        assert_eq!(translate(&json!({"cmd": "toggle"}), true).unwrap()["command"], "pause");
        assert_eq!(translate(&json!({"cmd": "toggle"}), false).unwrap()["command"], "play");
        assert_eq!(translate(&json!({"cmd": "seek", "arg": 1500}), false).unwrap()["position_ms"], 1500);
        assert_eq!(translate(&json!({"cmd": "volume", "arg": 0.5}), false).unwrap()["volume"], 50);
    }
}
