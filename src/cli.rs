//! One-shot remote-control subcommands: `hum play`, `hum toggle`, `hum cycle`, `hum set`, `hum get`, ...

use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Subcommand, ValueEnum};
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::{
    api::Api,
    auth,
    proto::{self, Cmd, Conn, Msg, NowPlaying, Repeat},
    spawn,
};

#[derive(Subcommand)]
pub enum Remote {
    /// Play a Spotify URI or open.spotify.com link (track, album, playlist, artist)
    Play { uri: String },
    /// Next track
    Next,
    /// Previous track
    Prev,
    /// Add a track or episode (URI or open.spotify.com link) to the queue
    Queue { uri: String },
    /// List Connect devices (`*` marks the active one)
    Devices,
    /// Move playback to a Connect device, by name or id
    Transfer { device: String },
    /// Flip play/pause, shuffle, repeat or like
    #[command(subcommand)]
    Toggle(Toggle),
    /// Step through the states of repeat
    #[command(subcommand)]
    Cycle(Cycle),
    /// Set play/pause, shuffle, repeat, like, volume or position
    #[command(subcommand)]
    Set(Set),
    /// Print playback state; fails when the daemon is not running, never starts it
    Get {
        /// Print JSON
        #[arg(long, global = true)]
        json: bool,
        /// Keep running, printing a new line whenever the value changes
        #[arg(long, short, global = true)]
        follow: bool,
        #[command(subcommand)]
        what: Get,
    },
}

#[derive(Subcommand)]
pub enum Toggle {
    Play,
    Shuffle,
    /// Repeat off, or on (context) when off
    Repeat,
    /// Like/unlike the playing track
    Like,
    Mute,
}

#[derive(Subcommand, Clone, Copy, Debug, PartialEq, serde::Deserialize)]
pub enum Cycle {
    /// Off, on (context), track
    Repeat,
}

#[derive(Subcommand)]
pub enum Set {
    Play {
        state: OnOff,
    },
    Shuffle {
        state: OnOff,
    },
    /// `on` repeats the context, `track` the playing track
    Repeat {
        state: RepeatArg,
    },
    /// Like/unlike the playing track
    Like {
        state: OnOff,
    },
    /// Mute keeping the volume level
    Mute {
        state: OnOff,
    },
    /// Volume in percent; `+N`/`-N` is relative
    Volume {
        #[arg(allow_hyphen_values = true)]
        level: String,
    },
    /// Position in the track, in seconds; `+N`/`-N` is relative
    Position {
        #[arg(allow_hyphen_values = true)]
        seconds: String,
    },
}

#[derive(Subcommand, Clone, Copy)]
pub enum Get {
    /// `on` while playing
    Play,
    Shuffle,
    /// `off`, `on` (context) or `track`
    Repeat,
    /// Whether the playing track is liked
    Like,
    /// Volume in percent
    Volume,
    Mute,
    /// Position in the track, in seconds
    Position,
    /// Upcoming tracks, one per line
    Queue,
    /// Current track; with `--json` the whole state
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum, serde::Deserialize)]
pub enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(s: OnOff) -> bool {
        matches!(s, OnOff::On)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum, serde::Deserialize)]
pub enum RepeatArg {
    Off,
    On,
    Track,
}

impl From<RepeatArg> for Repeat {
    fn from(r: RepeatArg) -> Repeat {
        match r {
            RepeatArg::Off => Repeat::Off,
            RepeatArg::On => Repeat::Context,
            RepeatArg::Track => Repeat::Track,
        }
    }
}

pub async fn run(remote: Remote) -> Result<()> {
    if let Remote::Get { json, follow, what } = remote {
        // Status lines poll this: never spawn the daemon, an idle one should be allowed to exit.
        let Some(conn) = spawn::try_connect().await? else {
            bail!("daemon not running")
        };
        return get(conn, what, json, follow).await;
    }

    auth::ensure_login().await?;
    let mut conn = spawn::connect().await?;
    let mut np = state(&mut conn).await?;
    let cmd = match remote {
        Remote::Play { uri } => play(&uri),
        Remote::Next => Cmd::Next,
        Remote::Prev => Cmd::Prev,
        Remote::Queue { uri } => Cmd::Queue(spotify_uri(&uri)),
        Remote::Toggle(Toggle::Play) => Cmd::PlayPause,
        Remote::Toggle(Toggle::Shuffle) => Cmd::Shuffle(!np.shuffle),
        Remote::Toggle(Toggle::Repeat) => Cmd::Repeat(if np.repeat == Repeat::Off {
            Repeat::Context
        } else {
            Repeat::Off
        }),
        Remote::Toggle(Toggle::Mute) => Cmd::Mute(!np.muted),
        Remote::Cycle(Cycle::Repeat) => Cmd::Repeat(np.repeat.cycle()),
        Remote::Toggle(Toggle::Like) => return like(&mut conn, &mut np, None).await,
        Remote::Set(Set::Like { state }) => {
            return like(&mut conn, &mut np, Some(state.into())).await;
        }
        Remote::Set(Set::Play { state }) if bool::from(state) == np.playing => return Ok(()),
        Remote::Set(Set::Play { .. }) => Cmd::PlayPause,
        Remote::Set(Set::Shuffle { state }) => Cmd::Shuffle(state.into()),
        Remote::Set(Set::Repeat { state }) => Cmd::Repeat(state.into()),
        Remote::Set(Set::Mute { state }) => Cmd::Mute(state.into()),
        Remote::Set(Set::Volume { level }) => {
            Cmd::Volume(volume(Level::parse("volume", &level)?, np.volume))
        }
        Remote::Set(Set::Position { seconds }) => Cmd::Seek(seek(
            Level::parse("position", &seconds)?,
            &np,
            proto::now_ms(),
        )),
        Remote::Devices => {
            for d in devices(&mut conn, &mut np).await? {
                println!(
                    "{} {}\t{}\t{}",
                    if d.active { "*" } else { " " },
                    d.name,
                    d.kind,
                    d.id
                );
            }
            return Ok(());
        }
        Remote::Transfer { device } => {
            let devices = devices(&mut conn, &mut np).await?;
            let Some(d) = devices
                .iter()
                .find(|d| d.id == device || d.name.eq_ignore_ascii_case(&device))
            else {
                let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
                bail!("no device {device:?}; available: {}", names.join(", "));
            };
            Cmd::Transfer(d.id.clone())
        }
        Remote::Get { .. } => unreachable!(),
    };
    finish(conn, cmd).await
}

/// `hum get`: prints `what` once, or on every change with `follow`.
async fn get(mut conn: Conn, what: Get, json: bool, follow: bool) -> Result<()> {
    let mut np = state(&mut conn).await?;
    let mut view = View {
        conn,
        json,
        api: None,
        liked: None,
        queue: None,
    };
    let mut last: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let before = np.clone();
        let out = view.render(what, &mut np).await?;
        // An empty value still prints when following, so a status bar clears.
        if last.as_ref() != Some(&out) && (follow || !out.is_empty()) {
            println!("{out}");
        }
        last = Some(out);
        if !follow {
            return Ok(());
        }
        // States that arrived during a request: render them before waiting.
        if np != before {
            continue;
        }
        tokio::select! {
            msg = proto::recv::<Msg>(&mut view.conn) => match msg? {
                Some(Msg::State(s)) => np = s,
                Some(Msg::Library) => view.liked = None,
                Some(Msg::Error(e)) => bail!(e),
                Some(_) => {}
                None => bail!("daemon closed the connection"),
            },
            // Only the position changes without events.
            _ = tick.tick(), if matches!(what, Get::Position) => {}
        }
    }
}

struct View {
    conn: Conn,
    json: bool,
    api: Option<Api>,
    /// Liked state of a track uri; cleared on `Msg::Library`.
    liked: Option<(String, bool)>,
    /// Rendered queue for these upcoming uris.
    queue: Option<(Vec<String>, String)>,
}

impl View {
    async fn render(&mut self, what: Get, np: &mut NowPlaying) -> Result<String> {
        let json = self.json;
        let flag = |on: bool| {
            if json {
                on.to_string()
            } else {
                (if on { "on" } else { "off" }).to_string()
            }
        };
        Ok(match what {
            Get::Play => flag(np.playing),
            Get::Shuffle => flag(np.shuffle),
            Get::Repeat => {
                let r = match np.repeat {
                    Repeat::Off => "off",
                    Repeat::Context => "on",
                    Repeat::Track => "track",
                };
                if json {
                    format!("\"{r}\"")
                } else {
                    r.to_string()
                }
            }
            Get::Like => {
                let uri = np.uri.clone();
                if self.liked.as_ref().is_none_or(|(u, _)| *u != uri) {
                    let on =
                        !uri.is_empty() && liked(&mut self.conn, np, &mut self.api, &uri).await?;
                    self.liked = Some((uri, on));
                }
                flag(self.liked.as_ref().is_some_and(|(_, on)| *on))
            }
            Get::Volume => percent(np.volume).to_string(),
            Get::Mute => flag(np.muted),
            Get::Position => (np.position(proto::now_ms()) / 1000).to_string(),
            Get::Queue => {
                if self.queue.as_ref().is_none_or(|(next, _)| *next != np.next) {
                    let next = np.next.clone();
                    let tracks =
                        match request(&mut self.conn, np, Cmd::Tracks(next.clone())).await? {
                            Msg::Tracks(tracks) => tracks,
                            other => bail!("unexpected reply: {other:?}"),
                        };
                    let out = if json {
                        let v: Vec<_> = tracks
                            .iter()
                            .map(|t| json!({"uri": t.uri, "name": t.name, "artists": t.sub}))
                            .collect();
                        serde_json::to_string(&v)?
                    } else {
                        tracks
                            .iter()
                            .map(|t| format!("{} - {}", t.sub, t.name))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    self.queue = Some((next, out));
                }
                self.queue
                    .as_ref()
                    .map(|(_, out)| out.clone())
                    .unwrap_or_default()
            }
            Get::Status if json => serde_json::to_string(&np)?,
            Get::Status if np.uri.is_empty() => String::new(),
            Get::Status => format!(
                "{} {} - {}",
                if np.playing { "▶" } else { "⏸" },
                np.artist_line(),
                np.name
            ),
        })
    }
}

/// Likes/unlikes the playing track: sets it to `on`, or flips it when `None`.
async fn like(conn: &mut Conn, np: &mut NowPlaying, on: Option<bool>) -> Result<()> {
    if np.uri.is_empty() {
        bail!("nothing playing");
    }
    let uri = np.uri.clone();
    let mut api = None;
    let was = liked(conn, np, &mut api, &uri).await?;
    let on = on.unwrap_or(!was);
    if on != was {
        api.as_ref()
            .expect("set by liked")
            .set_saved(&uri, on)
            .await?;
    }
    Ok(())
}

async fn liked(
    conn: &mut Conn,
    np: &mut NowPlaying,
    api: &mut Option<Api>,
    uri: &str,
) -> Result<bool> {
    if api.is_none() {
        let Msg::Tokens(tokens) = request(conn, np, Cmd::Token).await? else {
            bail!("no tokens from daemon")
        };
        *api = Some(Api::new(tokens));
    }
    let saved = api
        .as_ref()
        .expect("just set")
        .saved(&[uri.to_string()])
        .await?;
    Ok(saved.first().copied().unwrap_or(false))
}

async fn devices(conn: &mut Conn, np: &mut NowPlaying) -> Result<Vec<proto::Device>> {
    match request(conn, np, Cmd::Devices).await? {
        Msg::Devices(devices) => Ok(devices),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Plays a track alone, anything else as a context.
pub fn play(uri: &str) -> Cmd {
    let uri = spotify_uri(uri);
    if uri.starts_with("spotify:track:") {
        Cmd::Load {
            context_uri: None,
            tracks: vec![uri],
            track: None,
        }
    } else {
        Cmd::Load {
            context_uri: Some(uri),
            tracks: vec![],
            track: None,
        }
    }
}

/// `spotify:…` uris pass through; `https://open.spotify.com/[intl-xx/]<kind>/<id>?…` links become uris.
pub fn spotify_uri(s: &str) -> String {
    let Some(path) = s.strip_prefix("https://open.spotify.com/") else {
        return s.to_string();
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let parts: Vec<_> = path
        .split('/')
        .filter(|p| !p.is_empty() && !p.starts_with("intl-"))
        .collect();
    match parts[..] {
        [kind, id] => format!("spotify:{kind}:{id}"),
        _ => s.to_string(),
    }
}

/// The snapshot the daemon sends right after `Hello` (already consumed by the connect).
async fn state(conn: &mut Conn) -> Result<NowPlaying> {
    match proto::recv::<Msg>(conn).await? {
        Some(Msg::State(np)) => Ok(np),
        other => bail!("unexpected first message from daemon: {other:?}"),
    }
}

/// Sends `cmd` and returns its reply, keeping `np` current meanwhile.
async fn request(conn: &mut Conn, np: &mut NowPlaying, cmd: Cmd) -> Result<Msg> {
    proto::send(conn, &cmd).await?;
    loop {
        match proto::recv::<Msg>(conn).await? {
            Some(Msg::State(s)) => *np = s,
            Some(Msg::Error(e)) => bail!(e),
            Some(Msg::Hello { .. } | Msg::Library | Msg::Playlist(_) | Msg::Log(_)) => {}
            Some(reply) => return Ok(reply),
            None => bail!("daemon closed the connection"),
        }
    }
}

/// Sends `cmd`, which has no reply, and waits until the daemon has handled it.
async fn finish(mut conn: Conn, cmd: Cmd) -> Result<()> {
    proto::send(&mut conn, &cmd).await?;
    // Commands are handled in order and EOF ends the daemon's loop, so it closes after `cmd`.
    conn.get_mut().shutdown().await?;
    while let Some(msg) = proto::recv::<Msg>(&mut conn).await? {
        if let Msg::Error(e) = msg {
            bail!(e);
        }
    }
    Ok(())
}

/// A volume or position: absolute, or relative to the current one.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
pub enum Level {
    Set(u32),
    Relative(i32),
}

impl Level {
    /// `N` (absolute) or `+N`/`-N` (relative).
    pub fn parse(what: &str, arg: &str) -> Result<Level> {
        let rel = arg.starts_with(['+', '-']);
        match (rel, arg.parse::<i32>(), arg.parse::<u32>()) {
            (true, Ok(n), _) => Ok(Level::Relative(n)),
            (false, _, Ok(n)) => Ok(Level::Set(n)),
            _ => bail!("{what}: expected N, +N or -N, got {arg:?}"),
        }
    }

    fn apply(self, current: i64) -> i64 {
        match self {
            Level::Set(n) => n as i64,
            Level::Relative(n) => current + n as i64,
        }
    }
}

/// `level` is a percentage.
pub fn volume(level: Level, current: u16) -> u16 {
    let pct = level.apply(percent(current) as i64);
    (pct.clamp(0, 100) * u16::MAX as i64 / 100) as u16
}

/// Target position in ms for `level` in seconds (relative to the extrapolated position).
pub fn seek(level: Level, np: &NowPlaying, now_ms: u64) -> u32 {
    let current = np.position(now_ms) as i64;
    // Relative to the position in seconds, so sub-second precision survives.
    let target = level.apply(current / 1000) * 1000
        + if matches!(level, Level::Relative(_)) {
            current % 1000
        } else {
            0
        };
    target.clamp(0, np.duration_ms as i64) as u32
}

fn percent(volume: u16) -> u32 {
    (volume as u32 * 100 + u16::MAX as u32 / 2) / u16::MAX as u32
}

#[cfg(test)]
mod tests {
    use super::{Level, seek, spotify_uri, volume};
    use crate::proto::NowPlaying;

    fn l(s: &str) -> Level {
        Level::parse("test", s).unwrap()
    }

    #[test]
    fn volume_levels() {
        assert_eq!(volume(l("100"), 0), u16::MAX);
        assert_eq!(volume(l("0"), u16::MAX), 0);
        assert_eq!(volume(l("+5"), volume(l("40"), 0)), volume(l("45"), 0));
        assert_eq!(volume(l("-50"), volume(l("40"), 0)), 0);
        assert_eq!(volume(l("+500"), 0), u16::MAX);
        assert!(Level::parse("volume", "loud").is_err());
        assert!(Level::parse("volume", "-").is_err());
    }

    #[test]
    fn seek_positions() {
        let np = NowPlaying {
            duration_ms: 200_000,
            position_ms: 60_000,
            position_at_ms: 1_000,
            playing: true,
            ..Default::default()
        };
        assert_eq!(seek(l("30"), &np, 5_000), 30_000);
        assert_eq!(seek(l("+10"), &np, 5_000), 74_000);
        assert_eq!(
            seek(
                l("-10"),
                &NowPlaying {
                    playing: false,
                    ..np.clone()
                },
                5_000
            ),
            50_000
        );
        assert_eq!(seek(l("-100"), &np, 5_000), 0);
        assert_eq!(seek(l("999"), &np, 5_000), 200_000);
        assert!(Level::parse("position", "1:30").is_err());
    }

    #[test]
    fn links_to_uris() {
        assert_eq!(spotify_uri("spotify:album:x"), "spotify:album:x");
        assert_eq!(
            spotify_uri("https://open.spotify.com/track/abc?si=123"),
            "spotify:track:abc"
        );
        assert_eq!(
            spotify_uri("https://open.spotify.com/intl-it/playlist/p1"),
            "spotify:playlist:p1"
        );
        assert_eq!(
            spotify_uri("https://open.spotify.com/"),
            "https://open.spotify.com/"
        );
    }
}
