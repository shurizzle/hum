# Playback

Audio comes from Spotify's official **Web Playback SDK** running inside **castLabs Electron for
Content Security (ECS)**, which ships Google's Widevine CDM. librespot's own player is not used:
Spotify refuses its audio keys on some accounts (see [known-issues.md](known-issues.md)).

## Components

- `player/main.js`, `player/player.html`: the engine. Embedded in the binary (`include_str!`),
  written to `~/Library/Caches/hum/player/` and run as `<ECS> main.js` by the daemon.
- `src/player.rs`: daemon side. Supervises the child (restart after 3 s on exit), relays messages,
  folds state into `NowPlaying`, calls the Web API for the player's device.

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

## Connect state mirror

`player::Mirror` follows the account's connect-state cluster for the life of the daemon, on its own
librespot session: the dealer starts once per session, so any failure (network down at startup,
dealer gone) logs a warning and retries after 5 s with a fresh session. Start the librespot dealer, `PUT /connect-state/v1/devices/<id>` as a hidden observer device on every new
connection id (first connect, dealer reconnects), then read `hm://connect-state/v1/cluster` pushes.
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

Linux: same download (`linux-x64`, extract with `unzip`), no VMP; headless (no X/Wayland) uses
`--ozone-platform=headless`, still untested.

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
