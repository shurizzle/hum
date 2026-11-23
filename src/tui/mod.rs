mod draw;
mod images;
pub mod keys;

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, layout::Rect};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    api::{Api, Item},
    cli::{self, OnOff},
    proto::{self, Cmd, Conn, Msg, NowPlaying},
};
use images::{Done, Images};
use keys::{
    Action, Dir, Keymap, LikeTarget, PlayTarget, QueueTarget, Set, ShareTarget, Step, Toggle,
    Vertical,
};

const TOKEN_REFRESH: Duration = Duration::from_secs(30 * 60);
/// Upcoming tracks shown; each uncached one costs a Web API call.
const QUEUE_LEN: usize = 20;
/// Synced lyrics follow the song again this long after a manual scroll.
pub const LYRICS_FOLLOW: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Horizontal row of cover cards.
    Shelf,
    /// Vertical track list.
    List,
    /// Compact tiles in rows, like the shortcuts at the top of Spotify's home.
    Grid,
}

pub struct Sec {
    pub title: String,
    pub kind: Kind,
    pub items: Vec<Item>,
    /// First visible card of a shelf.
    pub off: usize,
    /// Tiles per row of a grid, set by the renderer.
    pub cols: usize,
}

impl Sec {
    fn new(title: impl Into<String>, kind: Kind, items: Vec<Item>) -> Self {
        Self {
            title: title.into(),
            kind,
            items,
            off: 0,
            cols: 1,
        }
    }

    /// Item to land on when entering this section from above or below.
    fn entry(&self, from_above: bool) -> usize {
        match self.kind {
            Kind::Shelf => self.off,
            Kind::List if !from_above => self.items.len() - 1,
            Kind::Grid if !from_above => {
                (self.items.len() - 1) / self.cols.max(1) * self.cols.max(1)
            }
            _ => 0,
        }
    }
}

pub struct Page {
    pub title: String,
    /// The opened entity (playlist, album, artist), shown as a big header.
    pub header: Option<Item>,
    /// Context to play tracks in.
    pub context: Option<String>,
    pub secs: Vec<Sec>,
    /// Selected (section, item).
    pub sel: (usize, usize),
    /// First visible layout block, maintained by the renderer.
    pub top: usize,
    /// More items are still being fetched.
    pub loading: bool,
    /// Item to select once it streams in (the playing track on startup).
    focus: Option<String>,
}

impl Page {
    fn new(title: impl Into<String>, header: Option<Item>, secs: Vec<Sec>) -> Self {
        let context = header.as_ref().map(|h| h.uri.clone());
        let secs: Vec<Sec> = secs.into_iter().filter(|s| !s.items.is_empty()).collect();
        Self {
            title: title.into(),
            header,
            context,
            secs,
            sel: (0, 0),
            top: 0,
            loading: false,
            focus: None,
        }
    }

    pub fn selected(&self) -> Option<&Item> {
        self.secs.get(self.sel.0)?.items.get(self.sel.1)
    }

    fn vertical(&mut self, down: bool) {
        let (s, i) = self.sel;
        let Some(sec) = self.secs.get(s) else { return };
        let step = match sec.kind {
            Kind::List => 1,
            Kind::Grid => sec.cols.max(1),
            Kind::Shelf => usize::MAX,
        };
        if sec.kind != Kind::Shelf && down && i + step < sec.items.len() {
            self.sel.1 += step;
        } else if sec.kind != Kind::Shelf && !down && i >= step {
            self.sel.1 -= step;
        } else if down && s + 1 < self.secs.len() {
            self.sel = (s + 1, self.secs[s + 1].entry(true));
        } else if !down && s > 0 {
            self.sel = (s - 1, self.secs[s - 1].entry(false));
        }
    }

    /// To the first or last section, landing like `vertical` does (`gg` / `G`).
    fn edge(&mut self, last: bool) {
        if let Some(s) = if last {
            self.secs.len().checked_sub(1)
        } else {
            (!self.secs.is_empty()).then_some(0)
        } {
            self.sel = (s, self.secs[s].entry(!last));
        }
    }

    fn horizontal(&mut self, right: bool) {
        let Some(sec) = self.secs.get(self.sel.0).filter(|s| s.kind != Kind::List) else {
            return;
        };
        if right && self.sel.1 + 1 < sec.items.len() {
            self.sel.1 += 1;
        } else if !right && self.sel.1 > 0 {
            self.sel.1 -= 1;
        }
    }
}

/// Adds items as a track list first, then one shelf per entity kind, like Spotify's artist pages. Items already present are skipped, so pages can arrive incrementally.
fn add(secs: &mut Vec<Sec>, items: Vec<Item>, songs: &str) {
    for item in items {
        if secs
            .iter()
            .any(|s| s.items.iter().any(|i| i.uri == item.uri))
        {
            continue;
        }
        if matches!(item.kind(), "track" | "episode") {
            match secs.iter_mut().find(|s| s.kind == Kind::List) {
                Some(sec) => sec.items.push(item),
                None => secs.insert(0, Sec::new(songs, Kind::List, vec![item])),
            }
            continue;
        }
        let title = match item.kind() {
            "album" => "Albums",
            "artist" => "Artists",
            "playlist" => "Playlists",
            "show" => "Podcasts",
            other => other,
        }
        .to_string();
        match secs.iter_mut().find(|s| s.title == title) {
            Some(sec) => sec.items.push(item),
            None => secs.push(Sec::new(title, Kind::Shelf, vec![item])),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Library,
    Main,
    Queue,
}

/// Clickable regions, collected while drawing.
#[derive(Clone)]
pub enum Hit {
    Main(usize, usize),
    Library(usize),
    Queue(usize),
    /// Device picker toggle in the player bar.
    Devices,
    /// Lyrics toggle in the player bar.
    Lyrics,
    /// Row of the device picker.
    Device(usize),
    Cmd(Cmd),
    Seek,
    Volume,
    /// Speaker icon: mute toggle.
    Mute,
    Search,
    Home,
    Back,
    ToggleQueue,
    /// A heart: like/unlike this uri.
    Like(String),
}

enum Loaded {
    Page {
        generation: u64,
        push: bool,
        page: Page,
    },
    /// Next chunk of the page opened at `generation`.
    Fill {
        generation: u64,
        items: Vec<Item>,
        more: bool,
        root: Option<Item>,
    },
    Library(Vec<Item>),
    /// Liked state of tracks, albums, artists; also restores it when a like fails.
    Saved(Vec<(String, bool)>),
    Status(String),
}

pub struct App {
    api: Api,
    tx: mpsc::UnboundedSender<Loaded>,
    pub images: Images,
    pub stack: Vec<Page>,
    generation: u64,
    /// Fetch for the current page; aborted when the user navigates away.
    page_task: Option<AbortHandle>,
    pub library: Vec<Item>,
    /// Liked (saved) state by uri, for what `api.saved` supports; playlists use `library`.
    saved: std::collections::HashMap<String, bool>,
    pub lib_sel: usize,
    pub lib_top: usize,
    pub lib_area: Rect,
    pub queue_sel: usize,
    pub queue_top: usize,
    pub queue_area: Rect,
    focus: Focus,
    pub np: NowPlaying,
    pub queue: Vec<Item>,
    /// Commands queued outside input handling, sent by the event loop.
    outbox: Vec<Cmd>,
    /// Tracks already fetched for the queue, by uri.
    queue_cache: std::collections::HashMap<String, Item>,
    pub show_queue: bool,
    pub show_library: bool,
    /// Lyrics replace the main page while on.
    pub show_lyrics: bool,
    /// Lyrics of the track in `.0`; `None` or another uri while loading.
    pub lyrics: Option<(String, Vec<proto::LyricsLine>)>,
    /// First lyrics line drawn in the last frame.
    pub lyrics_top: usize,
    /// Manual scroll position and when it was set; synced lyrics follow the song again
    /// `LYRICS_FOLLOW` after the last scroll.
    pub lyrics_scroll: Option<(usize, Instant)>,
    /// Key help overlay; any key or click closes it.
    pub help: bool,
    /// Connect device picker overlay and its selection.
    pub devices_open: bool,
    pub dev_sel: usize,
    /// Last `Msg::Devices`; `None` while loading.
    pub devices: Option<Vec<proto::Device>>,
    pub shuffle: bool,
    pub repeat: proto::Repeat,
    /// Text being typed in the search box.
    pub search: Option<String>,
    pub status: String,
    pub hits: Vec<(Rect, Hit)>,
    quit: bool,
    /// Startup found no playing context; open it if the daemon reports one before the user acts.
    resume_pending: bool,
    pub keymap: Keymap,
    /// Keys of a sequence typed so far, and when it stops waiting for more.
    pending: Vec<keys::Key>,
    key_deadline: Option<tokio::time::Instant>,
    /// Kept alive: on X11/Wayland the copied text lives only while its owner does.
    clipboard: Option<arboard::Clipboard>,
}

pub async fn run(mut conn: Conn, keymap: Keymap) -> Result<()> {
    proto::send(&mut conn, &Cmd::Token).await?;
    let mut np = NowPlaying::default();
    let tokens = loop {
        match proto::recv::<Msg>(&mut conn).await? {
            Some(Msg::Tokens(t)) => break t,
            Some(Msg::State(s)) => np = s,
            Some(
                Msg::Hello { .. }
                | Msg::Tracks(_)
                | Msg::Devices(_)
                | Msg::Lyrics { .. }
                | Msg::Library
                | Msg::Playlist(_)
                | Msg::Log(_),
            ) => {}
            // Usually the network (tokens need Spotify); the daemon keeps running, so ask again.
            Some(Msg::Error(e)) => {
                eprintln!("daemon: {e}; retrying in 3 s (Ctrl-C to quit)");
                tokio::time::sleep(Duration::from_secs(3)).await;
                proto::send(&mut conn, &Cmd::Token).await?;
            }
            None => bail!("daemon closed the connection"),
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (img_tx, mut img_rx) = mpsc::unbounded_channel();
    let mut app = App {
        api: Api::new(tokens),
        tx,
        images: Images::new(img_tx),
        stack: vec![],
        generation: 0,
        page_task: None,
        library: vec![],
        saved: Default::default(),
        lib_sel: 0,
        lib_top: 0,
        lib_area: Rect::default(),
        queue_sel: 0,
        queue_top: 0,
        queue_area: Rect::default(),
        focus: Focus::Main,
        np,
        queue: vec![],
        outbox: vec![],
        queue_cache: Default::default(),
        show_queue: true,
        show_library: true,
        show_lyrics: false,
        lyrics: None,
        lyrics_top: 0,
        lyrics_scroll: None,
        help: false,
        devices_open: false,
        dev_sel: 0,
        devices: None,
        shuffle: false,
        repeat: proto::Repeat::Off,
        search: None,
        status: String::new(),
        hits: vec![],
        quit: false,
        resume_pending: false,
        keymap,
        pending: vec![],
        key_deadline: None,
        clipboard: None,
    };
    // Resume like the desktop app: the playing context, else the last page, else home.
    let playing = Some(Item::from_context(&app.np.context)).filter(Item::browsable);
    app.resume_pending = playing.is_none();
    match playing
        .map(|i| (i, Some(app.np.uri.clone())))
        .or_else(|| last_page().map(|i| (i, None)))
    {
        Some((item, focus)) => app.open_at(item, focus),
        None => app.home(),
    }
    app.load_library();
    app.load_queue();

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let res = event_loop(&mut terminal, &mut app, &mut conn, &mut rx, &mut img_rx).await;
    save_last_page(app.page().and_then(|p| p.header.as_ref()));
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    conn: &mut Conn,
    rx: &mut mpsc::UnboundedReceiver<Loaded>,
    img_rx: &mut mpsc::UnboundedReceiver<Done>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(500));
    let mut refresh = tokio::time::interval(TOKEN_REFRESH);
    refresh.tick().await;
    let mut dirty = true;
    while !app.quit {
        if dirty {
            terminal.draw(|f| draw::draw(f, app))?;
        }
        dirty = true;
        let cmds = tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    app.resume_pending = false;
                    app.key(key)
                }
                // Terminals report every pointer motion; redrawing (and re-encoding covers) on each one lags clicks.
                Some(Ok(Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, .. }))) => {
                    dirty = false;
                    vec![]
                }
                Some(Ok(Event::Mouse(mouse))) => {
                    app.resume_pending = false;
                    app.mouse(mouse)
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
                _ => vec![],
            },
            msg = proto::recv::<Msg>(conn) => {
                match msg? {
                    Some(Msg::State(s)) => {
                        let queue_changed = (&s.uri, &s.context, &s.next) != (&app.np.uri, &app.np.context, &app.np.next);
                        // The daemon restores the last playback a few seconds after it starts.
                        if app.resume_pending && let Some(item) = Some(Item::from_context(&s.context)).filter(Item::browsable) {
                            app.resume_pending = false;
                            app.open_at(item, Some(s.uri.clone()));
                        }
                        // Follow changes made elsewhere; a stale state must not undo a local toggle.
                        if (s.shuffle, s.repeat) != (app.np.shuffle, app.np.repeat) {
                            (app.shuffle, app.repeat) = (s.shuffle, s.repeat);
                        }
                        app.np = s;
                        app.check_saved(vec![app.np.uri.clone()]);
                        app.request_lyrics();
                        // After the assignment: the fallback reads `np.next`.
                        if queue_changed {
                            app.load_queue();
                        }
                    }
                    Some(Msg::Tokens(t)) => *app.api.tokens.write() = t,
                    Some(Msg::Lyrics { uri, lines }) => {
                        if uri == app.np.uri {
                            app.lyrics = Some((uri, lines));
                        }
                    }
                    Some(Msg::Devices(devices)) => {
                        app.dev_sel = devices.iter().position(|d| d.active).unwrap_or(0);
                        app.devices = Some(devices);
                    }
                    Some(Msg::Tracks(items)) => {
                        app.queue_cache.extend(items.into_iter().map(|i| (i.uri.clone(), i)));
                        app.show_queue_window();
                    }
                    Some(Msg::Library) => app.library_changed(),
                    Some(Msg::Playlist(uri)) => app.playlist_changed(&uri),
                    Some(Msg::Hello { .. }) => {}
                    Some(Msg::Log(e)) => app.status = e,
                    Some(Msg::Error(e)) => {
                        app.status = e;
                        // Undo an optimistic queue removal that failed.
                        app.show_queue_window();
                    }
                    None => bail!("daemon closed the connection"),
                }
                vec![]
            }
            Some(loaded) = rx.recv() => {
                match loaded {
                    Loaded::Page { generation, push, page } if generation == app.generation => {
                        if !push {
                            app.stack.clear();
                        }
                        let uris = page.secs.iter().flat_map(|s| &s.items).map(|i| i.uri.clone()).collect();
                        app.stack.push(page);
                        app.focus = Focus::Main;
                        app.status.clear();
                        app.check_saved(uris);
                    }
                    Loaded::Fill { generation, items, more, root } if generation == app.generation => {
                        let header = app.page().and_then(|p| p.header.as_ref()).map(|h| h.uri.clone());
                        app.check_saved(items.iter().map(|i| i.uri.clone()).chain(header).collect());
                        if let Some(page) = app.stack.last_mut() {
                            let songs = if page.context.as_deref().is_some_and(|c| c.starts_with("spotify:artist:")) { "Popular" } else { "Songs" };
                            add(&mut page.secs, items, songs);
                            page.loading = more;
                            if let Some(root) = root.filter(|r| page.header.as_ref().is_some_and(|h| h.name.is_empty() && h.uri == r.uri)) {
                                page.title = root.name.clone();
                                page.header = Some(root);
                            }
                            if let Some(uri) = page.focus.clone()
                                && let Some(sel) = page.secs.iter().enumerate().find_map(|(s, sec)| sec.items.iter().position(|i| i.uri == uri).map(|i| (s, i)))
                            {
                                page.sel = sel;
                                page.focus = None;
                            }
                        }
                        if !more {
                            app.status.clear();
                        }
                    }
                    Loaded::Page { .. } | Loaded::Fill { .. } => {}
                    Loaded::Library(items) => app.library = items,
                    Loaded::Saved(saved) => app.saved.extend(saved),
                    Loaded::Status(s) => app.status = s,
                }
                vec![]
            }
            Some(done) = img_rx.recv() => {
                app.images.done(done);
                vec![]
            }
            _ = tokio::time::sleep_until(app.key_deadline.unwrap_or_else(tokio::time::Instant::now)), if app.key_deadline.is_some() => app.key_timeout(),
            _ = refresh.tick() => vec![Cmd::Token],
            _ = redraw.tick() => vec![],
        };
        for cmd in cmds.into_iter().chain(std::mem::take(&mut app.outbox)) {
            proto::send(conn, &cmd).await?;
        }
    }
    Ok(())
}

impl App {
    fn spawn<F>(&self, fetch: F) -> AbortHandle
    where
        F: Future<Output = Result<Loaded>> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(
                fetch
                    .await
                    .unwrap_or_else(|e| Loaded::Status(format!("{e:#}"))),
            );
        })
        .abort_handle()
    }

    /// Cancels the in-flight page fetch and starts tracking a new navigation.
    fn navigate(&mut self) -> u64 {
        if let Some(task) = self.page_task.take() {
            task.abort();
        }
        self.generation += 1;
        self.generation
    }

    /// Fetches a page in the background; `generation` drops results of navigation the user already left.
    fn load<F>(&mut self, push: bool, fetch: F)
    where
        F: Future<Output = Result<Page>> + Send + 'static,
    {
        let generation = self.navigate();
        self.status = "loading…".into();
        self.page_task = Some(self.spawn(async move {
            Ok(Loaded::Page {
                generation,
                push,
                page: fetch.await?,
            })
        }));
    }

    fn home(&mut self) {
        let api = self.api.clone();
        self.load(false, async move {
            // The untitled first section is Spotify's shortcuts block.
            let secs = api
                .home()
                .await?
                .into_iter()
                .enumerate()
                .map(|(i, mut s)| {
                    if i == 0 && s.title.is_empty() {
                        // Spotify shows at most 8 shortcuts.
                        s.items.truncate(8);
                        return Sec::new(s.title, Kind::Grid, s.items);
                    }
                    Sec::new(s.title, Kind::Shelf, s.items)
                })
                .collect();
            Ok(Page::new("Home", None, secs))
        });
    }

    /// Fetches the liked state of the `uris` not known yet.
    fn check_saved(&mut self, mut uris: Vec<String>) {
        uris.retain(|u| {
            saveable(u) && !u.starts_with("spotify:playlist:") && !self.saved.contains_key(u)
        });
        uris.dedup();
        if uris.is_empty() {
            return;
        }
        // Shown as not liked until the answer arrives; also keeps the uris from being asked again.
        self.saved.extend(uris.iter().map(|u| (u.clone(), false)));
        self.fetch_saved(uris);
    }

    fn fetch_saved(&self, uris: Vec<String>) {
        let api = self.api.clone();
        for chunk in uris.chunks(50).map(<[String]>::to_vec) {
            let api = api.clone();
            self.spawn(async move {
                Ok(Loaded::Saved(
                    chunk
                        .iter()
                        .cloned()
                        .zip(api.saved(&chunk).await?)
                        .collect(),
                ))
            });
        }
    }

    pub fn liked(&self, uri: &str) -> Option<bool> {
        liked(&self.saved, &self.library, uri)
    }

    /// Your Library changed (`Msg::Library`), maybe on another device: reload it and the hearts on
    /// screen. Other known hearts are forgotten and asked again when shown.
    fn library_changed(&mut self) {
        self.load_library();
        let page = self
            .page()
            .into_iter()
            .flat_map(|p| p.header.iter().chain(p.secs.iter().flat_map(|s| &s.items)));
        let shown: std::collections::HashSet<String> = page
            .map(|i| i.uri.clone())
            .chain([self.np.uri.clone()])
            .filter(|u| self.saved.contains_key(u))
            .collect();
        self.saved.retain(|u, _| shown.contains(u));
        self.fetch_saved(shown.into_iter().collect());
    }

    /// The open playlist changed (`Msg::Playlist`): reload it in place, keeping the selection.
    fn playlist_changed(&mut self, uri: &str) {
        let Some(page) = self
            .page()
            .filter(|p| p.header.as_ref().is_some_and(|h| h.uri == uri))
        else {
            return;
        };
        let (header, focus) = (
            page.header.clone().unwrap(),
            page.selected().map(|i| i.uri.clone()),
        );
        let panel = self.focus;
        self.stack.pop();
        self.open_at(header, focus);
        self.focus = panel;
    }

    /// The heart: likes or unlikes at once, restoring the shown state if Spotify refuses.
    fn toggle_like(&mut self, uri: String) {
        let Some(was) = self.liked(&uri) else { return };
        let playlist = uri.starts_with("spotify:playlist:");
        if playlist {
            if was {
                self.library.retain(|i| i.uri != uri);
            } else {
                // Shown until the library reload brings the real entry.
                self.library.insert(0, Item::from_uri(&uri));
            }
        } else {
            self.saved.insert(uri.clone(), !was);
        }
        let place = if uri.starts_with("spotify:track:") {
            "Liked Songs"
        } else {
            "Your Library"
        };
        self.status = if was {
            format!("Removed from {place}")
        } else {
            format!("Added to {place}")
        };
        let (api, tx) = (self.api.clone(), self.tx.clone());
        tokio::spawn(async move {
            let res = api.set_saved(&uri, !was).await;
            if let Err(e) = &res {
                let _ = tx.send(Loaded::Status(format!("{e:#}")));
                if !playlist {
                    let _ = tx.send(Loaded::Saved(vec![(uri, was)]));
                }
            }
            // Albums, artists, shows and playlists live in Your Library; a failed playlist change
            // also needs it to drop the shown placeholder.
            if (res.is_ok() || playlist)
                && let Ok(items) = api.library().await
            {
                let _ = tx.send(Loaded::Library(items));
            }
        });
    }

    fn load_library(&self) {
        let api = self.api.clone();
        self.spawn(async move { Ok(Loaded::Library(api.library().await?)) });
    }

    /// Upcoming tracks shown, from the daemon's Connect cluster (`NowPlaying.next`): the same queue
    /// the official clients show. The Web API queue lags behind context changes and is empty while
    /// no device is active.
    fn queue_window(&self) -> Vec<String> {
        self.np.next.iter().take(QUEUE_LEN).cloned().collect()
    }

    fn show_queue_window(&mut self) {
        self.queue = self
            .queue_window()
            .iter()
            .filter_map(|u| self.queue_cache.get(u).cloned())
            .collect();
        self.queue_sel = self.queue_sel.min(self.queue.len().saturating_sub(1));
    }

    fn toggle_lyrics(&mut self) {
        self.show_lyrics = !self.show_lyrics;
        self.lyrics_scroll = None;
        self.request_lyrics();
    }

    /// Scrolls the lyrics by `lines` from what was last drawn.
    fn scroll_lyrics(&mut self, lines: isize) {
        let len = self.lyrics.as_ref().map_or(0, |(_, l)| l.len());
        let top = self
            .lyrics_top
            .saturating_add_signed(lines)
            .min(len.saturating_sub(1));
        self.lyrics_scroll = Some((top, Instant::now()));
    }

    /// Asks the daemon for the playing track's lyrics while they are shown and not loaded yet.
    fn request_lyrics(&mut self) {
        let loaded = self
            .lyrics
            .as_ref()
            .is_some_and(|(uri, _)| *uri == self.np.uri);
        if self.show_lyrics && !self.np.uri.is_empty() && !loaded {
            self.lyrics = None;
            self.lyrics_scroll = None;
            self.outbox.push(Cmd::Lyrics(self.np.uri.clone()));
        }
    }

    fn open_devices(&mut self) {
        self.devices_open = true;
        self.devices = None;
        self.outbox.push(Cmd::Devices);
    }

    fn pick_device(&mut self, i: usize) -> Vec<Cmd> {
        self.devices_open = false;
        let Some(d) = self.devices.as_ref().and_then(|d| d.get(i)) else {
            return vec![];
        };
        if d.active {
            return vec![];
        }
        self.status = format!("Playing on {}", d.name);
        vec![Cmd::Transfer(d.id.clone())]
    }

    fn move_queue(&mut self, down: bool) {
        self.queue_sel = if down {
            (self.queue_sel + 1).min(self.queue.len().saturating_sub(1))
        } else {
            self.queue_sel.saturating_sub(1)
        };
    }

    /// Tab order: library, main, queue (side panels only when shown).
    fn cycle_focus(&mut self, forward: bool) {
        let order: Vec<Focus> = [
            (self.show_library, Focus::Library),
            (true, Focus::Main),
            (self.show_queue, Focus::Queue),
        ]
        .into_iter()
        .filter_map(|(shown, f)| shown.then_some(f))
        .collect();
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(1);
        let n = order.len();
        self.focus = order[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    fn toggle_library(&mut self) {
        self.show_library = !self.show_library;
        if !self.show_library && self.focus == Focus::Library {
            self.focus = Focus::Main;
        }
    }

    fn toggle_queue(&mut self) {
        self.show_queue = !self.show_queue;
        if !self.show_queue && self.focus == Focus::Queue {
            self.focus = Focus::Main;
        }
    }

    fn load_queue(&mut self) {
        self.show_queue_window();
        let missing: Vec<String> = self
            .queue_window()
            .into_iter()
            .filter(|u| !self.queue_cache.contains_key(u))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.outbox.push(Cmd::Tracks(missing));
    }

    fn search(&mut self, term: String) {
        let api = self.api.clone();
        self.load(true, async move {
            let secs = api.search(&term).await?.into_iter().map(|(title, items)| {
                Sec::new(
                    title,
                    if title == "Songs" {
                        Kind::List
                    } else {
                        Kind::Shelf
                    },
                    items,
                )
            });
            Ok(Page::new(format!("Search: {term}"), None, secs.collect()))
        });
    }

    fn open(&mut self, item: Item) {
        self.open_at(item, None);
    }

    /// Shows the page at once with the header we already have, then streams tracks in chunks,
    /// selecting `focus` when it arrives.
    fn open_at(&mut self, item: Item, focus: Option<String>) {
        let generation = self.navigate();
        let mut page = Page::new(item.name.clone(), Some(item.clone()), vec![]);
        page.loading = true;
        page.focus = focus;
        self.stack.push(page);
        self.focus = Focus::Main;
        self.status = "loading…".into();
        let (api, tx) = (self.api.clone(), self.tx.clone());
        let task = tokio::spawn(async move {
            // Small first chunk so the tracklist shows up fast; bigger ones after.
            let (mut offset, mut limit) = (0, 30);
            loop {
                let (items, more, root) = match api.open(&item.uri, offset, limit).await {
                    Ok(page) => page,
                    Err(e) => {
                        let _ = tx.send(Loaded::Fill {
                            generation,
                            items: vec![],
                            more: false,
                            root: None,
                        });
                        let _ = tx.send(Loaded::Status(format!("{e:#}")));
                        return;
                    }
                };
                if tx
                    .send(Loaded::Fill {
                        generation,
                        items,
                        more,
                        root,
                    })
                    .is_err()
                    || !more
                {
                    return;
                }
                offset += limit;
                limit = 100;
            }
        });
        self.page_task = Some(task.abort_handle());
    }

    /// Enter on an item: browsable entities open a page, everything else plays.
    fn activate(&mut self, item: Item, context: Option<String>) -> Vec<Cmd> {
        match item.kind() {
            _ if item.browsable() => {
                self.open(item);
                vec![]
            }
            "track" | "episode" => vec![match context {
                Some(context) => Cmd::Load {
                    context_uri: Some(context),
                    tracks: vec![],
                    track: Some(item.uri),
                },
                None => Cmd::Load {
                    context_uri: None,
                    tracks: vec![item.uri],
                    track: None,
                },
            }],
            _ => vec![play_context(item.uri)],
        }
    }

    /// Esc: cancels any fetch in flight, then leaves the page.
    fn back(&mut self) {
        let loading = self.page_task.as_ref().is_some_and(|t| !t.is_finished());
        self.navigate();
        if let Some(page) = self.stack.last_mut() {
            page.loading = false;
        }
        if loading {
            self.status = "cancelled".into();
        }
        if self.stack.len() > 1 {
            self.stack.pop();
        } else if self.page().is_some_and(|p| p.header.is_some()) {
            // Resumed straight into a page: there is nothing below it, so back means home.
            self.home();
        }
    }

    pub fn page(&self) -> Option<&Page> {
        self.stack.last()
    }

    pub fn position(&self) -> u32 {
        // From `position_at_ms`, not arrival time: queue-only updates resend an old `position_ms`.
        self.np.position(proto::now_ms()) as u32
    }

    fn selected(&self) -> Option<Item> {
        match self.focus {
            Focus::Library => self.library.get(self.lib_sel).cloned(),
            Focus::Main => self.page().and_then(Page::selected).cloned(),
            Focus::Queue => self.queue.get(self.queue_sel).cloned(),
        }
    }

    fn context(&self) -> Option<String> {
        match self.focus {
            Focus::Library => None,
            Focus::Main => self.page().and_then(|p| p.context.clone()),
            // Jumping ahead in the queue keeps playing the current context where possible.
            Focus::Queue => Some(self.np.context.clone()).filter(|c| !c.is_empty()),
        }
    }

    fn move_library(&mut self, down: bool) {
        self.lib_sel = if down {
            (self.lib_sel + 1).min(self.library.len().saturating_sub(1))
        } else {
            self.lib_sel.saturating_sub(1)
        };
    }

    /// Handles a key press; returns the commands to send to the daemon.
    fn key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.help {
            self.help = false;
            return vec![];
        }
        if self.devices_open {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.dev_sel = (self.dev_sel + 1).min(
                        self.devices
                            .as_ref()
                            .map_or(0, |d| d.len().saturating_sub(1)),
                    )
                }
                KeyCode::Char('k') | KeyCode::Up => self.dev_sel = self.dev_sel.saturating_sub(1),
                KeyCode::Enter => return self.pick_device(self.dev_sel),
                KeyCode::Esc | KeyCode::Char('c' | 'q') => self.devices_open = false,
                _ => {}
            }
            return vec![];
        }
        if let Some(term) = &mut self.search {
            match key.code {
                KeyCode::Char(c) => term.push(c),
                KeyCode::Backspace => _ = term.pop(),
                KeyCode::Esc => self.search = None,
                KeyCode::Enter if !term.is_empty() => {
                    let term = std::mem::take(term);
                    self.search = None;
                    self.search(term);
                }
                _ => {}
            }
            return vec![];
        }
        let k = keys::key(key);
        if self.pending.is_empty() || k != (KeyCode::Esc, KeyModifiers::NONE) {
            self.pending.push(k);
        } else {
            // Like vim: Esc cancels a pending sequence.
            self.pending.clear();
        }
        let mut cmds = vec![];
        self.key_deadline = None;
        while !self.pending.is_empty() {
            match self.keymap.lookup(&self.pending) {
                keys::Lookup::Prefix(_) => {
                    self.key_deadline =
                        self.keymap.timeout.map(|t| tokio::time::Instant::now() + t);
                    break;
                }
                keys::Lookup::Exact(actions) => {
                    let actions = actions.to_vec();
                    self.pending.clear();
                    self.run_actions(&actions, &mut cmds);
                }
                // Not a sequence: what came before runs on its own if bound, then the last key alone.
                keys::Lookup::None => {
                    let last = self.pending.pop().expect("not empty");
                    if !self.pending.is_empty() {
                        if let keys::Lookup::Exact(a) | keys::Lookup::Prefix(Some(a)) =
                            self.keymap.lookup(&self.pending)
                        {
                            let a = a.to_vec();
                            self.run_actions(&a, &mut cmds);
                        }
                        self.pending = vec![last];
                    }
                }
            }
        }
        cmds
    }

    /// The pending sequence timed out: it runs what it is bound to, if anything.
    fn key_timeout(&mut self) -> Vec<Cmd> {
        self.key_deadline = None;
        let mut cmds = vec![];
        if let keys::Lookup::Prefix(Some(a)) | keys::Lookup::Exact(a) =
            self.keymap.lookup(&std::mem::take(&mut self.pending))
        {
            let a = a.to_vec();
            self.run_actions(&a, &mut cmds);
        }
        cmds
    }

    fn run_actions(&mut self, actions: &[Action], cmds: &mut Vec<Cmd>) {
        for a in actions {
            cmds.extend(self.action(a));
        }
    }

    fn like_target(&self, t: LikeTarget) -> Option<String> {
        match t {
            LikeTarget::Playing => Some(self.np.uri.clone()).filter(|u| !u.is_empty()),
            LikeTarget::Selected => self.selected().map(|i| i.uri),
        }
    }

    /// Copies the link with OSC 52 (works over SSH, if the terminal allows it) and the native
    /// clipboard (for terminals that ignore OSC 52).
    fn share(&mut self, t: ShareTarget) {
        let uri = match t {
            ShareTarget::Playing => Some(self.np.uri.clone()),
            ShareTarget::Selected => self.selected().map(|i| i.uri),
            ShareTarget::Page => self
                .page()
                .and_then(|p| p.header.as_ref())
                .map(|h| h.uri.clone()),
        };
        let Some(link) = uri.as_deref().and_then(cli::open_link) else {
            self.status = "Nothing to share".into();
            return;
        };
        let osc = execute!(
            std::io::stdout(),
            crossterm::clipboard::CopyToClipboard::to_clipboard_from(&link)
        );
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let native = self.clipboard.as_mut().map(|c| c.set_text(&link));
        self.status = match (osc, native) {
            (Err(_), None | Some(Err(_))) => "Clipboard unavailable".into(),
            _ => format!("Copied {link}"),
        };
    }

    fn action(&mut self, a: &Action) -> Option<Cmd> {
        let main = self.focus == Focus::Main;
        let queue = self.focus == Focus::Queue;
        match a {
            Action::Nop => {}
            Action::Quit => self.quit = true,
            Action::Kill => {
                self.quit = true;
                return Some(Cmd::Shutdown);
            }
            Action::Focus(step) => self.cycle_focus(*step == Step::Next),
            Action::Search => self.search = Some(String::new()),
            Action::Home => self.home(),
            Action::Help => self.help = true,
            Action::Devices => self.open_devices(),
            Action::Toggle(Toggle::Queue) => self.toggle_queue(),
            Action::Toggle(Toggle::Library) => self.toggle_library(),
            Action::Toggle(Toggle::Lyrics) => self.toggle_lyrics(),
            Action::Select(d @ (Dir::Down | Dir::Up)) if main && self.show_lyrics => {
                self.scroll_lyrics(if *d == Dir::Down { 1 } else { -1 })
            }
            Action::Select(d @ (Dir::First | Dir::Last)) if main && self.show_lyrics => {
                self.scroll_lyrics(if *d == Dir::Last {
                    isize::MAX
                } else {
                    isize::MIN
                });
            }
            Action::Back if self.show_lyrics => self.show_lyrics = false,
            Action::Back => self.back(),
            Action::Select(d) if main => {
                if let Some(p) = self.stack.last_mut() {
                    match d {
                        Dir::Down | Dir::Up => p.vertical(*d == Dir::Down),
                        Dir::Left | Dir::Right => p.horizontal(*d == Dir::Right),
                        Dir::First | Dir::Last => p.edge(*d == Dir::Last),
                    }
                }
            }
            Action::Select(d @ (Dir::Down | Dir::Up)) if queue => self.move_queue(*d == Dir::Down),
            Action::Select(Dir::First) if queue => self.queue_sel = 0,
            Action::Select(Dir::Last) if queue => {
                self.queue_sel = self.queue.len().saturating_sub(1)
            }
            Action::Select(Dir::First) => self.lib_sel = 0,
            Action::Select(Dir::Last) => self.lib_sel = self.library.len().saturating_sub(1),
            Action::Select(Dir::Left) if queue => self.focus = Focus::Main,
            Action::Select(d @ (Dir::Down | Dir::Up)) => self.move_library(*d == Dir::Down),
            Action::Select(Dir::Right) if !queue => self.focus = Focus::Main,
            Action::Select(Dir::Left | Dir::Right) => {}
            Action::Move(v) if queue => {
                let down = *v == Vertical::Down;
                let (i, j) = (
                    self.queue_sel,
                    if down {
                        self.queue_sel + 1
                    } else {
                        self.queue_sel.wrapping_sub(1)
                    },
                );
                if let (Some(item), true) = (self.queue.get(i).cloned(), j < self.queue.len()) {
                    let nth = self.queue[..i].iter().filter(|q| q.uri == item.uri).count();
                    // Shown moved at once; the cluster update confirms it (an error restores it).
                    self.queue.swap(i, j);
                    self.queue_sel = j;
                    return Some(Cmd::MoveQueue {
                        uri: item.uri,
                        nth,
                        down,
                    });
                }
            }
            Action::Unqueue if queue => {
                if let Some(item) = self.queue.get(self.queue_sel).cloned() {
                    // Occurrence among equal uris before it: the same track can be queued twice.
                    let nth = self.queue[..self.queue_sel]
                        .iter()
                        .filter(|i| i.uri == item.uri)
                        .count();
                    // Shown gone at once; the cluster update confirms it (or brings it back on failure).
                    self.queue.remove(self.queue_sel);
                    self.queue_sel = self.queue_sel.min(self.queue.len().saturating_sub(1));
                    self.status = format!("Removed from queue: {}", item.name);
                    return Some(Cmd::Unqueue { uri: item.uri, nth });
                }
            }
            Action::Move(_) | Action::Unqueue => {}
            Action::Open => {
                if let Some(item) = self.selected() {
                    let context = self.context();
                    let cmds = self.activate(item, context);
                    self.outbox.extend(cmds);
                }
            }
            Action::Play(PlayTarget::Selected) => {
                return self.selected().map(|item| play_context(item.uri));
            }
            Action::Play(PlayTarget::Page) => {
                return self
                    .page()
                    .and_then(|p| p.context.clone())
                    .map(play_context);
            }
            Action::Queue(QueueTarget::Selected) => {
                if let Some(item) = self
                    .selected()
                    .filter(|i| matches!(i.kind(), "track" | "episode"))
                {
                    // Failures come back as `Msg::Error` and replace this.
                    self.status = format!("Added to queue: {}", item.name);
                    return Some(Cmd::Queue(item.uri));
                }
            }
            Action::Play(PlayTarget::Uri(uri)) => return Some(cli::play(uri)),
            Action::Queue(QueueTarget::Uri(uri)) => return Some(Cmd::Queue(cli::spotify_uri(uri))),
            Action::Share(t) => self.share(*t),
            Action::Next => return Some(Cmd::Next),
            Action::Prev => return Some(Cmd::Prev),
            Action::Toggle(Toggle::Play) => return Some(Cmd::PlayPause),
            Action::Set(Set::Play(on)) => {
                return (bool::from(*on) != self.np.playing).then_some(Cmd::PlayPause);
            }
            Action::Toggle(Toggle::Shuffle) => {
                return self.action(&Action::Set(Set::Shuffle(if self.shuffle {
                    OnOff::Off
                } else {
                    OnOff::On
                })));
            }
            Action::Set(Set::Shuffle(on)) => {
                self.shuffle = (*on).into();
                return Some(Cmd::Shuffle(self.shuffle));
            }
            Action::Toggle(Toggle::Repeat) => {
                self.repeat = if self.repeat == proto::Repeat::Off {
                    proto::Repeat::Context
                } else {
                    proto::Repeat::Off
                };
                return Some(Cmd::Repeat(self.repeat));
            }
            Action::Cycle(cli::Cycle::Repeat) => {
                self.repeat = self.repeat.cycle();
                return Some(Cmd::Repeat(self.repeat));
            }
            Action::Set(Set::Repeat(r)) => {
                self.repeat = (*r).into();
                return Some(Cmd::Repeat(self.repeat));
            }
            Action::Toggle(Toggle::Like(t)) => {
                if let Some(uri) = self.like_target(*t) {
                    self.toggle_like(uri);
                }
            }
            Action::Set(Set::Like(t, on)) => {
                if let Some(uri) = self
                    .like_target(*t)
                    .filter(|uri| self.liked(uri) == Some(!bool::from(*on)))
                {
                    self.toggle_like(uri);
                }
            }
            Action::Toggle(Toggle::Mute) => return Some(Cmd::Mute(!self.np.muted)),
            Action::Set(Set::Mute(on)) => return Some(Cmd::Mute((*on).into())),
            Action::Set(Set::Volume(v)) => {
                return Some(Cmd::Volume(cli::volume(*v, self.np.volume)));
            }
            Action::Set(Set::Position(p)) => {
                return Some(Cmd::Seek(cli::seek(*p, &self.np, proto::now_ms())));
            }
        }
        None
    }

    fn mouse(&mut self, m: MouseEvent) -> Vec<Cmd> {
        let at = |r: &Rect| r.contains((m.column, m.row).into());
        let over_library = at(&self.lib_area);
        if self.help && matches!(m.kind, MouseEventKind::Down(_)) {
            self.help = false;
            return vec![];
        }
        if self.devices_open && matches!(m.kind, MouseEventKind::Down(_)) {
            // A row transfers; anything else just closes the picker.
            if let Some((_, Hit::Device(i))) = self.hits.iter().rev().find(|(r, _)| at(r)).cloned()
            {
                return self.pick_device(i);
            }
            self.devices_open = false;
            return vec![];
        }
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if over_library {
                    self.move_library(down);
                    self.lib_top = self.lib_sel.min(self.lib_top);
                } else if at(&self.queue_area) {
                    self.move_queue(down);
                } else if self.show_lyrics {
                    self.scroll_lyrics(if down { 3 } else { -3 });
                } else if let Some(page) = self.stack.last_mut() {
                    page.vertical(down);
                }
                vec![]
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((rect, hit)) = self.hits.iter().rev().find(|(r, _)| at(r)).cloned() else {
                    return vec![];
                };
                let ratio = (m.column - rect.x) as f64 / rect.width.max(1) as f64;
                match hit {
                    Hit::Main(s, i) => {
                        self.focus = Focus::Main;
                        let Some(page) = self.stack.last_mut() else {
                            return vec![];
                        };
                        let again = page.sel == (s, i);
                        page.sel = (s, i);
                        let item = page.secs[s].items[i].clone();
                        // Cards open on first click; track rows select first and play on the second, like Spotify.
                        if again || page.secs[s].kind != Kind::List {
                            let context = page.context.clone();
                            return self.activate(item, context);
                        }
                    }
                    Hit::Devices => self.open_devices(),
                    Hit::Lyrics => self.toggle_lyrics(),
                    Hit::Device(i) => return self.pick_device(i),
                    Hit::Queue(i) => {
                        let again = self.focus == Focus::Queue && self.queue_sel == i;
                        self.focus = Focus::Queue;
                        self.queue_sel = i;
                        // Like track rows: select first, play on the second click.
                        if again && let Some(item) = self.queue.get(i).cloned() {
                            let context = self.context();
                            return self.activate(item, context);
                        }
                    }
                    Hit::Library(i) => {
                        self.focus = Focus::Library;
                        self.lib_sel = i;
                        if let Some(item) = self.library.get(i).cloned() {
                            return self.activate(item, None);
                        }
                    }
                    Hit::Cmd(cmd) => {
                        match cmd {
                            Cmd::Shuffle(on) => self.shuffle = on,
                            Cmd::Repeat(r) => self.repeat = r,
                            _ => {}
                        }
                        return vec![cmd];
                    }
                    Hit::Seek => {
                        return vec![Cmd::Seek((ratio * self.np.duration_ms as f64) as u32)];
                    }
                    Hit::Volume => return vec![Cmd::Volume((ratio * u16::MAX as f64) as u16)],
                    Hit::Mute => return vec![Cmd::Mute(!self.np.muted)],
                    Hit::Search => self.search = Some(String::new()),
                    Hit::Home => self.home(),
                    Hit::Back => self.back(),
                    Hit::ToggleQueue => self.toggle_queue(),
                    Hit::Like(uri) => self.toggle_like(uri),
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

fn last_page_path() -> std::path::PathBuf {
    crate::auth::config_dir().join("last-page.json")
}

fn last_page() -> Option<Item> {
    serde_json::from_slice(&std::fs::read(last_page_path()).ok()?).ok()
}

/// Remembers the open playlist/album/artist for the next start; home clears it.
fn save_last_page(item: Option<&Item>) {
    let _ = match item {
        Some(item) => serde_json::to_vec(item)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(last_page_path(), b)),
        None => std::fs::remove_file(last_page_path()),
    };
}

/// Liked state shown for `uri`; `None` for what cannot be liked. A free function so the renderer
/// can call it while a page is borrowed.
fn liked(
    saved: &std::collections::HashMap<String, bool>,
    library: &[Item],
    uri: &str,
) -> Option<bool> {
    if !saveable(uri) {
        return None;
    }
    if uri.starts_with("spotify:playlist:") {
        return Some(library.iter().any(|i| i.uri == uri));
    }
    saved.get(uri).copied()
}

/// What the web player's heart works on.
fn saveable(uri: &str) -> bool {
    let mut parts = uri.split(':');
    parts.next() == Some("spotify")
        && matches!(
            parts.next(),
            Some("track" | "album" | "artist" | "playlist" | "show" | "episode")
        )
}

fn play_context(uri: String) -> Cmd {
    Cmd::Load {
        context_uri: Some(uri),
        tracks: vec![],
        track: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(uri: &str) -> Item {
        Item {
            name: uri.into(),
            ..Item::from_uri(uri)
        }
    }

    #[test]
    fn group_and_navigate() {
        let items = [
            "spotify:track:1",
            "spotify:album:a",
            "spotify:track:2",
            "spotify:artist:x",
            "spotify:album:b",
        ];
        let mut secs = vec![];
        add(&mut secs, items.map(item).to_vec(), "Songs");
        let mut page = Page::new("t", None, secs);
        let titles: Vec<_> = page.secs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Songs", "Albums", "Artists"]);
        page.vertical(true);
        assert_eq!(page.sel, (0, 1));
        page.vertical(true);
        page.horizontal(true);
        assert_eq!(page.selected().unwrap().uri, "spotify:album:b");
        page.vertical(false);
        assert_eq!(page.sel, (0, 1));
        let mut secs = vec![];
        add(&mut secs, vec![item("spotify:album:a")], "Songs");
        assert!(secs[0].kind == Kind::Shelf);

        let mut grid = Sec::new(
            "",
            Kind::Grid,
            (0..7)
                .map(|i| item(&format!("spotify:album:{i}")))
                .collect(),
        );
        grid.cols = 4;
        let mut page = Page::new(
            "t",
            None,
            vec![
                grid,
                Sec::new("S", Kind::Shelf, vec![item("spotify:album:z")]),
            ],
        );
        page.horizontal(true);
        page.vertical(true);
        assert_eq!(page.sel, (0, 5));
        page.vertical(true);
        assert_eq!(page.sel, (1, 0));
        page.vertical(false);
        assert_eq!(page.sel, (0, 4));
    }
}
