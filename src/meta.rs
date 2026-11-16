//! Track metadata from Spotify's internal extended-metadata service over a librespot session:
//! many tracks per request and no Web API rate limit (the developer app's `GET /tracks/{id}` gets
//! 429 under queue-sized bursts, and its batch `GET /tracks?ids=` answers 403). Results go through
//! the disk cache (`track:<id>`).

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use librespot_core::{Session, SpotifyId, error::ErrorKind};
use librespot_protocol::{
    extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
    extension_kind::ExtensionKind,
    metadata::{self, Image},
};
use protobuf::{EnumOrUnknown, Message};

use crate::{
    api::Item,
    cache::Cache,
    proto::{LyricsLine, NowPlaying},
};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub album: String,
    pub cover: String,
    pub duration_ms: u32,
}

impl Track {
    pub fn item(&self) -> Item {
        Item { uri: self.uri.clone(), name: self.name.clone(), sub: self.artists.clone(), image: Some(self.cover.clone()).filter(|c| !c.is_empty()), desc: String::new() }
    }

    /// `NowPlaying` (paused, at the start) for this track.
    pub fn now_playing(&self) -> NowPlaying {
        NowPlaying {
            uri: self.uri.clone(),
            name: self.name.clone(),
            artists: self.artists.clone(),
            album: self.album.clone(),
            cover: self.cover.clone(),
            duration_ms: self.duration_ms,
            ..Default::default()
        }
    }
}

/// Track metadata hardly ever changes.
const TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Tracks in the order of `uris`; non-track uris and unknown tracks are skipped. Fresh disk cache
/// entries answer locally, one request fetches the rest; when it fails, expired entries are used.
pub async fn tracks(session: &Session, uris: &[String]) -> Result<Vec<Track>> {
    let cache = Cache::global();
    let uris: Vec<&String> = uris.iter().filter(|u| u.starts_with("spotify:track:")).collect();
    let parse = |b: Vec<u8>| serde_json::from_slice::<Track>(&b).ok();
    let key = |u: &str| format!("track:{}", u.trim_start_matches("spotify:track:"));
    let mut found: HashMap<String, Track> = uris.iter().filter_map(|u| Some((u.to_string(), cache.read_fresh(&key(u), TTL).and_then(parse)?))).collect();
    let missing: Vec<&String> = uris.iter().copied().filter(|u| !found.contains_key(*u)).collect();
    if !missing.is_empty() {
        match fetch(session, &missing).await {
            Ok(fetched) => {
                for t in fetched {
                    if let Ok(bytes) = serde_json::to_vec(&t) {
                        cache.write(&key(&t.uri), &bytes);
                    }
                    found.insert(t.uri.clone(), t);
                }
            }
            Err(e) => {
                let stale: Vec<(String, Track)> = missing.iter().filter_map(|u| Some((u.to_string(), cache.read_any(&key(u)).and_then(parse)?))).collect();
                if stale.is_empty() {
                    return Err(e);
                }
                found.extend(stale);
            }
        }
    }
    Ok(uris.into_iter().filter_map(|u| found.remove(u)).collect())
}

/// Lyrics change rarely; tracks without lyrics are cached (empty) too.
const LYRICS_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Lyrics of a track from Spotify's color-lyrics service (what the official clients show), through
/// the disk cache (`lyrics:<id>`). No lines: the track has none (404).
pub async fn lyrics(session: &Session, uri: &str) -> Result<Vec<LyricsLine>> {
    let id = uri.strip_prefix("spotify:track:").ok_or_else(|| anyhow::anyhow!("no lyrics for {uri}"))?;
    let (cache, key) = (Cache::global(), format!("lyrics:{id}"));
    let parse = |b: Vec<u8>| serde_json::from_slice::<Vec<LyricsLine>>(&b).ok();
    if let Some(lines) = cache.read_fresh(&key, LYRICS_TTL).and_then(parse) {
        return Ok(lines);
    }
    let lines = match session.spclient().get_lyrics(&SpotifyId::from_base62(id)?).await {
        Ok(body) => parse_lyrics(&serde_json::from_slice(&body)?),
        Err(e) if matches!(e.kind, ErrorKind::NotFound) => vec![],
        Err(e) => return cache.read_any(&key).and_then(parse).ok_or(e.into()),
    };
    if let Ok(bytes) = serde_json::to_vec(&lines) {
        cache.write(&key, &bytes);
    }
    Ok(lines)
}

fn parse_lyrics(v: &serde_json::Value) -> Vec<LyricsLine> {
    let synced = v.pointer("/lyrics/syncType").and_then(|s| s.as_str()) == Some("LINE_SYNCED");
    v.pointer("/lyrics/lines")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .map(|l| LyricsLine {
            start_ms: if synced { l["startTimeMs"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0) } else { 0 },
            words: l["words"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// One extended-metadata request for `uris` (track uris only).
async fn fetch(session: &Session, uris: &[&String]) -> Result<Vec<Track>> {
    let req = BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|u| EntityRequest {
                entity_uri: u.to_string(),
                query: vec![ExtensionQuery { extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4), ..Default::default() }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let res = session.spclient().get_extended_metadata(req).await?;
    Ok(res
        .extended_metadata
        .iter()
        .flat_map(|a| &a.extension_data)
        .filter_map(|data| {
            let t = metadata::Track::parse_from_bytes(&data.extension_data.as_ref()?.value).ok()?;
            Some(track(&data.entity_uri, &t))
        })
        .collect())
}

fn track(uri: &str, t: &metadata::Track) -> Track {
    let album = t.album.as_ref();
    let images: Vec<&Image> = album.map(|a| a.cover_group.image.iter().chain(&a.cover).collect()).unwrap_or_default();
    Track {
        uri: uri.to_string(),
        name: t.name().to_string(),
        artists: t.artist.iter().map(|a| a.name()).collect::<Vec<_>>().join(", "),
        album: album.map(|a| a.name().to_string()).unwrap_or_default(),
        cover: cover_url(&images),
        duration_ms: t.duration().max(0) as u32,
    }
}

/// The ~300 px cover (`DEFAULT` size), else any.
fn cover_url(images: &[&Image]) -> String {
    let pick = images.iter().find(|i| i.size() == metadata::image::Size::DEFAULT).or(images.first());
    pick.map(|i| format!("https://i.scdn.co/image/{}", hex(i.file_id()))).unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyrics_lines_synced_and_unsynced() {
        let v = serde_json::json!({"lyrics": {"syncType": "LINE_SYNCED", "lines": [{"startTimeMs": "5240", "words": "Eh"}, {"startTimeMs": "8520", "words": "Ma"}]}});
        assert_eq!(parse_lyrics(&v), [LyricsLine { start_ms: 5240, words: "Eh".into() }, LyricsLine { start_ms: 8520, words: "Ma".into() }]);
        let v = serde_json::json!({"lyrics": {"syncType": "UNSYNCED", "lines": [{"startTimeMs": "0", "words": "A"}]}});
        assert_eq!(parse_lyrics(&v)[0].start_ms, 0);
    }

    #[test]
    fn cover_prefers_default_size() {
        let img = |id: u8, size| Image { file_id: Some(vec![id, 0xab]), size: Some(EnumOrUnknown::new(size)), ..Default::default() };
        let (small, default) = (img(1, metadata::image::Size::SMALL), img(2, metadata::image::Size::DEFAULT));
        assert_eq!(cover_url(&[&small, &default]), "https://i.scdn.co/image/02ab");
        assert_eq!(cover_url(&[&small]), "https://i.scdn.co/image/01ab");
    }
}
