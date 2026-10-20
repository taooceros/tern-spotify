//! The catalog: search, the user's library, and what is inside a playlist,
//! album, artist or Liked Songs, read the way Spotify's own desktop and web
//! clients read them: "pathfinder" GraphQL persisted queries, authorized with
//! the session's login5 token and client token.
//!
//! The public Web API (api.spotify.com) isn't used: its rate limit counts per
//! client id, and librespot's client id is shared by every librespot-based
//! player, so it answers 429 most of the time.
//!
//! A persisted query is named by the SHA-256 of its text, which changes when
//! Spotify ships a new client. When pathfinder no longer knows a hash, the
//! catalog reads the current ones from the web player's public JavaScript
//! (open.spotify.com), keeps them in the cache dir and retries once. When
//! pathfinder fails anyway, each call falls back to what librespot reads over
//! the session itself: the search context, the rootlist, a context's tracks.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, Track};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};

const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const WEB_PLAYER: &str = "https://open.spotify.com/";
const BUNDLE_BASE: &str = "https://open.spotifycdn.com/cdn/build/web-player/";
/// Route chunks of the web player that hold queries the main bundle lacks.
const ROUTE_CHUNKS: &[&str] = &["xpui-routes-search"];
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// The hashes current when this was written; `refresh_hashes` replaces them.
const DEFAULT_HASHES: &[(&str, &str)] = &[
    ("searchDesktop", "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03"),
    ("fetchPlaylist", "21ea11a6ac75a5f3e01fd365d29c18e0a4a8b7dc09f89bbb5d73910190c6acaf"),
    ("getAlbum", "0b6ce3511e1b02e4a40fa95440f992acbe85080eb308af9194322d708fc1914f"),
    ("queryArtistOverview", "9f8134ef565e78621f1e1793555bd6633c5ac144ae0f89604ed3ae3f80b3c8e6"),
    ("fetchLibraryTracks", "087278b20b743578a6262c2b0b4bcd20d879c503cc359a2285baf083ef944240"),
    ("libraryV3", "390c78e5b951029bad359785e69b07b536a509c581cbcd0aded5e5067f187455"),
];

/// Rows read at most from one container (pages of `PAGE`).
const MAX_ROWS: u64 = 500;
const PAGE: u64 = 100;
/// Tracks named from metadata in a fallback list (one request each).
const FALLBACK_TRACKS: usize = 100;
/// Rows' thumbnails: the smallest size Spotify serves (60–64 px; artists
/// 160), drawn 40 px wide.
const THUMB_PX: u64 = 60;
/// A page's cover: the 300 px size, drawn 132 px wide.
const COVER_PX: u64 = 300;
/// Liked Songs' heart tile as Spotify's clients draw it: page and row sizes.
const LIKED_SONGS_COVER: &str = "https://misc.scdn.co/liked-songs/liked-songs-300.png";
const LIKED_SONGS_THUMB: &str = "https://misc.scdn.co/liked-songs/liked-songs-64.png";

/// Something to play or open.
#[derive(Debug, Serialize)]
pub struct Item {
    /// `track`, `album`, `artist`, `playlist` or `collection` (Liked Songs).
    pub kind: &'static str,
    pub uri: String,
    pub name: String,
    /// Artists, owner, year: one muted line.
    pub detail: String,
    /// The context to play a track in (its album, playlist, artist or Liked
    /// Songs), so playback continues with what follows it there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Its cover (an artist's photo, a playlist's picture), thumbnail-sized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// What an opened playlist, album, artist or Liked Songs shows above its
/// rows, as the desktop app's page header does.
#[derive(Debug, Serialize)]
pub struct Head {
    /// `album`, `artist`, `playlist` or `collection` (an artist's photo is
    /// drawn round).
    pub kind: &'static str,
    /// Above the name: `Album`, `Single`, `Playlist`, `Artist`.
    pub label: String,
    pub name: String,
    /// Under it: artists · year · songs, owner · songs, listeners.
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Listing {
    /// What the list is (`New Music Friday`, `范特西 — Jay Chou`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The context the whole list plays as, when it is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub items: Vec<Item>,
    /// Why the list is partial (pathfinder failed and a fallback answered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Head>,
}

// ── JSON helpers ────────────────────────────────────────────────────────

fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn number(v: &Value, pointer: &str) -> Option<u64> {
    v.pointer(pointer).and_then(|n| n.as_u64().or_else(|| n.as_str()?.parse().ok()))
}

fn array<'a>(v: &'a Value, pointer: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(pointer).and_then(Value::as_array).into_iter().flatten()
}

/// `artists.items[].profile.name`, joined.
fn artists(v: &Value) -> String {
    array(v, "/artists/items")
        .map(|a| text(a, "/profile/name"))
        .filter(|n| !n.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn join(a: String, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (_, true) => a,
        (true, false) => b.to_owned(),
        (false, false) => format!("{a} · {b}"),
    }
}

/// i.scdn.co album-cover ids start with their size's code; some queries (an
/// artist's top tracks) send sources without widths.
const COVER_CODES: &[(&str, u64)] = &[("ab67616d00004851", 64), ("ab67616d00001e02", 300), ("ab67616d0000b273", 640)];

fn source_width(s: &Value) -> u64 {
    number(s, "/width").filter(|w| *w > 0).unwrap_or_else(|| {
        let id = text(s, "/url").rsplit('/').next().unwrap_or_default();
        COVER_CODES.iter().find(|(code, _)| id.starts_with(code)).map_or(0, |(_, w)| *w)
    })
}

/// The URL of the smallest of `pointers`' first non-empty image `sources`
/// at least `px` wide, else of its widest.
fn image(v: &Value, pointers: &[&str], px: u64) -> Option<String> {
    pointers.iter().find_map(|p| {
        let sources: Vec<(&str, u64)> = array(v, p)
            .map(|s| (text(s, "/url"), source_width(s)))
            .filter(|(url, _)| !url.is_empty())
            .collect();
        sources
            .iter()
            .filter(|(_, w)| *w >= px)
            .min_by_key(|(_, w)| *w)
            .or_else(|| sources.iter().max_by_key(|(_, w)| *w))
            .map(|(url, _)| (*url).to_owned())
    })
}

const TRACK_IMAGE: &[&str] = &["/albumOfTrack/coverArt/sources"];
const ALBUM_IMAGE: &[&str] = &["/coverArt/sources"];
const ARTIST_IMAGE: &[&str] = &["/visuals/avatarImage/sources"];
const PLAYLIST_IMAGE: &[&str] = &["/images/items/0/sources", "/image/sources"];

/// `n` songs, `1 song`.
fn songs(n: u64) -> String {
    if n == 1 { "1 song".to_owned() } else { format!("{} songs", grouped(n)) }
}

/// `1234567` as `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A metadata image group's smallest cover at least `THUMB_PX` wide (the
/// fallbacks' rows).
fn metadata_image(images: &librespot_metadata::image::Images) -> Option<String> {
    let px = THUMB_PX as i32;
    images
        .iter()
        .filter(|i| i.width >= px)
        .min_by_key(|i| i.width)
        .or_else(|| images.iter().max_by_key(|i| i.width))
        .and_then(|i| i.id.to_base16().ok())
        .map(|hex| format!("https://i.scdn.co/image/{hex}"))
}

/// A Track object (search, playlist, album, library and artist rows all use
/// one shape, with small differences).
fn track(t: &Value, uri: Option<&str>, context: Option<&str>, with_album: bool) -> Option<Item> {
    let uri = uri.unwrap_or_else(|| text(t, "/uri"));
    let name = text(t, "/name");
    if !uri.starts_with("spotify:track:") || name.is_empty() {
        return None;
    }
    let album = if with_album { text(t, "/albumOfTrack/name") } else { "" };
    Some(Item {
        kind: "track",
        uri: uri.to_owned(),
        name: name.to_owned(),
        detail: join(artists(t), album),
        context: context.map(str::to_owned).or_else(|| {
            Some(text(t, "/albumOfTrack/uri").to_owned()).filter(|u| !u.is_empty())
        }),
        duration_ms: number(t, "/duration/totalMilliseconds").or_else(|| number(t, "/trackDuration/totalMilliseconds")),
        image: image(t, TRACK_IMAGE, THUMB_PX),
    })
}

fn album(a: &Value) -> Option<Item> {
    let uri = text(a, "/uri");
    let name = text(a, "/name");
    if uri.is_empty() || name.is_empty() {
        return None;
    }
    let year = number(a, "/date/year")
        .map(|y| y.to_string())
        .or_else(|| text(a, "/date/isoString").get(..4).map(str::to_owned))
        .unwrap_or_default();
    Some(Item {
        kind: "album",
        uri: uri.to_owned(),
        name: name.to_owned(),
        detail: join(artists(a), &year),
        context: None,
        duration_ms: None,
        image: image(a, ALBUM_IMAGE, THUMB_PX),
    })
}

fn artist(a: &Value) -> Option<Item> {
    let uri = text(a, "/uri");
    let name = text(a, "/profile/name");
    (!uri.is_empty() && !name.is_empty()).then(|| Item {
        kind: "artist",
        uri: uri.to_owned(),
        name: name.to_owned(),
        detail: "Artist".to_owned(),
        context: None,
        duration_ms: None,
        image: image(a, ARTIST_IMAGE, THUMB_PX),
    })
}

fn playlist(p: &Value) -> Option<Item> {
    let uri = text(p, "/uri");
    let name = text(p, "/name");
    (!uri.is_empty() && !name.is_empty()).then(|| Item {
        kind: "playlist",
        uri: uri.to_owned(),
        name: name.to_owned(),
        detail: join("Playlist".to_owned(), text(p, "/ownerV2/data/name")),
        context: None,
        duration_ms: None,
        image: image(p, PLAYLIST_IMAGE, THUMB_PX),
    })
}

fn liked_songs_uri(session: &Session) -> String {
    format!("spotify:user:{}:collection", session.username())
}

fn is_liked_songs(uri: &str) -> bool {
    uri == "spotify:collection:tracks" || (uri.starts_with("spotify:user:") && uri.ends_with(":collection"))
}

// ── Pathfinder ──────────────────────────────────────────────────────────

pub struct Catalog {
    http: reqwest::Client,
    hashes: Mutex<HashMap<String, String>>,
    hash_file: PathBuf,
    /// One refresh at a time; the others wait for it and use its result.
    refreshing: tokio::sync::Mutex<()>,
    /// Until when pathfinder asked not to be called (a 429's Retry-After).
    blocked_until: Mutex<Option<Instant>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Catalog {
    pub fn new(cache_dir: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .expect("an HTTP client");
        let hash_file = cache_dir.join("pathfinder.json");
        let mut hashes: HashMap<String, String> =
            DEFAULT_HASHES.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        if let Ok(saved) = std::fs::read_to_string(&hash_file)
            && let Ok(saved) = serde_json::from_str::<HashMap<String, String>>(&saved)
        {
            hashes.extend(saved);
        }
        Catalog {
            http,
            hashes: Mutex::new(hashes),
            hash_file,
            refreshing: tokio::sync::Mutex::new(()),
            blocked_until: Mutex::new(None),
        }
    }

    /// Runs persisted query `op`; answers its `data`.
    async fn query(&self, session: &Session, op: &str, variables: Value) -> Result<Value, String> {
        if let Some(until) = *lock(&self.blocked_until) {
            let left = until.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                return Err(format!("Spotify asked to wait {}s", left.as_secs() + 1));
            }
        }
        let token = session.login5().auth_token().await.map_err(|e| format!("no access token: {e}"))?;
        let client_token = session.spclient().client_token().await.map_err(|e| format!("no client token: {e}"))?;
        for attempt in 0..2 {
            let hash = lock(&self.hashes).get(op).cloned().unwrap_or_default();
            let body = json!({
                "variables": variables,
                "operationName": op,
                "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
            });
            let response = self
                .http
                .post(PATHFINDER)
                .bearer_auth(&token.access_token)
                .header("client-token", &client_token)
                .header("app-platform", "WebPlayer")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("pathfinder: {e}"))?;
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.trim().parse().ok())
                    .unwrap_or(30);
                *lock(&self.blocked_until) = Some(Instant::now() + Duration::from_secs(wait));
                return Err(format!("Spotify answered 429, retry after {wait}s"));
            }
            let text = response.text().await.map_err(|e| format!("pathfinder: {e}"))?;
            // An unknown hash answers 412 (or a PersistedQueryNotFound error).
            if attempt == 0
                && (status == reqwest::StatusCode::PRECONDITION_FAILED || text.contains("PersistedQueryNotFound"))
            {
                log::info!("pathfinder no longer knows {op} {hash}; reading the web player's queries");
                self.refresh_hashes(&hash, op).await?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("pathfinder {op}: {status}"));
            }
            let mut reply: Value = serde_json::from_str(&text).map_err(|e| format!("pathfinder {op}: {e}"))?;
            let data = reply.get_mut("data").map(Value::take).unwrap_or(Value::Null);
            if data.is_null() {
                let why = reply.pointer("/errors/0/message").and_then(Value::as_str).unwrap_or("no data");
                return Err(format!("pathfinder {op}: {why}"));
            }
            return Ok(data);
        }
        Err(format!("pathfinder doesn't know the {op} query"))
    }

    /// Reads the persisted-query hashes from the web player's bundles, unless
    /// another call did so since `stale` was read for `op`.
    async fn refresh_hashes(&self, stale: &str, op: &str) -> Result<(), String> {
        let _guard = self.refreshing.lock().await;
        if lock(&self.hashes).get(op).is_some_and(|h| h != stale) {
            return Ok(());
        }
        let found = self.read_web_player().await?;
        log::info!("read {} persisted queries from the web player", found.len());
        let mut hashes = lock(&self.hashes);
        hashes.extend(found);
        if let Err(e) = std::fs::write(&self.hash_file, serde_json::to_string_pretty(&*hashes).unwrap_or_default()) {
            log::warn!("cannot store {}: {e}", self.hash_file.display());
        }
        if hashes.get(op).is_some_and(|h| h != stale) {
            Ok(())
        } else {
            Err(format!("the web player has no new {op} query"))
        }
    }

    async fn fetch_text(&self, url: &str) -> Result<String, String> {
        let r = self.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("{url}: {}", r.status()));
        }
        r.text().await.map_err(|e| format!("{url}: {e}"))
    }

    async fn read_web_player(&self) -> Result<HashMap<String, String>, String> {
        let page = self.fetch_text(WEB_PLAYER).await?;
        let bundle = Regex::new(r#"https://open\.spotifycdn\.com/cdn/build/web-player/[\w~.\-]+\.js"#).expect("regex");
        let query = Regex::new(r#""(\w+)","(?:query|mutation)","([0-9a-f]{64})""#).expect("regex");
        let mut found = HashMap::new();
        let mut sources = Vec::new();
        for url in bundle.find_iter(&page).map(|m| m.as_str().to_owned()) {
            if let Ok(js) = self.fetch_text(&url).await {
                sources.push(js);
            }
        }
        // Route chunks load on demand: their file name is `<name>.<8 hex>.js`,
        // the hex looked up by chunk id in the main bundle's maps.
        let mut chunks = Vec::new();
        for js in &sources {
            for name in ROUTE_CHUNKS {
                let id = Regex::new(&format!(r#"(\d+):"{}""#, regex::escape(name))).expect("regex");
                for id in id.captures_iter(js).map(|c| c[1].to_owned()) {
                    let hex = Regex::new(&format!(r#"[,{{]{id}:"([0-9a-f]{{8}})""#)).expect("regex");
                    for h in hex.captures_iter(js) {
                        chunks.push(format!("{BUNDLE_BASE}{name}.{}.js", &h[1]));
                    }
                }
            }
        }
        chunks.sort();
        chunks.dedup();
        for url in chunks {
            if let Ok(js) = self.fetch_text(&url).await {
                sources.push(js);
            }
        }
        for js in &sources {
            for c in query.captures_iter(js) {
                found.insert(c[1].to_owned(), c[2].to_owned());
            }
        }
        if found.is_empty() {
            return Err("the web player's bundles hold no queries".to_owned());
        }
        Ok(found)
    }

    // ── Calls ───────────────────────────────────────────────────────────

    pub async fn search(&self, session: &Session, q: &str) -> Result<Listing, String> {
        let q = q.trim();
        let title = Some(format!("Search: {q}"));
        if q.is_empty() {
            return Ok(Listing { title, ..Listing::default() });
        }
        let vars = json!({
            "searchTerm": q, "offset": 0, "limit": 10, "numberOfTopResults": 5,
            "includeAudiobooks": false, "includeArtistHasConcertsField": false,
            "includePreReleases": false, "includeAlbumPreReleases": false,
            "includeAuthors": false, "includeEpisodeContentRatingsV2": false,
        });
        match self.query(session, "searchDesktop", vars).await {
            Ok(data) => {
                let s = &data["searchV2"];
                let mut items: Vec<Item> =
                    array(s, "/tracksV2/items").filter_map(|t| track(&t["item"]["data"], None, None, true)).collect();
                items.extend(array(s, "/albumsV2/items").filter_map(|a| album(&a["data"])).take(6));
                items.extend(array(s, "/artists/items").filter_map(|a| artist(&a["data"])).take(5));
                items.extend(array(s, "/playlists/items").filter_map(|p| playlist(&p["data"])).take(6));
                Ok(Listing { title, items, ..Listing::default() })
            }
            Err(e) => {
                log::warn!("search {q:?}: {e}; falling back to the search context");
                let query: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
                let mut listing = context_tracks(session, &format!("spotify:search:{query}"), false)
                    .await
                    .map_err(|f| format!("{e}; fallback: {f}"))?;
                listing.title = title;
                listing.note = Some(format!("{e}: showing tracks only"));
                Ok(listing)
            }
        }
    }

    /// Liked Songs, then the library's playlists, albums and artists, most
    /// recently used first (as the desktop client's sidebar).
    pub async fn library(&self, session: &Session) -> Result<Listing, String> {
        let title = Some("Your Library".to_owned());
        let mut items = Vec::new();
        let mut offset = 0;
        let result: Result<(), String> = async {
            loop {
                let vars = json!({
                    "filters": [], "order": null, "textFilter": "", "features": ["LIKED_SONGS"],
                    "limit": 50, "offset": offset, "flatten": true, "expandedFolders": [],
                    "folderUri": null, "includeFoldersWhenFlattening": false,
                });
                let data = self.query(session, "libraryV3", vars).await?;
                let page = &data["me"]["libraryV3"];
                let rows: Vec<&Value> = array(page, "/items").collect();
                for row in &rows {
                    let d = &row["item"]["data"];
                    let item = match text(d, "/__typename") {
                        "PseudoPlaylist" if is_liked_songs(text(d, "/uri")) => Some(Item {
                            kind: "collection",
                            uri: liked_songs_uri(session),
                            name: text(d, "/name").to_owned(),
                            detail: number(d, "/count").map(songs).unwrap_or_default(),
                            context: None,
                            duration_ms: None,
                            image: image(d, PLAYLIST_IMAGE, THUMB_PX),
                        }),
                        "Playlist" => playlist(d),
                        "Album" => album(d),
                        "Artist" => artist(d),
                        _ => None,
                    };
                    items.extend(item);
                }
                offset += rows.len() as u64;
                if rows.is_empty() || offset >= number(page, "/totalCount").unwrap_or(0) || offset >= MAX_ROWS {
                    return Ok(());
                }
            }
        }
        .await;
        match result {
            Ok(()) => Ok(Listing { title, items, ..Listing::default() }),
            Err(e) if items.is_empty() => {
                log::warn!("library: {e}; falling back to the rootlist");
                let mut items = vec![Item {
                    kind: "collection",
                    uri: liked_songs_uri(session),
                    name: "Liked Songs".to_owned(),
                    detail: String::new(),
                    context: None,
                    duration_ms: None,
                    image: Some(LIKED_SONGS_THUMB.to_owned()),
                }];
                let note = match rootlist(session).await {
                    Ok(found) => {
                        items.extend(found);
                        Some(format!("{e}: showing playlists only"))
                    }
                    Err(r) => Some(format!("{e}; rootlist: {r}: showing Liked Songs only")),
                };
                Ok(Listing { title, items, note, ..Listing::default() })
            }
            Err(e) => Ok(Listing { title, items, note: Some(format!("{e}: the list is cut short")), ..Listing::default() }),
        }
    }

    /// What is inside `uri`: a playlist's, album's or Liked Songs' tracks,
    /// or an artist's top tracks and releases.
    pub async fn open(&self, session: &Session, uri: &str) -> Result<Listing, String> {
        let result = if is_liked_songs(uri) {
            self.liked_songs(session).await
        } else if uri.starts_with("spotify:playlist:") {
            self.playlist(session, uri).await
        } else if uri.starts_with("spotify:album:") {
            self.album(session, uri).await
        } else if uri.starts_with("spotify:artist:") {
            self.artist(session, uri).await
        } else {
            return Err(format!("cannot open {uri}"));
        };
        match result {
            Ok(listing) => Ok(listing),
            Err(e) => {
                log::warn!("open {uri}: {e}; falling back to the context");
                let context = if is_liked_songs(uri) { liked_songs_uri(session) } else { uri.to_owned() };
                let mut listing = context_tracks(session, &context, true).await.map_err(|f| format!("{e}; fallback: {f}"))?;
                listing.note = Some(format!("{e}: names from track metadata"));
                Ok(listing)
            }
        }
    }

    /// Pages of a list until `MAX_ROWS`; `page` reads one at an offset and
    /// answers its rows and the total.
    async fn pages<'a, F, Fut>(&'a self, mut page: F) -> Result<(Value, Vec<Value>), String>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<(Value, Vec<Value>, u64), String>> + 'a,
    {
        let mut rows = Vec::new();
        let mut first = Value::Null;
        let mut offset = 0;
        loop {
            let (data, got, total) = page(offset).await?;
            if first.is_null() {
                first = data;
            }
            let n = got.len() as u64;
            rows.extend(got);
            offset += n;
            if n == 0 || offset >= total || offset >= MAX_ROWS {
                return Ok((first, rows));
            }
        }
    }

    async fn liked_songs(&self, session: &Session) -> Result<Listing, String> {
        let context = liked_songs_uri(session);
        let (head, rows) = self
            .pages(|offset| async move {
                let data = self.query(session, "fetchLibraryTracks", json!({ "offset": offset, "limit": PAGE })).await?;
                let page = &data["me"]["library"]["tracks"];
                let rows: Vec<Value> = array(page, "/items").cloned().collect();
                let total = number(page, "/totalCount").unwrap_or(0);
                Ok((json!({ "totalCount": total }), rows, total))
            })
            .await?;
        let items = rows
            .iter()
            .filter_map(|r| track(&r["track"]["data"], Some(text(r, "/track/_uri")), Some(&context), true))
            .collect();
        let head = Head {
            kind: "collection",
            label: "Playlist".to_owned(),
            name: "Liked Songs".to_owned(),
            detail: number(&head, "/totalCount").map(songs).unwrap_or_default(),
            image: Some(LIKED_SONGS_COVER.to_owned()),
        };
        Ok(Listing {
            title: Some("Liked Songs".to_owned()),
            context: Some(context),
            items,
            note: None,
            head: Some(head),
        })
    }

    async fn playlist(&self, session: &Session, uri: &str) -> Result<Listing, String> {
        let (head, rows) = self
            .pages(|offset| async move {
                let vars = json!({ "uri": uri, "offset": offset, "limit": PAGE, "enableWatchFeedEntrypoint": false });
                let data = self.query(session, "fetchPlaylist", vars).await?;
                let p = &data["playlistV2"];
                if text(p, "/__typename") != "Playlist" {
                    return Err(format!("{uri} is {}", text(p, "/__typename")));
                }
                let rows: Vec<Value> = array(p, "/content/items").cloned().collect();
                let total = number(p, "/content/totalCount").unwrap_or(0);
                // The page's own fields, its rows' count in place of its rows.
                let mut head = p.clone();
                if let Some(obj) = head.as_object_mut() {
                    obj.insert("content".to_owned(), json!({ "totalCount": total }));
                }
                Ok((head, rows, total))
            })
            .await?;
        let items = rows.iter().filter_map(|r| track(&r["itemV2"]["data"], None, Some(uri), true)).collect();
        let name = text(&head, "/name").to_owned();
        let owner = text(&head, "/ownerV2/data/name");
        let head = Head {
            kind: "playlist",
            label: "Playlist".to_owned(),
            detail: join(owner.to_owned(), &number(&head, "/content/totalCount").map(songs).unwrap_or_default()),
            image: image(&head, PLAYLIST_IMAGE, COVER_PX),
            name: name.clone(),
        };
        Ok(Listing {
            title: Some(join(name, owner)),
            context: Some(uri.to_owned()),
            items,
            note: None,
            head: Some(head),
        })
    }

    async fn album(&self, session: &Session, uri: &str) -> Result<Listing, String> {
        let (head, rows) = self
            .pages(|offset| async move {
                let vars = json!({ "uri": uri, "locale": "", "offset": offset, "limit": PAGE });
                let data = self.query(session, "getAlbum", vars).await?;
                let a = &data["albumUnion"];
                if text(a, "/__typename") != "Album" {
                    return Err(format!("{uri} is {}", text(a, "/__typename")));
                }
                let rows: Vec<Value> = array(a, "/tracksV2/items").cloned().collect();
                let total = number(a, "/tracksV2/totalCount").unwrap_or(0);
                let mut head = a.clone();
                if let Some(obj) = head.as_object_mut() {
                    obj.insert("tracksV2".to_owned(), json!({ "totalCount": total }));
                    obj.remove("moreAlbumsByArtist");
                }
                Ok((head, rows, total))
            })
            .await?;
        // Album rows leave the album out: it's the title.
        let items = rows.iter().filter_map(|r| track(&r["track"], None, Some(uri), false)).collect();
        let year = text(&head, "/date/isoString").get(..4).unwrap_or_default();
        let name = text(&head, "/name").to_owned();
        let by = artists(&head);
        let kind = text(&head, "/type");
        let head_line = Head {
            kind: "album",
            label: if kind.is_empty() { "Album".to_owned() } else { title_case(kind) },
            detail: join(
                join(by.clone(), year),
                &number(&head, "/tracksV2/totalCount").map(songs).unwrap_or_default(),
            ),
            image: image(&head, ALBUM_IMAGE, COVER_PX),
            name: name.clone(),
        };
        Ok(Listing {
            title: Some(join(format!("{name} — {by}"), year)),
            context: Some(uri.to_owned()),
            items,
            note: None,
            head: Some(head_line),
        })
    }

    async fn artist(&self, session: &Session, uri: &str) -> Result<Listing, String> {
        let data = self
            .query(session, "queryArtistOverview", json!({ "uri": uri, "locale": "", "includePrerelease": false }))
            .await?;
        let a = &data["artistUnion"];
        if text(a, "/__typename") != "Artist" {
            return Err(format!("{uri} is {}", text(a, "/__typename")));
        }
        let d = &a["discography"];
        // Top tracks play in the artist's context, as the desktop client's do.
        let mut items: Vec<Item> =
            array(d, "/topTracks/items").filter_map(|t| track(&t["track"], None, Some(uri), true)).collect();
        let mut seen = std::collections::HashSet::new();
        let releases = array(d, "/popularReleasesAlbums/items")
            .chain(array(d, "/albums/items").flat_map(|g| array(g, "/releases/items")))
            .chain(array(d, "/singles/items").flat_map(|g| array(g, "/releases/items")));
        for r in releases {
            if seen.insert(text(r, "/uri").to_owned())
                && let Some(mut item) = album(r)
            {
                // The row already says "Album ›"; name singles and compilations.
                let kind = text(r, "/type");
                if !kind.is_empty() && kind != "ALBUM" {
                    item.detail = join(title_case(kind), &item.detail);
                }
                items.push(item);
            }
        }
        let name = text(a, "/profile/name").to_owned();
        let head = Head {
            kind: "artist",
            label: "Artist".to_owned(),
            detail: number(a, "/stats/monthlyListeners")
                .map(|n| format!("{} monthly listeners", grouped(n)))
                .unwrap_or_default(),
            image: image(a, ARTIST_IMAGE, COVER_PX),
            name: name.clone(),
        };
        Ok(Listing {
            title: Some(name),
            context: Some(uri.to_owned()),
            items,
            note: None,
            head: Some(head),
        })
    }

    /// Whether each of `uris` is in the user's library (Liked Songs for a
    /// track; saved for an album, playlist or artist).
    pub async fn saved(&self, session: &Session, uris: &[String]) -> Result<Vec<bool>, String> {
        let data = self.query(session, "areEntitiesInLibrary", json!({ "uris": uris })).await?;
        let found: Vec<bool> = array(&data, "/lookup").map(|e| e.pointer("/data/saved").and_then(Value::as_bool).unwrap_or(false)).collect();
        if found.len() != uris.len() {
            return Err(format!("areEntitiesInLibrary answered {} of {} uris", found.len(), uris.len()));
        }
        Ok(found)
    }

    /// Adds `uri` to the library (a track: to Liked Songs) or removes it.
    pub async fn set_saved(&self, session: &Session, uri: &str, on: bool) -> Result<(), String> {
        let (op, vars) = if on {
            ("addToLibrary", json!({ "libraryItemUris": [uri], "interactionId": null }))
        } else {
            ("removeFromLibrary", json!({ "libraryItemUris": [uri] }))
        };
        self.query(session, op, vars).await.map(|_| ())
    }
}

/// A track's lyrics; `lines` is empty when Spotify has none.
#[derive(Debug, Serialize)]
pub struct Lyrics {
    /// Whether lines carry their start times (`LINE_SYNCED`).
    pub synced: bool,
    pub provider: String,
    pub lines: Vec<LyricLine>,
}

#[derive(Debug, Serialize)]
pub struct LyricLine {
    pub ms: u64,
    pub text: String,
}

pub async fn lyrics(session: &Session, uri: &str) -> Result<Lyrics, String> {
    let id = match SpotifyUri::from_uri(uri).map_err(|e| e.to_string())? {
        SpotifyUri::Track { id } => id,
        _ => return Err(format!("{uri} is not a track")),
    };
    match librespot_metadata::lyrics::Lyrics::get(session, &id).await {
        Ok(l) => Ok(Lyrics {
            synced: matches!(l.lyrics.sync_type, librespot_metadata::lyrics::SyncType::LineSynced),
            provider: l.lyrics.provider_display_name,
            lines: l
                .lyrics
                .lines
                .into_iter()
                .map(|line| LyricLine { ms: line.start_time_ms.parse().unwrap_or(0), text: line.words })
                .collect(),
        }),
        Err(e) if e.kind == librespot_core::error::ErrorKind::NotFound => {
            Ok(Lyrics { synced: false, provider: String::new(), lines: Vec::new() })
        }
        Err(e) => Err(format!("lyrics: {e}")),
    }
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut chars = lower.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

// ── Fallbacks over the session ─────────────────────────────────────────

/// A context's tracks (`spotify:search:…`, a playlist, an album…), named from
/// track metadata. `in_context`: rows play inside it.
async fn context_tracks(session: &Session, uri: &str, in_context: bool) -> Result<Listing, String> {
    let context = session.spclient().get_context(uri).await.map_err(|e| e.to_string())?;
    let uris: Vec<SpotifyUri> = context
        .pages
        .iter()
        .flat_map(|page| page.tracks.iter())
        .filter_map(|t| t.uri.as_deref())
        .filter(|u| u.starts_with("spotify:track:"))
        .filter_map(|u| SpotifyUri::from_uri(u).ok())
        .take(FALLBACK_TRACKS)
        .collect();
    let tracks = futures_util::future::join_all(uris.iter().map(|u| Track::get(session, u))).await;
    let mut items = Vec::with_capacity(uris.len());
    for (id, t) in uris.iter().zip(tracks) {
        let Ok(t) = t else { continue };
        let names = t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
        items.push(Item {
            kind: "track",
            uri: id.to_uri().unwrap_or_default(),
            name: t.name,
            detail: join(names, &t.album.name),
            context: in_context.then(|| uri.to_owned()),
            duration_ms: u64::try_from(t.duration).ok(),
            image: metadata_image(&t.album.covers),
        });
    }
    Ok(Listing { title: None, context: in_context.then(|| uri.to_owned()), items, note: None, head: None })
}

/// The user's playlists from their rootlist (the list Spotify's clients
/// keep), named from playlist metadata.
async fn rootlist(session: &Session) -> Result<Vec<Item>, String> {
    use librespot_metadata::playlist::Playlist;
    use protobuf::Message;

    let bytes = session.spclient().get_rootlist(0, None).await.map_err(|e| e.to_string())?;
    let msg = <Playlist as Metadata>::Message::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
    // Raw item URIs: the list also holds folder markers
    // (`spotify:start-group:…`), which librespot's own conversion rejects.
    let uris: Vec<SpotifyUri> = msg
        .contents
        .items
        .iter()
        .filter_map(|item| item.uri.as_deref())
        .filter(|u| u.contains(":playlist:"))
        .filter_map(|u| SpotifyUri::from_uri(u).ok())
        .take(200)
        .collect();
    let lists = futures_util::future::join_all(uris.iter().map(|u| Playlist::get(session, u))).await;
    let mut out = Vec::with_capacity(uris.len());
    for (uri, list) in uris.iter().zip(lists) {
        let Ok(list) = list else { continue };
        out.push(Item {
            kind: "playlist",
            uri: uri.to_uri().unwrap_or_default(),
            name: list.name().to_owned(),
            detail: "Playlist".to_owned(),
            context: None,
            duration_ms: None,
            // Playlist metadata names no cover URL: the row draws its placeholder.
            image: None,
        });
    }
    Ok(out)
}
