//! Linux video view and frame rendering.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use zenlink_core::nal::{parse_config, to_annexb, ParamSets};
use zenlink_core::source::VideoSource;
use zenlink_core::stream::Record;

use crate::linux_decoder::LinuxDecoder;
use crate::stats::{RateMeter, Stats, StatsSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Contain = 0,
    Cover = 1,
    Fill = 2,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerEvent {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    Back,
    Secondary,
    Middle,
    Wheel(f32, f32, f32),
}

pub type Hole = (i32, i32, i32, i32);

pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

struct FrameBuffer {
    width: u32,
    height: u32,
    data: Arc<Vec<u8>>,
}

type PointerCb = Box<dyn Fn(PointerEvent) + Send + 'static>;

pub struct VideoView {
    parent: isize,
    stats: Arc<Stats>,
    meter: Arc<Mutex<RateMeter>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    latest_frame: Arc<Mutex<Option<FrameBuffer>>>,
    frame_seq: Arc<AtomicU64>,
    rotation: Arc<AtomicU32>,
    fit: Arc<AtomicU8>,
    bg: Arc<AtomicU32>,
    crop: [Arc<AtomicU32>; 4],
    pointer: Arc<Mutex<Option<PointerCb>>>,
}

impl VideoView {
    pub fn new(parent: isize) -> Result<Self, String> {
        let stats = Arc::new(Stats::default());
        let meter = Arc::new(Mutex::new(RateMeter::new(&stats)));
        Ok(Self {
            parent,
            stats,
            meter,
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            latest_frame: Arc::new(Mutex::new(None)),
            frame_seq: Arc::new(AtomicU64::new(0)),
            rotation: Arc::new(AtomicU32::new(0)),
            fit: Arc::new(AtomicU8::new(0)),
            bg: Arc::new(AtomicU32::new(0)),
            crop: [
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
            ],
            pointer: Arc::new(Mutex::new(None)),
        })
    }

    pub fn hwnd(&self) -> isize {
        self.parent
    }

    pub fn snapshot(&self, _timeout: Duration) -> Option<Snapshot> {
        let guard = self.latest_frame.lock().ok()?;
        guard.as_ref().map(|b| {
            let mut bgra = (*b.data).clone();
            for px in bgra.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            Snapshot {
                width: b.width,
                height: b.height,
                bgra,
            }
        })
    }

    pub fn poll_frame(&self, last_seq: u64) -> Option<(u64, u32, u32, Arc<Vec<u8>>)> {
        let cur = self.frame_seq.load(Relaxed);
        if cur == last_seq || cur == 0 {
            return None;
        }
        let guard = self.latest_frame.lock().ok()?;
        let b = guard.as_ref()?;
        Some((cur, b.width, b.height, Arc::clone(&b.data)))
    }

    pub fn send_pointer(&self, e: PointerEvent) {
        if let Ok(g) = self.pointer.lock() {
            if let Some(cb) = g.as_ref() {
                cb(e);
            }
        }
    }

    pub fn set_bounds(&self, _x: i32, _y: i32, _w: i32, _h: i32, _visible: bool, _radius: i32) {
        // Bounds are managed by Slint layout on Linux
    }

    pub fn set_holes(&self, _holes: &[Hole]) {
        // Hole punch not needed on Linux desktop
    }

    pub fn set_rotation(&self, deg: u32) {
        self.rotation.store(deg, Relaxed);
    }

    pub fn set_fit(&self, fit: Fit) {
        self.fit.store(fit as u8, Relaxed);
    }

    pub fn set_crop(&self, l: u32, t: u32, r: u32, b: u32) {
        self.crop[0].store(l, Relaxed);
        self.crop[1].store(t, Relaxed);
        self.crop[2].store(r, Relaxed);
        self.crop[3].store(b, Relaxed);
    }

    pub fn source_size(&self) -> (u32, u32) {
        (
            self.stats.width.load(Relaxed),
            self.stats.height.load(Relaxed),
        )
    }

    pub fn shown_size(&self) -> (u32, u32) {
        let (w, h) = self.source_size();
        let deg = self.rotation.load(Relaxed);
        if deg == 90 || deg == 270 {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub fn set_background(&self, rgb: u32) {
        self.bg.store(rgb, Relaxed);
    }

    pub fn on_pointer(&self, cb: impl Fn(PointerEvent) + Send + 'static) {
        if let Ok(mut g) = self.pointer.lock() {
            *g = Some(Box::new(cb));
        }
    }

    pub fn frame_size(&self) -> (u32, u32) {
        self.shown_size()
    }

    pub fn stats(&self) -> StatsSnapshot {
        if let Ok(mut meter) = self.meter.lock() {
            meter.sample(&self.stats)
        } else {
            StatsSnapshot::default()
        }
    }

    pub fn start_source(&mut self, mut source: Box<dyn VideoSource>) {
        self.stop();

        let stop = self.stop.clone();
        stop.store(false, Relaxed);
        let stats = self.stats.clone();
        let latest = self.latest_frame.clone();
        let frame_seq = self.frame_seq.clone();

        let handle = std::thread::Builder::new()
            .name("zenlink-video-linux".into())
            .spawn(move || {
                while !stop.load(Relaxed) && !source.finished() {
                    let Some((info, mut stream)) = source.open() else {
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    };

                    let mut ps: ParamSets = parse_config(&info.codec, &info.description).unwrap_or_default();
                    let mut decoder = match LinuxDecoder::new(&info.codec) {
                        Ok(d) => d,
                        Err(e) => {
                            log::warn!("linux video: {e}");
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    };

                    log::info!("linux video: started decoder for {:?}", info.codec);
                    stats.hardware.store(false, Relaxed);

                    let mut got_key = false;

                    while !stop.load(Relaxed) {
                        let rec = match stream.next_record() {
                            Some(r) => r,
                            None => break,
                        };

                        match rec {
                            Record::Config(cfg) => {
                                ps = parse_config(&cfg.codec, &cfg.description).unwrap_or_default();
                            }
                            Record::Frame { key, data, .. } => {
                                if key {
                                    got_key = true;
                                }
                                if !got_key {
                                    continue;
                                }

                                stats.bytes.fetch_add(data.len() as u64, Relaxed);
                                let start_t = Instant::now();
                                let au = to_annexb(&data, &ps, key);

                                match decoder.decode(&au) {
                                    Ok(Some((w, h, rgba))) => {
                                        stats.width.store(w, Relaxed);
                                        stats.height.store(h, Relaxed);
                                        stats.decoded.fetch_add(1, Relaxed);
                                        stats.presented.fetch_add(1, Relaxed);
                                        stats.decode_us.store(start_t.elapsed().as_micros() as u32, Relaxed);

                                        let arc = Arc::new(rgba.to_vec());
                                        if let Ok(mut g) = latest.lock() {
                                            *g = Some(FrameBuffer {
                                                width: w,
                                                height: h,
                                                data: arc,
                                            });
                                        }
                                        frame_seq.fetch_add(1, Relaxed);
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        stats.errors.fetch_add(1, Relaxed);
                                        log::trace!("decode error: {e}");
                                    }
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .ok();

        self.thread = handle;
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        self.stop();
    }
}
