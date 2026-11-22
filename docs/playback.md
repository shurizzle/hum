# Playback

Audio comes from one of two **engines**. By default hum drives **Spotify Soloist** (the official
headless Connect player) — no Widevine, lossless. With the `ecs` Cargo feature (on by default;
`--no-default-features` drops it), the official **Web Playback SDK** can run instead inside **castLabs Electron for Content
Security (ECS)**, which ships Google's Widevine CDM. Without the feature no ECS code is compiled in
(`setup::Engine::Ecs`, `player::run_once`, the embedded `player/` files, the installer).
librespot's own player is not used: Spotify refuses its audio keys on some accounts (see
[known-issues.md](known-issues.md)).

## Engine selection (`src/setup.rs`)

`setup::engine()` (daemon) / `setup::ensure_engine()` (foreground, before the daemon): whatever is
already set up wins, else an installed Soloist is paired, else ECS is downloaded.

1. **Soloist** if `<config>/soloist-key` exists (written only after a successful pairing) **and** the
   `soloist` binary is found (`$HUM_SOLOIST`, else on `PATH`).
2. else (`ecs` feature only) **ECS** if `$HUM_ECS` is set or `<data>/ecs` is installed.
3. else, with nothing set up: pair an installed Soloist if present, otherwise download ECS (`ecs`
   feature) or fail with an error pointing here.

`Engine` is passed to `Player::spawn`; `player::supervise` runs `run_once` (ECS) or `soloist::run`
over the same command/state channels, so the daemon's dispatch (`src/daemon.rs`) is engine-agnostic.

## Soloist engine (`src/soloist.rs`)

Soloist is a closed-source binary the user installs themselves (Spotify forbids redistribution; builds
expire 90 days after their build date, exiting `10`). A per-user developer API key from
<https://developer.spotify.com/dashboard/soloist> is required. Packages install it as a dependency:
the Homebrew cask (tap `shurizzle/homebrew-hum`) pulls the `spotify-soloist` cask on Apple Silicon (no Intel
Soloist build: x86_64 macOS hum uses ECS); the AUR package depends on `spotify-soloist`.

### Pairing (`soloist::setup`, foreground, one-time)

No browser, no device picker. hum mints Soloist's login itself on the librespot session:

1. Prompt for the API key, get the session access token
   (`session.login5().auth_token()`) and the canonical user id (`session.username()`). The
   dashboard is opened in the login tab: when `auth::ensure_login` runs a browser login and pairing
   comes next, its last redirect goes to the dashboard (`soloist::DASHBOARD_SHOWN` is set) instead
   of the "logged in" page; otherwise `setup` opens it in a new tab.
2. Spawn `soloist -n hum -k <key> -D <config>/soloist --pair -v`; read the zeroconf HTTP port from its
   log line `Started ZeroConf service on port <N>`.
3. `GET http://127.0.0.1:<N>/zc?action=getInfo` → `authInfo` (base64 protobuf):
   `auth_info_id`, Soloist's `client_id`, and a `DpopBoundAuthCodeInfo { jkt, pkce_code_challenge }`.
4. **Mint the auth code** — `POST https://accounts.spotify.com/api/token`, RFC 8693 token-exchange:
   `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`, `client_id=<SESSION_CLIENT_ID>`,
   `subject_token=<session token>`, `requested_token_type=urn:spotify:params:oauth:authorization_code`,
   `audience=<Soloist client_id>`, `resource=urn:spotify:resources:connect`, `scope=streaming`,
   `code_challenge=<pkce_code_challenge>`, `code_challenge_method=S256`, `dpop_jkt=<jkt>`, plus a
   **`DPoP` header** (ES256 JWT, `ring` P-256 key). The first POST returns `400 use_dpop_nonce` with a
   `DPoP-Nonce`; retry once with the nonce. The response `access_token` is the DPoP-bound code (bound
   to Soloist's jkt/pkce, **no redirect_uri**).
5. **Deliver** — `POST http://127.0.0.1:<N>/zc` `action=addUser`, `tokenType=auth_token`,
   `userName=<user id>`, `blob` = base64 of the **plaintext** protobuf
   `AuthToken { 1: auth_info_id, 10: DpopBoundAuthCode { 1: code } }` (no DH encryption for this
   token type). Soloist replies `{"status":101}` and logs in; the session is stored in
   `<config>/soloist`. On the child exiting 0, the key is written to `<config>/soloist-key` (mode 0600).

### Runtime (`soloist::run`, mirrors `player::run_once`)

First kills a Soloist left alive by a daemon that died without reaping it (pid in
`<config>/soloist/soloist.pid`, checked to be `soloist`): a second instance on the same directory
exits before serving. Spawns `soloist -n hum -k <key> -D <config>/soloist --ws 127.0.0.1:0 -i <saved volume>`, reads the
chosen port from `<config>/soloist/ws.port`, and connects to `ws://127.0.0.1:<port>/`.

- **Events → state**: `playback_state` and `volume_changed` frames are folded into `NowPlaying` via
  `player::apply` (position extrapolated from `timestamp_ms`·`speed`; volume 0..100 → `u16`). Volume
  is persisted to the disk cache (`volume` key) once the restored value is seen; `-i` takes whole
  percents, so the expected value is the saved one rounded to a percent (otherwise saving never
  resumes and every restart falls back to a stale value). The track Soloist restores at startup comes with partial decorations (no artist names, `duration_ms: 0`) until it
  plays: empty fields keep the state's values for the same uri, and the rest is fetched once via
  `meta::tracks` (`soloist::fill_missing`). Its queue, shuffle and repeat are not reported, and its
  position is reported as 0 although play resumes where the account left off: while hum is idle
  on that same track they come from the connect-state cluster (`Mirror::apply`; the position only
  while paused).
- **Commands**: the daemon's `player.command("toggle"|"next"|"prev"|"seek"|"volume", arg)` is
  translated to WebSocket commands (`play`/`pause`, `skip_next`, `skip_prev`, `seek`, `set_volume`).
  Soloist acks an applied command at once with `{"type":"command_result","command":...}` (a rejected
  one gets `{"type":"error","message":...}`), but the matching `playback_state` comes 0.5-1.5 s
  later (measured); the `play`/`pause` ack already sets `playing` and freezes the position.
  Soloist has no mute: `mute true` keeps the current level, sends `set_volume 0` and sets
  `muted`; while muted, reported volumes are ignored (neither shown nor saved) and a non-zero one
(Soloist re-applies its level when play from idle activates its Connect device) is answered with
`set_volume 0` again; `mute false`
  restores the kept level. A restart starts unmuted at the saved level.
  Everything else (`Load`, `Shuffle`, `Repeat`, `Queue`, `Transfer`) goes through the Web API against
  Soloist's Connect device, published to the device watch once `GET /me/player/devices` lists it. Its
  id is read from `<data dir>/.device_id` (written by Soloist; equal to the zeroconf `getInfo`
  `deviceID` and the Web API id); without that file the device is matched by name (`hum`). The `player::Mirror` still mirrors playback on other devices, as for ECS.
- Exit `10` (expired build) and any other exit surface an error; `supervise` retries after 3 s.

### OS media controls (`src/media/`)

Soloist has no media session, so on this engine the daemon publishes one itself
(`media::spawn`, only for `Engine::Soloist`; ECS keeps Chromium's). It mirrors the `NowPlaying`
watch and sends controls back as daemon `Cmd`s, run through the same `handle` as client commands
(so they reach remote devices too). Play and Pause become `PlayPause` only when the state differs.

- **Linux** (`media/mpris.rs`): MPRIS on the session bus as `org.mpris.MediaPlayer2.hum` via `zbus`
  (pure Rust, keeps musl builds static). `org.mpris.MediaPlayer2` (no raise/quit/track list) and
  `.Player`: PlaybackStatus (`Stopped` with no track), Metadata (trackid
  `/org/mpris/MediaPlayer2/Track/<base62 id>`, length, title, artist, album, artUrl), Volume,
  Shuffle, LoopStatus (`None`/`Playlist`/`Track` ↔ `Repeat`), Position (extrapolated, no change
  signal), Next/Previous/Play/Pause/PlayPause/Stop (= pause)/Seek/SetPosition; Rate fixed at 1, OpenUri
  ignored. PropertiesChanged is emitted per changed field; `Seeked` when a report's position is more
  than 1.5 s away from where the previous report extrapolates. No session bus: a `mpris:` warning.
- **macOS** (`media/now_playing.rs`): `MPNowPlayingInfoCenter` (title, artist, album, duration,
  elapsed time, rate, artwork downloaded from the cover URL) and `MPRemoteCommandCenter`
  (toggle/play/pause/next/previous/change position). MediaPlayer delivers commands on the main
  queue, so `hum daemon` on macOS runs the tokio runtime on a second thread while the main thread
  serves the main run loop (`media::main_loop`, kept alive by an `NSMachPort`); every MediaPlayer
  call is dispatched to the main queue.

## Components

- `player/main.js`, `player/player.html`: the engine. Embedded in the binary (`include_str!`),
  written to `~/Library/Caches/hum/player/` and run as `<ECS> main.js` by the daemon.
- `src/player.rs`: daemon side. Supervises the child (restart after 3 s on exit), relays messages,
  folds state into `NowPlaying`, calls the Web API for the player's device.
- `src/media/`: MPRIS / macOS Now Playing for the Soloist engine (see above).

## Engine protocol

JSON lines. Daemon → player on stdin, player → daemon on stdout prefixed with `HUM `
(other stdout is Electron noise and is ignored). The page reports to `main.js` through
`console.log("HUM ...")`.

| Direction | Message | Meaning |
|---|---|---|
| player → daemon | `{"ready": "<device_id>"}` | Registered as Connect device `hum`; the daemon answers with a `volume` command restoring the volume saved in the disk cache (`volume` key) |
| player → daemon | `{"want_token": true}` | SDK asks for an access token |
| player → daemon | `{"state": {...}}` | Track, context uri, position, paused, volume (or volume only); also sent right after `ready`, so the volume is known before anything plays |
| player → daemon | `{"error": "..."}` | SDK error; logged and forwarded to clients |
| daemon → player | `{"token": "..."}` | Web token (hum app) |
| daemon → player | `{"cmd": "toggle"\|"next"\|"prev"\|"seek"\|"volume", "arg": ...}` | SDK controls; `volume` is 0..1 |
| daemon → player | `{"cmd": "mute", "arg": bool}` | Handled in `main.js`, not the SDK: `webContents.setAudioMuted`, answered with `{"state": {"muted": bool}}`; the SDK volume is untouched. A new Electron starts unmuted (the supervisor clears `muted` on restart) |

## Connect state mirror

`player::Mirror` follows the account's connect-state cluster for the life of the daemon, on its own
librespot session: the dealer starts once per session, so any failure (network down at startup,
dealer gone) logs a warning and retries after 5 s with a fresh session. Start the librespot dealer, `PUT /connect-state/v1/devices/<id>` as a hidden observer device on every new
connection id (first connect, dealer reconnects), then read `hm://connect-state/v1/cluster` pushes. The same dealer turns library pushes, from any
device, into client messages:

| Push | When | Message |
|---|---|---|
| `hm://collection/{collection,artist,show}/<user>` | like/unlike of tracks and albums, artists, shows | `Msg::Library` |
| `hm://playlist/v2/user/<user>/rootlist` | playlist saved/removed | `Msg::Library` |
| `hm://playlist/v2/playlist/<id>` | playlist contents changed (also Spotify mixes after a like) | `Msg::Playlist(uri)` |

Ignored: `hm://playlist/v2/list/liked-songs-artist/<artist>` and the legacy
`hm://playlist/user/<user>/rootlist` (duplicates of the above). Each collection push also comes as
`…/json`, which librespot 0.8 cannot parse (JSON, base64 expected) and logs as a warning; the
daemon keeps that warning out of the status bar.
From each cluster's `player_state` (track, context, `next_tracks`, `position_as_of_timestamp`,
extrapolated while playing):

- **Startup restore**: with no active device, the first cluster shows where the account left off,
  paused, like the official clients. The Web API has no position once no device is active.
- **Remote playback**: when another device (web, phone) is active, `NowPlaying` mirrors it and
  `Player::remote()` is set. `PlayPause`/`Next`/`Prev`/`Seek` then go to the Web API without a
  `device_id`, controlling that device instead of taking playback over. The SDK's paused leftover
  state is ignored meanwhile; an SDK report of playing means hum took over and clears `remote`.
- When the SDK's own device is active, only `NowPlaying.next` comes from the cluster (autoplay fills
  the queue after the track starts); the SDK reports everything else directly.

Track metadata comes from spclient extended-metadata (`meta::tracks`) when the track changes; non-track items (episodes)
are not mirrored. `PlayPause` while the SDK has nothing loaded (after startup, or after another
device took over) plays the mirrored track in its context from its position (`resume_body`).

`main.js` mirrors every state report into `navigator.mediaSession` (metadata, cover, playback
state, position) and handles its play/pause/next/previous/seek actions, so Chromium publishes the
track to macOS Now Playing (Control Center, media keys) and MPRIS on Linux. Chromium routes the
session to the frame that owns the media player: the SDK's cross-origin iframe
(`https://sdk.scdn.co/embedded/`), which only the main process can reach
(`WebFrameMain.executeJavaScript`). Actions come back as `HUM_MEDIA {cmd, arg}` console messages
and run as page commands, not through the daemon.

Daemon commands map as follows: `Load` → `PUT /me/player/play?device_id=` (context + offset uri, or
uris; a single loose track plays as `{context_uri: <its Song Radio>, offset: {uri}}` via
`meta::radio`, so related music follows like in the official clients, falling back to the track
alone on any error), `Shuffle`/`Repeat` → `PUT /me/player/shuffle|repeat`,
`Devices` → `GET /me/player/devices`, `Transfer` → `PUT /me/player` (keeps play/pause),
`Unqueue`/`MoveQueue` → Connect `set_queue` command to the active device with the cluster's next
tracks edited (`Player::set_queue`; activates hum first if nothing is active, which makes Spotify
rebuild the upcoming tracks from the context, dropping earlier edits to context tracks). Moves
swap two shown tracks and keep each slot's provider, so queued tracks stay ahead of context ones, `Tracks` → spclient
extended-metadata, `Queue` → `POST /me/player/queue` (after `PUT /me/player {device_ids, play: false}` if no device is
active: the Web API answers `NO_ACTIVE_DEVICE` otherwise), everything else → SDK command. The Web API takes a track offset only in playlist, album, show and
`spotify:list:` contexts. Artists refuse offsets, so they play as their "Popular" list
`spotify:list:popular-release-segments-main-roles:artist_<id>`, like the web player. In any other
context (`spotify:webapi` left by `uris` playback, ...) the track
plays as an explicit `uris` list (`player::play_body`): alone when started from the TUI, followed by
the cluster's `next_tracks` (max 100 items) when resuming the restored playback. The
player exits when its stdin closes; the daemon kills it on drop.

## Setup

ECS location: `$HUM_ECS`, default `~/Library/Application Support/hum/ecs/` (Linux: `$XDG_DATA_HOME/hum/ecs/`) (`Electron.app` on
macOS, `electron` on Linux). hum pins the ECS version it is tested with: `player::ECS_VERSION`
(the start error links to its release).

### Automatic install

`setup::ensure_ecs` runs in the client right before it spawns the daemon (`spawn::connect`, with
`hum.lock` held, still in the plain terminal), so a running daemon never triggers it:

- skipped with `$HUM_ECS`, and for an install without `ecs/VERSION` (manual or dev install);
- otherwise, if `ecs/VERSION` differs from `ECS_VERSION` (missing install, or a bumped pin), it
  downloads with progress:
  - macOS: `hum-ecs-macos-<aarch64|x86_64>.zip` from this repository's `ecs-<ECS_VERSION>` release
    (renamed, VMP-signed);
  - Linux: castLabs' `electron-<ECS_VERSION>-linux-<x64|arm64>.zip` (no signing needed);
- extracts into `ecs.new` (`ditto` on macOS to keep framework symlinks, `unzip` on Linux), writes
  `VERSION`, replaces `ecs/`. Downloads made by hum carry no quarantine attribute.

Without the VMP signature Spotify's licence server answers 500 on macOS.

### Publishing signed Electron (maintainer)

`.github/workflows/ecs.yml`, run by hand (Actions → Signed Electron → Run workflow):

1. `check`: newest non-prerelease `+wvcus` release of castlabs/electron-releases. If
   `ecs-<that tag>` already exists here, the run stops (unless `force`).
2. `sign` (macOS runner, arm64 and x64): download, extract with `ditto`, rename the bundle to `hum`
   (`CFBundleName`/`CFBundleDisplayName`, `dev.hum.player`), VMP-sign with
   `castlabs_evs.vmp -n sign-pkg --streaming`, `verify-pkg`, zip.
3. `publish`: release `ecs-<tag>` with both zips, `--latest=false` so "Latest" stays hum's own
   `vX.Y.Z`. With `force`, the assets of an existing tag are replaced (re-sign before the
   signature expires; `verify-pkg` prints the days left, ~3.5 years from signing).

Secrets: `EVS_ACCOUNT`, `EVS_PASSWD` (castLabs EVS account, created once with
`python -m castlabs_evs.account signup`). Electron is released only when castLabs ships a new
version, independently of hum releases; after trying it, bump `ECS_VERSION`.

### Manual (development, Linux)

```sh
gh release download 'v45.2.1+wvcus' -R castlabs/electron-releases -p 'electron-v45.2.1+wvcus-darwin-arm64.zip'
ECS="$HOME/Library/Application Support/hum/ecs"; mkdir -p "$ECS"
ditto -x -k electron-v45.2.1+wvcus-darwin-arm64.zip "$ECS"
xattr -dr com.apple.quarantine "$ECS"
/usr/libexec/PlistBuddy -c "Set :CFBundleName hum" -c "Set :CFBundleDisplayName hum" \
  -c "Set :CFBundleIdentifier dev.hum.player" "$ECS/Electron.app/Contents/Info.plist"
python3 -m venv .venv && .venv/bin/pip install castlabs-evs
.venv/bin/python -m castlabs_evs.account signup          # once, interactive
.venv/bin/python -m castlabs_evs.vmp sign-pkg --streaming "$ECS"
```

Linux: same download (`linux-x64`, extract with `unzip`), no VMP. The daemon always passes
`--ozone-platform=headless --disable-gpu` on Electron's argv, display server or not: the window is
never shown, and the player then survives the graphical session (logout, compositor crash).
`app.commandLine` in `main.js` is too late: the ozone platform is picked from `XDG_SESSION_TYPE`
before it runs. Tested 2026-11-19 (x86_64), with and without X/Wayland: Widevine licence and audio
work unsigned; MPRIS (metadata, cover, PlayPause) still works headless, it goes over D-Bus.

## Authentication

Two tokens, both from `src/auth.rs` (`ensure_login`), obtained by the first `hum` in the foreground:

- librespot session (keymaster client id): stored credentials → login5 token + client token for
  pathfinder browsing, spclient and the dealer.
- hum developer app (`7bd5b13670b74564b1de554c858d469e`): `web-token.json` with refresh token and
  granted scopes, used by the SDK and the Web API. If `WEB_SCOPES` gains a scope, the next `hum`
  redoes this one.

They cannot be merged: the access point refuses the developer app's token ("Bad credentials"),
and the keymaster id gets 429 from the Web API and the SDK. Both are authorized in one browser tab
with hum's own PKCE flow (`Flow`, `collect_codes`): a listener on `http://127.0.0.1:8898/login`
answers the first app's code with a redirect to the second app's consent page, then shows "logged
in". Each app asks for consent only the first time ever; later logins just bounce through.
Refreshing the web token still uses `librespot-oauth`.
