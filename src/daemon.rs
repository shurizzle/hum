use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use librespot_core::{Session, SessionConfig};
use serde_json::{Value, json};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Notify, broadcast, watch},
};

use crate::{
    auth,
    player::Player,
    proto::{self, Cmd, Msg, NowPlaying, Tokens},
};

const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// Warnings and errors (daemon, player, librespot), forwarded to every client's status bar.
static ERRORS: std::sync::OnceLock<broadcast::Sender<String>> = std::sync::OnceLock::new();

struct Logger(env_logger::Logger);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        self.0.enabled(m)
    }

    fn log(&self, r: &log::Record) {
        if !self.0.matches(r) {
            return;
        }
        if r.level() <= log::Level::Warn
            && let Some(tx) = ERRORS.get()
        {
            let _ = tx.send(format!("{}: {}", r.target().rsplit("::").next().unwrap_or(""), r.args()));
        }
        self.0.log(r);
    }

    fn flush(&self) {
        self.0.flush();
    }
}

pub fn init_logger() {
    let inner = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    log::set_max_level(inner.filter());
    ERRORS.get_or_init(|| broadcast::channel(64).0);
    let _ = log::set_boxed_logger(Box::new(Logger(inner)));
}

struct Shared {
    player: Player,
    session: Session,
    state: watch::Receiver<NowPlaying>,
    clients: AtomicUsize,
    shutdown: Notify,
}

pub async fn run() -> Result<()> {
    let path = proto::socket_path();
    if UnixStream::connect(&path).await.is_ok() {
        bail!("daemon already running on {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let pid_path = auth::config_dir().join("daemon.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // The librespot session only serves browsing tokens (login5, client token); playback is the player's.
    let cache = auth::cache()?;
    let creds = cache.credentials().context("not logged in: run `hum` first")?;
    let session = Session::new(SessionConfig::default(), Some(cache));
    session.connect(creds, true).await.context("spotify login")?;
    {
        // Product info arrives asynchronously after login; account type helps diagnose playback refusals.
        let session = session.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let mut attrs: Vec<_> = session.user_data().attributes.into_iter().collect();
            attrs.sort();
            let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map_or("?", |(_, v)| v.as_str());
            log::info!("account: type={} catalogue={} country={}", get("type"), get("catalogue"), session.country());
            log::debug!("user attributes: {attrs:?}");
        });
    }

    let (state_tx, state_rx) = watch::channel(NowPlaying::default());
    let shared = Arc::new(Shared {
        player: Player::spawn(state_tx),
        session,
        state: state_rx,
        clients: AtomicUsize::new(0),
        shutdown: Notify::new(),
    });
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let mut idle_since: Option<Instant> = None;
    log::info!("listening on {}", path.display());

    loop {
        tokio::select! {
            conn = listener.accept() => {
                let (stream, _) = conn?;
                let shared = shared.clone();
                tokio::spawn(async move {
                    shared.clients.fetch_add(1, Ordering::SeqCst);
                    if let Err(e) = client(stream, &shared).await {
                        log::warn!("client: {e:#}");
                    }
                    shared.clients.fetch_sub(1, Ordering::SeqCst);
                });
            }
            _ = tick.tick() => {
                let idle = shared.clients.load(Ordering::SeqCst) == 0 && !shared.state.borrow().playing;
                idle_since = if idle { idle_since.or(Some(Instant::now())) } else { None };
                if idle_since.is_some_and(|t| t.elapsed() >= IDLE_EXIT) {
                    log::info!("idle, exiting");
                    break;
                }
            }
            _ = shared.shutdown.notified() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&pid_path);
    // Exiting drops the player handle; the child is killed on drop.
    Ok(())
}

async fn client(stream: UnixStream, shared: &Shared) -> Result<()> {
    let mut conn = proto::frame(stream);
    proto::send(&mut conn, &Msg::Hello { version: proto::VERSION.into() }).await?;
    let mut state = shared.state.clone();
    let mut errors = ERRORS.get_or_init(|| broadcast::channel(64).0).subscribe();
    let snapshot = state.borrow_and_update().clone();
    proto::send(&mut conn, &Msg::State(snapshot)).await?;
    loop {
        tokio::select! {
            Ok(e) = errors.recv() => proto::send(&mut conn, &Msg::Error(e)).await?,
            changed = state.changed() => {
                changed?;
                let snapshot = state.borrow_and_update().clone();
                proto::send(&mut conn, &Msg::State(snapshot)).await?;
            }
            cmd = proto::recv::<Cmd>(&mut conn) => {
                let Some(cmd) = cmd? else { return Ok(()) };
                let reply = match handle(cmd, shared).await {
                    Ok(reply) => reply,
                    Err(e) => Some(Msg::Error(format!("{e:#}"))),
                };
                if let Some(reply) = reply {
                    proto::send(&mut conn, &reply).await?;
                }
            }
        }
    }
}

async fn handle(cmd: Cmd, shared: &Shared) -> Result<Option<Msg>> {
    let player = &shared.player;
    match cmd {
        Cmd::Load { context_uri, tracks, track } => {
            let body = match context_uri {
                Some(uri) => match track {
                    Some(track) => crate::player::play_body(&uri, &track, 0, &[]),
                    None => json!({"context_uri": uri}),
                },
                None => json!({"uris": tracks}),
            };
            player.web("/me/player/play", Some(body)).await?;
        }
        // Another device is playing: control it like the official clients do, without taking over.
        Cmd::PlayPause if player.remote() => {
            let path = if shared.state.borrow().playing { "/me/player/pause" } else { "/me/player/play" };
            player.web_active(reqwest::Method::PUT, path).await?;
        }
        Cmd::Next if player.remote() => player.web_active(reqwest::Method::POST, "/me/player/next").await?,
        Cmd::Prev if player.remote() => player.web_active(reqwest::Method::POST, "/me/player/previous").await?,
        Cmd::Seek(ms) if player.remote() => player.web_active(reqwest::Method::PUT, &format!("/me/player/seek?position_ms={ms}")).await?,
        Cmd::PlayPause if !player.loaded() => {
            // Nothing loaded in the SDK yet: play the restored last playback from where it stopped.
            let np = shared.state.borrow().clone();
            if !np.uri.is_empty() {
                player.web("/me/player/play", Some(player.resume_body(&np))).await?;
            }
        }
        Cmd::PlayPause => player.command("toggle", Value::Null),
        Cmd::Next => player.command("next", Value::Null),
        Cmd::Prev => player.command("prev", Value::Null),
        Cmd::Seek(ms) => player.command("seek", json!(ms)),
        Cmd::Volume(v) => player.command("volume", json!(v as f64 / u16::MAX as f64)),
        Cmd::Shuffle(on) => player.web(&format!("/me/player/shuffle?state={on}"), None).await?,
        Cmd::Repeat(on) => player.web(&format!("/me/player/repeat?state={}", if on { "context" } else { "off" }), None).await?,
        Cmd::Queue(uri) => {
            // The Web API queues only on an active device ("NO_ACTIVE_DEVICE" otherwise).
            let path = format!("/me/player/queue?uri={uri}");
            if player.remote() || player.loaded() {
                player.web_active(reqwest::Method::POST, &path).await?;
            } else {
                player.activate().await?;
                // The transfer takes a moment to register; until then the queue answers 404.
                let mut tries = 0;
                while let Err(e) = player.web_active(reqwest::Method::POST, &path).await {
                    tries += 1;
                    if tries == 10 {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        Cmd::Tracks(uris) => {
            let tracks = crate::meta::tracks(&shared.session, &uris).await?;
            return Ok(Some(Msg::Tracks(tracks.iter().map(crate::meta::Track::item).collect())));
        }
        Cmd::Lyrics(uri) => {
            let lines = crate::meta::lyrics(&shared.session, &uri).await?;
            return Ok(Some(Msg::Lyrics { uri, lines }));
        }
        Cmd::Unqueue { uri, nth } => player.unqueue(&uri, nth).await?,
        Cmd::Devices => return Ok(Some(Msg::Devices(player.devices().await?))),
        Cmd::Transfer(device) => {
            let playing = shared.state.borrow().playing;
            player.transfer(&device, playing).await?;
        }
        Cmd::MoveQueue { uri, nth, down } => player.move_queued(&uri, nth, down).await?,
        Cmd::Token => {
            let access_token = shared.session.login5().auth_token().await?.access_token;
            let client_token = shared.session.spclient().client_token().await?;
            let web_token = auth::web_token().await?;
            return Ok(Some(Msg::Tokens(Tokens { access_token, client_token, web_token })));
        }
        Cmd::Shutdown => shared.shutdown.notify_one(),
    }
    Ok(None)
}
