use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::future::join_all;
use regex::Regex;
use serde_json::{Value, json};

use crate::{cache::Cache, proto::Tokens};

const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const WEB_PLAYER: &str = "https://open.spotify.com/";
const CDN: &str = "https://open.spotifycdn.com/cdn/build/web-player/";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";
/// Disk cache lifetimes; playlists, home, library and search are never cached.
const ALBUM_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ARTIST_TTL: Duration = Duration::from_secs(24 * 3600);
const OPS: &[&str] = &[
    "home",
    "searchDesktop",
    "fetchPlaylist",
    "getAlbum",
    "queryArtistOverview",
    "libraryV3",
    "fetchLibraryTracks",
    "areEntitiesInLibrary",
    "addToLibrary",
    "removeFromLibrary",
];
const SPCLIENT: &str = "https://spclient.wg.spotify.com";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Item {
    pub uri: String,
    pub name: String,
    pub sub: String,
    pub image: Option<String>,
    /// Editorial description (playlists), shown under cards like Spotify does.
    #[serde(default)]
    pub desc: String,
}

impl Item {
    pub fn kind(&self) -> &str {
        self.uri.split(':').nth(1).unwrap_or("")
    }

    /// An entity known only by uri; `Api::open` fills in the rest.
    pub fn from_uri(uri: &str) -> Self {
        Self { uri: uri.into(), name: String::new(), sub: String::new(), image: None, desc: String::new() }
    }

    /// Page for a playback context. Artist "Popular" playback runs in a list context
    /// (`spotify:list:popular-...:artist_<id>`, see `player::play_body`) that maps to the artist.
    pub fn from_context(uri: &str) -> Self {
        match uri.strip_prefix("spotify:list:").and_then(|l| l.rsplit_once(":artist_")) {
            Some((_, id)) => Self::from_uri(&format!("spotify:artist:{id}")),
            None => Self::from_uri(uri),
        }
    }

    pub fn browsable(&self) -> bool {
        matches!(self.kind(), "playlist" | "album" | "artist" | "collection")
    }
}

#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    pub tokens: Arc<RwLock<Tokens>>,
    /// Loaded from the disk cache on first use.
    hashes: Arc<RwLock<Option<Hashes>>>,
}

/// Disk cache key of the persisted query hashes.
pub const HASHES_KEY: &str = "pathfinder-hashes";

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Hashes {
    /// Operation name → persisted query hash.
    ops: HashMap<String, String>,
    /// Chunks holding an operation, by name (stable across releases, unlike the file hash).
    chunks: Vec<String>,
    /// Operations absent from every chunk → unix time of that scrape, so a lookup does not
    /// download the whole bundle again before `MISSING_TTL`.
    missing: HashMap<String, u64>,
}

/// How long an operation absent from the whole bundle is not looked up again.
const MISSING_TTL: u64 = 24 * 3600;

impl Hashes {
    fn recently_missing(&self, op: &str, now: u64) -> bool {
        self.missing.get(op).is_some_and(|t| now.saturating_sub(*t) < MISSING_TTL)
    }

    /// Merges a scrape; `complete` when every chunk was read, so operations still absent are
    /// really missing rather than in a chunk that failed to download.
    fn merge(&mut self, fresh: Hashes, complete: bool, now: u64) {
        for op in fresh.ops.keys() {
            self.missing.remove(op);
        }
        self.ops.extend(fresh.ops);
        self.chunks = fresh.chunks;
        if complete {
            for op in OPS.iter().filter(|op| !self.ops.contains_key(**op)) {
                self.missing.insert(op.to_string(), now);
            }
        }
    }
}

impl Api {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            http: reqwest::Client::builder().user_agent(UA).build().unwrap(),
            tokens: Arc::new(RwLock::new(tokens)),
            hashes: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn home(&self) -> Result<Vec<Section>> {
        let v = self
            .query(
                "home",
                json!({"timeZone": timezone(), "sp_t": "", "facet": "", "sectionItemsLimit": 10,
                       "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER", "includeHomeChapterVideoCards": false,
                       "includeEpisodeContentRatingsV2": false}),
            )
            .await?;
        let sections = v.pointer("/data/home/sectionContainer/sections/items").and_then(Value::as_array);
        Ok(sections
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let title = ["/data/title/transformedLabel", "/data/title/text"]
                    .iter()
                    .find_map(|p| s.pointer(p)?.as_str())
                    .unwrap_or("")
                    .to_string();
                let items = items(s.get("sectionItems")?, "");
                (!items.is_empty()).then_some(Section { title, items })
            })
            .collect())
    }

    /// Search results as titled sections.
    pub async fn search(&self, term: &str) -> Result<Vec<(&'static str, Vec<Item>)>> {
        let v = self
            .query(
                "searchDesktop",
                json!({"searchTerm": term, "offset": 0, "limit": 10, "numberOfTopResults": 5,
                       "includeAudiobooks": false, "includeArtistHasConcertsField": false,
                       "includePreReleases": false, "includeLocalConcertsField": false}),
            )
            .await?;
        let s = v.pointer("/data/searchV2").unwrap_or(&v);
        // The web player's "All" tab: its sections, order and sizes.
        Ok([
            ("Top result", "/topResultsV2/itemsV2", 1),
            ("Songs", "/tracksV2", 4),
            ("Artists", "/artists", 10),
            ("Albums", "/albumsV2", 10),
            ("Playlists", "/playlists", 10),
            ("Podcasts", "/podcasts", 10),
            ("Episodes", "/episodes", 10),
        ]
        .into_iter()
        .map(|(title, path, n)| {
            let mut found = s.pointer(path).map(|x| items(x, "")).unwrap_or_default();
            found.truncate(n);
            (title, found)
        })
        .filter(|(_, found)| !found.is_empty())
        .collect())
    }

    /// Whether each uri is in Your Library (Liked Songs for tracks). Playlists are not supported
    /// by this operation: they are saved when they show up in `library`.
    pub async fn saved(&self, uris: &[String]) -> Result<Vec<bool>> {
        let v = errors(self.query("areEntitiesInLibrary", json!({"uris": uris})).await?)?;
        let lookup = v.pointer("/data/lookup").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok((0..uris.len()).map(|i| lookup.get(i).and_then(|l| l.pointer("/data/saved")?.as_bool()).unwrap_or(false)).collect())
    }

    /// Like/unlike, like the heart of the web player: tracks, albums, artists, shows, episodes go
    /// through pathfinder, playlists through the rootlist (`addToLibrary` refuses them).
    pub async fn set_saved(&self, uri: &str, on: bool) -> Result<()> {
        if uri.starts_with("spotify:playlist:") {
            return self.set_followed(uri, on).await;
        }
        let op = if on { "addToLibrary" } else { "removeFromLibrary" };
        errors(self.query(op, json!({"libraryItemUris": [uri], "interactionId": null})).await?)?;
        Ok(())
    }

    /// Adds a playlist at the top of the rootlist, or removes it, as the web player does.
    async fn set_followed(&self, uri: &str, on: bool) -> Result<()> {
        let (access, client, user) = {
            let t = self.tokens.read().unwrap();
            (t.access_token.clone(), t.client_token.clone(), t.user.clone())
        };
        let root = format!("{SPCLIENT}/playlist/v2/user/{user}/rootlist");
        let send = |req: reqwest::RequestBuilder| req.bearer_auth(&access).header("client-token", &client).header("accept", "application/json").send();
        let v: Value = send(self.http.get(format!("{root}?decorate=revision"))).await?.error_for_status()?.json().await?;
        let at = v.pointer("/contents/items").and_then(Value::as_array).into_iter().flatten().position(|i| i["uri"] == uri);
        let op = match (on, at) {
            (true, None) => json!({"kind": "ADD", "add": {"fromIndex": 0, "items": [{"uri": uri, "attributes": {"timestamp": crate::proto::now_ms().to_string()}}]}}),
            (false, Some(i)) => json!({"kind": "REM", "rem": {"fromIndex": i, "length": 1, "items": [{"uri": uri}]}}),
            _ => return Ok(()),
        };
        let changes = json!({"baseRevision": v["revision"], "deltas": [{"ops": [op], "info": {"source": {"client": "WEBPLAYER"}}}],
                             "wantResultingRevisions": false, "wantSyncResult": false, "nonces": []});
        let res = send(self.http.post(format!("{root}/changes")).json(&changes)).await?;
        if !res.status().is_success() {
            bail!("{uri}: {} {}", res.status(), res.text().await.unwrap_or_default());
        }
        Ok(())
    }

    pub async fn library(&self) -> Result<Vec<Item>> {
        let v = self
            .query(
                "libraryV3",
                json!({"filters": [], "order": null, "textFilter": "", "features": ["LIKED_SONGS", "YOUR_EPISODES"],
                       "limit": 50, "offset": 0, "flatten": false, "expandedFolders": [], "folderUri": null,
                       "includeFoldersWithoutChildren": true}),
            )
            .await?;
        Ok(items(&v, ""))
    }

    /// One page of a playlist, album, liked songs or artist, whether more pages follow, and the
    /// opened entity itself when the response describes it.
    pub async fn open(&self, uri: &str, offset: usize, limit: usize) -> Result<(Vec<Item>, bool, Option<Item>)> {
        let id = uri.rsplit(':').next().unwrap_or_default();
        let v = match uri.split(':').nth(1) {
            Some("playlist") => {
                self.query("fetchPlaylist", json!({"uri": uri, "offset": offset, "limit": limit, "enableWatchFeedEntrypoint": false})).await?
            }
            Some("album") => {
                let vars = json!({"uri": uri, "locale": "", "offset": offset, "limit": limit});
                self.cached_query(&format!("album:{id}:{offset}:{limit}"), ALBUM_TTL, "getAlbum", vars).await?
            }
            Some("collection") => self.query("fetchLibraryTracks", json!({"offset": offset, "limit": limit})).await?,
            Some("artist") => {
                let vars = json!({"uri": uri, "locale": "", "includePrerelease": false});
                let v = self.cached_query(&format!("artist:{id}"), ARTIST_TTL, "queryArtistOverview", vars).await?;
                return Ok((items(&v, uri), false, root(&v, uri)));
            }
            _ => return Ok((vec![], false, None)),
        };
        let more = match paged(&v) {
            Some((len, total)) => len > 0 && offset + len < total,
            None => false,
        };
        Ok((items(&v, uri), more, root(&v, uri)))
    }

    /// `query` through the disk cache: a fresh entry answers without a request; when the request
    /// fails, an expired entry is the fallback.
    async fn cached_query(&self, key: &str, ttl: Duration, op: &str, variables: Value) -> Result<Value> {
        let cache = Cache::global();
        let parse = |b: Vec<u8>| serde_json::from_slice::<Value>(&b).ok();
        if let Some(v) = cache.read_fresh(key, ttl).and_then(parse) {
            return Ok(v);
        }
        match self.query(op, variables).await {
            Ok(v) => {
                cache.write(key, &serde_json::to_vec(&v)?);
                Ok(v)
            }
            Err(e) => cache.read_any(key).and_then(parse).ok_or(e),
        }
    }

    async fn query(&self, op: &str, variables: Value) -> Result<Value> {
        let hash = self.hash(op).await?;
        let (access, client) = {
            let t = self.tokens.read().unwrap();
            (t.access_token.clone(), t.client_token.clone())
        };
        let res = self
            .http
            .post(PATHFINDER)
            .bearer_auth(access)
            .header("client-token", client)
            .header("app-platform", "WebPlayer")
            .json(&json!({"variables": variables, "operationName": op,
                          "extensions": {"persistedQuery": {"version": 1, "sha256Hash": hash}}}))
            .send()
            .await?;
        let status = res.status();
        let body = res.text().await?;
        if !status.is_success() {
            // A rotated hash surfaces as an error: drop it so the next call refetches.
            if stale_hash(status, &body) {
                let mut hashes = self.hashes.write().unwrap();
                if let Some(h) = hashes.as_mut() {
                    h.ops.remove(op);
                    save(h);
                }
            }
            bail!("{op}: {status} {}", body.chars().take(200).collect::<String>());
        }
        Ok(serde_json::from_str(&body)?)
    }

    async fn hash(&self, op: &str) -> Result<String> {
        let now = crate::auth::now();
        let known = {
            let mut hashes = self.hashes.write().unwrap();
            let h = hashes.get_or_insert_with(|| {
                Cache::global().read_any(HASHES_KEY).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
            });
            if let Some(hash) = h.ops.get(op) {
                return Ok(hash.clone());
            }
            if h.recently_missing(op, now) {
                bail!("no persisted query hash for {op} (missing from the web player)");
            }
            h.chunks.clone()
        };
        let (fresh, complete) = self.scrape_hashes(&known).await?;
        let mut hashes = self.hashes.write().unwrap();
        let h = hashes.get_or_insert_default();
        h.merge(fresh, complete, now);
        save(h);
        h.ops.get(op).cloned().with_context(|| format!("no persisted query hash for {op}"))
    }

    /// Reads persisted query hashes from the web player bundle and its chunks (see `passes`), each
    /// pass only while an operation is still missing. True when every chunk fetched was read.
    async fn scrape_hashes(&self, known: &[String]) -> Result<(Hashes, bool)> {
        let html = self.http.get(WEB_PLAYER).send().await?.text().await?;
        let main = Regex::new(r#"https://open\.spotifycdn\.com/cdn/build/web-player/web-player\.[0-9a-f]+\.js"#)?
            .find(&html)
            .context("web player bundle not found")?
            .as_str()
            .to_string();
        let main_js = self.http.get(&main).send().await?.text().await?;
        let mut out = Hashes::default();
        extract(&main_js, &mut out.ops);
        let mut complete = true;
        for pass in passes(chunk_urls(&main_js), known) {
            if OPS.iter().all(|op| out.ops.contains_key(*op)) {
                break;
            }
            let chunks = join_all(pass.iter().map(|(_, u)| async move { self.http.get(u).send().await?.text().await })).await;
            for ((name, _), js) in pass.iter().zip(&chunks) {
                complete &= js.is_ok();
                if js.as_deref().is_ok_and(|js| extract(js, &mut out.ops)) {
                    out.chunks.push(name.clone());
                }
            }
        }
        Ok((out, complete))
    }
}

/// Chunks of the bundle's webpack chunk map as (name, url); unnamed chunks are named by id.
fn chunk_urls(main_js: &str) -> Vec<(String, String)> {
    static MAP: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"\(\{([^}]*)\}\)\[e\]\|\|e\)\+"\."\+\(\{([^}]*)\}\)\[e\]\+"\.js""#).unwrap()
    });
    static PAIR: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r#"(\d+):"([^"]+)""#).unwrap());
    let Some(c) = MAP.captures(main_js) else { return vec![] };
    let names: HashMap<_, _> = PAIR.captures_iter(&c[1]).map(|p| (p[1].to_string(), p[2].to_string())).collect();
    PAIR.captures_iter(&c[2])
        .map(|p| {
            let name = names.get(&p[1]).cloned().unwrap_or_else(|| p[1].to_string());
            let url = format!("{CDN}{name}.{}.js", &p[2]);
            (name, url)
        })
        .collect()
}

/// Lookup order: chunks that held an operation last time (`known`), route chunks, the rest.
fn passes(chunks: Vec<(String, String)>, known: &[String]) -> [Vec<(String, String)>; 3] {
    let (first, rest): (Vec<_>, Vec<_>) = chunks.into_iter().partition(|(name, _)| known.contains(name));
    let (routes, others) = rest.into_iter().partition(|(name, _)| name.contains("routes"));
    [first, routes, others]
}

/// Adds the hashes of `OPS` found in `js`; true when any was found.
fn extract(js: &str, ops: &mut HashMap<String, String>) -> bool {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#""(\w+)","(?:query|mutation)","([0-9a-f]{64})""#).unwrap());
    let mut found = false;
    for c in RE.captures_iter(js) {
        if OPS.contains(&&c[1]) {
            ops.insert(c[1].to_string(), c[2].to_string());
            found = true;
        }
    }
    found
}

/// GraphQL errors come back with status 200.
fn errors(v: Value) -> Result<Value> {
    match v.pointer("/errors/0/message").and_then(Value::as_str) {
        Some(e) => bail!("{e}"),
        None => Ok(v),
    }
}

fn save(hashes: &Hashes) {
    if let Ok(bytes) = serde_json::to_vec(hashes) {
        Cache::global().write(HASHES_KEY, &bytes);
    }
}

fn timezone() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|p| p.to_str()?.split("zoneinfo/").nth(1).map(str::to_string))
        .unwrap_or_else(|| "UTC".into())
}

/// Collects every Spotify entity in a response, schema-agnostic. Entities are leaves, except
/// `skip` (the entity being opened), whose children are collected instead.
pub fn items(v: &Value, skip: &str) -> Vec<Item> {
    fn walk(v: &Value, skip: &str, out: &mut Vec<Item>) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, skip, out)),
            Value::Object(o) => {
                if let Some(item) = as_item(o).filter(|i| i.uri != skip) {
                    if !out.iter().any(|i| i.uri == item.uri) {
                        out.push(item);
                    }
                    return;
                }
                o.values().for_each(|x| walk(x, skip, out));
            }
            _ => {}
        }
    }
    let mut out = vec![];
    walk(v, skip, &mut out);
    out
}

/// The paged list of a response: the object with an `items` array and a `totalCount`, as
/// (page length, total). Several match (e.g. a playlist's `content` and `members`): the longest page
/// wins, then the larger total. Does not descend into `items`.
fn paged(v: &Value) -> Option<(usize, usize)> {
    match v {
        Value::Object(o) => {
            let own = o.get("items").and_then(Value::as_array).zip(o.get("totalCount").and_then(Value::as_u64));
            let own = own.map(|(items, total)| (items.len(), total as usize));
            o.iter().filter(|(k, _)| *k != "items").filter_map(|(_, x)| paged(x)).chain(own).max()
        }
        Value::Array(a) => a.iter().filter_map(paged).max(),
        _ => None,
    }
}

fn root(v: &Value, uri: &str) -> Option<Item> {
    items(v, "").into_iter().find(|i| i.uri == uri)
}

fn as_item(o: &serde_json::Map<String, Value>) -> Option<Item> {
    let uri = o.get("uri")?.as_str()?;
    let kind = uri.strip_prefix("spotify:")?.split(':').next()?;
    if matches!(kind, "user" | "image" | "mosaic") {
        return None;
    }
    let name = o.get("name").or_else(|| o.get("profile")?.get("name"))?.as_str()?;
    let artists = o.get("artists").map(names).unwrap_or_default();
    let owner = ["/ownerV2/data/name", "/owner/display_name", "/publisher/name"]
        .iter()
        .find_map(|p| Value::Object(o.clone()).pointer(p)?.as_str().map(str::to_string))
        .unwrap_or_default();
    // Spotify's subtitles: "Playlist • owner", "Album • artists", artists alone for tracks.
    let label = match kind {
        "playlist" | "collection" => "Playlist",
        "album" => "Album",
        "artist" => "Artist",
        "show" => "Podcast",
        "episode" => "Episode",
        _ => "",
    };
    let who = if artists.is_empty() { owner } else { artists };
    let sub = match (kind, label, who.is_empty()) {
        ("track", ..) => who,
        (_, label, true) => label.to_string(),
        (_, label, false) => format!("{label} • {who}"),
    };
    let desc = o.get("description").and_then(Value::as_str).map(plain).unwrap_or_default();
    let image = image(&Value::Object(o.clone()));
    Some(Item { uri: uri.to_string(), name: name.to_string(), sub, image, desc })
}

/// Strips the HTML links and entities Spotify puts in playlist descriptions.
fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&#x27;", "'").replace("&#39;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">")
}

/// Cover url closest to 300px among all `{url, width}` image sources under `v`.
fn image(v: &Value) -> Option<String> {
    fn walk<'a>(v: &'a Value, out: &mut Vec<(&'a str, i64)>) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => {
                if let Some(url) = o.get("url").and_then(Value::as_str).filter(|u| u.starts_with("https://")) {
                    out.push((url, o.get("width").and_then(Value::as_i64).unwrap_or(300)));
                }
                o.values().for_each(|x| walk(x, out));
            }
            _ => {}
        }
    }
    let mut found = vec![];
    walk(v, &mut found);
    found.into_iter().min_by_key(|(_, w)| (w - 300).abs()).map(|(u, _)| u.to_string())
}

/// Artist names from either `[{name}]` (Web API) or `{items: [{profile: {name}}]}` (pathfinder).
fn names(v: &Value) -> String {
    let list = v.get("items").unwrap_or(v).as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|a| a.get("name").or_else(|| a.get("profile")?.get("name"))?.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether an error means the hash is wrong: 412 `Invalid query hash` (unknown or rotated) or 400
/// `GRAPHQL_UNKNOWN_OPERATION_NAME` (hash of another operation). Anything else, including 400 for
/// bad variables, keeps the hash: rescraping the bundle is ~46 downloads for nothing.
fn stale_hash(status: reqwest::StatusCode, body: &str) -> bool {
    use reqwest::StatusCode as S;
    status == S::PRECONDITION_FAILED || (status == S::BAD_REQUEST && body.contains("GRAPHQL_UNKNOWN_OPERATION_NAME"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_are_scanned_known_then_routes_then_rest() {
        let js = r#"({1:"xpui-routes-search",2:"vendor",3:"xpui-routes-album"})[e]||e)+"."+({1:"a1",2:"b2",3:"c3",4:"d4"})[e]+".js""#;
        let chunks = chunk_urls(js);
        assert_eq!(chunks[0], ("xpui-routes-search".into(), format!("{CDN}xpui-routes-search.a1.js")));
        assert_eq!(chunks[3].0, "4");
        let names = |v: &Vec<(String, String)>| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        let [first, routes, others] = passes(chunks, &["xpui-routes-album".into()]);
        assert_eq!(names(&first), ["xpui-routes-album"]);
        assert_eq!(names(&routes), ["xpui-routes-search"]);
        assert_eq!(names(&others), ["vendor", "4"]);
    }

    #[test]
    fn missing_ops_wait_before_a_new_scrape() {
        let mut h = Hashes::default();
        let found = |op: &str| Hashes { ops: HashMap::from([(op.to_string(), "x".into())]), ..Default::default() };
        // An incomplete scrape (a chunk failed) never marks anything missing.
        h.merge(found("home"), false, 100);
        assert!(h.missing.is_empty());
        h.merge(found("home"), true, 100);
        assert!(h.recently_missing("getAlbum", 100 + MISSING_TTL - 1));
        assert!(!h.recently_missing("getAlbum", 100 + MISSING_TTL));
        assert!(!h.recently_missing("home", 100));
        h.merge(found("getAlbum"), true, 200);
        assert!(!h.recently_missing("getAlbum", 200));
    }

    /// Live check against open.spotify.com: `cargo test -- --ignored scrape`.
    #[tokio::test]
    #[ignore]
    async fn scrape_finds_every_op() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let api = Api::new(Tokens { access_token: String::new(), client_token: String::new(), user: String::new() });
        let (h, complete) = api.scrape_hashes(&[]).await.unwrap();
        let missing: Vec<_> = OPS.iter().filter(|op| !h.ops.contains_key(**op)).collect();
        assert!(missing.is_empty(), "missing {missing:?} (complete: {complete})");
    }

    #[test]
    fn paged_picks_the_track_list() {
        // Shapes as returned by fetchPlaylist and getAlbum (measured 2026-11-19).
        let playlist = json!({"data": {"playlistV2": {
            "content": {"items": [{"itemV2": {"data": {"artists": {"items": [{}, {}, {}]}}}}, {}], "totalCount": 50},
            "members": {"items": [{}], "totalCount": 1}}}});
        assert_eq!(paged(&playlist), Some((2, 50)));
        let album = json!({"data": {"albumUnion": {
            "artists": {"items": [{}], "totalCount": 1},
            "tracksV2": {"items": [{}, {}, {}, {}, {}], "totalCount": 18},
            "discs": {"items": [{"tracks": {"totalCount": 18}}], "totalCount": 1}}}});
        assert_eq!(paged(&album), Some((5, 18)));
        assert_eq!(paged(&json!({"data": null})), None);
    }

    #[test]
    fn only_wrong_hash_errors_drop_the_hash() {
        let st = |c| reqwest::StatusCode::from_u16(c).unwrap();
        // Bodies as returned by pathfinder (measured 2026-11-19).
        let unknown_op = r#"{"errors":[{"message":"Unknown operation named \"getAlbum\"","extensions":{"code":"GRAPHQL_UNKNOWN_OPERATION_NAME"}}]}"#;
        let bad_var = r#"{"errors":[{"message":"missing variable `$uri`: for required GraphQL type `ID!`","extensions":{"name":"uri","code":"VALIDATION_INVALID_TYPE_VARIABLE"}}]}"#;
        assert!(stale_hash(st(412), r#""Invalid query hash""#));
        assert!(stale_hash(st(400), unknown_op));
        assert!(!stale_hash(st(400), bad_var));
        for c in [401, 403, 404, 408, 429, 500, 503] {
            assert!(!stale_hash(st(c), ""), "{c}");
        }
    }

    #[test]
    fn artist_list_context_opens_artist() {
        assert_eq!(Item::from_context("spotify:list:popular-release-segments-main-roles:artist_abc").uri, "spotify:artist:abc");
        assert_eq!(Item::from_context("spotify:playlist:p").uri, "spotify:playlist:p");
    }

    #[test]
    fn items_skip_root_and_stop_at_leaves() {
        let v = json!({"data": {"playlistV2": {"uri": "spotify:playlist:p", "name": "P",
            "ownerV2": {"data": {"uri": "spotify:user:u", "name": "me"}},
            "content": {"items": [
                {"itemV2": {"data": {"uri": "spotify:track:1", "name": "One",
                    "artists": {"items": [{"uri": "spotify:artist:a", "profile": {"name": "A"}}]}}}},
                {"itemV2": {"data": {"uri": "spotify:track:1", "name": "One"}}},
                {"itemV2": {"data": {"uri": "spotify:track:2", "name": "Two", "artists": [{"name": "B"}, {"name": "C"}]}}}
            ]}}}});
        let got = items(&v, "spotify:playlist:p");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].sub, "A");
        assert_eq!(got[1].sub, "B, C");
        assert_eq!(plain("<a href=\"x\">Mecna</a>, Ernia &amp; more"), "Mecna, Ernia & more");
        let pl = json!({"uri": "spotify:playlist:q", "name": "Q", "ownerV2": {"data": {"name": "Spotify"}}});
        assert_eq!(items(&pl, "")[0].sub, "Playlist • Spotify");
        assert_eq!(items(&v, "")[0].uri, "spotify:playlist:p");
        let cover = json!({"uri": "spotify:album:x", "name": "X", "coverArt": {"sources": [
            {"url": "https://i.scdn.co/64", "width": 64}, {"url": "https://i.scdn.co/300", "width": 300},
            {"url": "https://i.scdn.co/640", "width": 640}]}});
        assert_eq!(items(&cover, "")[0].image.as_deref(), Some("https://i.scdn.co/300"));
    }
}
