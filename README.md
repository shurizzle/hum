# hum

A Spotify client for the terminal, laid out like the Spotify desktop app, with covers.

- Same home feed, suggestions and pages as the official app (Spotify's internal web player APIs).
- Playback runs in a background daemon: close every terminal and the music keeps playing.
- Shows up as a Spotify Connect device named `hum`.
- Keyboard and mouse.
- Media keys and system now-playing: MPRIS on Linux, Control Center on macOS.
- Audio through Spotify's official Web Playback SDK (Widevine), or optionally Spotify Soloist; no
  Spotify app needed.

## Requirements

- Spotify Premium.
- Rust (edition 2024).
- A playback engine, one of:
  - **castLabs Electron** (Widevine), VMP-signed on macOS: downloaded automatically (~120 MB) the
    first time hum starts its daemon, from the
    [`ecs-v45.2.1+wvcus` release](https://github.com/shurizzle/hum/releases/tag/ecs-v45.2.1%2Bwvcus).
    Details in [docs/playback.md](docs/playback.md#setup).
  - **Spotify Soloist** (lossless, no Widevine): install the `soloist` binary yourself from
    <https://developer.spotify.com/documentation/soloist> and have a Soloist developer API key. If
    `soloist` is on `PATH` (or `$HUM_SOLOIST`) and Electron is not set up, hum pairs it automatically
    on first run (it asks for the key). See [docs/playback.md](docs/playback.md#soloist-engine-srcsoloistrs).
- For covers: Kitty, Ghostty, WezTerm or iTerm2. Other terminals fall back to block characters.

## Install

- Prebuilt binary with [`cargo binstall`](https://github.com/cargo-bins/cargo-binstall):

  ```sh
  cargo binstall --git https://github.com/shurizzle/hum hum-tui
  ```

  Or download `hum-<target>.tar.gz` from a `vX.Y.Z` release: `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (Linux ones fully
  static). macOS binaries are not notarized: after a browser download run
  `xattr -d com.apple.quarantine hum`.
- Arch Linux (AUR, prebuilt binary + shell completions, pulls in Soloist): `paru -S hum-bin`
- From source:

  ```sh
  cargo install --git https://github.com/shurizzle/hum --locked
  ```

Electron is downloaded at first start; on Linux it still needs glibc and its usual libraries.

## Usage

```sh
cargo run --release        # or: cargo install --path . && hum
```

The first run opens the browser once to log in (two Spotify consent pages the very first time, in
the same tab). Then:

```sh
hum          # open the TUI (starts the daemon if needed)
hum start    # start the daemon in the background (exits after 10 min idle with nothing playing)
hum kill     # stop the daemon
```

Remote control from scripts or key bindings (starts the daemon if needed, except `get`):

```sh
hum play URI|LINK                   # track/album/playlist/artist; open.spotify.com links work too
hum next | prev
hum queue URI|LINK
hum toggle play|shuffle|repeat|like|mute
hum set play|shuffle|like|mute on|off
hum set repeat off|on|track         # on = repeat the context
hum set volume 40                   # percent; +5 / -5 relative
hum set position 90                 # seconds; +10 / -10 relative
hum get play|shuffle|repeat|like|volume|mute|position|queue|status [--json] [-f]
                                    # -f/--follow: a new line on every change; fails when no daemon runs
hum devices                         # Connect devices, `*` = active
hum transfer NAME|ID
hum completions zsh|bash|fish|...   # shell completion script
```

## Keys

`hjkl`/arrows move · `Enter` open/play · `Esc` back · `/` search · `g` home · `Space` play/pause ·
`n`/`b` next/previous · `Shift+←/→` seek · `+`/`-` volume · `m` mute · `s` shuffle · `r` repeat ·
`a` add to queue · `f`/`F` like selected/playing · `[` / `]` library / now-playing panel · `c` pick device · `y` lyrics · `?` all keys · `q` quit · `Q` quit and stop the daemon.

Full reference: [docs/tui.md](docs/tui.md).

## Cache

Albums, artists, track metadata, covers and the volume are cached on disk in `~/Library/Caches/hum/cache.db`
(LMDB): fresh entries load without network, expired ones are refetched and used as fallback when
offline. Playlists, home, library and search always come from the network. Delete the directory to
reset it.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md)
- [docs/ipc.md](docs/ipc.md)
- [docs/playback.md](docs/playback.md)
- [docs/spotify-apis.md](docs/spotify-apis.md)
- [docs/tui.md](docs/tui.md)
- [docs/known-issues.md](docs/known-issues.md)

## Disclaimer

Not affiliated with Spotify. Uses undocumented APIs that can change at any time.
