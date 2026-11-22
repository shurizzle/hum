use std::{collections::HashMap, hash::Hash};

use image::DynamicImage;
use ratatui::{Frame, layout::Rect};
use ratatui_image::{
    FontSize, Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::cache::Cache;

/// Background results delivered to the UI loop.
pub enum Done {
    Downloaded(String, Option<DynamicImage>),
    Encoded(Key, ResizeResponse),
}

/// A cover at one on-screen size. The same cover shows at several sizes (card, bar, now playing):
/// sharing one encoder between them re-encodes every frame and blanks the image meanwhile.
type Key = (String, u16, u16);

/// Decoded covers kept in memory (320 px RGBA, ~400 KB each); evicted ones reload from the disk cache.
const MAX_COVERS: usize = 128;
/// Encoders kept, one per cover and size; sizes left over from resizes age out.
const MAX_ENCODERS: usize = 64;

enum Slot {
    Loading {
        task: AbortHandle,
        seen: bool,
    },
    /// The u64 is the last frame the cover was drawn in.
    Ready(DynamicImage, u64),
    Failed(u64),
}

/// Cover cache. Download, decode and terminal encoding all run off the UI thread; downloads of
/// covers that leave the screen are cancelled.
pub struct Images {
    picker: Picker,
    http: reqwest::Client,
    slots: HashMap<String, Slot>,
    protos: HashMap<Key, (ThreadProtocol, u64)>,
    tx: mpsc::UnboundedSender<Done>,
    frame: u64,
}

impl Images {
    pub fn new(tx: mpsc::UnboundedSender<Done>) -> Self {
        Self {
            picker: picker(),
            http: reqwest::Client::new(),
            slots: HashMap::new(),
            protos: HashMap::new(),
            tx,
            frame: 0,
        }
    }

    pub fn done(&mut self, done: Done) {
        match done {
            Done::Downloaded(url, Some(image)) => {
                _ = self.slots.insert(url, Slot::Ready(image, self.frame))
            }
            Done::Downloaded(url, None) => _ = self.slots.insert(url, Slot::Failed(self.frame)),
            Done::Encoded(key, res) => {
                if let Some((proto, _)) = self.protos.get_mut(&key) {
                    proto.update_resized_protocol(res);
                }
            }
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
        for slot in self.slots.values_mut() {
            if let Slot::Loading { seen, .. } = slot {
                *seen = false;
            }
        }
    }

    /// Cancels downloads of covers that were not drawn this frame; they restart when visible again.
    /// Then drops the least recently drawn covers and encoders over their caps.
    pub fn end_frame(&mut self) {
        self.slots.retain(|_, slot| match slot {
            Slot::Loading { task, seen: false } => {
                task.abort();
                false
            }
            _ => true,
        });
        evict(&mut self.slots, MAX_COVERS, self.frame, |slot| match slot {
            Slot::Ready(_, at) | Slot::Failed(at) => Some(*at),
            Slot::Loading { .. } => None,
        });
        // A dropped protocol drops its sender, which ends its encoder task.
        evict(&mut self.protos, MAX_ENCODERS, self.frame, |(_, at)| {
            Some(*at)
        });
    }

    /// Draws the cover at `url` over `area`, if it is ready. Callers draw a placeholder first.
    pub fn draw(&mut self, f: &mut Frame, url: Option<&str>, area: Rect) {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return;
        };
        let frame = self.frame;
        match self.slots.get_mut(url) {
            Some(Slot::Ready(image, at)) => {
                *at = frame;
                let key = (url.to_string(), area.width, area.height);
                let proto = self.protos.entry(key.clone()).or_insert_with(|| {
                    // Each encoder task ends when its protocol (and so its sender) drops.
                    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<ResizeRequest>();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        while let Some(req) = req_rx.recv().await {
                            let Ok(Ok(res)) =
                                tokio::task::spawn_blocking(|| req.resize_encode()).await
                            else {
                                continue;
                            };
                            if tx.send(Done::Encoded(key.clone(), res)).is_err() {
                                return;
                            }
                        }
                    });
                    (
                        ThreadProtocol::new(
                            req_tx,
                            Some(self.picker.new_resize_protocol(image.clone())),
                        ),
                        frame,
                    )
                });
                proto.1 = frame;
                f.render_stateful_widget(
                    StatefulImage::default().resize(Resize::Scale(None)),
                    area,
                    &mut proto.0,
                );
            }
            Some(Slot::Loading { seen, .. }) => *seen = true,
            Some(Slot::Failed(at)) => *at = frame,
            None => {
                let (http, tx, url) = (self.http.clone(), self.tx.clone(), url.to_string());
                let key = url.clone();
                let task = tokio::spawn(async move {
                    // Covers never change for a url: a disk hit skips the network for good.
                    let key = format!("cover:{url}");
                    let bytes = match Cache::global().read_touch(&key) {
                        Some(b) => Some(b.into()),
                        None => {
                            let b = async {
                                http.get(&url)
                                    .send()
                                    .await?
                                    .error_for_status()?
                                    .bytes()
                                    .await
                            }
                            .await
                            .ok();
                            if let Some(b) = &b {
                                Cache::global().write(&key, b);
                            }
                            b
                        }
                    };
                    let image = match bytes {
                        Some(b) => tokio::task::spawn_blocking(move || {
                            // Small source keeps terminal encoding cheap; covers render at most ~40 cells wide.
                            image::load_from_memory(&b)
                                .ok()
                                .map(|i| i.thumbnail(320, 320))
                        })
                        .await
                        .ok()
                        .flatten(),
                        None => None,
                    };
                    let _ = tx.send(Done::Downloaded(url, image));
                })
                .abort_handle();
                self.slots.insert(key, Slot::Loading { task, seen: true });
            }
        }
    }
}

/// Over `cap`, removes the entries with the oldest stamps down to 3/4 of `cap` (so sorting is rare).
/// Entries without a stamp (`None`) or drawn in frame `now` are never removed.
fn evict<K: Clone + Eq + Hash, V>(
    map: &mut HashMap<K, V>,
    cap: usize,
    now: u64,
    stamp: impl Fn(&V) -> Option<u64>,
) {
    if map.len() <= cap {
        return;
    }
    let mut old: Vec<_> = map
        .iter()
        .filter_map(|(k, v)| stamp(v).filter(|&at| at < now).map(|at| (at, k.clone())))
        .collect();
    old.sort_unstable_by_key(|(at, _)| *at);
    let excess = map.len() - cap * 3 / 4;
    for (_, k) in old.into_iter().take(excess) {
        map.remove(&k);
    }
}

/// Picks the image protocol from the environment instead of `Picker::from_query_stdio`: when a
/// terminal leaves a query unanswered, that query's thread stays blocked on stdin, eats the first
/// input and then restores cooked mode under the running TUI.
#[allow(deprecated)]
fn picker() -> Picker {
    let font = crossterm::terminal::window_size()
        .ok()
        .filter(|s| s.width > 0 && s.height > 0 && s.columns > 0 && s.rows > 0)
        .map(|s| FontSize::new(s.width / s.columns, s.height / s.rows))
        .unwrap_or(FontSize::new(8, 16));
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let (term, program) = (var("TERM"), var("TERM_PROGRAM"));
    let protocol = if !var("KITTY_WINDOW_ID").is_empty()
        || term == "xterm-kitty"
        || term == "xterm-ghostty"
        || program == "ghostty"
    {
        ProtocolType::Kitty
    } else if program == "iTerm.app" || program == "WezTerm" {
        // WezTerm lacks the kitty unicode placeholders ratatui-image relies on, but supports iTerm2 images.
        ProtocolType::Iterm2
    } else {
        ProtocolType::Halfblocks
    };
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(protocol);
    picker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_drops_oldest_down_to_three_quarters() {
        // Stamps 0..10; key 0 is unstamped, key 9 was drawn this frame.
        let mut map: HashMap<u32, Option<u64>> = (0..10).map(|k| (k, Some(k as u64))).collect();
        map.insert(0, None);
        evict(&mut map, 10, 9, |v| *v);
        assert_eq!(map.len(), 10, "at cap: untouched");
        map.insert(10, Some(9));
        evict(&mut map, 10, 9, |v| *v);
        let mut keys: Vec<_> = map.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, [0, 5, 6, 7, 8, 9, 10]);
    }
}
