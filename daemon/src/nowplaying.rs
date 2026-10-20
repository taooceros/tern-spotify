//! macOS Now Playing: what plays shows in Control Center and on the lock
//! screen, and the keyboard's media keys (and AirPods, and Control Center's
//! buttons) control it, as they do the desktop client.
//!
//! The system delivers those commands on the main dispatch queue, so the main
//! thread runs a CoreFoundation run loop (`run_main_loop`) while the daemon
//! itself runs on tokio's worker threads.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection,
};

use crate::engine::Engine;

const SEEK_STEP_MS: u32 = 10_000;
/// How often the published state is compared with the player's.
const TICK: Duration = Duration::from_millis(500);
/// A position this far from where the system extrapolates it is re-sent.
const DRIFT_MS: i64 = 1500;

/// Runs the main thread's run loop until `done` is set.
pub fn run_main_loop(done: &AtomicBool) {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
    while !done.load(Ordering::SeqCst) {
        // SAFETY: kCFRunLoopDefaultMode is an immutable CoreFoundation constant.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, Duration::from_millis(250), false);
    }
}

fn handle(engine: &Engine, event: MediaControlEvent) {
    let status = engine.status();
    let result = match event {
        MediaControlEvent::Toggle => engine.toggle(),
        MediaControlEvent::Play => engine.play(),
        MediaControlEvent::Pause | MediaControlEvent::Stop => engine.pause(),
        MediaControlEvent::Next => engine.next(),
        MediaControlEvent::Previous => engine.prev(),
        MediaControlEvent::SetPosition(MediaPosition(at)) => engine.seek(at.as_millis().min(u32::MAX as u128) as u32),
        MediaControlEvent::Seek(direction) | MediaControlEvent::SeekBy(direction, _) => {
            let step = match event {
                MediaControlEvent::SeekBy(_, by) => by.as_millis().min(u32::MAX as u128) as u32,
                _ => SEEK_STEP_MS,
            };
            let at = match direction {
                SeekDirection::Forward => status.position_ms.saturating_add(step),
                SeekDirection::Backward => status.position_ms.saturating_sub(step),
            };
            engine.seek(at)
        }
        _ => Ok(()),
    };
    if let Err(e) = result {
        log::info!("media key {event:?}: {e}");
    }
}

/// Registers the media-key handlers and keeps Now Playing in step with the
/// player. Call from the main thread before `run_main_loop`.
pub fn start(engine: Engine, runtime: &tokio::runtime::Handle) {
    let mut controls = match MediaControls::new(PlatformConfig {
        dbus_name: "tern_spotify",
        display_name: "Spotify (Tern)",
        hwnd: None,
    }) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("no Now Playing controls: {e:?}");
            return;
        }
    };
    let keys = engine.clone();
    if let Err(e) = controls.attach(move |event| handle(&keys, event)) {
        log::warn!("no media keys: {e:?}");
    }
    runtime.spawn(publish(engine, controls));
}

/// What was last sent to the system.
#[derive(PartialEq)]
enum Shown {
    Nothing,
    Track { uri: String, playing: bool },
}

async fn publish(engine: Engine, mut controls: MediaControls) {
    let mut shown = Shown::Nothing;
    // Where the system thinks the position is: sent position + elapsed.
    let mut sent_at = std::time::Instant::now();
    let mut sent_ms: i64 = 0;
    loop {
        tokio::time::sleep(TICK).await;
        let s = engine.status();
        let track = s.track.as_ref().filter(|_| s.active && s.phase == crate::engine::Phase::Ready);
        let Some(t) = track else {
            if shown != Shown::Nothing {
                let _ = controls.set_playback(MediaPlayback::Stopped);
                shown = Shown::Nothing;
            }
            continue;
        };
        let new_track = !matches!(&shown, Shown::Track { uri, .. } if *uri == t.uri);
        if new_track {
            let artists = t.artists.join(", ");
            let _ = controls.set_metadata(MediaMetadata {
                title: Some(&t.name),
                artist: Some(&artists),
                album: Some(&t.album),
                cover_url: t.cover.as_deref(),
                duration: Some(Duration::from_millis(t.duration_ms as u64)),
            });
        }
        let expected = if matches!(shown, Shown::Track { playing: true, .. }) {
            sent_ms + sent_at.elapsed().as_millis() as i64
        } else {
            sent_ms
        };
        let changed_state = !matches!(&shown, Shown::Track { playing, .. } if *playing == s.playing);
        if new_track || changed_state || (expected - s.position_ms as i64).abs() > DRIFT_MS {
            let progress = Some(MediaPosition(Duration::from_millis(s.position_ms as u64)));
            let _ = controls.set_playback(if s.playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            });
            sent_ms = s.position_ms as i64;
            sent_at = std::time::Instant::now();
            shown = Shown::Track { uri: t.uri.clone(), playing: s.playing };
        }
    }
}
