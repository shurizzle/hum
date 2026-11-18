# AGENTS.md

`hum` (package `hum-tui`): terminal Spotify client in Rust. A daemon owns playback through a
child process (Spotify Web Playback SDK in castLabs Electron); each TUI client does its own browsing
via Spotify's internal APIs.

## Documentation — keep it current

Every change to behavior, architecture, protocol, keys, files on disk, dependencies or known issues
must update the docs **in the same change**:

- `ARCHITECTURE.md` — components, responsibilities, lifetimes, files on disk.
- `docs/ipc.md` — `Cmd`, `Msg`, `NowPlaying`, compatibility rules.
- `docs/playback.md` — player engine, its stdin/stdout protocol, ECS setup and signing, logins.
- `docs/spotify-apis.md` — auth, pathfinder operations/variables, Web API, hash scraping, parsing.
- `docs/tui.md` — layout, keys, mouse, covers, rendering rules.
- `docs/known-issues.md` — open problems, workarounds, decisions not to pursue something.
- `README.md` — user-facing usage, install, keys summary.
- `AGENTS.md` — this file, when workflow or conventions change.

Do not finish a task with docs out of date.

## Layout

```
src/main.rs        entry and clap parsing: `hum`, `hum kill`, `hum daemon`, remote subcommands
src/cli.rs         remote-control subcommands (play, toggle, set, get, devices, ...)
src/proto.rs       IPC types and framing
src/daemon.rs      librespot session (tokens), socket server, command dispatch, log forwarding
src/player.rs      supervised player child (ECS or Soloist), Web API calls for its device
src/soloist.rs     Spotify Soloist backend: pairing (token-exchange + DPoP) and WebSocket runtime
src/media/         OS media controls for Soloist: MPRIS (zbus, Linux), Now Playing (objc2, macOS)
player/            engine JS/HTML embedded into the binary
src/setup.rs       engine selection; installs the pinned Electron / pairs Soloist before first daemon start
src/spawn.rs       connect-or-spawn, version check, kill
src/auth.rs        OAuth logins (session + hum app), web token refresh, paths
src/cache.rs       persistent LMDB cache (albums, artists, track metadata, covers)
src/api.rs         pathfinder client, hash scraping, JSON item extraction
src/meta.rs        track metadata via spclient extended-metadata (daemon side)
src/tui/mod.rs     app state, input, cancellable fetches
src/tui/draw.rs    Spotify-like layout and rendering
src/tui/images.rs  async cover pipeline
.github/workflows/ecs.yml    publishes signed Electron as `ecs-<castLabs tag>` releases
.github/workflows/release.yml  on `vX.Y.Z` tags: binaries for macOS and Linux (static musl), aarch64 + x86_64
```

## Commands

```sh
cargo build
cargo test
cargo clippy          # keep it warning-free
cargo run             # TUI; spawns the daemon
./target/debug/hum kill   # stop the daemon (needed after every rebuild in dev)
```

Rust toolchain comes from Homebrew `rustup`; `cargo` lives in `/opt/homebrew/opt/rustup/bin`.

## Conventions

- UI responsiveness first: never block the `tokio::select!` loop; network and image work goes to
  tasks reporting over channels; fetches tied to navigation must be abortable.
- Bump `proto::PROTOCOL` (not the app version) on any IPC change so stale daemons are replaced.
- Disk cache (`src/cache.rs`) is best effort and never used from the render path or the
  `select!` loop, only from tasks. Playlists, home, library and search are never cached.
- Parse Spotify responses schema-agnostically (`api::items`), not with rigid structs.
- Prefer existing deps; dependencies build with `opt-level = 3` in dev.
- Releases: hum is `vX.Y.Z`; signed Electron is a separate `ecs-<castLabs tag>` release made by
  `.github/workflows/ecs.yml` (never latest). Bump `player::ECS_VERSION` only after trying a new one.
- TLS is rustls only (no OpenSSL), so Linux builds can be fully static (musl): librespot-core/oauth
  use `rustls-tls-webpki-roots` without default features, and so does our `reqwest` (0.12, same as
  `oauth2`, so it is linked once). Only ring is compiled in (no aws-lc-rs); `main` installs the ring
  `CryptoProvider` first; never add `native-tls` back.
- `vergen` is pinned to 9.0.6 in `Cargo.lock` (9.1.0 breaks the librespot-core build script).
- Shortcuts with known limits are marked `shortcut: <limit>, <when to upgrade>`.
- Debugging the TUI headless: drive `./target/debug/hum` in a Python `pty` with `TERM_PROGRAM` set;
  inject SGR mouse sequences like `\x1b[<0;10;2M`.
- Debugging the daemon: `~/Library/Application Support/hum/daemon.log` (Linux: `~/.local/state/hum/daemon.log`), or talk to the socket
  directly (see `docs/ipc.md`).
