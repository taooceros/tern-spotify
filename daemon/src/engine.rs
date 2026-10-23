//! The engine: the account's librespot session (sign-in, the catalog, lyrics,
//! and pairing the player) and the Soloist player that plays the audio
//! (`soloist/`), behind the commands and state the API reports.
//!
//! A supervisor task owns the session lifecycle: it connects with fresh OAuth
//! credentials or the stored ones, watches the connection, and reconnects
//! (with backoff) unless the user signed out. Soloist keeps its own session
//! once paired, so playback doesn't depend on this one staying up.

use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use librespot_core::{Session, SessionConfig, authentication::Credentials, cache::Cache, error::ErrorKind};
use librespot_oauth::OAuthClientBuilder;
use serde::Serialize;
use tokio::sync::Notify;

use crate::soloist::{Player, PlayerStatus, Quality};

/// The OAuth redirect librespot's own binary uses; Spotify accepts it for the
/// keymaster client id.
const OAUTH_ADDR: &str = "127.0.0.1:5588";

/// The scopes librespot's binary requests (the desktop client's).
const OAUTH_SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-modify",
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "ugc-image-upload",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify",
    "user-modify-playback-state",
    "user-modify-private",
    "user-personalized",
    "user-read-birthdate",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-play-history",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

const SIGNED_IN_PAGE: &str = "Signed in to Spotify. You can close this tab and go back to Tern.";

const FIRST_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How often the supervisor checks the session's connection to Spotify.
const SESSION_CHECK: Duration = Duration::from_secs(1);

pub struct EngineConfig {
    pub cache_dir: PathBuf,
    /// Where Soloist, its key and its data live (see `soloist::prefs::Layout`).
    pub soloist_dir: PathBuf,
    pub device_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No credentials: the user has to sign in.
    SignedOut,
    /// The OAuth page is open in the browser; waiting for its redirect.
    SigningIn,
    /// Connecting to Spotify with credentials.
    Connecting,
    /// Signed in: the catalog answers, and the player can be paired.
    Ready,
    /// Connecting failed; retrying after a backoff.
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrackInfo {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    /// The artists' URIs, in `artists`' order (empty for episodes and local files).
    pub artist_uris: Vec<String>,
    pub album: String,
    pub album_uri: Option<String>,
    pub cover: Option<String>,
    pub duration_ms: u32,
}

/// What the API reports at `/status`.
#[derive(Debug, Serialize)]
pub struct Status {
    /// The account's sign-in.
    pub phase: Phase,
    pub error: Option<String>,
    pub user: Option<String>,
    /// The account's profile name, once read (the `user` id is opaque).
    pub display_name: Option<String>,
    pub device: String,
    /// The player (Soloist): whether it runs, why not, quality and format.
    pub player: PlayerStatus,
    /// Whether this device is the user's active Spotify Connect device.
    pub active: bool,
    pub playing: bool,
    pub loading: bool,
    pub track: Option<TrackInfo>,
    pub position_ms: u32,
    /// 0–100.
    pub volume: u8,
    pub shuffle: bool,
    /// `off`, `context` or `track`.
    pub repeat_mode: &'static str,
    pub version: &'static str,
}

struct Account {
    phase: Phase,
    error: Option<String>,
    user: Option<String>,
    display_name: Option<String>,
}

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

struct Inner {
    cache_dir: PathBuf,
    device_name: String,
    session_config: SessionConfig,
    cache: Cache,
    session: Mutex<Option<Session>>,
    account: Mutex<Account>,
    player: Player,
    /// Credentials from a finished OAuth sign-in, taken by the supervisor.
    fresh: Mutex<Option<Credentials>>,
    /// Set by sign-out so the supervisor doesn't reconnect.
    signing_out: AtomicBool,
    quitting: AtomicBool,
    /// Wakes the supervisor: credentials arrived, the user asked to retry,
    /// or signed out.
    wake: Notify,
    quit: Notify,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stable device id for the account's session, made once and kept in the
/// cache dir (the stored credentials were issued to it).
fn device_id(cache_dir: &std::path::Path) -> String {
    let path = cache_dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return id.to_owned();
        }
    }
    use std::hash::{BuildHasher, Hasher};
    let mut id = String::with_capacity(40);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    for part in 0..3u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u64(part);
        h.write_u32(std::process::id());
        id.push_str(&format!("{:016x}", h.finish()));
    }
    id.truncate(40);
    if let Err(e) = std::fs::write(&path, &id) {
        log::warn!("cannot store the device id in {}: {e}", path.display());
    }
    id
}

impl Engine {
    pub fn start(cfg: EngineConfig) -> Result<Self, String> {
        std::fs::create_dir_all(&cfg.cache_dir)
            .map_err(|e| format!("cannot create {}: {e}", cfg.cache_dir.display()))?;
        // The librespot player's audio files: nothing reads them any more.
        let old_audio = cfg.cache_dir.join("audio");
        if old_audio.is_dir() {
            match std::fs::remove_dir_all(&old_audio) {
                Ok(()) => log::info!("removed the old player's audio cache {}", old_audio.display()),
                Err(e) => log::warn!("cannot remove {}: {e}", old_audio.display()),
            }
        }
        let cache = Cache::new(Some(&cfg.cache_dir), None, None, None)
            .map_err(|e| format!("cannot open the cache in {}: {e}", cfg.cache_dir.display()))?;
        let session_config = SessionConfig { device_id: device_id(&cfg.cache_dir), ..SessionConfig::default() };
        let player = Player::start(cfg.soloist_dir, cfg.device_name.clone())?;
        let inner = Arc::new(Inner {
            cache_dir: cfg.cache_dir,
            device_name: cfg.device_name,
            session_config,
            cache,
            session: Mutex::new(None),
            account: Mutex::new(Account { phase: Phase::Connecting, error: None, user: None, display_name: None }),
            player,
            fresh: Mutex::new(None),
            signing_out: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            wake: Notify::new(),
            quit: Notify::new(),
        });
        tokio::spawn(inner.clone().supervise());
        Ok(Engine(inner))
    }

    pub fn status(&self) -> Status {
        let account = lock(&self.0.account);
        let p = self.0.player.snapshot();
        Status {
            phase: account.phase,
            error: account.error.clone(),
            user: account.user.clone(),
            display_name: account.display_name.clone(),
            device: self.0.device_name.clone(),
            player: p.player,
            active: p.active,
            playing: p.playing,
            loading: p.loading,
            track: p.track,
            position_ms: p.position_ms,
            volume: p.volume,
            shuffle: p.shuffle,
            repeat_mode: p.repeat_mode,
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// The current session, once signed in.
    pub fn session(&self) -> Result<Session, String> {
        if lock(&self.0.account).phase != Phase::Ready {
            return Err("not signed in to Spotify".to_owned());
        }
        lock(&self.0.session).clone().ok_or_else(|| "not signed in to Spotify".to_owned())
    }

    /// The player, for a command: refused while signed out.
    fn player(&self) -> Result<&Player, String> {
        self.session()?;
        Ok(&self.0.player)
    }

    /// Starts the OAuth sign-in: opens Spotify's page in the browser and waits
    /// for its redirect on a blocking thread.
    pub fn login(&self) -> Result<(), String> {
        {
            let mut account = lock(&self.0.account);
            match account.phase {
                Phase::SigningIn => return Ok(()),
                Phase::Ready => return Err("already signed in; sign out first".to_owned()),
                _ => {}
            }
            account.phase = Phase::SigningIn;
            account.error = None;
        }
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let redirect = format!("http://{OAUTH_ADDR}/login");
            let token = OAuthClientBuilder::new(&inner.session_config.client_id, &redirect, OAUTH_SCOPES.to_vec())
                .open_in_browser()
                .with_custom_message(SIGNED_IN_PAGE)
                .build()
                .and_then(|client| client.get_access_token());
            let mut account = lock(&inner.account);
            match token {
                Ok(token) => {
                    *lock(&inner.fresh) = Some(Credentials::with_access_token(token.access_token));
                    account.phase = Phase::Connecting;
                    drop(account);
                    inner.wake.notify_one();
                }
                Err(e) => {
                    log::warn!("sign-in failed: {e}");
                    account.phase = Phase::SignedOut;
                    account.error = Some(format!("Sign-in did not finish: {e}"));
                }
            }
        });
        Ok(())
    }

    /// Ends a pending sign-in: the redirect listener takes one request, so a
    /// request without a code makes it return.
    pub fn cancel_login(&self) {
        if lock(&self.0.account).phase != Phase::SigningIn {
            return;
        }
        let addr: SocketAddr = OAUTH_ADDR.parse().expect("valid address");
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            let _ = stream.write_all(b"GET /login?cancelled=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        }
    }

    /// Signs out of both: forgets the stored credentials, ends the session,
    /// and signs Soloist out (it stops, and its session is deleted).
    pub async fn logout(&self) {
        self.0.signing_out.store(true, Ordering::SeqCst);
        let creds = self.0.cache_dir.join("credentials.json");
        if let Err(e) = std::fs::remove_file(&creds)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot remove {}: {e}", creds.display());
        }
        lock(&self.0.fresh).take();
        {
            let mut account = lock(&self.0.account);
            account.phase = Phase::SignedOut;
            account.error = None;
            account.user = None;
            account.display_name = None;
        }
        self.0.player.forget().await;
        if let Some(session) = lock(&self.0.session).take() {
            session.shutdown();
        }
        self.0.wake.notify_one();
    }

    /// Plays or pauses here; when another device plays, takes its playback over.
    pub async fn toggle(&self) -> Result<(), String> {
        self.player()?.toggle().await
    }

    pub async fn play(&self) -> Result<(), String> {
        self.player()?.play().await
    }

    pub fn pause(&self) -> Result<(), String> {
        self.player()?.pause()
    }

    pub fn next(&self) -> Result<(), String> {
        self.player()?.next()
    }

    pub fn prev(&self) -> Result<(), String> {
        self.player()?.prev()
    }

    /// Takes the user's playback over from the device playing it now.
    pub async fn transfer(&self) -> Result<(), String> {
        self.player()?.activate().await
    }

    pub fn seek(&self, ms: u32) -> Result<(), String> {
        self.player()?.seek(ms)
    }

    /// Sets the volume (0–100); while another device plays, the volume this
    /// one takes when it plays.
    pub fn volume(&self, percent: u8) -> Result<(), String> {
        self.player()?.volume(percent)
    }

    /// Shuffle on or off; while inactive, kept for the next `load`.
    pub fn shuffle(&self, on: bool) -> Result<(), String> {
        self.player()?.shuffle(on)
    }

    /// Repeat off, the context, or the track; kept for the next `load` while
    /// inactive, as `shuffle` is.
    pub fn repeat(&self, mode: &str) -> Result<(), String> {
        self.player()?.repeat(mode)
    }

    /// Plays `uri` here: a track alone, or a context (album, playlist, artist,
    /// liked songs) from its first track or from `track` in it. A context
    /// keeps the shuffle and repeat set now, as in the desktop app.
    pub async fn load(&self, uri: &str, track: Option<&str>) -> Result<(), String> {
        self.player()?.load(uri, track).await
    }

    /// Adds a track to the queue.
    pub fn enqueue(&self, uri: &str) -> Result<(), String> {
        self.player()?.enqueue(uri)
    }

    pub async fn queue(&self) -> Result<crate::catalog::Listing, String> {
        self.player()?.queue().await
    }

    /// The quality asked for from now on; Soloist restarts to take it.
    pub fn set_quality(&self, q: Quality) -> Result<(), String> {
        self.0.player.set_quality(q)
    }

    /// Retries connecting (and starting the player) now instead of after the backoff.
    pub fn retry(&self) {
        self.0.wake.notify_one();
        self.0.player.retry();
    }

    pub async fn quit_requested(&self) {
        self.0.quit.notified().await
    }

    pub fn quit(&self) {
        self.0.quit.notify_one();
    }

    /// Stops the player and ends the session.
    pub async fn shutdown(&self) {
        self.0.quitting.store(true, Ordering::SeqCst);
        self.0.wake.notify_one();
        self.0.player.shutdown().await;
        if let Some(session) = lock(&self.0.session).take() {
            session.shutdown();
        }
    }
}

impl Inner {
    fn set_phase(&self, phase: Phase, error: Option<String>) {
        let mut account = lock(&self.account);
        account.phase = phase;
        account.error = error;
    }

    async fn supervise(self: Arc<Self>) {
        let mut backoff = FIRST_BACKOFF;
        loop {
            if self.quitting.load(Ordering::SeqCst) {
                return;
            }
            self.signing_out.store(false, Ordering::SeqCst);
            let fresh = lock(&self.fresh).take();
            let stored = fresh.is_none();
            let Some(credentials) = fresh.or_else(|| self.cache.credentials()) else {
                {
                    let mut account = lock(&self.account);
                    if account.phase != Phase::SigningIn {
                        account.phase = Phase::SignedOut;
                    }
                }
                self.player.set_session(None);
                self.wake.notified().await;
                continue;
            };
            self.set_phase(Phase::Connecting, None);

            // A failed or ended session can't be reused: each attempt gets a new one.
            let session = Session::new(self.session_config.clone(), Some(self.cache.clone()));
            match session.connect(credentials, true).await {
                Ok(()) => {
                    backoff = FIRST_BACKOFF;
                    if self.signing_out.load(Ordering::SeqCst) {
                        session.shutdown();
                        continue;
                    }
                    *lock(&self.session) = Some(session.clone());
                    {
                        let mut account = lock(&self.account);
                        account.phase = Phase::Ready;
                        account.error = None;
                        account.user = Some(session.username());
                        account.display_name = None;
                    }
                    self.player.set_session(Some(session.clone()));
                    // The profile's display name, for the UI (the username
                    // librespot signs in with is an opaque id).
                    {
                        let inner = self.clone();
                        let session = session.clone();
                        tokio::spawn(async move {
                            let Ok(body) = session.spclient().get_user_profile(&session.username(), Some(0), Some(0)).await
                            else {
                                return;
                            };
                            let name = serde_json::from_slice::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|v| v.get("name")?.as_str().map(str::to_owned))
                                .filter(|n| !n.is_empty());
                            let mut account = lock(&inner.account);
                            if account.user.as_deref() == Some(session.username().as_str()) {
                                account.display_name = name;
                            }
                        });
                    }
                    log::info!("signed in as {}", session.username());
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(SESSION_CHECK) => {}
                            _ = self.wake.notified() => {}
                        }
                        if session.is_invalid()
                            || self.signing_out.load(Ordering::SeqCst)
                            || self.quitting.load(Ordering::SeqCst)
                        {
                            break;
                        }
                    }
                    if self.quitting.load(Ordering::SeqCst) {
                        return;
                    }
                    if self.signing_out.load(Ordering::SeqCst) {
                        log::info!("signed out");
                        continue;
                    }
                    // The player keeps the session it has (it's only used
                    // for pairing and loads) and plays on with its own.
                    lock(&self.session).take();
                    session.shutdown();
                    log::warn!("the connection to Spotify closed; reconnecting");
                    self.set_phase(
                        Phase::Connecting,
                        Some("Lost the connection to Spotify; reconnecting".to_owned()),
                    );
                }
                Err(e) => {
                    session.shutdown();
                    log::warn!("cannot connect to Spotify: {e}");
                    if stored && matches!(e.kind, ErrorKind::PermissionDenied | ErrorKind::Unauthenticated) {
                        let _ = std::fs::remove_file(self.cache_dir.join("credentials.json"));
                        self.set_phase(
                            Phase::SignedOut,
                            Some(format!("Spotify refused the stored sign-in ({e}); sign in again")),
                        );
                        continue;
                    }
                    self.set_phase(
                        Phase::Error,
                        Some(format!("Cannot connect to Spotify: {e} (retrying in {}s)", backoff.as_secs())),
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.wake.notified() => {}
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}
