# Architecture

`hum` is a terminal Spotify client with a client/server split. A single binary is both the
client (TUI) and the server (daemon).

```
 hum (TUI) ──┐                         ┌── pathfinder GraphQL (home, search, pages)
 hum (TUI) ──┼── Unix socket (IPC) ──► │   cover images
 hum (TUI) ──┘        hum daemon       └── called directly by each client
                         ├── librespot Session: browsing tokens (login5, client token), track
                         │   metadata (extended-metadata)
                         ├── Mirror: own librespot Session, Connect cluster (dealer), set_queue
                         └── player child: castLabs Electron + Web Playback SDK (Widevine),
                             Spotify Connect device "hum", driven via SDK + Web API
```

## Responsibilities

| | Daemon (`hum daemon`) | Client (`hum`) |
|---|---|---|
| Lifetime | Spawned on demand, survives all clients | One per terminal |
| Owns | librespot session, player process, playback state | Navigation, views, selection, cover cache |
| Network | Spotify AP (login, metadata, Connect), Web API (play/shuffle/repeat/queue/transfer); audio via the player | pathfinder, cover images |
| Talks to | Clients via IPC | Daemon via IPC, Spotify via HTTP |

Only playback is shared. Everything else is per-client: two TUIs can browse different pages while
both see the same now-playing state.

## Daemon

- `src/daemon.rs`. Started by a client as `current_exe() daemon`, detached (`process_group(0)`),
  stdout/stderr to `daemon.log`.
- Reads cached credentials; never runs the OAuth flow (it has no terminal or browser).
- Connects a librespot session only to mint browsing tokens. Playback runs in a supervised child
  process (`src/player.rs`): castLabs Electron with Spotify's Web Playback SDK, registered as the
  Connect device `hum`. See [docs/playback.md](docs/playback.md).
- Folds the player's state reports into a `NowPlaying` snapshot in a `watch` channel. Every client
  connection gets the current snapshot, then each change.
- Follows the account's Connect cluster over the librespot dealer (hidden observer device): restores
  the last playback on startup and mirrors playback on other devices into `NowPlaying`, routing
  controls to them through the Web API. See [docs/playback.md](docs/playback.md).
- With the Soloist engine, publishes OS media controls itself (MPRIS on Linux, Now Playing on
  macOS; `src/media/`); ECS gets them from Chromium. On macOS the daemon's main thread serves the
  main run loop and tokio runs on another thread. See [docs/playback.md](docs/playback.md).
- Logs the account type (`type`, `catalogue`, country) 3 s after login, for diagnosing playback
  refusals.
- Forwards `warn`/`error` log records (daemon, player, librespot) to all clients as
  `Msg::Log`, shown in the TUI status bar.
- Exits after 10 minutes with no clients and nothing playing, on `Cmd::Shutdown` or on Ctrl-C.
  Removes socket and pid file on exit; the player child dies with it.

## Spawn and versioning

- `src/spawn.rs`. The client connects to the socket. On failure it takes an exclusive `flock` on
  `hum.lock` (so concurrent clients spawn only one daemon), retries, then spawns and polls for up
  to 10 s.
- First message on every connection is `Msg::Hello { version, protocol }` (`CARGO_PKG_VERSION`,
  `proto::PROTOCOL`). A daemon with another protocol (or one that does not send `Hello`) is
  stopped and replaced; app versions alone never restart it.
- `hum start` only connects (spawning the daemon if needed) and exits.
- `hum kill` sends `Cmd::Shutdown` and stays connected until the daemon closes the socket (closing
  at once made the daemon fail its hello write and never read the command); after 5 s, `kill -9`
  using `hum.pid`.

## Remote-control subcommands

- `src/cli.rs` (`hum play`, `toggle`, `set`, `get`, `devices`, `transfer`, ...; parsed with
  clap in `src/main.rs`). Each is a short-lived IPC client: connect (spawning the daemon, after
  `auth::ensure_login`), read the initial `State`, send one `Cmd`, half-close the socket and read
  until the daemon closes. Commands with a reply (`Devices`, `Token`, `Tracks`) read until it.
  `Msg::Error` makes the command fail; `Msg::Log` is ignored.
- Likes (`toggle|set|get like`) take tokens from the daemon (`Cmd::Token`) and call
  `api::Api::saved`/`set_saved` from the CLI, like the TUI.
- `hum get --follow` keeps the connection and prints a line whenever the value changes
  (position: every second while playing; like: on track change or `Msg::Library`; queue: on
  `NowPlaying.next` change, titles via `Cmd::Tracks`).
- `hum get` only connects to a running daemon (`spawn::try_connect`) so polling it from a
  status bar does not keep an idle daemon alive; without one it fails with "daemon not running".

## Client

- `src/tui/`. Gets tokens from the daemon (`Cmd::Token`, refreshed every 30 min): login5 + client
  token for pathfinder, web token for the Web API. It calls Spotify itself. The daemon never proxies browsing data.
- One `tokio::select!` loop: terminal events, daemon messages, background fetch results, image
  results, timers. UI responsiveness has priority: all network and image work runs in tasks and
  reports back over channels; nothing blocks the loop.
- Network fetches for pages are cancellable: every navigation aborts the previous fetch
  (`AbortHandle`) and bumps a generation counter that discards stale results.
- Playlists/albums open immediately with the known header, then stream tracks in chunks (30, then
  100 per request).

## Files on disk

| Path (macOS) | Content |
|---|---|
| `~/Library/Application Support/hum/credentials.json` | Stored librespot credentials |
| `~/Library/Application Support/hum/web-token.json` | hum developer app token + refresh token |
| `~/Library/Application Support/hum/hashes.json` | pathfinder persisted query hashes |
| `~/Library/Application Support/hum/last-page.json` | Last open playlist/album/artist (client resume) |
| `~/Library/Application Support/hum/ecs/` (Linux: `$XDG_DATA_HOME/hum/ecs/`) | castLabs Electron, installed automatically before the first daemon start (`setup::ensure_ecs`); `VERSION` holds the installed `ECS_VERSION` |
| `~/Library/Application Support/hum/soloist-key` (Linux: `$XDG_CONFIG_HOME/hum/soloist-key`) | Spotify Soloist developer API key, written after pairing; its presence selects the Soloist engine (`src/soloist.rs`, [docs/playback.md](docs/playback.md)) |
| `~/Library/Application Support/hum/soloist/` (Linux: `$XDG_CONFIG_HOME/hum/soloist/`) | Soloist session and state (`-D`); `ws.port` holds the runtime WebSocket port |
| `~/Library/Caches/hum/player/` | Engine files written from the binary |
| `~/Library/Caches/hum/cache.db/` | Disk cache (LMDB, shared by daemon and clients): albums, artists, track metadata, covers. See [docs/spotify-apis.md](docs/spotify-apis.md#disk-cache) |
| `~/Library/Application Support/hum/daemon.log` (Linux: `$XDG_STATE_HOME/hum/daemon.log`) | Daemon stdout/stderr |
| `$XDG_RUNTIME_DIR` or `$TMPDIR`: `hum.sock`, `hum.pid`, `hum.lock` | IPC socket, daemon pid, spawn lock |

Details: [docs/ipc.md](docs/ipc.md), [docs/playback.md](docs/playback.md), [docs/spotify-apis.md](docs/spotify-apis.md),
[docs/tui.md](docs/tui.md), [docs/known-issues.md](docs/known-issues.md).
