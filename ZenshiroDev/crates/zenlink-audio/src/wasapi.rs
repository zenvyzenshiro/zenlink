//! WASAPI shared-mode output, event-driven, on its own thread (it sets up
//! COM itself: v2.2.31 measured that a thread with foreign COM state made
//! WASAPI fail and fall back to MME at ~91 ms instead of ~23 ms).
//!
//! The stream asks for 48 kHz stereo s16 and lets Windows convert to the
//! mix format (AUTOCONVERTPCM), like v2.2.31's `auto_convert=True`. When the
//! device goes away (headset unplugged), the stream reopens on the new
//! default output.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::buffer::{CHANNELS, RATE};
use crate::{samples, Shared};

pub(crate) fn spawn(shared: Arc<Shared>) -> std::result::Result<JoinHandle<()>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let stopper = shared.clone();
    let shared_stop = &stopper.stop;
    let t = std::thread::Builder::new()
        .name("zenlink-audio".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut first = Some(tx);
            while !shared.stop.load(Relaxed) {
                match Stream::open() {
                    Ok(s) => {
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Ok(()));
                        }
                        shared.output_ms.store(s.latency_ms.to_bits(), Relaxed);
                        shared.running.store(true, Relaxed);
                        log::info!(
                            "sound: WASAPI shared, buffer {} frames, output latency {:.1} ms",
                            s.size,
                            s.latency_ms
                        );
                        if let Err(e) = s.run(&shared) {
                            log::warn!("sound: output stopped: {e}");
                        }
                        shared.running.store(false, Relaxed);
                        shared.buf.lock().unwrap().clear();
                    }
                    Err(e) => {
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Err(e.to_string()));
                            break;
                        }
                        log::warn!("sound: reopening the output failed: {e}");
                    }
                }
                if !shared.stop.load(Relaxed) {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
            unsafe { CoUninitialize() };
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(t),
        Ok(Err(e)) => {
            let _ = t.join();
            Err(e)
        }
        Err(_) => {
            // The thread may still open later: make it stop instead of playing on its own.
            shared_stop.store(true, Relaxed);
            Err("the sound output did not open".into())
        }
    }
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    size: u32,
    latency_ms: f32,
}

impl Stream {
    fn open() -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: CHANNELS as u16,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 4,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            // 0 = the engine's own period (about 10 ms): the lowest shared-mode latency.
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &wfx, None)?;
            let event = CreateEventW(None, false, false, None)?;
            let rest = (|| -> Result<_> {
                client.SetEventHandle(event)?;
                let size = client.GetBufferSize()?;
                let render: IAudioRenderClient = client.GetService()?;
                // Start with a buffer of silence.
                render.GetBuffer(size)?;
                render.ReleaseBuffer(size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                client.Start()?;
                Ok((size, render))
            })();
            let (size, render) = match rest {
                Ok(v) => v,
                Err(e) => {
                    // A failed open is retried every 300 ms: do not leak an event each time.
                    let _ = CloseHandle(event);
                    return Err(e);
                }
            };
            let stream_latency = client.GetStreamLatency().unwrap_or(0) as f32 / 10_000.0;
            let latency_ms = stream_latency + size as f32 * 1000.0 / RATE as f32;
            Ok(Self {
                client,
                render,
                event,
                size,
                latency_ms,
            })
        }
    }

    fn run(&self, shared: &Shared) -> Result<()> {
        while !shared.stop.load(Relaxed) {
            unsafe {
                if WaitForSingleObject(self.event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                let padding = self.client.GetCurrentPadding()?;
                let avail = self.size.saturating_sub(padding);
                if avail == 0 {
                    continue;
                }
                let p = self.render.GetBuffer(avail)?;
                let out = std::slice::from_raw_parts_mut(p as *mut i16, samples(avail as usize));
                shared.render(out);
                self.render.ReleaseBuffer(avail, 0)?;
            }
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}
