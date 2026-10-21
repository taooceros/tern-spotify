//! The player: one librespot `Player` for the daemon's life, a Spotify Connect
//! device (`Spirc`) per signed-in session, and the state the API reports.
//!
//! A supervisor task owns the session lifecycle: it connects with fresh OAuth
//! credentials or the stored ones, runs the Connect device until it ends, and
//! reconnects (with backoff) unless the user signed out.

use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use librespot_connect::{ConnectConfig, LoadRequest, LoadRequestOptions, PlayingTrack, Spirc};
use librespot_core::{
    Session, SessionConfig, SpotifyUri, authentication::Credentials, cache::Cache, config::DeviceType,
    error::ErrorKind,
};
use librespot_metadata::{
    Metadata, Track,
    audio::{AudioItem, UniqueFields},
};
use librespot_oauth::OAuthClientBuilder;
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, Bitrate, NormalisationMethod, NormalisationType, PlayerConfig, VolumeCtrl},
    mixer::{self, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use serde::Serialize;
use tokio::sync::Notify;

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
/// Room for played audio files in the cache dir (oldest go first).
const AUDIO_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The volume slider's range: 0% to 100% spans this many dB (log curve).
const VOLUME_DB_RANGE: f64 = 30.0;

pub struct EngineConfig {
    pub cache_dir: PathBuf,
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
    /// Signed in; the Connect device is up.
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
    /// Filled from track metadata shortly after the track starts.
    pub album_uri: Option<String>,
    pub cover: Option<String>,
    pub duration_ms: u32,
}

/// What the API reports at `/status`.
#[derive(Debug, Serialize)]
pub struct Status {
    pub phase: Phase,
    pub error: Option<String>,
    pub user: Option<String>,
    /// The account's profile name, once read (the `user` id is opaque).
    pub display_name: Option<String>,
    pub device: String,
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

struct Live {
    phase: Phase,
    error: Option<String>,
    user: Option<String>,
    display_name: Option<String>,
    active: bool,
    playing: bool,
    loading: bool,
    track: Option<TrackInfo>,
    position_ms: u32,
    position_at: Instant,
    volume: u16,
    shuffle: bool,
    repeat: bool,
    repeat_track: bool,
}

impl Live {
    fn at(&mut self, position_ms: u32) {
        self.position_ms = position_ms;
        self.position_at = Instant::now();
    }

    fn position(&self) -> u32 {
        let mut ms = self.position_ms;
        if self.playing && !self.loading {
            ms = ms.saturating_add(self.position_at.elapsed().as_millis().min(u32::MAX as u128) as u32);
        }
        match &self.track {
            Some(t) if t.duration_ms > 0 => ms.min(t.duration_ms),
            _ => ms,
        }
    }
}

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

struct Inner {
    cache_dir: PathBuf,
    device_name: String,
    session_config: SessionConfig,
    connect_config: ConnectConfig,
    cache: Cache,
    player: Arc<Player>,
    /// Pauses the audio output ahead of the player (see output.rs).
    output: crate::output::Control,
    mixer: Arc<dyn Mixer>,
    session: Mutex<Session>,
    spirc: Mutex<Option<Arc<Spirc>>>,
    live: Mutex<Live>,
    /// Credentials from a finished OAuth sign-in, taken by the supervisor.
    fresh: Mutex<Option<Credentials>>,
    /// Set by sign-out so the supervisor doesn't reconnect when the device ends.
    signing_out: AtomicBool,
    quitting: AtomicBool,
    /// Wakes the supervisor: credentials arrived or the user asked to retry.
    wake: Notify,
    quit: Notify,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A stable Spotify Connect device id, made once and kept in the cache dir.
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

fn track_info(item: &AudioItem) -> TrackInfo {
    let (artists, artist_uris, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists.iter().map(|a| a.name.clone()).collect(),
            artists.iter().filter_map(|a| a.id.to_uri().ok()).collect(),
            album.clone(),
        ),
        UniqueFields::Episode { show_name, .. } => (vec![show_name.clone()], Vec::new(), String::new()),
        UniqueFields::Local { artists, album, .. } => (
            artists.iter().cloned().collect(),
            Vec::new(),
            album.clone().unwrap_or_default(),
        ),
    };
    // The cover nearest 300 px wide: big enough for the block, small to fetch.
    let cover = item
        .covers
        .iter()
        .min_by_key(|c| (c.width - 300).abs())
        .map(|c| c.url.clone());
    TrackInfo {
        uri: item.uri.clone(),
        name: item.name.clone(),
        artists,
        artist_uris,
        album,
        album_uri: None,
        cover,
        duration_ms: item.duration_ms,
    }
}

impl Engine {
    pub fn start(cfg: EngineConfig) -> Result<Self, String> {
        std::fs::create_dir_all(&cfg.cache_dir)
            .map_err(|e| format!("cannot create {}: {e}", cfg.cache_dir.display()))?;
        // Played files are kept (up to AUDIO_CACHE_BYTES), as the desktop
        // client keeps them: a replay starts at once and needs no download.
        let audio_dir = cfg.cache_dir.join("audio");
        let cache = Cache::new(Some(&cfg.cache_dir), Some(&cfg.cache_dir), Some(&audio_dir), Some(AUDIO_CACHE_BYTES))
            .map_err(|e| format!("cannot open the cache in {}: {e}", cfg.cache_dir.display()))?;
        let session_config = SessionConfig {
            device_id: device_id(&cfg.cache_dir),
            ..SessionConfig::default()
        };
        let volume = cache.volume().unwrap_or(u16::MAX / 2);
        let connect_config = ConnectConfig {
            name: cfg.device_name.clone(),
            device_type: DeviceType::Computer,
            initial_volume: volume,
            ..ConnectConfig::default()
        };
        // librespot's default curve spans 60 dB, which puts 50% at -30 dB: far
        // quieter than the desktop app at the same slider. Over 30 dB, 50% is
        // -15 dB and the slider still feels even.
        let mixer_config = MixerConfig { volume_ctrl: VolumeCtrl::Log(VOLUME_DB_RANGE), ..MixerConfig::default() };
        let mixer = (mixer::find(None).ok_or("librespot has no mixer")?)(mixer_config)
            .map_err(|e| format!("cannot open the mixer: {e}"))?;
        let backend = audio_backend::find(None).ok_or("librespot has no audio backend")?;
        let session = Session::new(session_config.clone(), Some(cache.clone()));
        let output = crate::output::Control::default();
        let control = output.clone();
        let player = Player::new(
            // What the desktop client plays a Premium account at: "Very high"
            // quality (320 kbit/s; Spotify's lossless files need keys librespot
            // can't get) with loudness normalisation on. `Basic` normalisation
            // only scales a track, never past its peak, as the desktop app's
            // "Normal" level does; librespot's default (`Dynamic`) runs every
            // sample through a limiter, which squashes the peaks of quiet,
            // dynamic recordings it turns up.
            PlayerConfig {
                bitrate: Bitrate::Bitrate320,
                normalisation: true,
                normalisation_type: NormalisationType::Auto,
                normalisation_method: NormalisationMethod::Basic,
                ..PlayerConfig::default()
            },
            session.clone(),
            mixer.get_soft_volume(),
            // Our own output (see output.rs): float samples to the device at its
            // own rate, resampled properly when that isn't 44.1 kHz. librespot's
            // rodio backend (in float) when the device can't be opened that way.
            move || match crate::output::Output::open(control) {
                Ok(output) => Box::new(output),
                Err(e) => {
                    log::warn!("audio output: {e}; using librespot's rodio output");
                    backend(None, AudioFormat::F32)
                }
            },
        );
        let events = player.get_player_event_channel();

        let inner = Arc::new(Inner {
            cache_dir: cfg.cache_dir,
            device_name: cfg.device_name,
            session_config,
            connect_config,
            cache,
            player,
            output,
            mixer,
            session: Mutex::new(session),
            spirc: Mutex::new(None),
            live: Mutex::new(Live {
                phase: Phase::Connecting,
                error: None,
                user: None,
                display_name: None,
                active: false,
                playing: false,
                loading: false,
                track: None,
                position_ms: 0,
                position_at: Instant::now(),
                volume,
                shuffle: false,
                repeat: false,
                repeat_track: false,
            }),
            fresh: Mutex::new(None),
            signing_out: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            wake: Notify::new(),
            quit: Notify::new(),
        });
        tokio::spawn(inner.clone().watch(events));
        tokio::spawn(inner.clone().supervise());
        Ok(Engine(inner))
    }

    pub fn status(&self) -> Status {
        let live = lock(&self.0.live);
        Status {
            phase: live.phase,
            error: live.error.clone(),
            user: live.user.clone(),
            display_name: live.display_name.clone(),
            device: self.0.device_name.clone(),
            active: live.active,
            playing: live.playing,
            loading: live.loading,
            track: live.track.clone(),
            position_ms: live.position(),
            volume: ((live.volume as u32 * 100 + u16::MAX as u32 / 2) / u16::MAX as u32) as u8,
            shuffle: live.shuffle,
            repeat_mode: if live.repeat_track {
                "track"
            } else if live.repeat {
                "context"
            } else {
                "off"
            },
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// The current session, once signed in.
    pub fn session(&self) -> Result<Session, String> {
        if lock(&self.0.live).phase != Phase::Ready {
            return Err("not signed in to Spotify".to_owned());
        }
        Ok(lock(&self.0.session).clone())
    }

    fn spirc(&self) -> Result<Arc<Spirc>, String> {
        lock(&self.0.spirc).clone().ok_or_else(|| "not signed in to Spotify".to_owned())
    }

    fn active(&self) -> bool {
        lock(&self.0.live).active
    }

    /// Silences the output now when this device plays: the player's own
    /// pause is heard only after the queued half second plays out.
    fn hold_if_playing(&self) {
        let live = lock(&self.0.live);
        if live.active && live.playing {
            self.0.output.hold();
        }
    }

    /// Starts the OAuth sign-in: opens Spotify's page in the browser and waits
    /// for its redirect on a blocking thread.
    pub fn login(&self) -> Result<(), String> {
        {
            let mut live = lock(&self.0.live);
            match live.phase {
                Phase::SigningIn => return Ok(()),
                Phase::Ready => return Err("already signed in; sign out first".to_owned()),
                _ => {}
            }
            live.phase = Phase::SigningIn;
            live.error = None;
        }
        let inner = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let redirect = format!("http://{OAUTH_ADDR}/login");
            let token = OAuthClientBuilder::new(
                &inner.session_config.client_id,
                &redirect,
                OAUTH_SCOPES.to_vec(),
            )
            .open_in_browser()
            .with_custom_message(SIGNED_IN_PAGE)
            .build()
            .and_then(|client| client.get_access_token());
            let mut live = lock(&inner.live);
            match token {
                Ok(token) => {
                    *lock(&inner.fresh) = Some(Credentials::with_access_token(token.access_token));
                    live.phase = Phase::Connecting;
                    drop(live);
                    inner.wake.notify_one();
                }
                Err(e) => {
                    log::warn!("sign-in failed: {e}");
                    live.phase = Phase::SignedOut;
                    live.error = Some(format!("Sign-in did not finish: {e}"));
                }
            }
        });
        Ok(())
    }

    /// Ends a pending sign-in: the redirect listener takes one request, so a
    /// request without a code makes it return.
    pub fn cancel_login(&self) {
        if lock(&self.0.live).phase != Phase::SigningIn {
            return;
        }
        let addr: SocketAddr = OAUTH_ADDR.parse().expect("valid address");
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            let _ = stream.write_all(b"GET /login?cancelled=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        }
    }

    /// Forgets the stored credentials and ends the Connect device.
    pub fn logout(&self) {
        self.0.signing_out.store(true, Ordering::SeqCst);
        let creds = self.0.cache_dir.join("credentials.json");
        if let Err(e) = std::fs::remove_file(&creds)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot remove {}: {e}", creds.display());
        }
        lock(&self.0.fresh).take();
        {
            let mut live = lock(&self.0.live);
            live.error = None;
            live.user = None;
            live.display_name = None;
            live.track = None;
        }
        match lock(&self.0.spirc).as_ref() {
            Some(spirc) => {
                let _ = spirc.shutdown();
            }
            None => {
                self.0.signing_out.store(false, Ordering::SeqCst);
                self.0.wake.notify_one();
            }
        }
    }

    /// Plays or pauses here; when another device plays, takes its playback over.
    pub fn toggle(&self) -> Result<(), String> {
        let spirc = self.spirc()?;
        if !self.active() {
            return spirc.transfer(None).map_err(|e| e.to_string());
        }
        self.hold_if_playing();
        spirc.play_pause().map_err(|e| e.to_string())
    }

    pub fn play(&self) -> Result<(), String> {
        let spirc = self.spirc()?;
        if self.active() { spirc.play() } else { spirc.transfer(None) }.map_err(|e| e.to_string())
    }

    pub fn pause(&self) -> Result<(), String> {
        let spirc = self.spirc()?;
        self.hold_if_playing();
        spirc.pause().map_err(|e| e.to_string())
    }

    pub fn next(&self) -> Result<(), String> {
        let spirc = self.spirc()?;
        self.0.output.discard_held();
        spirc.next().map_err(|e| e.to_string())
    }

    pub fn prev(&self) -> Result<(), String> {
        let spirc = self.spirc()?;
        self.0.output.discard_held();
        spirc.prev().map_err(|e| e.to_string())
    }

    /// Takes the user's playback over from the device playing it now.
    pub fn transfer(&self) -> Result<(), String> {
        self.spirc()?.transfer(None).map_err(|e| e.to_string())
    }

    pub fn seek(&self, ms: u32) -> Result<(), String> {
        let spirc = self.spirc()?;
        self.0.output.discard_held();
        spirc.set_position_ms(ms).map_err(|e| e.to_string())
    }

    /// Sets the volume (0–100). The Connect device applies it only while
    /// active, so an inactive one sets the mixer directly.
    pub fn volume(&self, percent: u8) -> Result<(), String> {
        let volume = (percent.min(100) as u32 * u16::MAX as u32 / 100) as u16;
        let spirc = self.spirc()?;
        if self.active() {
            spirc.set_volume(volume).map_err(|e| e.to_string())
        } else {
            self.0.mixer.set_volume(volume);
            self.0.cache.save_volume(volume);
            lock(&self.0.live).volume = volume;
            Ok(())
        }
    }

    pub fn shuffle(&self, on: bool) -> Result<(), String> {
        self.spirc()?.shuffle(on).map_err(|e| e.to_string())
    }

    pub fn repeat(&self, mode: &str) -> Result<(), String> {
        let spirc = self.spirc()?;
        let (context, track) = match mode {
            "off" => (false, false),
            "context" => (true, false),
            "track" => (true, true),
            _ => return Err(format!("unknown repeat mode {mode:?} (off, context, track)")),
        };
        spirc.repeat_track(track).map_err(|e| e.to_string())?;
        spirc.repeat(context).map_err(|e| e.to_string())
    }

    /// Plays `uri` here: a track alone, or a context (album, playlist, artist,
    /// liked songs) from its first track or from `track` in it.
    pub fn load(&self, uri: &str, track: Option<&str>) -> Result<(), String> {
        if !uri.starts_with("spotify:") {
            return Err(format!("not a Spotify URI: {uri:?}"));
        }
        let spirc = self.spirc()?;
        self.0.output.discard_held();
        // Commands other than activate are ignored until this device is active.
        if !self.active() {
            spirc.activate().map_err(|e| e.to_string())?;
        }
        let options = LoadRequestOptions {
            start_playing: true,
            playing_track: track.map(|t| PlayingTrack::Uri(t.to_owned())),
            ..LoadRequestOptions::default()
        };
        let request = if uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:") {
            LoadRequest::from_tracks(vec![uri.to_owned()], options)
        } else {
            LoadRequest::from_context_uri(uri.to_owned(), options)
        };
        spirc.load(request).map_err(|e| e.to_string())
    }

    /// Retries connecting now instead of after the backoff.
    pub fn retry(&self) {
        self.0.wake.notify_one();
    }

    pub async fn quit_requested(&self) {
        self.0.quit.notified().await
    }

    pub fn quit(&self) {
        self.0.quit.notify_one();
    }

    /// Ends the Connect device (so Spotify drops it from the device list) and
    /// waits briefly for it to go.
    pub async fn shutdown(&self) {
        self.0.quitting.store(true, Ordering::SeqCst);
        let spirc = lock(&self.0.spirc).clone();
        if let Some(spirc) = spirc {
            let _ = spirc.shutdown();
            for _ in 0..30 {
                if lock(&self.0.spirc).is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        lock(&self.0.session).shutdown();
    }
}

impl Inner {
    fn set_phase(&self, phase: Phase, error: Option<String>) {
        let mut live = lock(&self.live);
        live.phase = phase;
        live.error = error;
        if phase != Phase::Ready {
            live.active = false;
            live.playing = false;
            live.loading = false;
        }
    }

    async fn watch(self: Arc<Self>, mut events: librespot_playback::player::PlayerEventChannel) {
        while let Some(event) = events.recv().await {
            let mut live = lock(&self.live);
            match event {
                PlayerEvent::Playing { position_ms, .. } => {
                    live.playing = true;
                    live.loading = false;
                    live.at(position_ms);
                }
                PlayerEvent::Paused { position_ms, .. } => {
                    live.playing = false;
                    live.loading = false;
                    live.at(position_ms);
                }
                PlayerEvent::Loading { position_ms, .. } => {
                    live.loading = true;
                    live.at(position_ms);
                }
                PlayerEvent::Stopped { .. } => {
                    live.playing = false;
                    live.loading = false;
                }
                PlayerEvent::Seeked { position_ms, .. }
                | PlayerEvent::PositionCorrection { position_ms, .. }
                | PlayerEvent::PositionChanged { position_ms, .. } => live.at(position_ms),
                PlayerEvent::Unavailable { track_id, .. } => {
                    live.error = Some(format!("{track_id} is not available to play"));
                }
                PlayerEvent::TrackChanged { audio_item } => {
                    live.track = Some(track_info(&audio_item));
                    if live.error.as_deref().is_some_and(|e| e.ends_with("not available to play")) {
                        live.error = None;
                    }
                    // The audio item names the album but not its URI: track
                    // metadata has it.
                    if matches!(audio_item.track_id, SpotifyUri::Track { .. }) {
                        let inner = self.clone();
                        let session = lock(&self.session).clone();
                        let uri = audio_item.uri.clone();
                        tokio::spawn(async move {
                            let Ok(track) = Track::get(&session, &audio_item.track_id).await else { return };
                            let Ok(album_uri) = track.album.id.to_uri() else { return };
                            if let Some(t) = lock(&inner.live).track.as_mut().filter(|t| t.uri == uri) {
                                t.album_uri = Some(album_uri);
                            }
                        });
                    }
                }
                PlayerEvent::VolumeChanged { volume } => {
                    live.volume = volume;
                    self.cache.save_volume(volume);
                }
                PlayerEvent::SessionConnected { user_name, .. } => {
                    live.active = true;
                    live.user = Some(user_name);
                }
                PlayerEvent::SessionDisconnected { .. } => {
                    live.active = false;
                    live.playing = false;
                    live.loading = false;
                }
                PlayerEvent::ShuffleChanged { shuffle } => live.shuffle = shuffle,
                PlayerEvent::RepeatChanged { context, track } => {
                    live.repeat = context;
                    live.repeat_track = track;
                }
                _ => {}
            }
        }
    }

    async fn supervise(self: Arc<Self>) {
        let mut first = true;
        let mut backoff = FIRST_BACKOFF;
        loop {
            if self.quitting.load(Ordering::SeqCst) {
                return;
            }
            let fresh = lock(&self.fresh).take();
            let stored = fresh.is_none();
            let Some(credentials) = fresh.or_else(|| self.cache.credentials()) else {
                {
                    let mut live = lock(&self.live);
                    if live.phase != Phase::SigningIn {
                        live.phase = Phase::SignedOut;
                    }
                    live.active = false;
                    live.playing = false;
                }
                self.wake.notified().await;
                continue;
            };
            self.set_phase(Phase::Connecting, None);

            // A failed or ended session can't be reused: its message listeners
            // stay registered. Each attempt after the first gets a new one.
            let session = if first {
                first = false;
                lock(&self.session).clone()
            } else {
                let session = Session::new(self.session_config.clone(), Some(self.cache.clone()));
                *lock(&self.session) = session.clone();
                self.player.set_session(session.clone());
                session
            };

            let spirc = Spirc::new(
                self.connect_config.clone(),
                session.clone(),
                credentials,
                self.player.clone(),
                self.mixer.clone(),
            )
            .await;
            match spirc {
                Ok((spirc, task)) => {
                    backoff = FIRST_BACKOFF;
                    *lock(&self.spirc) = Some(Arc::new(spirc));
                    {
                        let mut live = lock(&self.live);
                        live.phase = Phase::Ready;
                        live.error = None;
                        live.user = Some(session.username());
                        live.display_name = None;
                    }
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
                            let mut live = lock(&inner.live);
                            if live.user.as_deref() == Some(session.username().as_str()) {
                                live.display_name = name;
                            }
                        });
                    }
                    log::info!("signed in as {}", session.username());
                    task.await;
                    lock(&self.spirc).take();
                    session.shutdown();
                    if self.quitting.load(Ordering::SeqCst) {
                        return;
                    }
                    if self.signing_out.swap(false, Ordering::SeqCst) {
                        log::info!("signed out");
                        continue;
                    }
                    log::warn!("the Spotify Connect device ended; reconnecting");
                    self.set_phase(
                        Phase::Connecting,
                        Some("Lost the connection to Spotify; reconnecting".to_owned()),
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
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
