//! ZenLink sound output (Rust side).
//!
//! - [`buffer`]: the v2.2.31 jitter buffer (portable, unit-tested).
//! - [`Player`]: feeds 48 kHz stereo s16 PCM through that buffer to the
//!   system's default output. Backend: WASAPI shared mode, event-driven
//!   (Windows). Other platforms report "no backend" until their ports.

pub mod buffer;
#[cfg(windows)]
mod wasapi;
#[cfg(target_os = "linux")]
mod linux_cpal;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use buffer::{frames_ms, JitterBuffer, CHANNELS};

/// What the window and the logs show about the sound.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioStats {
    pub running: bool,
    pub queue_ms: f32,
    /// Output device latency (WASAPI stream latency plus its buffer).
    pub output_ms: f32,
    pub underruns: u64,
    pub dropped_ms: f32,
    pub block_ms: f32,
    pub margin_ms: f32,
}

pub(crate) struct Shared {
    pub buf: Mutex<JitterBuffer>,
    pub stop: AtomicBool,
    /// f32 bits, 0.0..=1.0.
    pub volume: AtomicU32,
    pub muted: AtomicBool,
    pub running: AtomicBool,
    /// f32 bits, milliseconds.
    pub output_ms: AtomicU32,
}

impl Shared {
    /// Called by the backend for each block it needs (interleaved stereo).
    /// Writes silence when the buffer cannot fill it.
    pub fn render(&self, out: &mut [i16]) {
        let played = self.buf.lock().unwrap().pull(out, Instant::now());
        let volume = f32::from_bits(self.volume.load(Relaxed));
        if !played || self.muted.load(Relaxed) || volume <= 0.0 {
            out.fill(0);
        } else if volume < 0.999 {
            for s in out.iter_mut() {
                *s = (*s as f32 * volume) as i16;
            }
        }
    }
}

/// Plays PCM on the default output. Dropping it stops the sound.
pub struct Player {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    report_at: Instant,
    reported: (u64, u64),
}

impl Player {
    /// Opens the default output. Fails when no backend or device works.
    pub fn start() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            buf: Mutex::new(JitterBuffer::default()),
            stop: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            running: AtomicBool::new(false),
            output_ms: AtomicU32::new(0),
        });
        #[cfg(windows)]
        let thread = wasapi::spawn(shared.clone())?;
        #[cfg(target_os = "linux")]
        let thread = linux_cpal::spawn(shared.clone())?;
        #[cfg(not(any(windows, target_os = "linux")))]
        let thread: JoinHandle<()> = return Err("no sound backend on this platform yet".into());
        Ok(Self {
            shared,
            thread: Some(thread),
            report_at: Instant::now(),
            reported: (0, 0),
        })
    }

    /// Adds s16le stereo bytes. Logs gaps and dropped sound at most every 5 s.
    pub fn feed(&mut self, pcm: &[u8]) {
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let mut b = self.shared.buf.lock().unwrap();
        b.feed(&samples);
        if self.report_at.elapsed().as_secs_f32() >= 5.0 {
            let gaps = b.underruns - self.reported.0;
            let dropped = frames_ms(b.dropped_frames - self.reported.1);
            if gaps > 0 || dropped >= 1.0 {
                log::info!(
                    "sound: {gaps} gaps, {dropped:.0} ms dropped in {:.0} s; margin {:.1} ms, output block {:.1} ms, queued {:.0} ms",
                    self.report_at.elapsed().as_secs_f32(),
                    frames_ms(b.margin as u64),
                    frames_ms(b.block as u64),
                    frames_ms(b.queued_frames() as u64)
                );
            }
            self.reported = (b.underruns, b.dropped_frames);
            self.report_at = Instant::now();
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.shared.muted.store(m, Relaxed);
    }

    /// Rate-adjusted playback instead of chunk drops (Android, see buffer.rs).
    pub fn set_smooth(&self, on: bool) {
        self.shared.buf.lock().unwrap().set_smooth(on);
    }

    /// Minimum queue for bursty links (Wi-Fi); 0 for a cable.
    pub fn set_jitter_ms(&self, ms: f32) {
        self.shared.buf.lock().unwrap().set_jitter_ms(ms);
    }

    pub fn stats(&self) -> AudioStats {
        let b = self.shared.buf.lock().unwrap();
        AudioStats {
            running: self.shared.running.load(Relaxed),
            queue_ms: frames_ms(b.queued_frames() as u64),
            output_ms: f32::from_bits(self.shared.output_ms.load(Relaxed)),
            underruns: b.underruns,
            dropped_ms: frames_ms(b.dropped_frames),
            block_ms: frames_ms(b.block as u64),
            margin_ms: frames_ms(b.margin as u64),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Samples per block for a frame count (interleaved stereo).
#[allow(dead_code)]
pub(crate) fn samples(frames: usize) -> usize {
    frames * CHANNELS
}
