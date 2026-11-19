# IPC protocol

Defined in `src/proto.rs`.

- Transport: Unix socket at `$XDG_RUNTIME_DIR/hum.sock`, falling back to `$TMPDIR/hum.sock`.
- Framing: `tokio_util::codec::LengthDelimitedCodec` (4-byte big-endian length prefix).
- Payload: JSON (`serde_json`), externally tagged enums.

## Client → daemon: `Cmd`

| Variant | Effect |
|---|---|
| `Load { context_uri, tracks, track }` | Play a context (playlist, album, artist, station) or a track list on the `hum` device. `track` is the uri to start from. A single loose track (`tracks: [uri]`, no context) plays inside its Song Radio playlist. |
| `PlayPause`, `Next`, `Prev` | Player (SDK) controls |
| `Seek(ms)` | Player seek |
| `Volume(u16)` | 0..=65535; unmutes first when muted |
| `Mute(bool)` | Silence playback keeping `volume` (see [playback.md](playback.md)); no-op while another device plays |
| `Shuffle(bool)`, `Repeat(Repeat)` | Web API; `Repeat` is `"off"`, `"context"` or `"track"` |
| `Queue(uri)` | `POST /me/player/queue` on the active device; with none active, first transfers playback to hum paused (`Player::activate`), retrying while the transfer registers |
| `Unqueue { uri, nth }` | Remove the `nth` (0-based) occurrence of `uri` in `NowPlaying.next` via a Connect `set_queue` command |
| `MoveQueue { uri, nth, down }` | Swap that track with the next/previous shown one via `set_queue`; each slot keeps its provider (queue/context) |
| `Lyrics(uri)` | Reply with `Msg::Lyrics { uri, lines }` (`LyricsLine { start_ms, words }`, empty when the track has none) |
| `Devices` | Reply with `Msg::Devices`: the account's Connect devices (`GET /me/player/devices`; the cluster omits the SDK's own device) |
| `Transfer(id)` | `PUT /me/player {device_ids: [id], play}`, keeping the current play/pause state |
| `Tracks(uris)` | Reply with `Msg::Tracks`: track metadata as `Item`s, request order, unknown skipped |
| `Token` | Reply with `Msg::Tokens` |
| `Shutdown` | Stop the daemon |

Commands whose row says "Reply" get a direct reply. Failures of any command come back as `Msg::Error`.

Commands on a connection are handled in order, and the daemon closes it on EOF. One-shot clients
(`src/cli.rs`) rely on this: send a `Cmd`, shut down the write half, read until EOF; an
`Msg::Error` in between is that command's failure.

## Daemon → client: `Msg`

| Variant | When |
|---|---|
| `Hello { version, protocol }` | First message on every connection: app version (informative) and `proto::PROTOCOL` |
| `State(NowPlaying)` | Right after `Hello`, then on every change |
| `Tracks(Vec<Item>)` | Reply to `Cmd::Tracks` |
| `Lyrics { uri, lines }` | Reply to `Cmd::Lyrics` |
| `Devices(Vec<Device>)` | Reply to `Cmd::Devices`; `Device { id, name, kind, active }` |
| `Tokens { access_token, client_token, web_token }` | Reply to `Cmd::Token`. `access_token`/`client_token`: librespot session (pathfinder). `web_token`: hum developer app (Web API). |
| `Library` | Your Library or liked songs changed (any device); clients reload the library and the hearts on screen |
| `Playlist(uri)` | That playlist's contents changed (any device); clients reload it if open |
| `Error(String)` | Failure of the command this connection sent |
| `Log(String)` | A daemon/player/librespot warning or error, pushed to every client (TUI status bar) |

`NowPlaying`: `uri`, `name`, `artists`, `album`, `cover` (url, ~300 px), `context` (playlist/
album/artist uri the track plays from, empty for loose tracks), `duration_ms`,
`position_ms` (at the time of the event; clients extrapolate from `position_at_ms` while `playing`, never from when the `State` arrived: updates that change only the queue or options resend an old `position_ms`), `position_at_ms`
(unix ms when `position_ms` was measured), `playing`, `volume`, `muted` (`volume` is then the level to restore),
`shuffle`, `repeat` (`Repeat`; both from the Connect cluster of the active device),
`next` (upcoming track uris from the Connect cluster, max 100, kept current for every device;
the TUI's queue panel shows them).

## Compatibility

The protocol has its own version, `proto::PROTOCOL` (u32), independent of the app version in
`Cargo.toml`. Any change to `Cmd`, `Msg` or the types they carry (`NowPlaying`, `Tokens`, `Item`,
...) bumps it; a client finding a daemon with another protocol stops and replaces it (see
`spawn::try_connect`). Daemons predating the field send a `Hello` that does not parse and are
replaced too. A new app version with the same
protocol keeps the running daemon, so a TUI-only update does not restart playback; daemon-side
changes then need `hum kill`. During development, run `hum kill` after rebuilding.

Manual testing from Python: connect to the socket, read/write `struct.pack('>I', len) + json`.
