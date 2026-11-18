//! macOS Now Playing (Control Center, media keys) through MediaPlayer. Every MediaPlayer call runs
//! on the main queue, which `main_loop` serves.

use std::ptr::NonNull;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{AnyThread, rc::Retained, runtime::AnyObject};
use objc2_app_kit::NSImage;
use objc2_foundation::{NSData, NSDefaultRunLoopMode, NSMachPort, NSMutableDictionary, NSNumber, NSRunLoop, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState,
    MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use tokio::sync::watch;

use super::Commands;
use crate::proto::{self, Cmd, NowPlaying};

/// Serves the main run loop (and with it the main queue) forever; the daemon runs on another thread.
pub fn main_loop() -> ! {
    let run_loop = NSRunLoop::mainRunLoop();
    // A run loop without sources returns at once: a port keeps it waiting.
    unsafe { run_loop.addPort_forMode(&NSMachPort::new(), NSDefaultRunLoopMode) };
    loop {
        run_loop.run();
    }
}

pub async fn run(mut state: watch::Receiver<NowPlaying>, cmds: Commands) {
    let commands_state = state.clone();
    DispatchQueue::main().exec_async(move || commands(cmds, commands_state));
    let mut art: (String, Option<Vec<u8>>) = Default::default();
    loop {
        let np = state.borrow_and_update().clone();
        if np.cover != art.0 {
            art = (np.cover.clone(), cover(&np.cover).await);
        }
        let image = art.1.clone();
        DispatchQueue::main().exec_async(move || publish(&np, image.as_deref()));
        if state.changed().await.is_err() {
            return;
        }
    }
}

async fn cover(url: &str) -> Option<Vec<u8>> {
    if url.is_empty() {
        return None;
    }
    let bytes = async { reqwest::get(url).await?.error_for_status()?.bytes().await };
    bytes.await.inspect_err(|e| log::debug!("cover {url}: {e}")).ok().map(Vec::from)
}

fn commands(cmds: Commands, state: watch::Receiver<NowPlaying>) {
    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let on = |command: &MPRemoteCommand, f: fn(&MPRemoteCommandEvent, &NowPlaying) -> Option<Cmd>| {
        let (cmds, state) = (cmds.clone(), state.clone());
        let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            if let Some(cmd) = f(unsafe { event.as_ref() }, &state.borrow()) {
                let _ = cmds.send(cmd);
            }
            MPRemoteCommandHandlerStatus::Success
        });
        unsafe {
            command.setEnabled(true);
            command.addTargetWithHandler(&handler);
        }
    };
    unsafe {
        on(&center.togglePlayPauseCommand(), |_, _| Some(Cmd::PlayPause));
        on(&center.playCommand(), |_, np| super::set_playing(np, true));
        on(&center.pauseCommand(), |_, np| super::set_playing(np, false));
        on(&center.nextTrackCommand(), |_, _| Some(Cmd::Next));
        on(&center.previousTrackCommand(), |_, _| Some(Cmd::Prev));
        on(&center.changePlaybackPositionCommand(), |e, _| {
            let e = e.downcast_ref::<MPChangePlaybackPositionCommandEvent>()?;
            Some(Cmd::Seek((e.positionTime().max(0.0) * 1000.0) as u32))
        });
    }
}

fn publish(np: &NowPlaying, image: Option<&[u8]>) {
    let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
    if np.uri.is_empty() {
        unsafe {
            center.setNowPlayingInfo(None);
            center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
        return;
    }
    let info = NSMutableDictionary::<NSString, AnyObject>::new();
    let put = |key: &NSString, value: &AnyObject| info.insert(key, value);
    let secs = |ms: u64| NSNumber::new_f64(ms as f64 / 1000.0);
    unsafe {
        put(MPMediaItemPropertyTitle, &NSString::from_str(&np.name));
        put(MPMediaItemPropertyArtist, &NSString::from_str(&np.artists));
        put(MPMediaItemPropertyAlbumTitle, &NSString::from_str(&np.album));
        put(MPMediaItemPropertyPlaybackDuration, &secs(np.duration_ms as u64));
        put(MPNowPlayingInfoPropertyElapsedPlaybackTime, &secs(np.position(proto::now_ms())));
        put(MPNowPlayingInfoPropertyPlaybackRate, &NSNumber::new_f64(if np.playing { 1.0 } else { 0.0 }));
        if let Some(image) = image.and_then(|b| NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(b))) {
            let size = image.size();
            let handler = RcBlock::new(move |_| NonNull::from(&*image));
            let artwork: Retained<MPMediaItemArtwork> =
                MPMediaItemArtwork::initWithBoundsSize_requestHandler(MPMediaItemArtwork::alloc(), size, &handler);
            put(MPMediaItemPropertyArtwork, &artwork);
        }
        center.setNowPlayingInfo(Some(&info));
        center.setPlaybackState(if np.playing { MPNowPlayingPlaybackState::Playing } else { MPNowPlayingPlaybackState::Paused });
    }
}
