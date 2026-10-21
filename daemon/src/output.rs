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

/// Interleaved device-rate samples the stream callback takes from.
struct Queue {
    samples: Mutex<VecDeque<f32>>,
    room: Condvar,
    max: usize,
}

fn lock(m: &Mutex<VecDeque<f32>>) -> std::sync::MutexGuard<'_, VecDeque<f32>> {
    m.lock().unwrap_or_else(|p| p.into_inner())
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
    pub fn open() -> Result<Output, String> {
        let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
        let name = device.name().unwrap_or_else(|_| "unknown".to_owned());
        let config = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
        if config.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!("{name} takes {} samples, not f32", config.sample_format()));
        }
        let rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let queue = Arc::new(Queue {
            samples: Mutex::new(VecDeque::new()),
            room: Condvar::new(),
            max: (rate as f32 * QUEUE_SECS) as usize * channels,
        });
        let feed = queue.clone();
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _| {
                    let mut q = lock(&feed.samples);
                    let n = data.len().min(q.len());
                    for (out, s) in data.iter_mut().zip(q.drain(..n)) {
                        *out = s;
                    }
                    // An underrun plays silence rather than old samples.
                    data[n..].fill(0.0);
                    drop(q);
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
/// queue is full.
fn push(queue: &Queue, channels: usize, frames: impl ExactSizeIterator<Item = (f32, f32)>) {
    let mut q = lock(&queue.samples);
    while q.len() > queue.max {
        q = queue.room.wait(q).unwrap_or_else(|p| p.into_inner());
    }
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
        self.stream.play().map_err(|e| SinkError::StateChange(e.to_string()))
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Let what is queued play out (the end of a track, the moment before a
        // pause), then stop the device.
        let deadline = Instant::now() + DRAIN_MAX;
        let mut q = lock(&self.queue.samples);
        while !q.is_empty() && Instant::now() < deadline {
            q = self.queue.room.wait_timeout(q, Duration::from_millis(20)).unwrap_or_else(|p| p.into_inner()).0;
        }
        drop(q);
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
