//! OS media controls for the Soloist engine (ECS gets them from Chromium's media session): MPRIS
//! on Linux, Now Playing and remote commands on macOS. Mirrors the daemon state and sends the
//! controls back as daemon commands.

use tokio::sync::{mpsc, watch};

use crate::proto::{Cmd, NowPlaying};

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "macos")]
mod now_playing;

#[cfg(target_os = "macos")]
pub use now_playing::main_loop;

pub type Commands = mpsc::UnboundedSender<Cmd>;

pub fn spawn(state: watch::Receiver<NowPlaying>, cmds: Commands) {
    #[cfg(target_os = "linux")]
    tokio::spawn(async move {
        if let Err(e) = mpris::run(state, cmds).await {
            log::warn!("mpris: {e:#}");
        }
    });
    #[cfg(target_os = "macos")]
    tokio::spawn(now_playing::run(state, cmds));
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (state, cmds);
}

/// Explicit play or pause, as the daemon's toggle; nothing when already in that state.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn set_playing(np: &NowPlaying, play: bool) -> Option<Cmd> {
    (np.playing != play).then_some(Cmd::PlayPause)
}
