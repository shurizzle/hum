# TUI

Code: `src/tui/mod.rs` (state, input, fetching), `src/tui/draw.rs` (layout, rendering),
`src/tui/images.rs` (covers).

## Layout

Modeled on the Spotify desktop app.

- **Top**: `‹` back, `⌂ Home`, search box, status text (errors from the daemon included; a dim
  `? keys` hint when empty).
- **Left** (toggle `[`): Your Library with thumbnails.
- **Center**: a `Page` made of sections. A section is a `Grid` (compact tiles: the untitled first
  home section, Spotify's shortcuts, max 8), a `Shelf` (horizontal cover cards that stretch to
  fill the width, min 20 cells each; under the name the playlist description, else
  "Playlist • owner" / "Album • artists") or a `List` (numbered track rows). A section title is
  never drawn without the block below it. Opened playlists/albums/
  artists get a big header with cover and `▶ Play`. Search follows the web player's "All"
  tab: Top result (1 card), Songs (4 rows), then shelves for Artists, Albums, Playlists, Podcasts,
  Episodes. Artist pages are grouped by `add`: tracks first, then one shelf per kind.
- **Hearts** (like the web player): green `♥` at the right of liked track rows, `♥`/`♡` next to
  `▶ Play` in album/playlist/artist headers and after the playing track's name in the bar; all
  clickable. Liked state comes from `areEntitiesInLibrary` for every page's items and the playing
  track (`App::check_saved`, chunks of 50, unknown shown as not liked); a playlist is liked when it
  is in Your Library. Toggling shows at once and is restored if Spotify refuses; Your
  Library is reloaded after each change and on `Msg::Library` (changes from any device), which
  also refetches the hearts on screen. `Msg::Playlist` reloads the open playlist in place, keeping
  selection and focused panel.
- **Right** (toggle `]`): Now playing with big cover, then next in queue: the first 20 of
  `NowPlaying.next` (the Connect cluster's queue, the one the official clients show), resolved
  through the daemon (`Cmd::Tracks` → `Msg::Tracks`, spclient metadata, one request) and cached by
  uri. Not the Web API: its queue lags behind context changes and its track endpoints are rate
  limited for the developer app. In the queue, `d`/`Delete` removes the selected track
  (`Cmd::Unqueue`) and `J`/`K` move it (`Cmd::MoveQueue`), shown at once; an error restores it.
- **Bottom**: player bar with cover, `⇄ ⏮ ▶/⏸ ⏭ ↻`, progress, lyrics `♫`, device picker `⊡`, queue toggle,
  volume with a level speaker icon (`🔇 🔈 🔉 🔊`); click the icon or `m` to mute (`🔇`, gray bar
  at the kept level).
- **Lyrics** (`y` or `♫` in the bar): replace the main page; requested from the daemon (`Cmd::Lyrics`) for the
  playing track and again when it changes. Synced lyrics highlight the current line in green (past
  lines dim, empty lines as `♪`) and keep it a third from the top; unsynced ones are shown from the
  top. Wheel or `j`/`k` (main focused) scroll them; synced lyrics follow the song again 5 s after
  the last scroll (`LYRICS_FOLLOW`). `y`, `♫` or `Esc` goes back to the page.
- **Connect picker** (`c` or `⊡`): overlay above the bar's right end listing the account's devices
  (`Cmd::Devices`, fetched on open), the active one green with `♪`. `j`/`k` + `Enter` or a click
  moves playback there (`Cmd::Transfer`); `Esc`, `c` or a click elsewhere closes it.

## Startup

Before the terminal UI starts, the client asks the daemon for tokens (`Cmd::Token`). An error
(usually the network) is printed and the request repeated every 3 s; Ctrl-C quits.

Resumes like the desktop app:

1. A playlist/album/artist in `NowPlaying.context` (an artist's `spotify:list:...:artist_<id>`
   Popular context opens the artist, `Item::from_context`) (playing, or the account's last playback the
   daemon restored): open that page and select the track once it streams in. The daemon restores
   a few seconds after it starts, so this also happens late, unless a key or click came first.
2. Otherwise the page open when the last client quit (`last-page.json`; quitting on Home clears it).
3. Otherwise Home.

A resumed page has nothing below it in the stack, so `Esc` goes Home. Pages opened by uri only
get their title and cover from the first response (`Api::open` returns the root entity).

## Focus and selection

- The focused panel (Library, main or queue) has a bright border; the others have a dim one. `Tab`
  / `Shift+Tab` cycle library → main → queue (side panels only while shown; hiding the focused one moves focus to main).
- In the queue, `j`/`k` move, `h` goes back to main, `Enter` (or a second click) plays the track in
  the current context (`Cmd::Load` with `NowPlaying.context`), `a` queues it again, `d` removes it, `J`/`K` move it. The queue keeps
  its selection highlighted only while focused.
- Selected row in the focused panel: light background plus a green `▌` bar on the left. Selected
  card: light background plus a green underline. The unfocused panel keeps its selection with a
  dim background only.
- Green text means "now playing", not "selected".

## Keys

| Key | Action |
|---|---|
| `h j k l` / arrows | Move (shelves scroll horizontally) |
| `Tab` / `Shift+Tab` | Cycle focus library → main → queue |
| `Enter` | Open playlist/album/artist, play track |
| `Esc` / `Backspace` | Cancel loading, go back |
| `/` | Search |
| `g` | Home |
| `Space` | Play/pause |
| `n` / `b` | Next / previous |
| `H` / `L`, `Shift+←/→` | Seek ∓10 s |
| `+` / `-` | Volume (unmutes) |
| `m` | Mute / unmute |
| `s` / `r` | Shuffle / repeat; repeat cycles off, context, track (`↻¹`). Toggled locally, then follow `NowPlaying.shuffle`/`repeat` when another client or device changes them |
| `a` | Add the selected track/episode to the queue (`Cmd::Queue`; works with no active device) |
| `f` | Like/unlike the selected track, album, artist, playlist (`Api::set_saved`) |
| `F` | Like/unlike the playing track |
| `d` / `Delete` | Remove the selected track from the queue (queue focused) |
| `J` / `K` | Move the selected queue track down / up (queue focused) |
| `P` | Play the open playlist/album/artist from the start (same as `▶ Play`, hint shown next to it) |
| `p` | Play the selected playlist/album/artist without opening it |
| `[` / `]` | Toggle library / now-playing panel |
| `c` | Connect device picker |
| `y` | Toggle lyrics |
| `?` | Key help overlay (`draw::KEYS`); any key or click closes it |
| `q` | Quit client (music keeps playing) |
| `Q` | Quit and stop the daemon |

## Mouse

Click targets are collected during drawing in `App::hits` (`Hit`). Cards open on first click; track
rows select on first click and play on the second. Click the progress/volume bars to set them.
Wheel scrolls. Pointer motion events are ignored and do not redraw.

## Covers

- Protocol chosen from the environment in `images::picker`: Kitty for Kitty/Ghostty, iTerm2 inline
  images for iTerm2 and WezTerm, halfblocks otherwise. Do **not** use `Picker::from_query_stdio`:
  an unanswered query leaves a thread reading stdin that eats input and resets raw mode.
- Pipeline, all off the UI loop: disk cache (`cover:<url>`, never expires) or download (`reqwest`,
  then written to the disk cache) → decode + thumbnail 320 px
  (`spawn_blocking`) → per-(url, width, height) `ThreadProtocol` encoder task → `Done::Encoded`.
  One encoder per size: sharing one between sizes re-encodes every frame and makes covers blink.
- A placeholder (`♪`) is drawn first, the image over it when ready.
- Downloads of covers not drawn in a frame are aborted (`begin_frame`/`end_frame`).
- In-memory eviction by last drawn frame (`end_frame` → `evict`): over 128 decoded covers
  (`MAX_COVERS`) or 64 encoders (`MAX_ENCODERS`) the least recently drawn are dropped down to 3/4 of
  the cap; anything drawn this frame or still loading stays. A dropped encoder ends its task.
  Evicted covers reload from the disk cache, no network. Disk pruning: see
  [spotify-apis.md](spotify-apis.md#disk-cache).

## Rendering rules

- Never block the loop: network, decoding and encoding go through tasks and channels.
- Redraw on every event except pointer motion.
- Dependencies are built with `opt-level = 3` even in dev (`Cargo.toml`), or image work is too
  slow.
