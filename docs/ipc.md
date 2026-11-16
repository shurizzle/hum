# IPC protocol

Defined in `src/proto.rs`.

- Transport: Unix socket at `$XDG_RUNTIME_DIR/hum.sock`, falling back to `$TMPDIR/hum.sock`.
- Framing: `tokio_util::codec::LengthDelimitedCodec` (4-byte big-endian length prefix).
- Payload: JSON (`serde_json`), externally tagged enums.

## Client → daemon: `Cmd`

| Variant | Effect |
|---|---|
| `Load { context_uri, tracks, track }` | Play a context (playlist, album, artist, station) or a track list on the `hum` device. `track` is the uri to start from. |
| `PlayPause`, `Next`, `Prev` | Player (SDK) controls |
| `Seek(ms)` | Player seek |
| `Volume(u16)` | 0..=65535 |
| `Shuffle(bool)`, `Repeat(bool)` | Web API, `Repeat(true)` = repeat context |
| `Queue(uri)` | `POST /me/player/queue` on the active device; with none active, first transfers playback to hum paused (`Player::activate`), retrying while the transfer registers |
| `Unqueue { uri, nth }` | Remove the `nth` (0-based) occurrence of `uri` in `NowPlaying.next` via a Connect `set_queue` command |
| `MoveQueue { uri, nth, down }` | Swap that track with the next/previous shown one via `set_queue`; each slot keeps its provider (queue/context) |
| `Lyrics(uri)` | Reply with `Msg::Lyrics { uri, lines }` (`LyricsLine { start_ms, words }`, empty when the track has none) |
| `Devices` | Reply with `Msg::Devices`: the account's Connect devices (`GET /me/player/devices`; the cluster omits the SDK's own device) |
| `Transfer(id)` | `PUT /me/player {device_ids: [id], play}`, keeping the current play/pause state |
| `Tracks(uris)` | Reply with `Msg::Tracks`: track metadata as `Item`s, request order, unknown skipped |
| `Token` | Reply with `Msg::Tokens` |
| `Shutdown` | Stop the daemon |

Only `Token` gets a direct reply. Failures of any command come back as `Msg::Error`.

## Daemon → client: `Msg`

| Variant | When |
|---|---|
| `Hello { version }` | First message on every connection |
| `State(NowPlaying)` | Right after `Hello`, then on every change |
| `Tracks(Vec<Item>)` | Reply to `Cmd::Tracks` |
| `Lyrics { uri, lines }` | Reply to `Cmd::Lyrics` |
| `Devices(Vec<Device>)` | Reply to `Cmd::Devices`; `Device { id, name, kind, active }` |
| `Tokens { access_token, client_token, web_token }` | Reply to `Cmd::Token`. `access_token`/`client_token`: librespot session (pathfinder). `web_token`: hum developer app (Web API). |
| `Error(String)` | Command failure, or a daemon/librespot warning or error |

`NowPlaying`: `uri`, `name`, `artists`, `album`, `cover` (url, ~300 px), `context` (playlist/
album/artist uri the track plays from, empty for loose tracks), `duration_ms`,
`position_ms` (at the time of the event; clients extrapolate while `playing`), `playing`, `volume`,
`next` (upcoming track uris from the Connect cluster, max 100, kept current for every device;
the TUI's queue panel shows them).

## Compatibility

Any change to `Cmd`, `Msg` or `NowPlaying` is a protocol change: bump the version in `Cargo.toml`
so old daemons get replaced (see `spawn::try_connect`). During development the version does not
change, so run `hum kill` after rebuilding.

Manual testing from Python: connect to the socket, read/write `struct.pack('>I', len) + json`.
