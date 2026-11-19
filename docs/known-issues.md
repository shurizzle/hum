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

## Other

- Disk cache pruning goes by write time, not access time (LMDB reads cannot record it cheaply):
  a cover written long ago and seen daily can be pruned and refetched. Pruning runs only while a
  daemon is up. The file (`~/Library/Caches/hum/cache.db`) keeps its peak size; delete the
  directory to shrink it.
- pathfinder hashes rotate with web player releases; the first call after a rotation rescrapes the
  bundle (~46 chunk downloads).
- A pathfinder 4xx (except auth, timeout, rate limit) drops that operation's cached hash: a
  missing variable (400) also triggers a rescrape.
- Paging stops when a chunk returns fewer than 90% of `limit` tracks; playlists with many
  duplicates may stop early.
