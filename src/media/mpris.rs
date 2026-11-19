//! MPRIS (`org.mpris.MediaPlayer2.hum`) on the session bus.

use std::collections::HashMap;

use anyhow::Result;
use tokio::sync::watch;
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, OwnedValue, Value},
};

use super::Commands;
use crate::proto::{self, Cmd, NowPlaying, Repeat};

const PATH: &str = "/org/mpris/MediaPlayer2";

struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        "hum".into()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    state: watch::Receiver<NowPlaying>,
    cmds: Commands,
}

impl Player {
    fn send(&self, cmd: Option<Cmd>) {
        if let Some(cmd) = cmd {
            let _ = self.cmds.send(cmd);
        }
    }
}

/// MPRIS track id: the uri's base62 id under `/org/mpris/MediaPlayer2/Track/`.
fn track_id(np: &NowPlaying) -> ObjectPath<'static> {
    let id: String = np.uri.rsplit(':').next().unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).collect();
    let path = if id.is_empty() { "/org/mpris/MediaPlayer2/TrackList/NoTrack".into() } else { format!("{PATH}/Track/{id}") };
    ObjectPath::try_from(path).unwrap_or_else(|_| ObjectPath::from_static_str_unchecked("/org/mpris/MediaPlayer2/TrackList/NoTrack"))
}

fn us(ms: u64) -> i64 {
    ms as i64 * 1000
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        self.send(Some(Cmd::Next));
    }

    fn previous(&self) {
        self.send(Some(Cmd::Prev));
    }

    fn pause(&self) {
        self.send(super::set_playing(&self.state.borrow(), false));
    }

    fn play_pause(&self) {
        self.send(Some(Cmd::PlayPause));
    }

    fn stop(&self) {
        self.pause();
    }

    fn play(&self) {
        self.send(super::set_playing(&self.state.borrow(), true));
    }

    fn seek(&self, offset: i64) {
        let np = self.state.borrow().clone();
        let target = us(np.position(proto::now_ms())) + offset;
        // Seeking past the end goes to the next track, per the spec.
        self.send(Some(if target > us(np.duration_ms as u64) { Cmd::Next } else { Cmd::Seek((target.max(0) / 1000) as u32) }));
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        let np = self.state.borrow().clone();
        if track_id == self::track_id(&np) && (0..=us(np.duration_ms as u64)).contains(&position) {
            self.send(Some(Cmd::Seek((position / 1000) as u32)));
        }
    }

    fn open_uri(&self, _uri: String) {}

    #[zbus(property)]
    fn playback_status(&self) -> String {
        let np = self.state.borrow();
        match (np.uri.is_empty(), np.playing) {
            (true, _) => "Stopped",
            (_, true) => "Playing",
            _ => "Paused",
        }
        .into()
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        match self.state.borrow().repeat {
            Repeat::Off => "None",
            Repeat::Context => "Playlist",
            Repeat::Track => "Track",
        }
        .into()
    }

    #[zbus(property)]
    fn set_loop_status(&mut self, status: String) {
        let repeat = match status.as_str() {
            "Playlist" => Repeat::Context,
            "Track" => Repeat::Track,
            _ => Repeat::Off,
        };
        self.send(Some(Cmd::Repeat(repeat)));
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.state.borrow().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&mut self, on: bool) {
        self.send(Some(Cmd::Shuffle(on)));
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let np = self.state.borrow();
        let mut meta = HashMap::new();
        let mut put = |key: &str, value: Value<'_>| {
            if let Ok(value) = OwnedValue::try_from(value) {
                meta.insert(key.to_owned(), value);
            }
        };
        put("mpris:trackid", track_id(&np).into());
        if np.uri.is_empty() {
            return meta;
        }
        put("mpris:length", us(np.duration_ms as u64).into());
        put("xesam:title", np.name.as_str().into());
        put("xesam:artist", np.artists.clone().into());
        put("xesam:album", np.album.as_str().into());
        if !np.cover.is_empty() {
            put("mpris:artUrl", np.cover.as_str().into());
        }
        meta
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state.borrow().volume as f64 / u16::MAX as f64
    }

    #[zbus(property)]
    fn set_volume(&mut self, volume: f64) {
        self.send(Some(Cmd::Volume((volume.clamp(0.0, 1.0) * u16::MAX as f64) as u16)));
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        us(self.state.borrow().position(proto::now_ms()))
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.state.borrow().duration_ms > 0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

pub async fn run(mut state: watch::Receiver<NowPlaying>, cmds: Commands) -> Result<()> {
    let conn = zbus::connection::Builder::session()?
        .name("org.mpris.MediaPlayer2.hum")?
        .serve_at(PATH, Root)?
        .serve_at(PATH, Player { state: state.clone(), cmds })?
        .build()
        .await?;
    let iface = conn.object_server().interface::<_, Player>(PATH).await?;
    let mut last = state.borrow_and_update().clone();
    while state.changed().await.is_ok() {
        let np = state.borrow_and_update().clone();
        let (player, emitter) = (iface.get().await, iface.signal_emitter());
        if np.playing != last.playing || np.uri.is_empty() != last.uri.is_empty() {
            player.playback_status_changed(emitter).await?;
        }
        let track = |n: &NowPlaying| (n.uri.clone(), n.name.clone(), n.artists.clone(), n.album.clone(), n.cover.clone(), n.duration_ms);
        if track(&np) != track(&last) {
            player.metadata_changed(emitter).await?;
            player.can_seek_changed(emitter).await?;
        }
        if np.volume != last.volume {
            player.volume_changed(emitter).await?;
        }
        if np.shuffle != last.shuffle {
            player.shuffle_changed(emitter).await?;
        }
        if np.repeat != last.repeat {
            player.loop_status_changed(emitter).await?;
        }
        // A position away from where the previous report's clock puts it is a seek, not playback.
        if np.uri == last.uri && (np.position_ms as u64).abs_diff(last.position(np.position_at_ms)) > 1500 {
            Player::seeked(emitter, us(np.position_ms as u64)).await?;
        }
        last = np;
    }
    Ok(())
}
