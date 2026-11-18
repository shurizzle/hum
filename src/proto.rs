use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Client -> daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Cmd {
    /// Play a context (playlist, album, artist, station) or a list of tracks, starting at `track`.
    Load {
        context_uri: Option<String>,
        tracks: Vec<String>,
        track: Option<String>,
    },
    PlayPause,
    Next,
    Prev,
    Seek(u32),
    Volume(u16),
    /// Silence playback keeping `volume`; a `Volume` while muted unmutes.
    Mute(bool),
    Shuffle(bool),
    Repeat(Repeat),
    /// Add a track or episode to the queue; activates hum (paused) if no device is active.
    Queue(String),
    /// Track metadata for the queue panel; answered with `Msg::Tracks`.
    Tracks(Vec<String>),
    /// Remove the `nth` occurrence (0-based) of `uri` from `NowPlaying.next`.
    Unqueue { uri: String, nth: usize },
    /// Move the `nth` occurrence (0-based) of `uri` in `NowPlaying.next` one place down or up.
    MoveQueue { uri: String, nth: usize, down: bool },
    /// Lyrics of a track; answered with `Msg::Lyrics`.
    Lyrics(String),
    /// List the account's Connect devices; answered with `Msg::Devices`.
    Devices,
    /// Move playback to a Connect device, keeping it playing or paused.
    Transfer(String),
    Token,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    /// login5 token of the librespot session: internal APIs (pathfinder).
    pub access_token: String,
    pub client_token: String,
    /// Token of hum's developer app: public Web API.
    pub web_token: String,
}

/// One lyrics line; `start_ms` is 0 for every line of unsynced lyrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LyricsLine {
    pub start_ms: u32,
    pub words: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

impl Repeat {
    /// Next state of the repeat button, like the official clients: off, context, track.
    pub fn cycle(self) -> Self {
        match self {
            Repeat::Off => Repeat::Context,
            Repeat::Context => Repeat::Track,
            Repeat::Track => Repeat::Off,
        }
    }
}

/// A Spotify Connect device of the account.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Lowercase device type: "computer", "smartphone", "speaker", ...
    pub kind: String,
    pub active: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NowPlaying {
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    /// Playlist/album/artist the track plays from; empty for loose tracks.
    pub context: String,
    pub duration_ms: u32,
    pub position_ms: u32,
    /// Unix time (ms) `position_ms` was measured at, to extrapolate it while `playing`.
    #[serde(default)]
    pub position_at_ms: u64,
    pub playing: bool,
    pub volume: u16,
    /// Muted by `Cmd::Mute`; `volume` keeps the level to restore.
    #[serde(default)]
    pub muted: bool,
    /// Shuffle and repeat (context or track) of the active device, from the Connect cluster.
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
    /// Upcoming track uris from the account's Connect cluster (restored or mirrored playback);
    /// the Web API queue is empty while no device is active.
    #[serde(default)]
    pub next: Vec<String>,
}

impl NowPlaying {
    /// Current position in ms, extrapolated while playing.
    pub fn position(&self, now_ms: u64) -> u64 {
        let elapsed = if self.playing { now_ms.saturating_sub(self.position_at_ms) } else { 0 };
        (self.position_ms as u64 + elapsed).min(self.duration_ms as u64)
    }
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// IPC protocol version, independent of the app version: bump it on any change to `Cmd`, `Msg` or
/// the types they carry. A daemon speaking another one is replaced.
pub const PROTOCOL: u32 = 0;

/// Daemon -> client. Replies carry no id: a client sends one command at a time per reply it awaits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Msg {
    /// First message on every connection.
    Hello { version: String, protocol: u32 },
    State(NowPlaying),
    Tokens(Tokens),
    /// Answer to `Cmd::Tracks`, in request order, unknown tracks skipped.
    Tracks(Vec<crate::api::Item>),
    /// Answer to `Cmd::Lyrics`; no lines when the track has none.
    Lyrics { uri: String, lines: Vec<LyricsLine> },
    /// Answer to `Cmd::Devices`.
    Devices(Vec<Device>),
    /// Your Library or liked songs changed, here or on another device.
    Library,
    /// This playlist's tracks changed (`spotify:playlist:<id>`), here or on another device.
    Playlist(String),
    /// Failure of the command this connection sent last.
    Error(String),
    /// A daemon/player/librespot warning or error, pushed to every client; not a command failure.
    Log(String),
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub type Conn = Framed<UnixStream, LengthDelimitedCodec>;

pub fn socket_path() -> PathBuf {
    runtime_path("hum.sock")
}

/// `name` in `$XDG_RUNTIME_DIR`, else `$TMPDIR`: socket, pid file, spawn lock.
pub fn runtime_path(name: &str) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(name)
}

pub fn frame(stream: UnixStream) -> Conn {
    Framed::new(stream, LengthDelimitedCodec::new())
}

pub async fn send<T: Serialize>(conn: &mut Conn, msg: &T) -> Result<()> {
    conn.send(tokio_util::bytes::Bytes::from(serde_json::to_vec(msg)?)).await?;
    Ok(())
}

pub async fn recv<T: DeserializeOwned>(conn: &mut Conn) -> Result<Option<T>> {
    match conn.next().await {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes?).context("bad frame")?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut a, mut b) = (frame(a), frame(b));
        let cmd = Cmd::Load { context_uri: Some("spotify:playlist:x".into()), tracks: vec![], track: None };
        send(&mut a, &cmd).await.unwrap();
        assert_eq!(recv::<Cmd>(&mut b).await.unwrap(), Some(cmd));
        drop(a);
        assert_eq!(recv::<Cmd>(&mut b).await.unwrap(), None);
    }

    #[test]
    fn hello_without_protocol() {
        // Daemons predating `protocol` do not parse, so `spawn::try_connect` replaces them.
        assert!(serde_json::from_str::<Msg>(r#"{"Hello":{"version":"0.1.0"}}"#).is_err());
    }
}
