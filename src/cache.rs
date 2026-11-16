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
    fn values_without_header_are_ignored() {
        assert_eq!(decode(b"{}"), None);
        assert_eq!(decode(b"hc1\x05\0\0\0\0\0\0\0x"), Some((5, b"x".to_vec())));
    }
}
