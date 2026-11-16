# hum

A Spotify client for the terminal, laid out like the Spotify desktop app, with covers.

- Same home feed, suggestions and pages as the official app (Spotify's internal web player APIs).
- Playback runs in a background daemon: close every terminal and the music keeps playing.
- Shows up as a Spotify Connect device named `hum`.
- Keyboard and mouse.
- Audio through Spotify's official Web Playback SDK (Widevine), no Spotify app needed.

## Requirements

- Spotify Premium.
- Rust (edition 2024).
- castLabs Electron (Widevine), VMP-signed on macOS: downloaded automatically (~120 MB) the first
  time hum starts its daemon, from the
  [`ecs-v45.2.1+wvcus` release](https://github.com/shurizzle/hum/releases/tag/ecs-v45.2.1%2Bwvcus).
  Details in [docs/playback.md](docs/playback.md#setup).
- For covers: Kitty, Ghostty, WezTerm or iTerm2. Other terminals fall back to block characters.

## Install

- Binaries on each `vX.Y.Z` release: `hum-<target>.tar.gz` for `aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (Linux ones fully
  static). Also installable with `cargo binstall` (once the crate is on crates.io). Electron is downloaded at first start;
  on Linux it still needs glibc and its usual libraries. macOS binaries are not notarized: after a
  browser download run `xattr -d com.apple.quarantine hum`.
- From source: `cargo install --path .`.

## Usage

```sh
cargo run --release        # or: cargo install --path . && hum
```

The first run opens the browser once to log in (two Spotify consent pages the very first time, in
the same tab). Then:

```sh
hum          # open the TUI (starts the daemon if needed)
hum kill     # stop the daemon
```

## Keys

`hjkl`/arrows move · `Enter` open/play · `Esc` back · `/` search · `g` home · `Space` play/pause ·
`n`/`b` next/previous · `Shift+←/→` seek · `+`/`-` volume · `s` shuffle · `r` repeat ·
`a` add to queue · `[` / `]` library / now-playing panel · `c` pick device · `y` lyrics · `?` all keys · `q` quit · `Q` quit and stop the daemon.

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
