//! The audio output: librespot's decoded samples (44.1 kHz stereo) to the
//! default output device, at the device's own rate.
//!
//! librespot's rodio backend asks for 44.1 kHz, but takes the first stereo
//! configuration the device lists; a Mac's speakers list 48 kHz first, so it
//! runs at 48 kHz and rodio resamples by linear interpolation, which smears
//! the top octave and folds images back into the audible band. Here a device
//! already at 44.1 kHz gets the samples untouched, and any other rate gets
//! them through rubato's FFT resampler (band-limited, the way the desktop
//! app resamples). Samples stay `f32` to the device.
//!
//! About half a second is queued ahead of the device. A pause from this
//! daemon (`Control::hold`) silences the device at once and keeps that half
//! second for the resume; any other pause lets it play out first.

use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use librespot_playback::{
    NUM_CHANNELS, SAMPLE_RATE,
    audio_backend::{Sink, SinkError, SinkResult},
    convert::Converter,
    decoder::AudioPacket,
};
use rubato::{FftFixedIn, Resampler};

/// Input frames per resampler pass (about 23 ms).
const CHUNK_FRAMES: usize = 1024;
/// Audio queued for the device at most; `write` waits while there is more,
/// as rodio's sink does.
const QUEUE_SECS: f32 = 0.5;
/// How long `stop` lets the queue play out before pausing the stream.
const DRAIN_MAX: Duration = Duration::from_secs(1);
/// How long a pause asked for ahead of the player (`Control::hold`) keeps the
/// device silent waiting for the player to pause; when it doesn't (librespot
/// dropped the command), playback goes on.
const HOLD_MAX: Duration = Duration::from_millis(1500);

/// A pause asked for ahead of the player.
#[derive(Clone, Copy)]
enum Hold {
    /// Asked for then; the player hasn't paused yet.
    Asked(Instant),
    /// The player paused: what is queued waits for the resume.
    Paused,
}

struct Buf {
    /// Interleaved device-rate samples the stream callback takes from.
    samples: VecDeque<f32>,
    /// Samples queued at most, set when the device opens.
    max: usize,
    held: Option<Hold>,
}

struct Queue {
    buf: Mutex<Buf>,
    room: Condvar,
}

fn lock(m: &Mutex<Buf>) -> std::sync::MutexGuard<'_, Buf> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The engine's hand on the output, ahead of the player: librespot pauses
/// only after `Sink::stop` returns, which first plays out what is queued.
#[derive(Clone)]
pub struct Control(Arc<Queue>);

impl Default for Control {
    fn default() -> Control {
        Control(Arc::new(Queue {
            buf: Mutex::new(Buf { samples: VecDeque::new(), max: 0, held: None }),
            room: Condvar::new(),
        }))
    }
}

impl Control {
    /// Silences the device now, keeping what is queued for the resume; the
    /// player's pause follows.
    pub fn hold(&self) {
        let mut b = lock(&self.0.buf);
        if b.held.is_none() {
            b.held = Some(Hold::Asked(Instant::now()));
        }
        drop(b);
        // A `write` waiting for room goes on, so the player gets to the pause.
        self.0.room.notify_all();
    }

    /// Drops what a pause from here kept: a seek, skip or load while paused
    /// resumes somewhere else.
    pub fn discard_held(&self) {
        let mut b = lock(&self.0.buf);
        if b.held.is_some() {
            b.samples.clear();
        }
    }
}

pub struct Output {
    queue: Arc<Queue>,
    stream: cpal::Stream,
    /// The device's channel count (stereo goes to its first two).
    channels: usize,
    /// None when the device runs at 44.1 kHz.
    resampler: Option<FftFixedIn<f32>>,
    /// Left and right input frames waiting for a full resampler chunk.
    pending: [Vec<f32>; 2],
    resampled: Vec<Vec<f32>>,
}

impl Output {
    /// Opens the default output device at its current rate, in `f32`.
    pub fn open(control: Control) -> Result<Output, String> {
        let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
        let name = device.name().unwrap_or_else(|_| "unknown".to_owned());
        let config = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
        if config.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!("{name} takes {} samples, not f32", config.sample_format()));
        }
        let rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let queue = control.0;
        lock(&queue.buf).max = (rate as f32 * QUEUE_SECS) as usize * channels;
        let feed = queue.clone();
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _| {
                    let mut b = lock(&feed.buf);
                    match b.held {
                        Some(Hold::Asked(at)) if at.elapsed() >= HOLD_MAX => b.held = None,
                        Some(_) => {
                            data.fill(0.0);
                            return;
                        }
                        None => {}
                    }
                    let n = data.len().min(b.samples.len());
                    for (out, s) in data.iter_mut().zip(b.samples.drain(..n)) {
                        *out = s;
                    }
                    // An underrun plays silence rather than old samples.
                    data[n..].fill(0.0);
                    drop(b);
                    feed.room.notify_one();
                },
                |e| log::warn!("audio output: {e}"),
                None,
            )
            .map_err(|e| format!("{name}: {e}"))?;
        let resampler = if rate == SAMPLE_RATE {
            None
        } else {
            Some(
                FftFixedIn::new(SAMPLE_RATE as usize, rate as usize, CHUNK_FRAMES, 2, NUM_CHANNELS as usize)
                    .map_err(|e| format!("resampler {SAMPLE_RATE} → {rate} Hz: {e}"))?,
            )
        };
        log::info!(
            "audio output: {name}, {channels} channels at {rate} Hz, f32{}",
            if resampler.is_some() { ", resampled from 44.1 kHz (FFT)" } else { ", no resampling" }
        );
        let resampled = match &resampler {
            Some(r) => vec![vec![0.0; r.output_frames_max()]; NUM_CHANNELS as usize],
            None => Vec::new(),
        };
        Ok(Output {
            queue,
            stream,
            channels,
            resampler,
            pending: [Vec::with_capacity(2 * CHUNK_FRAMES), Vec::with_capacity(2 * CHUNK_FRAMES)],
            resampled,
        })
    }
}

/// Queues stereo frames for a `channels`-channel device, waiting while the
/// queue is full (but not while held: the player must get to its pause).
fn push(queue: &Queue, channels: usize, frames: impl ExactSizeIterator<Item = (f32, f32)>) {
    let mut b = lock(&queue.buf);
    while b.samples.len() > b.max && b.held.is_none() {
        b = queue.room.wait(b).unwrap_or_else(|p| p.into_inner());
    }
    let q = &mut b.samples;
    q.reserve(frames.len() * channels);
    for (l, r) in frames {
        match channels {
            1 => q.push_back((l + r) * 0.5),
            n => {
                q.push_back(l);
                q.push_back(r);
                for _ in 2..n {
                    q.push_back(0.0);
                }
            }
        }
    }
}

impl Sink for Output {
    fn start(&mut self) -> SinkResult<()> {
        lock(&self.queue.buf).held = None;
        self.stream.play().map_err(|e| SinkError::StateChange(e.to_string()))
    }

    fn stop(&mut self) -> SinkResult<()> {
        let mut b = lock(&self.queue.buf);
        // Let what is queued play out (the end of the last track, a pause from
        // another device), unless paused from here (`Control::hold`, even
        // while this waits): then the device is already silent and what is
        // queued waits for the resume.
        let deadline = Instant::now() + DRAIN_MAX;
        while b.held.is_none() && !b.samples.is_empty() && Instant::now() < deadline {
            b = self.queue.room.wait_timeout(b, Duration::from_millis(20)).unwrap_or_else(|p| p.into_inner()).0;
        }
        if b.held.is_some() {
            b.held = Some(Hold::Paused);
        }
        drop(b);
        self.stream.pause().map_err(|e| SinkError::StateChange(e.to_string()))
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        let samples = packet.samples().map_err(|e| SinkError::OnWrite(e.to_string()))?;
        let Some(resampler) = self.resampler.as_mut() else {
            push(&self.queue, self.channels, samples.chunks_exact(2).map(|f| (f[0] as f32, f[1] as f32)));
            return Ok(());
        };
        for f in samples.chunks_exact(2) {
            self.pending[0].push(f[0] as f32);
            self.pending[1].push(f[1] as f32);
        }
        while self.pending[0].len() >= resampler.input_frames_next() {
            let (used, made) = resampler
                .process_into_buffer(&self.pending, &mut self.resampled, None)
                .map_err(|e| SinkError::OnWrite(e.to_string()))?;
            for ch in &mut self.pending {
                ch.drain(..used);
            }
            let frames = self.resampled[0][..made].iter().copied().zip(self.resampled[1][..made].iter().copied());
            push(&self.queue, self.channels, frames);
        }
        Ok(())
    }
}
