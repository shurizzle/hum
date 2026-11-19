# Spotify APIs

`hum` uses the same internal APIs as the official web player, so home, suggestions and pages match
the official app. All of this is undocumented and can break without notice.

## Authentication

- `src/auth.rs`. First run does two browser OAuth (PKCE) logins, redirect
  `http://127.0.0.1:8898/login`:
  - librespot's keymaster client id `65b708073fc0480ea92a077233ca87bd`: its token connects a
    librespot session with `store = true`, saving reusable credentials. Browsing token:
    `session.login5().auth_token()`; client token: `session.spclient().client_token()`.
  - hum's developer app `7bd5b13670b74564b1de554c858d469e`: saved in `web-token.json` and refreshed
    by `auth::web_token`. Used for the Web API and the Web Playback SDK (the keymaster id gets 429
    there).
- All tokens reach clients via `Cmd::Token`.

## pathfinder (GraphQL)

`POST https://api-partner.spotify.com/pathfinder/v2/query` with headers `authorization: Bearer`,
`client-token`, `app-platform: WebPlayer` and body:

```json
{"operationName": "home", "variables": {...},
 "extensions": {"persistedQuery": {"version": 1, "sha256Hash": "..."}}}
```

| Operation | Used for | Notes |
|---|---|---|
| `home` | Home shelves | Requires `homeEndUserIntegration: "INTEGRATION_WEB_PLAYER"` |
| `searchDesktop` | Search | Hash lives in the `xpui-routes-search` chunk. Sections read by key from `data.searchV2` (`topResultsV2.itemsV2`, `tracksV2`, `artists`, `albumsV2`, `playlists`, `podcasts`, `episodes`), each through `api::items` |
| `libraryV3` | Your Library | |
| `fetchPlaylist`, `getAlbum`, `fetchLibraryTracks` | Pages, paged by `offset`/`limit` | |
| `queryArtistOverview` | Artist page | Not paged |
| `areEntitiesInLibrary` | Liked state (`Api::saved`) | `{uris}` → `data.lookup[i].data.saved`, same order. Not for playlists |
| `addToLibrary`, `removeFromLibrary` | Heart (`Api::set_saved`) | `{libraryItemUris: [uri], interactionId: null}`; tracks, albums, artists, shows, episodes (the web player's `pa` check). Mutations, hash in the main bundle. GraphQL errors arrive with status 200 (`api::errors`) |

When a variable is missing the API answers 400 with the variable name; copy the variables from the
web player bundle (search for the operation's variables object in the JS).

### Persisted query hashes

- Stored in the disk cache (`pathfinder-hashes`, see [Disk cache](#disk-cache)), loaded on first use.
  On a miss, `Api::scrape_hashes` downloads `https://open.spotify.com/` (desktop user agent required,
  otherwise the mobile player is served) and the `web-player.*.js` bundle, and extracts
  `"<op>","query","<sha256>"`. Operations still missing are looked up in the `xpui-routes-*` chunks
  of the bundle's chunk map: first the ones that held an operation last time (stored by name, which
  survives releases; today only `xpui-routes-search` for `searchDesktop`), then all of them (~46) only
  if something is still missing.
- On a 4xx response (except 401, 403, 408, 429) the hash for that operation is dropped and
  refetched next time; auth, timeout, rate-limit and 5xx errors keep it (`stale_hash`).

### Response parsing

Schemas are undocumented and change, so parsing is schema-agnostic (`api::items`): walk the JSON,
treat every object with a `spotify:` `uri` and a `name` (or `profile.name`) as an item, do not
descend into items (except the opened entity itself), dedupe by uri. Covers: the `{url, width}`
source closest to 300 px under the item.

## Disk cache

`src/cache.rs`, modeled on aniscrobble's: an LMDB environment (`heed`) in
`~/Library/Caches/hum/cache.db`, one database `cache` (`Str` → bytes), map size 1 GiB. Every
process (daemon, each client) opens it once (`Cache::global`); LMDB handles concurrent processes and
makes each write atomic. A value is `hc1` + write time (u64 LE epoch seconds) + payload; anything
else is ignored.

| Data | Key | TTL | Code |
|---|---|---|---|
| Album page (raw pathfinder JSON) | `album:<id>:<offset>:<limit>` | 7 days | `Api::open` |
| Artist overview (raw JSON) | `artist:<id>` | 24 h | `Api::open` |
| Track metadata (`meta::Track` JSON) | `track:<id>` | 30 days | `meta::tracks` (daemon) |
| Cover bytes | `cover:<url>` | never expires; a hit older than 24 h is rewritten (`Cache::read_touch`) | `tui::images` |
| Lyrics (`LyricsLine`s JSON, empty = none) | `lyrics:<id>` | 7 days | `meta::lyrics` (daemon) |
| pathfinder hashes (`api::Hashes` JSON) | `pathfinder-hashes` | never expires; a 4xx drops an operation | `Api::hash` |
| Player volume (u16 LE) | `volume` | never expires | `player::run_once`: set on `ready`, saved on change once the SDK reported the restored value |

- A fresh entry answers locally, no request. Expired: the request goes out and the response is
  written; if it fails, the expired entry is used instead (offline fallback). Never fetched: the
  normal error.
- Raw responses are cached, not parsed items, so parser changes apply to cached data too.
- Playlists, home, library (Liked Songs included) and search are never cached: they change often.
- Best effort: open or I/O failures disable or skip the cache silently (one warning on open).
- Pruning (`Cache::prune`): the daemon checks at start and every 15 min (`PRUNE_EVERY`). When the
  environment uses more than 768 MiB (`PRUNE_ABOVE`), it deletes entries by oldest write time (covers: last write or last
  disk hit, within 24 h) until
  keys + values sum to at most 512 MiB (`PRUNE_TO`), in one write transaction. Entries without a
  valid header go first; `volume` and `pathfinder-hashes` are never pruned. LMDB reuses the freed pages; the file never
  shrinks.

## Web API

`https://api.spotify.com/v1` with the web token:

Client: `PUT|DELETE /playlists/{id}/followers` (`Api::set_saved`): saving playlists, which
`addToLibrary` refuses (the web player edits its rootlist instead). Needs `playlist-modify-public`
and `playlist-modify-private`.

Daemon:

- `PUT /me/player/play|shuffle|repeat?device_id=<hum device>`, `PUT /me/player` (transfer),
  `POST /me/player/queue?uri=` (empty body: Spotify answers 411 to bodiless POSTs without a content
  length), and `play|pause|next|previous|seek` without `device_id` for remote devices.
- Not used: `GET /me/player/queue` (lags behind context changes, empty with no active device),
  `GET /tracks/{id}` (the developer app gets 429 under queue-sized bursts), `GET /tracks?ids=`
  (403 to development-mode apps). There is no Web API call to remove a queued track.

## spclient (librespot session)

- `POST /extended-metadata/v0/extended-metadata` (`meta::tracks`): `TRACK_V4` for many tracks in
  one request; covers are `https://i.scdn.co/image/<hex file_id>` (`DEFAULT` size ≈ 300 px).
- `GET /color-lyrics/v2/track/<base62 id>` (`spclient.get_lyrics`, `meta::lyrics`): what the
  official clients show. `lyrics.syncType` `LINE_SYNCED` gives each line a `startTimeMs` (string);
  `UNSYNCED` lines are shown without timing. 404 (librespot `ErrorKind::NotFound`) = no lyrics.
- `GET /inspiredby-mix/v2/seed_to_playlist/<track uri>?response-format=json`
  (`spclient.get_radio_for_track`, `meta::radio`): the track's "Song Radio" playlist,
  `mediaItems[0].uri`.
- `PUT /connect-state/v1/devices/<id>` + dealer `hm://connect-state/v1/cluster`: the Connect
  cluster (see [playback.md](playback.md)).
- `POST /connect-state/v1/player/command/from/<our device>/to/<active device>` with
  `{"command": {"endpoint": "set_queue", "next_tracks", "prev_tracks", "queue_revision"}}`:
  rewrites the active device's upcoming tracks (queue removal). Tracks are sent back as the
  cluster gave them (`uri`, `uid`, `provider`, `metadata`).

## Playback

Web Playback SDK in castLabs Electron, see [playback.md](playback.md). Premium required.
