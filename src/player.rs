use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use librespot_core::{Session, SessionConfig, dealer::protocol::Message};
use librespot_protocol::{
    connect::{
        Capabilities, Cluster, ClusterUpdate, Device, DeviceInfo, MemberType, PutStateRequest,
    },
    player::{PlayerState, ProvidedTrack},
};
use parking_lot::Mutex;
use protobuf::{EnumOrUnknown, Message as _, MessageField};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::{
    auth,
    proto::{self, NowPlaying},
};
#[cfg(feature = "ecs")]
use {
    crate::cache::Cache,
    std::{path::PathBuf, process::Stdio},
    tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        process::Command,
    },
};

#[cfg(feature = "ecs")]
const MAIN_JS: &str = include_str!("../player/main.js");
#[cfg(feature = "ecs")]
const PLAYER_HTML: &str = include_str!("../player/player.html");
const WEB_API: &str = "https://api.spotify.com/v1";
/// Context of an artist's "Popular" tracks, as the web player plays them; followed by the artist id.
const ARTIST_POPULAR: &str = "spotify:list:popular-release-segments-main-roles:artist_";
#[cfg(feature = "ecs")]
const VOLUME_KEY: &str = "volume";
/// Keeps `PUT /me/player/play` bodies well under the Web API's size limit.
const MAX_URIS: usize = 100;

/// Handle to the playback engine (ECS or Soloist), run as a supervised child process.
pub struct Player {
    tx: mpsc::UnboundedSender<Value>,
    device: watch::Receiver<Option<String>>,
    http: reqwest::Client,
    /// The daemon's session: Connect commands and transfers are sent from it.
    session: Session,
    /// Soloist: playback is driven with Connect commands on the session token instead of the Web
    /// API, so no web token is needed. ECS keeps the Web API (the SDK needs the web token anyway,
    /// and the cluster does not list the SDK's device).
    connect: bool,
    /// The SDK has a track loaded. Until then `NowPlaying` may hold the restored last playback.
    loaded: Arc<AtomicBool>,
    /// Another Connect device (web, phone) is the active one; `NowPlaying` mirrors it.
    remote: Arc<AtomicBool>,
    /// Latest cluster seen by the mirror, for Connect commands.
    cluster: Arc<Mutex<Option<Latest>>>,
}

/// What a Connect command needs: the mirror's session (its device sends the command), the active
/// device and its full player state.
#[derive(Clone)]
struct Latest {
    session: Session,
    active: String,
    state: PlayerState,
    devices: Vec<proto::Device>,
}

/// castLabs Electron version hum is tested with. Signed builds come from this repo's
/// `ecs-<version>` release (`.github/workflows/ecs.yml`); bump after trying a new one.
#[cfg(feature = "ecs")]
pub const ECS_VERSION: &str = "v45.2.1+wvcus";

/// castLabs Electron install: `$HUM_ECS`, else `<data dir>/ecs` (see docs/playback.md).
#[cfg(feature = "ecs")]
pub fn ecs_binary() -> PathBuf {
    let dir = std::env::var_os("HUM_ECS")
        .map(PathBuf::from)
        .unwrap_or_else(|| auth::data_dir().join("ecs"));
    if cfg!(target_os = "macos") {
        dir.join("Electron.app/Contents/MacOS/Electron")
    } else {
        dir.join("electron")
    }
}

impl Player {
    pub fn spawn(
        state: watch::Sender<NowPlaying>,
        engine: crate::setup::Engine,
        session: Session,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (device_tx, device) = watch::channel(None);
        let connect = matches!(engine, crate::setup::Engine::Soloist { .. });
        let loaded = Arc::new(AtomicBool::new(false));
        let state = Arc::new(state);
        let remote = Arc::default();
        let cluster = Arc::default();
        let mirror = Mirror {
            cluster: Arc::clone(&cluster),
            state: state.clone(),
            sdk_device: device.clone(),
            loaded: loaded.clone(),
            remote: Arc::clone(&remote),
        };
        tokio::spawn(async move {
            // librespot's dealer starts once per session, so every attempt gets a fresh one.
            loop {
                if let Err(e) = mirror.run().await {
                    log::warn!("connect state: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        tokio::spawn(supervise(
            rx,
            device_tx,
            state,
            loaded.clone(),
            Arc::clone(&remote),
            engine,
            session.clone(),
        ));
        Self {
            tx,
            device,
            http: reqwest::Client::new(),
            session,
            connect,
            loaded,
            remote,
            cluster,
        }
    }

    /// Playback runs on Connect commands (Soloist), not the Web API.
    pub fn connect(&self) -> bool {
        self.connect
    }

    pub fn remote(&self) -> bool {
        self.remote.load(Ordering::SeqCst)
    }

    /// Removes the `nth` occurrence of `uri` from the upcoming tracks.
    pub async fn unqueue(&self, uri: &str, nth: usize) -> Result<()> {
        self.set_queue(|next, shown| {
            let i = find(next, shown, uri, nth)?;
            next.remove(i);
            Ok(())
        })
        .await
    }

    /// Moves the `nth` occurrence of `uri` one shown position down (or up). The tracks swap but
    /// each slot keeps its provider, so "Next in queue" and "Next from" stay in order: a context
    /// track moved into the queue section becomes queued, like the official clients do.
    pub async fn move_queued(&self, uri: &str, nth: usize, down: bool) -> Result<()> {
        self.set_queue(|next, shown| {
            let i = find(next, shown, uri, nth)?;
            let at = shown.iter().position(|&s| s == i).unwrap_or_default();
            let Some(&j) = (if down {
                shown.get(at + 1)
            } else {
                at.checked_sub(1).and_then(|a| shown.get(a))
            }) else {
                return Ok(());
            };
            next.swap(i, j);
            let (pi, pj) = (next[i].provider.clone(), next[j].provider.clone());
            (next[i].provider, next[j].provider) = (pj, pi);
            Ok(())
        })
        .await
    }

    /// Rewrites the active device's upcoming tracks with a Connect `set_queue` command, as the
    /// official clients do; the Web API cannot remove or reorder. `edit` gets the cluster's next
    /// tracks and the indices of those shown in `NowPlaying.next`.
    async fn set_queue(
        &self,
        edit: impl FnOnce(&mut Vec<ProvidedTrack>, &[usize]) -> Result<()>,
    ) -> Result<()> {
        let mut latest = self
            .cluster
            .lock()
            .clone()
            .context("not connected to Spotify Connect")?;
        if latest.active.is_empty() {
            // Commands need a target: make hum active, then wait for the cluster to say so.
            self.activate().await?;
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                latest = self
                    .cluster
                    .lock()
                    .clone()
                    .context("not connected to Spotify Connect")?;
                if !latest.active.is_empty() {
                    break;
                }
            }
        }
        let p = &latest.state;
        let mut next = p.next_tracks.clone();
        let shown = shown(&next, &p.track.uri);
        edit(&mut next, &shown)?;
        let track = |t: &ProvidedTrack| json!({"uri": t.uri, "uid": t.uid, "provider": t.provider, "metadata": t.metadata});
        let body = json!({"command": {
            "endpoint": "set_queue",
            "next_tracks": next.iter().map(track).collect::<Vec<_>>(),
            "prev_tracks": p.prev_tracks.iter().map(track).collect::<Vec<_>>(),
            "queue_revision": p.queue_revision,
        }});
        let path = format!(
            "/connect-state/v1/player/command/from/{}/to/{}",
            latest.session.device_id(),
            latest.active
        );
        latest
            .session
            .spclient()
            .request(
                &reqwest::Method::POST,
                &path,
                None,
                Some(body.to_string().as_bytes()),
            )
            .await?;
        Ok(())
    }

    /// Makes hum the active device without starting playback: Spotify moves the account's last
    /// playback (context, position, queue) here, paused.
    pub async fn activate(&self) -> Result<()> {
        self.transfer(&self.device().await?, false).await
    }

    /// Sends a Connect command (`{"endpoint": ..., ...}`) to device `to`, as the official clients
    /// do; Spotify accepts it from the daemon's session, which is not a registered device.
    async fn connect_cmd(&self, to: &str, cmd: Value) -> Result<()> {
        let path = format!(
            "/connect-state/v1/player/command/from/{}/to/{to}",
            self.session.device_id()
        );
        self.session
            .spclient()
            .request(
                &reqwest::Method::POST,
                &path,
                None,
                Some(json!({"command": cmd}).to_string().as_bytes()),
            )
            .await?;
        Ok(())
    }

    /// The active device, else this player's.
    async fn active_device(&self) -> Result<String> {
        let active = self
            .cluster
            .lock()
            .as_ref()
            .map(|l| l.active.clone())
            .filter(|a| !a.is_empty());
        match active {
            Some(active) => Ok(active),
            None => self.device().await,
        }
    }

    /// Starts playback here; `body` as for `PUT /me/player/play`.
    pub async fn play(&self, body: Value) -> Result<()> {
        if self.connect {
            return self
                .connect_cmd(&self.device().await?, connect_play(&body))
                .await;
        }
        self.web("/me/player/play", Some(body)).await
    }

    /// A playback option on this player: Connect `cmds`, or the Web API `path` (`PUT`, ECS).
    pub async fn here(&self, cmds: &[Value], path: &str) -> Result<()> {
        if !self.connect {
            return self.web(path, None).await;
        }
        let device = self.device().await?;
        for cmd in cmds {
            self.connect_cmd(&device, cmd.clone()).await?;
        }
        Ok(())
    }

    /// A command for whichever device is active: Connect `cmd`, or the Web API `path` (ECS).
    pub async fn on_active(&self, cmd: Value, method: reqwest::Method, path: &str) -> Result<()> {
        if self.connect {
            return self.connect_cmd(&self.active_device().await?, cmd).await;
        }
        self.web_active(method, path).await
    }

    /// The account's Connect devices. With Soloist they come from the cluster; with ECS from the
    /// Web API, as the cluster does not list the SDK's own device.
    pub async fn devices(&self) -> Result<Vec<proto::Device>> {
        if self.connect {
            return Ok(self
                .cluster
                .lock()
                .as_ref()
                .context("not connected to Spotify Connect")?
                .devices
                .clone());
        }
        let res = self
            .http
            .get(format!("{WEB_API}/me/player/devices"))
            .bearer_auth(auth::web_token().await?)
            .send()
            .await?;
        let v: Value = res.error_for_status()?.json().await?;
        Ok(v["devices"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| proto::Device {
                id: d["id"].as_str().unwrap_or_default().to_string(),
                name: d["name"].as_str().unwrap_or_default().to_string(),
                kind: d["type"].as_str().unwrap_or_default().to_lowercase(),
                active: d["is_active"].as_bool().unwrap_or(false),
            })
            .filter(|d| !d.id.is_empty())
            .collect())
    }

    /// Moves playback to `device` (any Connect device of the account).
    pub async fn transfer(&self, device: &str, play: bool) -> Result<()> {
        if self.connect {
            let path = format!(
                "/connect-state/v1/connect/transfer/from/{}/to/{device}",
                self.session.device_id()
            );
            let body = json!({"transfer_options": {"restore_paused": if play { "resume" } else { "pause" }}});
            self.session
                .spclient()
                .request(
                    &reqwest::Method::POST,
                    &path,
                    None,
                    Some(body.to_string().as_bytes()),
                )
                .await?;
            return Ok(());
        }
        let res = self
            .http
            .put(format!("{WEB_API}/me/player"))
            .bearer_auth(auth::web_token().await?)
            .json(&json!({"device_ids": [device], "play": play}))
            .send()
            .await?;
        if !res.status().is_success() {
            bail!(
                "transfer playback: {} {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Web API call for whichever device is active (no `device_id`).
    async fn web_active(&self, method: reqwest::Method, path: &str) -> Result<()> {
        let res = self
            .http
            .request(method, format!("{WEB_API}{path}"))
            .bearer_auth(auth::web_token().await?)
            .header("content-length", "0")
            .send()
            .await?;
        if !res.status().is_success() {
            bail!(
                "{path}: {} {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Body for `PUT /me/player/play` resuming the restored `np` where it stopped, with its queue.
    pub fn resume_body(&self, np: &NowPlaying) -> Value {
        play_body(&np.context, &np.uri, np.position_ms, &np.next)
    }

    pub fn loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    /// Commands handled by the SDK in the player process.
    pub fn command(&self, cmd: &str, arg: Value) {
        let _ = self.tx.send(json!({"cmd": cmd, "arg": arg}));
    }

    /// Waits up to 10 s for the player to register as a Connect device.
    async fn device(&self) -> Result<String> {
        let mut device = self.device.clone();
        let id = tokio::time::timeout(Duration::from_secs(10), device.wait_for(Option::is_some))
            .await
            .context("player not ready")??;
        Ok(id.clone().unwrap_or_default())
    }

    /// Web API call targeting this player's device.
    async fn web(&self, path: &str, body: Option<Value>) -> Result<()> {
        let device = self.device().await?;
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut req = self
            .http
            .put(format!("{WEB_API}{path}{sep}device_id={device}"))
            .bearer_auth(auth::web_token().await?);
        req = match body {
            Some(body) => req.json(&body),
            None => req.header("content-length", "0"),
        };
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!(
                "{path}: {} {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }
}

/// Keeps one player process alive, restarting it when it dies.
async fn supervise(
    mut rx: mpsc::UnboundedReceiver<Value>,
    device: watch::Sender<Option<String>>,
    state: Arc<watch::Sender<NowPlaying>>,
    loaded: Arc<AtomicBool>,
    remote: Arc<AtomicBool>,
    engine: crate::setup::Engine,
    session: Session,
) {
    loop {
        let result = match &engine {
            #[cfg(feature = "ecs")]
            crate::setup::Engine::Ecs => run_once(&mut rx, &device, &state, &loaded, &remote).await,
            crate::setup::Engine::Soloist { bin, key } => {
                crate::soloist::run(
                    &mut rx, &device, &state, &loaded, &remote, bin, key, &session,
                )
                .await
            }
        };
        if let Err(e) = result {
            log::error!("player: {e:#}");
        }
        loaded.store(false, Ordering::SeqCst);
        device.send_replace(None);
        // A new engine starts unmuted.
        state.send_modify(|s| (s.playing, s.muted) = (false, false));
        if rx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(feature = "ecs")]
async fn run_once(
    rx: &mut mpsc::UnboundedReceiver<Value>,
    device: &watch::Sender<Option<String>>,
    state: &Arc<watch::Sender<NowPlaying>>,
    loaded: &AtomicBool,
    remote: &AtomicBool,
) -> Result<()> {
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("hum/player");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("main.js"), MAIN_JS)?;
    std::fs::write(dir.join("player.html"), PLAYER_HTML)?;
    let bin = ecs_binary();
    let mut cmd = Command::new(&bin);
    // The window is never shown: render offscreen even with a display server, so the player
    // survives the graphical session (logout, compositor crash). Must be on argv: Electron picks
    // the ozone platform (from XDG_SESSION_TYPE) before main.js runs, so app.commandLine is too late.
    if cfg!(target_os = "linux") {
        cmd.args(["--ozone-platform=headless", "--disable-gpu"]);
    }
    let mut child = cmd
        .arg(dir.join("main.js"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "start {} (signed Electron: https://github.com/shurizzle/hum/releases/tag/ecs-{ECS_VERSION}, see docs/playback.md)",
                bin.display()
            )
        })?;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    // Volume kept across restarts in the disk cache (`volume`, u16 LE, no expiry). Saving starts
    // once the SDK reports the restored value, so its initial 0.5 never overwrites it.
    let saved = Cache::global()
        .read_any(VOLUME_KEY)
        .and_then(|b| Some(u16::from_le_bytes(b.try_into().ok()?)));
    let mut restoring = saved;
    log::info!("player started: {}", bin.display());

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { bail!("player exited: {:?}", child.wait().await?) };
                let Some(msg) = line.strip_prefix("HUM ").and_then(|l| serde_json::from_str::<Value>(l).ok()) else { continue };
                if let Some(id) = msg.get("ready").and_then(Value::as_str) {
                    log::info!("player ready: device {id}");
                    device.send_replace(Some(id.to_string()));
                    if let Some(v) = saved {
                        let cmd = json!({"cmd": "volume", "arg": v as f64 / u16::MAX as f64});
                        stdin.write_all(format!("{cmd}\n").as_bytes()).await?;
                    }
                }
                if let Some(e) = msg.get("error").and_then(Value::as_str) {
                    log::error!("player: {e}");
                }
                if msg.get("want_token").is_some() {
                    let token = auth::web_token().await.unwrap_or_else(|e| {
                        log::error!("player token: {e:#}");
                        String::new()
                    });
                    stdin.write_all(format!("{}\n", json!({"token": token})).as_bytes()).await?;
                }
                if let Some(s) = msg.get("state") {
                    let playing = s.get("playing").and_then(Value::as_bool).unwrap_or(false);
                    if s.get("uri").and_then(Value::as_str).is_some_and(|u| !u.is_empty()) {
                        // Playback moved to another device: the SDK's paused leftover must not
                        // cover the mirrored state. Playing here means hum took over.
                        if remote.load(Ordering::SeqCst) && !playing {
                            continue;
                        }
                        remote.store(false, Ordering::SeqCst);
                        loaded.store(true, Ordering::SeqCst);
                    }
                    let before = state.borrow().volume;
                    state.send_if_modified(|np| apply(np, s));
                    let volume = state.borrow().volume;
                    match restoring {
                        // f64 round trip through the SDK: allow one step of difference.
                        Some(v) if volume.abs_diff(v) <= 1 => restoring = None,
                        Some(_) => {}
                        None if volume != before => Cache::global().write(VOLUME_KEY, &volume.to_le_bytes()),
                        None => {}
                    }
                }
            }
            Some(msg) = rx.recv() => stdin.write_all(format!("{msg}\n").as_bytes()).await?,
        }
    }
}

/// Follows the account's connect-state cluster over the librespot dealer, registered as a hidden
/// observer device. The first cluster restores where the account left off (paused, like the
/// official clients on startup): the Web API has no position once no device is active. Later ones
/// mirror playback on other devices, which the SDK does not report.
struct Mirror {
    state: Arc<watch::Sender<NowPlaying>>,
    sdk_device: watch::Receiver<Option<String>>,
    loaded: Arc<AtomicBool>,
    remote: Arc<AtomicBool>,
    cluster: Arc<Mutex<Option<Latest>>>,
}

impl Mirror {
    async fn run(&self) -> Result<()> {
        let cache = auth::cache()?;
        let creds = cache
            .credentials()
            .context("not logged in: run `hum` first")?;
        let session = Session::new(SessionConfig::default(), Some(cache));
        session
            .connect(creds, false)
            .await
            .context("spotify login")?;
        let res = self.follow(&session).await;
        *self.cluster.lock() = None;
        session.shutdown();
        res
    }

    async fn follow(&self, session: &Session) -> Result<()> {
        let mut ids =
            session
                .dealer()
                .listen_for("hm://pusher/v1/connections/", |m: Message| {
                    Ok(m.headers
                        .get("Spotify-Connection-Id")
                        .cloned()
                        .unwrap_or_default())
                })?;
        let mut updates = session.dealer().listen_for(
            "hm://connect-state/v1/cluster",
            Message::from_raw::<ClusterUpdate>,
        )?;
        // Liked songs/albums/artists/shows (`hm://collection/<kind>/<user>`), the playlist rootlist and
        // playlist contents.
        let uri = |m: Message| Ok(m.uri);
        let mut library = futures_util::stream::select_all([
            session.dealer().listen_for("hm://collection/", uri)?,
            session.dealer().listen_for("hm://playlist/v2/user/", uri)?,
            session
                .dealer()
                .listen_for("hm://playlist/v2/playlist/", uri)?,
        ]);
        session.dealer().start().await?;
        let req = PutStateRequest {
            member_type: EnumOrUnknown::new(MemberType::CONNECT_STATE),
            device: MessageField::some(Device {
                device_info: MessageField::some(DeviceInfo {
                    name: "hum".into(),
                    device_id: session.device_id().to_string(),
                    client_id: session.client_id(),
                    capabilities: MessageField::some(Capabilities {
                        hidden: true,
                        is_observable: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        loop {
            let cluster = tokio::select! {
                // A new connection id (first connect, dealer reconnect) needs the device put again.
                id = ids.next() => {
                    session.set_connection_id(&id.context("dealer closed")??);
                    Cluster::parse_from_bytes(&session.spclient().put_connect_state_request(&req).await?)?
                }
                Some(Ok(uri)) = library.next() => {
                    log::debug!("library push: {uri}");
                    if let Some(id) = uri.strip_prefix("hm://playlist/v2/playlist/") {
                        crate::daemon::playlist_changed(format!("spotify:playlist:{id}"));
                    } else if uri.ends_with("/rootlist") || uri.starts_with("hm://collection/") {
                        crate::daemon::library_changed();
                    }
                    continue;
                }
                update = updates.next() => match update.context("dealer closed")? {
                    Ok(mut u) => u.cluster.take().unwrap_or_default(),
                    Err(e) => {
                        log::debug!("cluster update: {e}");
                        continue;
                    }
                },
            };
            *self.cluster.lock() = Some(Latest {
                session: session.clone(),
                active: cluster.active_device_id.clone(),
                state: cluster.player_state.clone().unwrap_or_default(),
                devices: devices(&cluster),
            });
            if let Err(e) = self.apply(session, cluster).await {
                log::warn!("connect state: {e:#}");
            }
        }
    }

    async fn apply(&self, session: &Session, mut cluster: Cluster) -> Result<()> {
        let active = cluster.active_device_id.clone();
        let p = cluster.player_state.take().unwrap_or_default();
        let next: Vec<String> = shown(&p.next_tracks, &p.track.uri)
            .into_iter()
            .map(|i| p.next_tracks[i].uri.clone())
            .take(MAX_URIS)
            .collect();
        let shuffle = p.options.shuffling_context;
        let repeat = match (p.options.repeating_track, p.options.repeating_context) {
            (true, _) => proto::Repeat::Track,
            (_, true) => proto::Repeat::Context,
            _ => proto::Repeat::Off,
        };
        // hum's own player reports its state directly, with less lag; only the queue comes from here
        // (autoplay fills it after the track starts).
        if !active.is_empty() && self.sdk_device.borrow().as_deref() == Some(active.as_str()) {
            self.remote.store(false, Ordering::SeqCst);
            self.state.send_if_modified(|s| {
                let changed = (&s.next, s.shuffle, s.repeat) != (&next, shuffle, repeat);
                (s.next, s.shuffle, s.repeat) = (next, shuffle, repeat);
                changed
            });
            return Ok(());
        }
        let remote = !active.is_empty();
        self.remote.store(remote, Ordering::SeqCst);
        if remote {
            // Whatever the SDK had loaded is stale now; resuming here starts from the mirrored state.
            self.loaded.store(false, Ordering::SeqCst);
        }
        // An idle hum with its last state wins over an empty cluster; only the queue and options
        // come from the cluster then (Soloist restores the track itself, without them), and the
        // position while paused: Soloist reports a restored track at 0 but resumes it from here.
        if p.track.uri.is_empty()
            || (!remote
                && !self.state.borrow().uri.is_empty()
                && self.loaded.load(Ordering::SeqCst))
        {
            if !remote && self.state.borrow().uri == p.track.uri {
                let position = p.position_as_of_timestamp.clamp(0, p.duration.max(0)) as u32;
                self.state.send_if_modified(|s| {
                    let mut changed = (&s.next, s.shuffle, s.repeat) != (&next, shuffle, repeat);
                    (s.next, s.shuffle, s.repeat) = (next, shuffle, repeat);
                    if !s.playing && s.position_ms != position {
                        (s.position_ms, s.position_at_ms) = (position, proto::now_ms());
                        changed = true;
                    }
                    changed
                });
            }
            return Ok(());
        }
        let playing = remote && p.is_playing && !p.is_paused;
        let mut position = p.position_as_of_timestamp;
        if playing {
            let now = proto::now_ms() as i64;
            position += (now - p.timestamp).max(0);
        }
        let position = position.clamp(0, p.duration.max(0)) as u32;
        let uri = p.track.uri.clone();
        let mut np = if self.state.borrow().uri == uri {
            self.state.borrow().clone()
        } else {
            log::info!(
                "connect state: {uri} in {:?} at {position} ms on {active:?}",
                p.context_uri
            );
            // Episodes are not mirrored (no track metadata).
            let Some(track) = crate::meta::tracks(session, std::slice::from_ref(&uri))
                .await?
                .pop()
            else {
                return Ok(());
            };
            track.now_playing()
        };
        np.context = p.context_uri;
        np.position_ms = position;
        np.position_at_ms = proto::now_ms();
        np.playing = playing;
        np.next = next;
        (np.shuffle, np.repeat) = (shuffle, repeat);
        self.state.send_if_modified(|s| {
            let np = NowPlaying {
                volume: s.volume,
                muted: s.muted,
                ..np
            };
            let changed = *s != np;
            *s = np;
            changed
        });
        Ok(())
    }
}

/// The cluster's visible devices (hum's observer is hidden).
fn devices(cluster: &Cluster) -> Vec<proto::Device> {
    let mut devices: Vec<_> = cluster
        .device
        .iter()
        .filter(|(_, d)| !d.capabilities.hidden)
        .map(|(id, d)| proto::Device {
            id: id.clone(),
            name: d.name.clone(),
            kind: format!("{:?}", d.device_type.enum_value_or_default()).to_lowercase(),
            active: *id == cluster.active_device_id,
        })
        .collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    devices
}

/// Connect `play` command for a `PUT /me/player/play` body: a context, or `uris` as a one-page
/// context; `offset.uri` and `position_ms` become `skip_to` and `seek_to`.
fn connect_play(body: &Value) -> Value {
    let context = match body["context_uri"].as_str() {
        Some(uri) => json!({"uri": uri, "url": format!("context://{uri}")}),
        None => {
            let tracks: Vec<Value> = body["uris"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|u| json!({"uri": u}))
                .collect();
            json!({"pages": [{"tracks": tracks}]})
        }
    };
    let mut options = json!({});
    if let Some(uri) = body.pointer("/offset/uri").filter(|u| u.is_string()) {
        options["skip_to"] = json!({"track_uri": uri});
    }
    if let Some(ms) = body.get("position_ms").filter(|ms| ms.as_u64() != Some(0)) {
        options["seek_to"] = ms.clone();
    }
    json!({"endpoint": "play", "context": context, "options": options})
}

/// Indices of the playable upcoming tracks, as `NowPlaying.next` lists them. Next tracks also hold
/// delimiters and other pseudo uris, and a lone track played from `uris` repeats itself there.
fn shown(next: &[ProvidedTrack], current: &str) -> Vec<usize> {
    next.iter()
        .enumerate()
        .filter(|(_, t)| {
            (t.uri.starts_with("spotify:track:") || t.uri.starts_with("spotify:episode:"))
                && t.uri != current
        })
        .map(|(i, _)| i)
        .collect()
}

/// Index in `next` of the `nth` shown occurrence of `uri`.
fn find(next: &[ProvidedTrack], shown: &[usize], uri: &str, nth: usize) -> Result<usize> {
    shown
        .iter()
        .copied()
        .filter(|&i| next[i].uri == uri)
        .nth(nth)
        .context("track no longer queued")
}

/// Body for `PUT /me/player/play` starting `track` inside `context` (may be empty), followed by
/// `next` when the context cannot be used.
pub fn play_body(context: &str, track: &str, position_ms: u32, next: &[String]) -> Value {
    // The Web API takes an offset only in playlist/album/show contexts and in `spotify:list:` ones.
    // Artists answer "Can't have offset for context type: ARTIST", so they play as their "Popular"
    // list, like the web player does. Pseudo contexts like `spotify:webapi` (playback started from
    // `uris`) answer "Invalid context uri"; there the track plays with `next` as an explicit list.
    let artist_list = context
        .strip_prefix("spotify:artist:")
        .map(|id| format!("{ARTIST_POPULAR}{id}"));
    let context = artist_list.as_deref().unwrap_or(context);
    let offsettable = [
        "spotify:playlist:",
        "spotify:album:",
        "spotify:show:",
        "spotify:list:",
    ]
    .iter()
    .any(|p| context.starts_with(p));
    if !offsettable {
        let uris: Vec<&str> = std::iter::once(track)
            .chain(next.iter().map(String::as_str))
            .take(MAX_URIS)
            .collect();
        json!({"uris": uris, "position_ms": position_ms})
    } else {
        json!({"context_uri": context, "offset": {"uri": track}, "position_ms": position_ms})
    }
}

/// Merges a state report from the player; returns whether anything changed.
pub(crate) fn apply(np: &mut NowPlaying, s: &Value) -> bool {
    let mut next = np.clone();
    let str = |k: &str| s.get(k).and_then(Value::as_str).map(str::to_string);
    let num = |k: &str| s.get(k).and_then(Value::as_f64);
    if let Some(uri) = str("uri") {
        next.uri = uri;
        next.name = str("name").unwrap_or_default();
        next.artists = s
            .get("artists")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        next.album = str("album").unwrap_or_default();
        next.cover = str("cover").unwrap_or_default();
        next.context = str("context").unwrap_or_default();
        next.duration_ms = num("duration_ms").unwrap_or(0.0) as u32;
        next.position_ms = num("position_ms").unwrap_or(0.0) as u32;
        next.position_at_ms = proto::now_ms();
        next.playing = s.get("playing").and_then(Value::as_bool).unwrap_or(false);
    }
    if let Some(v) = num("volume") {
        next.volume = (v.clamp(0.0, 1.0) * u16::MAX as f64) as u16;
    }
    if let Some(m) = s.get("muted").and_then(Value::as_bool) {
        next.muted = m;
    }
    let changed = next != *np;
    *np = next;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_state_and_volume_only() {
        let mut np = NowPlaying::default();
        let s = json!({"uri": "spotify:track:1", "name": "One", "artists": ["A", "B"], "album": "X", "cover": "c",
                       "duration_ms": 1000, "position_ms": 10, "playing": true, "volume": 0.5});
        assert!(apply(&mut np, &s));
        assert_eq!(
            (np.name.as_str(), np.position_ms, np.playing, np.volume),
            ("One", 10, true, u16::MAX / 2)
        );
        assert_eq!(np.artists, ["A", "B"]);
        assert!(!apply(&mut np, &s));
        assert!(apply(&mut np, &json!({"volume": 1.0})));
        assert_eq!((np.uri.as_str(), np.volume), ("spotify:track:1", u16::MAX));
        assert!(apply(&mut np, &json!({"muted": true})));
        assert_eq!((np.muted, np.volume), (true, u16::MAX));
    }

    #[test]
    fn shown_skips_pseudo_and_current() {
        let t = |uri: &str| ProvidedTrack {
            uri: uri.into(),
            ..Default::default()
        };
        let next = [
            t("spotify:track:a"),
            t("spotify:delimiter"),
            t("spotify:track:cur"),
            t("spotify:track:b"),
            t("spotify:track:a"),
        ];
        let shown = shown(&next, "spotify:track:cur");
        assert_eq!(shown, [0, 3, 4]);
        assert_eq!(find(&next, &shown, "spotify:track:a", 1).unwrap(), 4);
        assert!(find(&next, &shown, "spotify:track:cur", 0).is_err());
    }

    #[test]
    fn connect_play_from_web_bodies() {
        let body = json!({"context_uri": "spotify:album:a", "offset": {"uri": "spotify:track:t"}, "position_ms": 45000});
        assert_eq!(
            connect_play(&body),
            json!({"endpoint": "play", "context": {"uri": "spotify:album:a", "url": "context://spotify:album:a"},
                   "options": {"skip_to": {"track_uri": "spotify:track:t"}, "seek_to": 45000}})
        );
        assert_eq!(
            connect_play(
                &json!({"uris": ["spotify:track:a", "spotify:track:b"], "position_ms": 0})
            ),
            json!({"endpoint": "play", "context": {"pages": [{"tracks": [{"uri": "spotify:track:a"}, {"uri": "spotify:track:b"}]}]}, "options": {}})
        );
    }

    #[test]
    fn restored_playback_resumes_in_context() {
        let np = NowPlaying {
            uri: "spotify:track:t".into(),
            context: "spotify:playlist:p".into(),
            position_ms: 45000,
            ..Default::default()
        };
        let next = ["spotify:track:n".to_string()];
        assert_eq!(
            play_body(&np.context, &np.uri, np.position_ms, &next),
            json!({"context_uri": "spotify:playlist:p", "offset": {"uri": "spotify:track:t"}, "position_ms": 45000})
        );
        assert_eq!(
            play_body("spotify:artist:a", "spotify:track:t", 0, &[])["context_uri"],
            format!("{ARTIST_POPULAR}a")
        );
        for context in ["", "spotify:webapi"] {
            assert_eq!(
                play_body(context, "spotify:track:t", 5, &next),
                json!({"uris": ["spotify:track:t", "spotify:track:n"], "position_ms": 5})
            );
        }
    }
}
