# Known issues

## librespot audio: `error audio key 0 1` (resolved by switching engine)

Resolved 2026-11-16 by playing through the Web Playback SDK in castLabs Electron
([playback.md](playback.md)). Kept for history.

Observed 2026-11-16 on the author's account (old Premium account): every track fails.

```
ERROR librespot_core::audio_key] error audio key 0 1
WARN  librespot_playback::player] Unable to load key, continuing without decryption: Service unavailable { audio key error }
ERROR librespot_playback::player] Unable to read audio file: Symphonia Decoder Error: ...
```

- Spotify's access point refuses the AES audio key (`AesKeyError`, code 1). Login, Connect,
  loading and metadata all work.
- Upstream: [librespot #1649](https://github.com/librespot-org/librespot/issues/1649) (open). It
  affects all librespot versions; reports mostly involve recent accounts, but not only. Code 1 is a
  permanent refusal; code 2 is transient rate limiting.
- No upstream fix. PR [#1869](https://github.com/librespot-org/librespot/pull/1869) only skips
  refused tracks faster.
- The daemon logs `account: type=... catalogue=... country=...` at startup (full attributes at
  `RUST_LOG=hum=debug`). Author's account: `type=premium`, `financial-product=...new-family-sub-1m...`,
  `multiuserplan-member-type=FAMILY_MEMBER`: an old account on a recently joined Family plan.
  #1649 also reports failing Family member accounts, so a new subscription is the likely trigger.
- Tried 2026-11-16: librespot's Android (`9a8d2f0ce77a4e248bb71fefcb557637`) and iOS
  (`58bd3c95768941ea9eb4350aaa033eb3`) client ids via `SessionConfig.client_id`. The daemon fails
  at startup with `Client specified an invalid argument { Response status code: 400 Bad Request }`
  from the client-token request (desktop platform data with a mobile client id). Stored credentials
  are also bound to the keymaster id, so login5 would fail too. Reverted.
- Not pursued: re-implementing Spotify's PlayPlay key obfuscation (DRM circumvention).

### Widevine spike (2026-11-16)

Spike `spike/widevine/` (ECS v45.2.1+wvcus, Widevine CDM 4.10.3112.0), macOS arm64:

- Keymaster token: Web API 429 on `melody/v1/check_scope` and `track-playback/v1/devices`; the SDK
  never gets `ready`. Fixed with hum's own developer app.
- Unsigned (development) ECS: `widevine-license/v1/audio/license` → 500. After
  `castlabs_evs.vmp sign-pkg --streaming`: licence 200, audio plays.

## Open

- Linux: the `hum` binary is fully static (musl), but Electron is a glibc build needing system
  libraries (`libnss3`, `libasound2`, `libgbm`, ...): no playback on musl distros (Alpine), and
  minimal installs need those packages.
- Linux without X/Wayland: `--ozone-platform=headless` with Widevine is untested.
- ECS is installed and VMP-signed by hand; every ECS update needs re-signing on macOS.
- Autoplay at the end of a context depends on Spotify's handling of SDK devices (untested). Loose
  tracks get no autoplay on the SDK device, so they play inside their Song Radio playlist instead:
  the context shown is that playlist, not the track.
- librespot warns "failure during data parsing for hm://collection/…/json" after every like (any
  device): that copy of the push is JSON where it expects base64. Harmless, the plain push arrives
  too; logged but kept out of the status bar.
- Playback state is event-driven; clients extrapolate position between events.

### Soloist engine

- No MPRIS / macOS Now Playing with Soloist: the ECS engine gets those from Chromium's media session;
  Soloist exposes none, so hum does not publish them when running on Soloist.
- The Soloist binary is user-installed and closed-source; Spotify forbids redistributing it, so hum
  never downloads it. Builds expire 90 days after their build date (exit `10`): the daemon surfaces an
  error and the user must download a fresh build.
- Soloist's Web API device is matched **by name** (`hum`) on `/me/player/devices`:
  `shortcut: breaks if another Connect device is also named hum; switch to the connect-state device id
  if Soloist ever exposes it`.
- Pairing uses an internal `/api/token` token-exchange and Soloist's zeroconf `addUser`, both
  undocumented; a Spotify change could break pairing. The one-time pairing is redone (delete
  `soloist-key`) if it stops working.

## Other

- Caches have no eviction: in-memory covers grow for the life of a client (sizes left over from
  resizes stay); the disk cache (`~/Library/Caches/hum/cache.db`, map capped at 1 GiB) grows
  forever. Delete the directory to reset it.
- pathfinder hashes rotate with web player releases; the first call after a rotation rescrapes the
  bundle (~46 chunk downloads).
- A pathfinder error for any reason drops that operation's cached hash.
- Paging stops when a chunk returns fewer than 90% of `limit` tracks; playlists with many
  duplicates may stop early.
- The client's local shuffle/repeat flags are not synced from the daemon.
- `Liked Songs` (`spotify:collection:tracks`) as a playback context is untested.
