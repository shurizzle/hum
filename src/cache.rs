//! Persistent cache for data that rarely changes (albums, artists, track metadata, covers), in an
//! LMDB environment shared by the daemon and every client.
//!
//! Keys are semantic: `album:<id>:<offset>:<limit>`, `artist:<id>`, `track:<id>`, `cover:<url>`.
//! Each entry carries the epoch second it was written. Fresh entries are served locally, expired
//! ones are fetched again and kept only as an offline fallback. Playlists, home, library and
//! search are never cached: they change too often. Every operation is best effort: failures are
//! ignored and never surface.

use std::{
    path::Path,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use heed::{
    Database, Env, EnvOpenOptions,
    types::{Bytes, Str},
};

const MAGIC: &[u8; 3] = b"hc1";
const HEADER: usize = 3 + 8;
/// Pruning starts when the environment uses more than this (the map is 1 GiB) ...
pub const PRUNE_ABOVE: u64 = 768 << 20;
/// ... and deletes the oldest entries until payloads sum to less than this.
pub const PRUNE_TO: u64 = 512 << 20;
/// Keys never pruned.
const KEEP: &[&str] = &["volume", crate::api::HASHES_KEY];
/// The daemon checks at start and then this often.
pub const PRUNE_EVERY: Duration = Duration::from_secs(15 * 60);
/// `read_touch` rewrites a hit at most this often, bounding write transactions on reads.
const TOUCH_AFTER: Duration = Duration::from_secs(24 * 3600);

pub struct Cache {
    db: Option<(Env, Database<Str, Bytes>)>,
}

impl Cache {
    /// The process-wide cache in `<cache dir>/hum/cache.db`. LMDB forbids opening one environment
    /// twice in a process, hence a single instance.
    pub fn global() -> &'static Cache {
        static CACHE: OnceLock<Cache> = OnceLock::new();
        CACHE.get_or_init(|| {
            let dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("hum/cache.db");
            Self::open(&dir).unwrap_or_else(|e| {
                log::warn!("cache disabled: {e}");
                Self { db: None }
            })
        })
    }

    fn open(dir: &Path) -> heed::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // SAFETY: the environment is opened once per process (`global`) and never with
        // conflicting flags; other processes open it the same way.
        let env = unsafe { EnvOpenOptions::new().max_dbs(1).map_size(1 << 30).open(dir)? };
        let mut wtxn = env.write_txn()?;
        let db = env.create_database(&mut wtxn, Some("cache"))?;
        wtxn.commit()?;
        Ok(Self { db: Some((env, db)) })
    }

    /// Cached payload when it is at most `ttl` old.
    pub fn read_fresh(&self, key: &str, ttl: Duration) -> Option<Vec<u8>> {
        let (at, value) = self.load(key)?;
        (now().saturating_sub(at) <= ttl.as_secs()).then_some(value)
    }

    /// Cached payload regardless of age, for fallback when the network fails.
    pub fn read_any(&self, key: &str) -> Option<Vec<u8>> {
        self.load(key).map(|(_, value)| value)
    }

    /// Cached payload regardless of age; a hit older than `TOUCH_AFTER` is rewritten with the current
    /// time, so pruning by write time spares it. Only for keys without a TTL: it resets freshness.
    pub fn read_touch(&self, key: &str) -> Option<Vec<u8>> {
        let (at, value) = self.load(key)?;
        if now().saturating_sub(at) > TOUCH_AFTER.as_secs() {
            self.store(key, &value, now());
        }
        Some(value)
    }

    pub fn write(&self, key: &str, bytes: &[u8]) {
        self.store(key, bytes, now());
    }

    fn store(&self, key: &str, bytes: &[u8], at: u64) {
        let Some((env, db)) = &self.db else { return };
        let mut value = Vec::with_capacity(HEADER + bytes.len());
        value.extend_from_slice(MAGIC);
        value.extend_from_slice(&at.to_le_bytes());
        value.extend_from_slice(bytes);
        let _ = (|| -> heed::Result<()> {
            let mut wtxn = env.write_txn()?;
            db.put(&mut wtxn, key, &value)?;
            wtxn.commit()
        })();
    }

    /// When the environment uses more than `above` bytes, deletes the least recently written entries
    /// until the remaining keys and values sum to at most `to` bytes. Entries without a valid header go
    /// first. Freed pages are reused by LMDB; the file does not shrink.
    pub fn prune(&self, above: u64, to: u64) {
        let Some((env, db)) = &self.db else { return };
        if env.non_free_pages_size().is_ok_and(|used| used <= above) {
            return;
        }
        let deleted = (|| -> heed::Result<usize> {
            let mut wtxn = env.write_txn()?;
            let mut entries = Vec::new();
            let mut total = 0u64;
            for item in db.iter(&wtxn)? {
                let (key, value) = item?;
                // Settings, not cached data: losing them changes behavior.
                if KEEP.contains(&key) {
                    continue;
                }
                let at = decode(value).map_or(0, |(at, _)| at);
                let size = (key.len() + value.len()) as u64;
                total += size;
                entries.push((at, size, key.to_owned()));
            }
            entries.sort_unstable_by_key(|(at, ..)| *at);
            let mut deleted = 0;
            for (_, size, key) in entries {
                if total <= to {
                    break;
                }
                db.delete(&mut wtxn, &key)?;
                total -= size;
                deleted += 1;
            }
            wtxn.commit()?;
            Ok(deleted)
        })();
        match deleted {
            Ok(n) => log::info!("cache: pruned {n} entries"),
            Err(e) => log::warn!("cache prune failed: {e}"),
        }
    }

    /// (write time, payload); entries without a valid header are ignored.
    fn load(&self, key: &str) -> Option<(u64, Vec<u8>)> {
        let (env, db) = self.db.as_ref()?;
        let rtxn = env.read_txn().ok()?;
        decode(db.get(&rtxn, key).ok()??)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn decode(raw: &[u8]) -> Option<(u64, Vec<u8>)> {
    if raw.len() < HEADER || &raw[..MAGIC.len()] != MAGIC {
        return None;
    }
    let at = u64::from_le_bytes(raw[MAGIC.len()..HEADER].try_into().ok()?);
    Some((at, raw[HEADER..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_without_a_database_is_a_no_op() {
        let cache = Cache { db: None };
        cache.write("album:1", b"x");
        assert_eq!(cache.read_any("album:1"), None);
    }

    #[test]
    fn fresh_entries_are_local_and_expired_ones_are_stale() {
        let dir = std::env::temp_dir().join(format!("hum-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::open(&dir).unwrap();
        cache.store("album:1", b"payload", now() - 120);
        assert_eq!(cache.read_fresh("album:1", Duration::from_secs(60)), None);
        assert_eq!(cache.read_fresh("album:1", Duration::from_secs(300)), Some(b"payload".to_vec()));
        assert_eq!(cache.read_any("album:1"), Some(b"payload".to_vec()));
        drop(cache);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_deletes_the_oldest_entries() {
        let dir = std::env::temp_dir().join(format!("hum-cache-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::open(&dir).unwrap();
        let at = now();
        cache.store("old", &[0; 100], at - 30);
        cache.store("mid", &[0; 100], at - 20);
        cache.store("new", &[0; 100], at - 10);
        cache.store("volume", b"\x10\0", 0);
        cache.prune(u64::MAX, 0);
        assert!(cache.read_any("old").is_some(), "under the threshold: untouched");
        // Each entry is ~114 bytes: room for one.
        cache.prune(0, 150);
        assert_eq!(cache.read_any("old"), None);
        assert_eq!(cache.read_any("mid"), None);
        assert!(cache.read_any("new").is_some());
        assert!(cache.read_any("volume").is_some());
        drop(cache);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_touch_refreshes_old_entries() {
        let dir = std::env::temp_dir().join(format!("hum-cache-touch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::open(&dir).unwrap();
        let old = now() - TOUCH_AFTER.as_secs() - 10;
        let recent = now() - 10;
        cache.store("cover:a", b"a", old);
        cache.store("cover:b", b"b", recent);
        assert_eq!(cache.read_touch("cover:a"), Some(b"a".to_vec()));
        assert_eq!(cache.read_touch("cover:b"), Some(b"b".to_vec()));
        assert!(cache.load("cover:a").unwrap().0 >= recent, "old hit rewritten");
        assert_eq!(cache.load("cover:b").unwrap().0, recent, "recent hit untouched");
        drop(cache);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn values_without_header_are_ignored() {
        assert_eq!(decode(b"{}"), None);
        assert_eq!(decode(b"hc1\x05\0\0\0\0\0\0\0x"), Some((5, b"x".to_vec())));
    }
}
