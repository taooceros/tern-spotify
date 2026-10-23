//! The player: Spotify's official Soloist plays the audio (it decrypts,
//! decodes, caches and outputs), and the daemon runs it, signs it in, and
//! drives it over its local WebSocket API.
//!
//! A supervisor task owns the Soloist process. With the account signed in
//! and the user's executable and API key in place, it starts Soloist with
//! its own data and cache directories and a loopback-only WebSocket, pairs
//! it on first use (see `pair.rs`), and follows its events into the state
//! `/status` reports. It restarts Soloist when it exits (not when its build
//! expired), reconnects when the WebSocket drops, and stops it when the
//! daemon quits or the user signs out.
//!
//! Soloist accepting a command doesn't mean it happened: the state shown is
//! only what its events report.

pub mod logs;
mod pair;
pub mod prefs;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use librespot_core::Session;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    sync::{Notify, mpsc, watch},
};
use tokio_tungstenite::tungstenite::Message;

use crate::{
    catalog::{Item, Listing},
    engine::TrackInfo,
};
use prefs::Layout;
pub use prefs::Quality;

/// Only Spotify's own signed Soloist gets the user's key and account.
const SIGNER: &str =
    r#"anchor apple generic and identifier "com.spotify.soloist" and certificate leaf[subject.OU] = "2FNC3A47ZF""#;
const DOWNLOADS: &str = "https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates";
/// Soloist's exit code when its build is past its 90 days.
const EXIT_EXPIRED: i32 = 10;
/// How long a started Soloist may take to open its WebSocket.
const WS_WAIT: Duration = Duration::from_secs(20);
/// How long Soloist may take to sign in after taking the pairing.
const LOGIN_WAIT: Duration = Duration::from_secs(30);
/// How long a `load` may take to show in Soloist's state before it is sent again.
const LOAD_WAIT: Duration = Duration::from_secs(4);
/// How long a `load` keeps being sent while Spotify doesn't take it.
const LOAD_RETRY: Duration = Duration::from_secs(12);
/// How long a Soloist signed in before may take to restore its session.
const RESTORE_WAIT: Duration = Duration::from_secs(10);
/// How long `activate` may take to show in Soloist's state.
const ACTIVATE_WAIT: Duration = Duration::from_secs(6);
/// How long Soloist gets to exit on SIGTERM before it is killed.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// How often a missing executable or key, or an expired build, is looked for again.
const SETUP_POLL: Duration = Duration::from_secs(3);
const FIRST_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Queue entries `/queue` lists.
const QUEUE_LIMIT: u32 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not signed in to Spotify: nothing to sign Soloist in with.
    Stopped,
    /// The Soloist executable or the API key is missing (`message` says which).
    Setup,
    Starting,
    /// Signing Soloist in with the account (first use).
    Pairing,
    Ready,
    /// Stopped to start again with another quality.
    Restarting,
    /// It failed or exited; trying again after a pause.
    Error,
    /// The build is past its date: a newer one has to be downloaded.
    Expired,
}

/// The player's part of `/status`.
#[derive(Clone, Debug, Serialize)]
pub struct PlayerStatus {
    pub state: State,
    pub message: Option<String>,
    /// Where the user puts the Soloist executable and their API key.
    pub binary: String,
    pub api_key_file: String,
    /// Days until this Soloist build stops starting.
    pub expires_in_days: Option<u32>,
    /// The quality asked for.
    pub quality: Quality,
    /// What the playing track is actually decoded from, when Soloist said.
    pub format: Option<logs::Format>,
}

/// What `/status` takes from the player.
pub struct Snapshot {
    pub player: PlayerStatus,
    /// Soloist is the account's active Spotify Connect device.
    pub active: bool,
    /// Playing on this device (another device playing doesn't count).
    pub playing: bool,
    pub loading: bool,
    pub track: Option<TrackInfo>,
    pub position_ms: u32,
    pub volume: u8,
    pub shuffle: bool,
    pub repeat_mode: &'static str,
}

/// Soloist's playback state, as its events report it.
#[derive(Default)]
struct Playback {
    logged_in: bool,
    active: bool,
    /// `idle`, `playing`, `paused` or `buffering`.
    status: String,
    item: Option<TrackInfo>,
    position_ms: f64,
    /// Soloist's wall clock at `position_ms` (Unix ms).
    timestamp_ms: f64,
    speed: f64,
    volume: u8,
    shuffle: bool,
    repeat: String,
    /// The upcoming entries of the last `queue_changed`.
    queue: Vec<Value>,
    /// Counts `queue_changed` events.
    queue_seq: u64,
    /// The playing context's URI, and a count of the times a context started.
    context_uri: String,
    context_seq: u64,
}

impl Playback {
    /// The position at `now_ms` (Unix ms), from the last anchor Soloist sent
    /// and its speed (0 while paused), within the track.
    fn position_at(&self, now_ms: f64) -> u32 {
        let mut ms = self.position_ms;
        if self.speed > 0.0 {
            ms += (now_ms - self.timestamp_ms).max(0.0) * self.speed;
        }
        let ms = ms.max(0.0).min(u32::MAX as f64) as u32;
        match self.item.as_ref().map_or(0, |t| t.duration_ms) {
            0 => ms,
            duration => ms.min(duration),
        }
    }

    fn position(&self) -> u32 {
        self.position_at(epoch_ms())
    }
}

struct Live {
    state: State,
    message: Option<String>,
    expires_in_days: Option<u32>,
    quality: Quality,
    format: Option<logs::Format>,
    pb: Playback,
    /// The WebSocket is up.
    connected: bool,
    /// Soloist's Spotify Connect device id.
    device_id: Option<String>,
    /// Set while another device is active; applied when this one plays.
    want_volume: Option<u8>,
    want_shuffle: Option<bool>,
    want_repeat: Option<&'static str>,
    /// What to play on once Soloist is back (it was playing it when a
    /// quality change stopped it): the track and the position.
    resume: Option<(String, u32)>,
    /// Soloist's last error line in this run (redacted), for error messages.
    last_problem: Option<String>,
}

#[derive(Clone)]
pub struct Player(Arc<Inner>);

struct Inner {
    layout: Layout,
    device_name: String,
    http: reqwest::Client,
    live: Mutex<Live>,
    /// The signed-in account's session, set by the engine.
    session: Mutex<Option<Session>>,
    /// To the WebSocket writer, while connected.
    out: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Bumped on every event Soloist sends.
    changed: watch::Sender<u64>,
    /// The running Soloist's pid (0: none).
    pid: AtomicI32,
    /// Wakes the supervisor: the session changed, or the user asked to retry.
    wake: Notify,
    /// Asks the supervisor to stop the running Soloist.
    stop: Notify,
    /// Start again at once after the stop (a quality change).
    restart: AtomicBool,
    quitting: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn epoch_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as f64
}

fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// A track or episode from Soloist's entity shape.
fn track_info(item: &Value) -> Option<TrackInfo> {
    let uri = text(item, "/uri");
    if uri.is_empty() {
        return None;
    }
    let d = &item["decorations"];
    let creators: Vec<&Value> = d["creators"].as_array().into_iter().flatten().map(|c| &c["entity"]).collect();
    let covers: Vec<&Value> = d.pointer("/visual_identity/cover").and_then(Value::as_array).into_iter().flatten().collect();
    // "default" is the 300 px size: big enough for the block, small to fetch.
    let cover = covers
        .iter()
        .find(|c| text(c, "/size") == "default")
        .or_else(|| covers.first())
        .map(|c| text(c, "/url").to_owned())
        .filter(|u| !u.is_empty());
    let album_uri = text(d, "/parent/entity/uri");
    Some(TrackInfo {
        uri: uri.to_owned(),
        name: text(d, "/identity/name").to_owned(),
        artists: creators.iter().map(|c| text(c, "/decorations/identity/name").to_owned()).collect(),
        artist_uris: creators.iter().map(|c| text(c, "/uri").to_owned()).filter(|u| !u.is_empty()).collect(),
        album: text(d, "/parent/entity/decorations/identity/name").to_owned(),
        album_uri: (!album_uri.is_empty()).then(|| album_uri.to_owned()),
        cover,
        duration_ms: d.pointer("/playback/duration_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
    })
}

fn repeat_name(mode: &str) -> &'static str {
    match mode {
        "context" => "context",
        "track" => "track",
        _ => "off",
    }
}

/// How a run of Soloist ended.
enum Outcome {
    Exited(ExitStatus),
    /// Stopped on request (sign-out, quality change, quit).
    Stopped,
    Failed(String),
    /// Soloist refused the pairing (as it does a wrong API key): no retry
    /// until the key changes or the user asks.
    Refused(String),
}

impl Player {
    pub fn start(root: PathBuf, device_name: String) -> Result<Self, String> {
        let layout = Layout { root };
        prefs::private_dir(&layout.root).map_err(|e| format!("cannot create {}: {e}", layout.root.display()))?;
        let quality = layout.quality();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| format!("no HTTP client: {e}"))?;
        let inner = Arc::new(Inner {
            layout,
            device_name,
            http,
            live: Mutex::new(Live {
                state: State::Stopped,
                message: None,
                expires_in_days: None,
                quality,
                format: None,
                pb: Playback::default(),
                connected: false,
                device_id: None,
                want_volume: None,
                want_shuffle: None,
                want_repeat: None,
                resume: None,
                last_problem: None,
            }),
            session: Mutex::new(None),
            out: Mutex::new(None),
            changed: watch::channel(0).0,
            pid: AtomicI32::new(0),
            wake: Notify::new(),
            stop: Notify::new(),
            restart: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
        });
        tokio::spawn(inner.clone().supervise());
        Ok(Player(inner))
    }

    pub fn snapshot(&self) -> Snapshot {
        let live = lock(&self.0.live);
        let pb = &live.pb;
        let ready = live.state == State::Ready;
        let active = ready && pb.logged_in && pb.active;
        let playing = active && matches!(pb.status.as_str(), "playing" | "buffering");
        // Another device's playback goes on there; here it stands still.
        let position = if active { pb.position() } else { pb.position_at(pb.timestamp_ms) };
        let format = live.format.clone().filter(|f| pb.item.as_ref().is_some_and(|t| t.uri == f.uri));
        Snapshot {
            player: PlayerStatus {
                state: live.state,
                message: live.message.clone(),
                binary: self.0.layout.binary().display().to_string(),
                api_key_file: self.0.layout.api_key().display().to_string(),
                expires_in_days: live.expires_in_days,
                quality: live.quality,
                format,
            },
            active,
            playing,
            loading: active && pb.status == "buffering",
            track: pb.item.clone(),
            position_ms: position,
            // Set while inactive, the volume is what this device will play at.
            volume: if active { pb.volume } else { live.want_volume.unwrap_or(pb.volume) },
            shuffle: if active { pb.shuffle } else { live.want_shuffle.unwrap_or(pb.shuffle) },
            repeat_mode: if active { repeat_name(&pb.repeat) } else { live.want_repeat.unwrap_or(repeat_name(&pb.repeat)) },
        }
    }

    /// The signed-in account (or none): Soloist runs only with one.
    pub fn set_session(&self, session: Option<Session>) {
        let changed = {
            let mut current = lock(&self.0.session);
            let changed = current.as_ref().map(Session::username) != session.as_ref().map(Session::username)
                || current.is_none() != session.is_none();
            *current = session;
            changed
        };
        if changed {
            self.0.wake.notify_one();
        }
    }

    /// Looks for the executable and key again, or restarts a failed Soloist, now.
    pub fn retry(&self) {
        self.0.wake.notify_one();
    }

    /// Signs Soloist out: gives up being the active device, stops it, and
    /// deletes its data directory (its session and settings). Its caches stay.
    pub async fn forget(&self) {
        *lock(&self.0.session) = None;
        if self.is_running() {
            let _ = self.send("deactivate", json!({}));
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.stop_and_wait().await;
        let data = self.0.layout.data();
        if let Err(e) = std::fs::remove_dir_all(&data)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("cannot remove {}: {e}", data.display());
        }
        {
            let mut live = lock(&self.0.live);
            live.pb = Playback::default();
            live.format = None;
            live.device_id = None;
            live.resume = None;
        }
        log::info!("signed Soloist out and removed its session");
        self.0.wake.notify_one();
    }

    /// Stops Soloist for good (the daemon is quitting).
    pub async fn shutdown(&self) {
        self.0.quitting.store(true, Ordering::SeqCst);
        self.0.wake.notify_one();
        self.stop_and_wait().await;
    }

    fn is_running(&self) -> bool {
        self.0.pid.load(Ordering::SeqCst) != 0
    }

    async fn stop_and_wait(&self) {
        if !self.is_running() {
            return;
        }
        self.0.stop.notify_one();
        let deadline = Instant::now() + STOP_WAIT + Duration::from_secs(2);
        while self.is_running() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Asks for `q` from now on; a running Soloist restarts to take it (and
    /// plays on when it was playing).
    pub fn set_quality(&self, q: Quality) -> Result<(), String> {
        self.0.layout.set_quality(q)?;
        let mut live = lock(&self.0.live);
        if live.quality == q {
            return Ok(());
        }
        live.quality = q;
        live.format = None;
        if self.is_running() {
            let playing = live.pb.active && matches!(live.pb.status.as_str(), "playing" | "buffering");
            live.resume = live.pb.item.as_ref().filter(|_| playing).map(|t| (t.uri.clone(), live.pb.position()));
            live.state = State::Restarting;
            live.message = None;
            drop(live);
            log::info!("restarting Soloist for {} quality", q.name());
            self.0.restart.store(true, Ordering::SeqCst);
            self.0.stop.notify_one();
        }
        Ok(())
    }

    fn ready(&self) -> Result<(), String> {
        let live = lock(&self.0.live);
        match live.state {
            State::Ready if live.connected && live.pb.logged_in => Ok(()),
            State::Ready => Err("reconnecting to the player; try again in a moment".to_owned()),
            State::Stopped => Err("not signed in to Spotify".to_owned()),
            state => Err(match &live.message {
                Some(m) => format!("the player is not ready: {m}"),
                None => format!("the player is not ready ({})", serde_json::to_value(state).unwrap_or_default().as_str().unwrap_or("")),
            }),
        }
    }

    fn send(&self, command: &str, fields: Value) -> Result<(), String> {
        let mut msg = json!({ "type": "command", "command": command });
        if let (Some(m), Value::Object(f)) = (msg.as_object_mut(), fields) {
            m.extend(f);
        }
        let out = lock(&self.0.out).clone().ok_or("the player is reconnecting; try again in a moment")?;
        out.send(msg.to_string()).map_err(|_| "the player is reconnecting; try again in a moment".to_owned())
    }

    /// Sends `command` after checking Soloist can take it.
    fn command(&self, command: &str, fields: Value) -> Result<(), String> {
        self.ready()?;
        self.send(command, fields)
    }

    fn active(&self) -> bool {
        lock(&self.0.live).pb.active
    }

    /// Commands that act on what plays: only while this device plays it.
    /// Sent while inactive, Soloist would pass them on to the active device.
    fn require_active(&self) -> Result<(), String> {
        self.ready()?;
        if self.active() {
            Ok(())
        } else {
            Err(format!("{} is not the active device; press Play here (t) first", self.0.device_name))
        }
    }

    async fn wait_for(&self, timeout: Duration, pred: impl Fn(&Live) -> bool) -> bool {
        let mut rx = self.0.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pred(&lock(&self.0.live)) {
                return true;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return pred(&lock(&self.0.live)),
            }
        }
    }

    /// Makes Soloist the active device (taking playback over from the device
    /// playing now), then sets the volume chosen meanwhile.
    pub async fn activate(&self) -> Result<(), String> {
        self.ready()?;
        if !self.active() {
            self.send("activate", json!({}))?;
            if !self.wait_for(ACTIVATE_WAIT, |l| l.pb.active).await {
                return Err(format!("{} did not become the active device", self.0.device_name));
            }
        }
        let volume = lock(&self.0.live).want_volume.take();
        if let Some(v) = volume {
            self.send("set_volume", json!({ "volume": v }))?;
        }
        Ok(())
    }

    fn playing(&self) -> bool {
        let live = lock(&self.0.live);
        live.pb.active && matches!(live.pb.status.as_str(), "playing" | "buffering")
    }

    /// Plays here: resumes, or takes playback over from another device.
    pub async fn play(&self) -> Result<(), String> {
        self.activate().await?;
        if !self.playing() {
            self.send("play", json!({}))?;
        }
        Ok(())
    }

    pub async fn toggle(&self) -> Result<(), String> {
        self.ready()?;
        if self.active() && self.playing() { self.pause() } else { self.play().await }
    }

    pub fn pause(&self) -> Result<(), String> {
        self.require_active()?;
        self.send("pause", json!({}))
    }

    pub fn next(&self) -> Result<(), String> {
        self.require_active()?;
        self.send("skip_next", json!({}))
    }

    pub fn prev(&self) -> Result<(), String> {
        self.require_active()?;
        self.send("skip_prev", json!({}))
    }

    pub fn seek(&self, ms: u32) -> Result<(), String> {
        self.require_active()?;
        self.send("seek", json!({ "position_ms": ms }))
    }

    pub fn volume(&self, percent: u8) -> Result<(), String> {
        self.ready()?;
        let percent = percent.min(100);
        if self.active() {
            self.send("set_volume", json!({ "volume": percent }))
        } else {
            lock(&self.0.live).want_volume = Some(percent);
            Ok(())
        }
    }

    /// Shuffle on or off; while inactive, kept for the next `load`.
    pub fn shuffle(&self, on: bool) -> Result<(), String> {
        self.ready()?;
        if self.active() {
            self.send("set_shuffle", json!({ "enabled": on }))
        } else {
            lock(&self.0.live).want_shuffle = Some(on);
            Ok(())
        }
    }

    /// Repeat off, the context or the track; while inactive, kept for the next `load`.
    pub fn repeat(&self, mode: &str) -> Result<(), String> {
        let mode = match mode {
            "off" => "off",
            "context" => "context",
            "track" => "track",
            _ => return Err(format!("unknown repeat mode {mode:?} (off, context, track)")),
        };
        self.ready()?;
        if !self.active() {
            lock(&self.0.live).want_repeat = Some(mode);
            return Ok(());
        }
        // The order Soloist's documentation gives: the one turned off first.
        let (context, track) = (mode == "context", mode == "track");
        if track {
            self.send("set_repeat_context", json!({ "enabled": false }))?;
            self.send("set_repeat_track", json!({ "enabled": true }))
        } else {
            self.send("set_repeat_track", json!({ "enabled": false }))?;
            self.send("set_repeat_context", json!({ "enabled": context }))
        }
    }

    /// Plays `uri` here: a track or episode alone, or a context (album,
    /// playlist, artist, Liked Songs) from its first track or from `track`
    /// in it, with the shuffle and repeat set now.
    ///
    /// Soloist's `play` takes a URI only, so a context goes as a Spotify
    /// Connect `play` command to Soloist's device (what Spotify's own apps
    /// send), sent with the account's session: it carries the track to start
    /// at and the options.
    pub async fn load(&self, uri: &str, track: Option<&str>) -> Result<(), String> {
        if !uri.starts_with("spotify:") {
            return Err(format!("not a Spotify URI: {uri:?}"));
        }
        if let Some(t) = track
            && !t.starts_with("spotify:")
        {
            return Err(format!("not a Spotify URI: {t:?}"));
        }
        let (shuffle, repeat) = {
            let s = self.snapshot();
            (s.shuffle, s.repeat_mode)
        };
        self.activate().await?;
        let single = uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:");
        let alone = single && track.is_none_or(|t| t == uri);
        // What shows it started: the track playing, or (a context from its
        // start) a new context playing.
        let target = if alone { Some(uri) } else { track };
        log::info!("load {uri}{}", track.map(|t| format!(" from {t}")).unwrap_or_default());
        // Spotify learns of a newly signed-in Soloist a few seconds late: until
        // then a context command answers 404, or is taken and goes nowhere.
        let deadline = Instant::now() + LOAD_RETRY;
        loop {
            let before = lock(&self.0.live).pb.context_seq;
            let sent = if alone {
                self.send("play", json!({ "uri": uri }))
            } else {
                self.play_context(uri, track, shuffle, repeat).await
            };
            let started = |l: &Live| {
                let playing = matches!(l.pb.status.as_str(), "playing" | "buffering");
                playing
                    && match target {
                        Some(t) => l.pb.item.as_ref().is_some_and(|i| i.uri == t),
                        None => l.pb.context_seq != before,
                    }
            };
            let why = match sent {
                Ok(()) if self.wait_for(LOAD_WAIT, started).await => break,
                Ok(()) => format!("{} did not start playing {uri}", self.0.device_name),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                return Err(why);
            }
            log::warn!("{why}; asking again");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let mut live = lock(&self.0.live);
        live.want_shuffle = None;
        if !single {
            live.want_repeat = None;
        }
        drop(live);
        let repeat = if single { lock(&self.0.live).want_repeat.take() } else { None };
        if let Some(mode) = repeat {
            self.repeat(mode)?;
        }
        Ok(())
    }

    /// Sends Soloist's device a Spotify Connect `play` of context `uri`, from
    /// `track` in it, with the options.
    async fn play_context(&self, uri: &str, track: Option<&str>, shuffle: bool, repeat: &str) -> Result<(), String> {
        let session = lock(&self.0.session).clone().ok_or("not signed in to Spotify")?;
        let device = lock(&self.0.live).device_id.clone().ok_or("the player's device id is unknown")?;
        let mut options = json!({
            "license": "premium",
            "player_options_override": {
                "shuffling_context": shuffle,
                "repeating_context": repeat != "off",
                "repeating_track": repeat == "track",
            },
        });
        if let Some(t) = track {
            options["skip_to"] = json!({ "track_uri": t });
        }
        let body = json!({
            "command": {
                "endpoint": "play",
                "context": { "uri": uri, "url": format!("context://{uri}"), "metadata": {} },
                "play_origin": { "feature_identifier": "tern-spotify", "feature_version": env!("CARGO_PKG_VERSION") },
                "options": options,
            }
        });
        // From the device to itself: the account's own command to it.
        let endpoint = format!("/connect-state/v1/player/command/from/{device}/to/{device}");
        session
            .spclient()
            .request_as_json(&reqwest::Method::POST, &endpoint, None, Some(&body.to_string()))
            .await
            .map(|_| ())
            .map_err(|e| format!("Spotify refused to play {uri}: {e}"))
    }

    /// Adds a track to the queue (after the playing one).
    pub fn enqueue(&self, uri: &str) -> Result<(), String> {
        if !uri.starts_with("spotify:track:") {
            return Err(format!("only tracks go in the queue, not {uri:?}"));
        }
        self.require_active()?;
        self.send("add_to_queue", json!({ "uri": uri }))
    }

    /// What plays next: the queue, then the context.
    pub async fn queue(&self) -> Result<Listing, String> {
        let before = lock(&self.0.live).pb.queue_seq;
        self.command("get_queue", json!({ "limit": QUEUE_LIMIT }))?;
        // Answered as a `queue_changed` event.
        self.wait_for(Duration::from_secs(3), |l| l.pb.queue_seq != before).await;
        let entries = lock(&self.0.live).pb.queue.clone();
        let items = entries
            .iter()
            .filter_map(|e| {
                let t = track_info(&e["item"])?;
                if !(t.uri.starts_with("spotify:track:") || t.uri.starts_with("spotify:episode:")) {
                    return None;
                }
                let source = match text(e, "/source") {
                    "queue" => "In queue",
                    "autoplay" => "Autoplay",
                    _ => "",
                };
                let mut detail = t.artists.join(", ");
                if !source.is_empty() {
                    detail = if detail.is_empty() { source.to_owned() } else { format!("{source} · {detail}") };
                }
                let image = e
                    .pointer("/item/decorations/visual_identity/cover")
                    .and_then(Value::as_array)
                    .and_then(|c| c.iter().find(|c| text(c, "/size") == "small").or_else(|| c.first()))
                    .map(|c| text(c, "/url").to_owned())
                    .filter(|u| !u.is_empty());
                Some(Item {
                    kind: "track",
                    uri: t.uri,
                    name: t.name,
                    detail,
                    context: None,
                    duration_ms: (t.duration_ms > 0).then_some(t.duration_ms as u64),
                    image,
                })
            })
            .collect();
        Ok(Listing { title: Some("Up next".to_owned()), items, ..Listing::default() })
    }
}

impl Inner {
    fn set(&self, state: State, message: Option<String>) {
        let mut live = lock(&self.live);
        if live.state != state || live.message != message {
            match (&message, state) {
                (Some(m), State::Error | State::Expired | State::Setup) => log::warn!("player: {m}"),
                (Some(m), _) => log::info!("player: {m}"),
                (None, _) => {}
            }
        }
        live.state = state;
        live.message = message;
    }

    fn bump(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    /// The API key, once the executable (Spotify's, signed) and the key are in place.
    async fn check_setup(&self) -> Result<String, String> {
        let binary = self.layout.binary();
        let key_file = self.layout.api_key();
        if !binary.exists() {
            return Err(format!(
                "Put the Soloist executable for this Mac at {} (download it from {DOWNLOADS})",
                binary.display()
            ));
        }
        let verified = Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg("--strict")
            .arg(format!("-R={SIGNER}"))
            .arg(&binary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|e| format!("cannot run codesign: {e}"))?;
        if !verified.success() {
            return Err(format!("{} is not Spotify's signed Soloist; download it again from {DOWNLOADS}", binary.display()));
        }
        let key = match std::fs::read_to_string(&key_file) {
            Ok(k) => k.trim().to_owned(),
            Err(_) => String::new(),
        };
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err(format!(
                "Put your own Soloist API key (from developer.spotify.com) alone in {}",
                key_file.display()
            ));
        }
        // The key is the user's secret: readable by them alone.
        if let Ok(meta) = std::fs::metadata(&key_file)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(key)
    }

    async fn supervise(self: Arc<Self>) {
        let mut backoff = FIRST_BACKOFF;
        loop {
            if self.quitting.load(Ordering::SeqCst) {
                return;
            }
            let Some(session) = lock(&self.session).clone() else {
                self.set(State::Stopped, None);
                self.wake.notified().await;
                continue;
            };
            let key = match self.check_setup().await {
                Ok(key) => key,
                Err(msg) => {
                    self.set(State::Setup, Some(msg));
                    self.pause(SETUP_POLL).await;
                    continue;
                }
            };
            let restarting = lock(&self.live).state == State::Restarting;
            if !restarting {
                self.set(State::Starting, None);
            }
            let mut reached_ready = false;
            let outcome = match self.spawn(&session, &key).await {
                Ok(mut child) => {
                    let outcome = self.run(&mut child, &mut reached_ready).await;
                    self.end(&mut child).await;
                    outcome
                }
                Err(e) => Outcome::Failed(e),
            };
            self.pid.store(0, Ordering::SeqCst);
            {
                let mut live = lock(&self.live);
                live.connected = false;
                live.pb.logged_in = false;
                live.pb.active = false;
                live.pb.status.clear();
            }
            self.bump();
            if reached_ready {
                backoff = FIRST_BACKOFF;
            }
            if self.quitting.load(Ordering::SeqCst) {
                return;
            }
            match outcome {
                Outcome::Stopped => {
                    if self.restart.swap(false, Ordering::SeqCst) {
                        self.set(State::Restarting, None);
                    }
                }
                Outcome::Exited(status) if status.code() == Some(EXIT_EXPIRED) => {
                    let binary = self.layout.binary();
                    let since = std::fs::metadata(&binary).and_then(|m| m.modified()).ok();
                    self.set(
                        State::Expired,
                        Some(format!(
                            "This Soloist build has expired. Download the current one from {DOWNLOADS} and put it at {}",
                            binary.display()
                        )),
                    );
                    lock(&self.live).expires_in_days = Some(0);
                    // Until a new executable is put in place, or a retry.
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(SETUP_POLL) => {}
                            _ = self.wake.notified() => break,
                        }
                        let now = std::fs::metadata(&binary).and_then(|m| m.modified()).ok();
                        if now != since || self.quitting.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    lock(&self.live).expires_in_days = None;
                }
                Outcome::Exited(status) => {
                    let why = lock(&self.live).last_problem.clone();
                    let mut msg = format!("Soloist exited ({status})");
                    if let Some(why) = why {
                        msg.push_str(&format!(": {why}"));
                    }
                    self.fail(msg, &mut backoff).await;
                }
                Outcome::Failed(msg) => self.fail(msg, &mut backoff).await,
                Outcome::Refused(msg) => {
                    self.set(State::Error, Some(msg));
                    let key = self.layout.api_key();
                    let since = std::fs::metadata(&key).and_then(|m| m.modified()).ok();
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(SETUP_POLL) => {}
                            _ = self.wake.notified() => break,
                        }
                        let now = std::fs::metadata(&key).and_then(|m| m.modified()).ok();
                        if now != since || self.quitting.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn fail(&self, msg: String, backoff: &mut Duration) {
        self.set(State::Error, Some(format!("{msg} (retrying in {}s)", backoff.as_secs())));
        tokio::select! {
            _ = tokio::time::sleep(*backoff) => {}
            _ = self.wake.notified() => {}
        }
        *backoff = (*backoff * 2).min(MAX_BACKOFF);
    }

    /// Waits `d`, or less when woken (a retry, or the session changed).
    async fn pause(&self, d: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = self.wake.notified() => {}
        }
    }

    /// Prepares the data directory and starts Soloist.
    async fn spawn(self: &Arc<Self>, session: &Session, key: &str) -> Result<Child, String> {
        let data = self.layout.data();
        stop_stale(&data, &self.layout.binary()).await;
        let username = session.username();
        // Soloist signed in to another account (signed in again as someone
        // else since): start over, so it pairs with this one.
        let users = data.join("settings").join("Users");
        if let Ok(entries) = std::fs::read_dir(&users) {
            let mine = format!("{username}-user");
            let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            if !names.is_empty() && !names.contains(&mine) {
                log::info!("Soloist's data belongs to another account; starting it over");
                std::fs::remove_dir_all(&data).map_err(|e| format!("cannot reset {}: {e}", data.display()))?;
            }
        }
        prefs::private_dir(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
        for stale in ["ws.addr", "ws.port"] {
            let _ = std::fs::remove_file(data.join(stale));
        }
        let quality = lock(&self.live).quality;
        self.layout.apply_quality(quality, &username)?;
        let cache = self.layout.cache(quality);
        prefs::private_dir(&cache).map_err(|e| format!("cannot create {}: {e}", cache.display()))?;
        let cache_mb = self.layout.cache_size_mb(quality);
        // Not logged: the key is in the arguments.
        let mut child = Command::new(self.layout.binary())
            .arg("--device-name")
            .arg(&self.device_name)
            .arg("--api-key")
            .arg(key)
            .arg("--data-dir")
            .arg(&data)
            .arg("--cache-dir")
            .arg(&cache)
            .arg("--initial-volume")
            .arg(self.layout.volume().to_string())
            .arg("--cache-size")
            .arg(cache_mb.to_string())
            .arg("--ws")
            .arg("127.0.0.1:0")
            .arg("--verbose")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group: a Ctrl-C meant for the daemon doesn't
            // reach it; the daemon stops it in order.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start Soloist: {e}"))?;
        let pid = child.id().map_or(0, |p| p as i32);
        self.pid.store(pid, Ordering::SeqCst);
        log::info!("started Soloist (pid {pid}, {} quality, cache {cache_mb} MB)", quality.name());
        lock(&self.live).last_problem = None;
        let reader = Arc::new(Mutex::new(logs::Reader::default()));
        if let Some(out) = child.stdout.take() {
            tokio::spawn(self.clone().follow(out, reader.clone(), key.to_owned()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(self.clone().follow(err, reader, key.to_owned()));
        }
        Ok(child)
    }

    async fn follow(self: Arc<Self>, stream: impl AsyncRead + Unpin, reader: Arc<Mutex<logs::Reader>>, key: String) {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let (note, shown) = lock(&reader).line(&line, &key);
            if let Some((level, text)) = shown {
                log::log!(target: "soloist", level, "{text}");
                if level == log::Level::Error {
                    lock(&self.live).last_problem = Some(text);
                }
            }
            match note {
                Some(logs::Note::ExpiresInDays(days)) => lock(&self.live).expires_in_days = Some(days),
                Some(logs::Note::Expired) => lock(&self.live).expires_in_days = Some(0),
                Some(logs::Note::Format(f)) => {
                    log::info!("playing {} from {} ({:?} kbit/s, {:?} Hz)", f.uri, f.codec, f.bitrate_kbps, f.sample_rate_hz);
                    lock(&self.live).format = Some(f);
                }
                None => {}
            }
        }
    }

    /// Stops `child`: SIGTERM (Soloist saves its state and removes its port
    /// files), then SIGKILL when it takes too long.
    async fn end(&self, child: &mut Child) {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if let Some(pid) = child.id() {
            // SAFETY: plain kill(2) on our own child's pid, which it still holds.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        if tokio::time::timeout(STOP_WAIT, child.wait()).await.is_err() {
            log::warn!("Soloist did not stop on SIGTERM; killing it");
            let _ = child.kill().await;
        }
    }

    /// Follows a started Soloist until it exits or is to be stopped: connects
    /// to its WebSocket (again, when the connection drops), pairs it when it
    /// isn't signed in, and takes in its events.
    async fn run(self: &Arc<Self>, child: &mut Child, reached_ready: &mut bool) -> Outcome {
        let data = self.layout.data();
        // Signed in before: Soloist restores its session on its own, a moment
        // after it starts. Pairing meanwhile would cut that short. (Soloist
        // makes its user's state directory once signed in; the settings one
        // the daemon makes itself, for the quality.)
        let username = lock(&self.session).as_ref().map(Session::username).unwrap_or_default();
        let stored = data.join("cache").join("Users").join(format!("{username}-user")).is_dir();
        let mut paired = false;
        let mut pair_at: Option<tokio::time::Instant> = None;
        let mut login_deadline: Option<tokio::time::Instant> = None;
        loop {
            let url = match self.ws_url(child, &data).await {
                Ok(url) => url,
                Err(o) => return o,
            };
            lock(&self.live).device_id =
                std::fs::read_to_string(data.join(".device_id")).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
            let ws = match tokio_tungstenite::connect_async(url.as_str()).await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    log::warn!("cannot connect to Soloist's WebSocket: {e}");
                    tokio::select! {
                        status = child.wait() => return exited(status),
                        _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                    }
                }
            };
            let (mut sink, mut stream) = ws.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let writer = tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if sink.send(Message::text(m)).await.is_err() {
                        break;
                    }
                }
                let _ = sink.close().await;
            });
            // Soloist sends its auth state (and playback state) on connect;
            // ask anyway, so the snapshot comes before anything else counts.
            let _ = tx.send(json!({ "type": "command", "command": "get_auth_state" }).to_string());
            *lock(&self.out) = Some(tx);
            lock(&self.live).connected = true;
            let end = loop {
                let now = tokio::time::Instant::now();
                tokio::select! {
                    status = child.wait() => break Some(exited(status)),
                    _ = self.stop.notified() => break Some(Outcome::Stopped),
                    _ = tokio::time::sleep_until(login_deadline.unwrap_or(now)), if login_deadline.is_some() => {
                        let mut msg = format!(
                            "Soloist did not sign in. Check the API key in {}",
                            self.layout.api_key().display()
                        );
                        if let Some(why) = lock(&self.live).last_problem.clone() {
                            msg.push_str(&format!(" (Soloist: {why})"));
                        }
                        break Some(Outcome::Failed(msg));
                    }
                    _ = tokio::time::sleep_until(pair_at.unwrap_or(now)), if pair_at.is_some() => {
                        pair_at = None;
                        log::info!("Soloist did not restore its sign-in; pairing it again");
                        paired = true;
                        if let Err(e) = self.pair().await {
                            break Some(self.pair_failed(e));
                        }
                        login_deadline = Some(tokio::time::Instant::now() + LOGIN_WAIT);
                    }
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            let Ok(event) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                            match self.event(&event) {
                                Some(true) => {
                                    login_deadline = None;
                                    pair_at = None;
                                    *reached_ready = true;
                                }
                                // Waiting for it to sign in (or to restore its sign-in).
                                Some(false) if login_deadline.is_some() || pair_at.is_some() => {}
                                // Signed out after signing in: it may sign back in on its own.
                                Some(false) if paired || *reached_ready => {
                                    self.set(State::Starting, Some("The player is signing in again".to_owned()));
                                    login_deadline = Some(tokio::time::Instant::now() + LOGIN_WAIT);
                                }
                                Some(false) if stored => {
                                    self.set(State::Starting, Some("Restoring the player's sign-in".to_owned()));
                                    pair_at = Some(tokio::time::Instant::now() + RESTORE_WAIT);
                                }
                                Some(false) => {
                                    paired = true;
                                    if let Err(e) = self.pair().await {
                                        break Some(self.pair_failed(e));
                                    }
                                    login_deadline = Some(tokio::time::Instant::now() + LOGIN_WAIT);
                                }
                                None => {}
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break None,
                        Some(Ok(_)) => {}
                    },
                }
            };
            *lock(&self.out) = None;
            lock(&self.live).connected = false;
            writer.abort();
            self.bump();
            if let Some(outcome) = end {
                return outcome;
            }
            log::warn!("the connection to Soloist closed; reconnecting");
            tokio::select! {
                status = child.wait() => return exited(status),
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }

    /// Soloist's WebSocket URL, from the port files it writes once listening.
    async fn ws_url(&self, child: &mut Child, data: &Path) -> Result<String, Outcome> {
        let deadline = Instant::now() + WS_WAIT;
        loop {
            let addr = std::fs::read_to_string(data.join("ws.addr")).ok();
            let port = std::fs::read_to_string(data.join("ws.port")).ok().and_then(|p| p.trim().parse::<u16>().ok());
            if let (Some(addr), Some(port)) = (addr, port) {
                let addr = addr.trim();
                if addr != "127.0.0.1" {
                    return Err(Outcome::Failed(format!("Soloist's WebSocket listens on {addr}, not loopback")));
                }
                return Ok(format!("ws://127.0.0.1:{port}"));
            }
            if Instant::now() > deadline {
                return Err(Outcome::Failed("Soloist did not open its WebSocket".to_owned()));
            }
            tokio::select! {
                status = child.wait() => return Err(exited(status)),
                _ = self.stop.notified() => return Err(Outcome::Stopped),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    /// Why pairing failed, with what Soloist itself logged about it.
    fn pair_failed(&self, e: String) -> Outcome {
        let mut msg = format!("Cannot sign the player in: {e}");
        if let Some(why) = lock(&self.live).last_problem.clone() {
            msg.push_str(&format!(" (Soloist: {why})"));
        }
        if e.contains("refused the sign-in") {
            msg.push_str(&format!(". Check the Soloist API key in {}", self.layout.api_key().display()));
            Outcome::Refused(msg)
        } else {
            Outcome::Failed(msg)
        }
    }

    async fn pair(&self) -> Result<(), String> {
        let session = lock(&self.session).clone().ok_or("not signed in to Spotify")?;
        let device = lock(&self.live).device_id.clone().ok_or("Soloist wrote no device id")?;
        self.set(State::Pairing, Some("Signing the player in with your account".to_owned()));
        log::info!("pairing Soloist with {}", session.username());
        if let Err(e) = pair::pair(&self.http, &session, &device, &self.device_name).await {
            // Soloist logs its side of a refusal as it answers: let it arrive.
            tokio::time::sleep(Duration::from_millis(300)).await;
            return Err(e);
        }
        log::info!("Soloist took the pairing; waiting for it to sign in");
        Ok(())
    }

    /// Takes in one event. `Some(logged_in)` for an auth state that changes
    /// whether Soloist is signed in (or says it isn't).
    fn event(self: &Arc<Self>, e: &Value) -> Option<bool> {
        let kind = text(e, "/type");
        let mut signed = None;
        {
            let mut live = lock(&self.live);
            let pb = &mut live.pb;
            let position = |pb: &mut Playback, p: &Value| {
                pb.position_ms = p.get("position_ms").and_then(Value::as_f64).unwrap_or(0.0);
                pb.timestamp_ms = p.get("timestamp_ms").and_then(Value::as_f64).unwrap_or_else(epoch_ms);
                pb.speed = p.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
            };
            let options = |pb: &mut Playback, o: &Value| {
                pb.shuffle = o.get("shuffle").and_then(Value::as_bool).unwrap_or(false);
                pb.repeat = o.get("repeat").and_then(Value::as_str).unwrap_or("off").to_owned();
            };
            match kind {
                "auth_state" => {
                    let logged_in = e.get("logged_in").and_then(Value::as_bool).unwrap_or(false);
                    pb.active = e.get("is_active").and_then(Value::as_bool).unwrap_or(false);
                    // Signed in: news only when it wasn't. Signed out: always.
                    signed = if logged_in { (!pb.logged_in).then_some(true) } else { Some(false) };
                    pb.logged_in = logged_in;
                }
                "playback_state" => {
                    pb.status = text(e, "/status").to_owned();
                    pb.item = track_info(&e["item"]);
                    position(pb, &e["position"]);
                    pb.volume = e.get("volume").and_then(Value::as_u64).unwrap_or(0).min(100) as u8;
                    pb.active = e.get("is_active").and_then(Value::as_bool).unwrap_or(false);
                    options(pb, &e["options"]);
                    let context = text(e, "/context/uri");
                    if context != pb.context_uri {
                        pb.context_uri = context.to_owned();
                        pb.context_seq += 1;
                    }
                }
                "track_changed" => pb.item = track_info(&e["item"]),
                "context_changed" => {
                    pb.context_uri = text(e, "/context/uri").to_owned();
                    pb.context_seq += 1;
                }
                "playback_changed" => pb.status = text(e, "/status").to_owned(),
                "volume_changed" => {
                    pb.volume = e.get("volume").and_then(Value::as_u64).unwrap_or(0).min(100) as u8;
                    self.layout.save_volume(pb.volume);
                }
                "device_changed" => pb.active = e.get("is_active").and_then(Value::as_bool).unwrap_or(false),
                "options_changed" => options(pb, &e["options"]),
                "position_sync" => position(pb, &e["position"]),
                "queue_changed" => {
                    pb.queue = e.get("upcoming").and_then(Value::as_array).cloned().unwrap_or_default();
                    pb.queue_seq += 1;
                }
                "error" => log::warn!("Soloist refused a command: {}", text(e, "/message")),
                _ => {}
            }
        }
        self.bump();
        if signed == Some(true) {
            self.set(State::Ready, None);
            log::info!("Soloist is signed in");
            let _ = self.out_send(json!({ "type": "command", "command": "get_state" }));
            let _ = self.out_send(json!({ "type": "command", "command": "get_queue", "limit": QUEUE_LIMIT }));
            if lock(&self.live).resume.is_some() {
                let player = Player(self.clone());
                tokio::spawn(async move {
                    let Some((uri, at)) = lock(&player.0.live).resume.take() else { return };
                    if let Err(e) = player.play().await {
                        log::warn!("cannot play on after the restart: {e}");
                        return;
                    }
                    // Soloist restores the track; the position it may not.
                    let same = |l: &Live| l.pb.item.as_ref().is_some_and(|t| t.uri == uri) && l.pb.status == "playing";
                    if at > 2000 && player.wait_for(ACTIVATE_WAIT, same).await {
                        let _ = player.seek(at);
                    }
                });
            }
        }
        signed
    }

    fn out_send(&self, msg: Value) -> Result<(), ()> {
        lock(&self.out).as_ref().ok_or(())?.send(msg.to_string()).map_err(|_| ())
    }
}

fn exited(status: std::io::Result<ExitStatus>) -> Outcome {
    match status {
        Ok(status) => Outcome::Exited(status),
        Err(e) => Outcome::Failed(format!("lost track of Soloist: {e}")),
    }
}

/// Stops a Soloist left running on this data directory (the daemon was
/// killed before it could stop it), if it is this executable.
async fn stop_stale(data: &Path, binary: &Path) {
    let Some(pid) = std::fs::read_to_string(data.join("soloist.pid")).ok().and_then(|p| p.trim().parse::<i32>().ok())
    else {
        return;
    };
    if pid <= 1 || !runs(pid, binary) {
        return;
    }
    log::warn!("stopping the Soloist (pid {pid}) a previous daemon left running");
    // SAFETY: kill(2) on a pid just checked to run our Soloist executable.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + STOP_WAIT;
    while runs(pid, binary) {
        if Instant::now() > deadline {
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether process `pid` runs the executable at `binary`.
fn runs(pid: i32, binary: &Path) -> bool {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes, as proc_pidpath requires.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return false;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned());
    let want = std::fs::canonicalize(binary).unwrap_or_else(|_| binary.to_owned());
    std::fs::canonicalize(&path).unwrap_or(path) == want
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_info_reads_soloist_entities() {
        let item = json!({
            "uri": "spotify:track:0aNdtLrUvMMSKZ2EqEFYJK",
            "entity_type": "track",
            "decorations": {
                "identity": { "name": "Don't be like mouse" },
                "visual_identity": { "cover": [
                    { "url": "https://i.scdn.co/image/small", "size": "small" },
                    { "url": "https://i.scdn.co/image/default", "size": "default" },
                ]},
                "parent": { "entity": { "uri": "spotify:album:6f7CThvZW0bwczICdR0yHV", "entity_type": "album",
                    "decorations": { "identity": { "name": "K1" } } } },
                "creators": [ { "entity": { "uri": "spotify:artist:48wt14F9gzlkNDRdXyJTQz", "entity_type": "artist",
                    "decorations": { "identity": { "name": "kmoe" } } } } ],
                "playback": { "duration_ms": 172544, "content_ratings": [] }
            }
        });
        let t = track_info(&item).unwrap();
        assert_eq!((t.name.as_str(), t.album.as_str(), t.duration_ms), ("Don't be like mouse", "K1", 172544));
        assert_eq!(t.artists, vec!["kmoe"]);
        assert_eq!(t.artist_uris, vec!["spotify:artist:48wt14F9gzlkNDRdXyJTQz"]);
        assert_eq!(t.album_uri.as_deref(), Some("spotify:album:6f7CThvZW0bwczICdR0yHV"));
        assert_eq!(t.cover.as_deref(), Some("https://i.scdn.co/image/default"));
        assert!(track_info(&json!({ "uri": "", "entity_type": "" })).is_none());
    }

    #[test]
    fn position_moves_at_speed_and_stops_at_the_end() {
        let mut pb = Playback {
            position_ms: 45_000.0,
            timestamp_ms: 1_000_000.0,
            speed: 1.0,
            item: track_info(&json!({ "uri": "spotify:track:x", "decorations": { "playback": { "duration_ms": 50_000 } } })),
            ..Playback::default()
        };
        assert_eq!(pb.position_at(1_002_000.0), 47_000);
        // A clock behind Soloist's never moves it back.
        assert_eq!(pb.position_at(999_000.0), 45_000);
        assert_eq!(pb.position_at(1_100_000.0), 50_000);
        pb.speed = 0.0;
        assert_eq!(pb.position_at(1_100_000.0), 45_000);
    }
}
