//! Linux audio output using cpal (PipeWire, PulseAudio, ALSA).
//!
//! Spawns an audio thread that requests a stereo 48 kHz output stream.
//! If the hardware natively requires f32 samples, it renders i16 into
//! an intermediate scratch buffer and converts them to f32.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

use crate::buffer::{CHANNELS, RATE};
use crate::Shared;

pub(crate) fn spawn(shared: Arc<Shared>) -> Result<JoinHandle<()>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::Builder::new()
        .name("zenlink-audio-linux".into())
        .spawn(move || {
            let host = cpal::default_host();
            let mut first = Some(tx);

            while !shared.stop.load(Relaxed) {
                let device = match host.default_output_device() {
                    Some(d) => d,
                    None => {
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Err("no default audio output device found on Linux".into()));
                            break;
                        }
                        log::warn!("sound: no default audio output device available");
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };

                let config = match device.default_output_config() {
                    Ok(c) => c,
                    Err(e) => {
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Err(format!("failed to get default audio config: {e}")));
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };

                let sample_format = config.sample_format();
                let stream_config = StreamConfig {
                    channels: CHANNELS as u16,
                    sample_rate: cpal::SampleRate(RATE),
                    buffer_size: cpal::BufferSize::Default,
                };

                let err_fn = |err| {
                    log::warn!("sound: cpal stream error: {err}");
                };

                let shared_clone = shared.clone();
                let stream_res = match sample_format {
                    SampleFormat::I16 => {
                        let sh = shared_clone.clone();
                        device.build_output_stream(
                            &stream_config,
                            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                                sh.render(data);
                            },
                            err_fn,
                            None,
                        )
                    }
                    SampleFormat::F32 => {
                        let sh = shared_clone.clone();
                        let mut scratch: Vec<i16> = Vec::new();
                        device.build_output_stream(
                            &stream_config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                scratch.resize(data.len(), 0);
                                sh.render(&mut scratch);
                                for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                                    *dst = src as f32 / 32768.0;
                                }
                            },
                            err_fn,
                            None,
                        )
                    }
                    _ => {
                        let sh = shared_clone.clone();
                        let mut scratch: Vec<i16> = Vec::new();
                        device.build_output_stream(
                            &stream_config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                scratch.resize(data.len(), 0);
                                sh.render(&mut scratch);
                                for (dst, &src) in data.iter_mut().zip(scratch.iter()) {
                                    *dst = src as f32 / 32768.0;
                                }
                            },
                            err_fn,
                            None,
                        )
                    }
                };

                match stream_res {
                    Ok(stream) => {
                        if let Err(e) = stream.play() {
                            if let Some(tx) = first.take() {
                                let _ = tx.send(Err(format!("failed to play audio stream: {e}")));
                                break;
                            }
                            log::warn!("sound: failed to start stream: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }

                        if let Some(tx) = first.take() {
                            let _ = tx.send(Ok(()));
                        }
                        shared.running.store(true, Relaxed);
                        shared.output_ms.store(20.0f32.to_bits(), Relaxed);
                        log::info!("sound: Linux cpal audio backend running at {} Hz (stereo)", RATE);

                        while !shared.stop.load(Relaxed) {
                            std::thread::sleep(Duration::from_millis(200));
                        }

                        shared.running.store(false, Relaxed);
                        shared.buf.lock().unwrap().clear();
                        break;
                    }
                    Err(e) => {
                        if let Some(tx) = first.take() {
                            let _ = tx.send(Err(format!("failed to build output stream: {e}")));
                            break;
                        }
                        log::warn!("sound: error creating audio stream: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(t),
        Ok(Err(e)) => {
            let _ = t.join();
            Err(e)
        }
        Err(_) => Err("the sound output did not open in time".into()),
    }
}
