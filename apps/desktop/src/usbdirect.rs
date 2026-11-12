//! ZenLink Studio over the cable in the window: the Rust capture session (zenlink-apple
//! usbdirect) feeds the decoder and the sound output directly (backend castmanager + hub.py).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenlink_apple::usbdirect::{h264, session, wire::Format};
use zenlink_core::api::{Codec, CodecInfo};
use zenlink_core::source::{RecordStream, VideoSource};
use zenlink_core::stream::Record;

const GOP_CAP: usize = 3600;
/// hub.py: the picture size the device is asked for.
const DISPLAY: (u32, u32) = (3840, 2160);

/// Why a listed device can't start (castmanager _NEED_MESSAGES, same text for tr_backend).
const NEED: &[(&str, &str)] = &[
    ("bind", "ZenLink Studio needs its driver on this device. Press Install ZenLink Studio driver in Settings > \
              Connection & Drivers, or switch to Apple Driver mode."),
    ("package", "ZenLink Studio uses libusb-win32, which isn't on this PC. Install TikTok LIVE Studio's iOS driver \
                 or libusb-win32 once, or switch to Apple Driver mode."),
    ("removefilter", "A libusb0 filter is stacked on Apple's driver. Press Remove libusb0 filter in Settings > \
                      Connection & Drivers first."),
    ("mode", "Choose a connection mode first."),
];

static SHARED: Mutex<Inner> = Mutex::new(Inner::new());
/// Studio over the cable is the chosen mode (last state poll).
static WANTED: AtomicBool = AtomicBool::new(false);
/// The backend's device list and pick, from the last state poll.
static DEVICES: Mutex<(Vec<Value>, Option<String>)> = Mutex::new((Vec::new(), None));

struct Inner {
    running: bool,
    control: Option<session::Control>,
    phase: &'static str,
    detail: String,
    error: Option<String>,
    device: Value,
    info: Option<CodecInfo>,
    format: Option<(u32, Vec<u8>)>,
    size: (u32, u32),
    gop: Vec<Vec<u8>>,
    key_in_gop: bool,
    tx: Option<SyncSender<Record>>,
    player: Option<zenlink_audio::Player>,
}

impl Inner {
    const fn new() -> Self {
        Self {
            running: false,
            control: None,
            phase: "idle",
            detail: String::new(),
            error: None,
            device: Value::Null,
            info: None,
            format: None,
            size: (0, 0),
            gop: Vec::new(),
            key_in_gop: false,
            tx: None,
            player: None,
        }
    }
}

pub fn running() -> bool {
    SHARED.lock().unwrap().running
}

struct Feed;

impl session::Sink for Feed {
    fn product(&self, product_type: &str) {
        let serial = SHARED.lock().unwrap().device["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        crate::backend::remember_product(&serial, product_type);
    }

    fn state(&self, phase: &'static str, detail: &str) {
        let mut g = SHARED.lock().unwrap();
        // The window's checklist knows connecting / activating / streaming.
        g.phase = if matches!(phase, "waiting" | "handshake") {
            "activating"
        } else {
            phase
        };
        g.detail = detail.into();
    }

    fn format(&self, f: &Format) {
        let mut g = SHARED.lock().unwrap();
        if g.format
            .as_ref()
            .is_some_and(|(c, cfg)| *c == f.codec && *cfg == f.config)
        {
            return; // the device repeats it with every keyframe
        }
        g.format = Some((f.codec, f.config.clone()));
        let hevc = f.is_hevc();
        let info = CodecInfo {
            codec: if hevc { Codec::Hevc } else { Codec::H264 },
            description: if hevc {
                f.config.clone()
            } else {
                h264::rewrite_avcc(&f.config)
            },
            keyframes_on_request: false,
            display: None,
        };
        log::info!(
            "usb direct: {} {}x{}",
            if hevc { "HEVC" } else { "H.264" },
            f.width,
            f.height
        );
        g.gop.clear();
        g.key_in_gop = false;
        g.size = (f.width, f.height);
        if let Some(tx) = &g.tx {
            if tx.try_send(Record::Config(info.clone())).is_err() {
                g.tx = None;
            }
        }
        g.info = Some(info);
    }

    fn video(&self, au: Vec<u8>) {
        let mut g = SHARED.lock().unwrap();
        let hevc = match g.info.as_ref() {
            Some(i) => matches!(i.codec, Codec::Hevc),
            None => return,
        };
        let (mut data, key) = crate::airplay::clean(&au, hevc);
        if data.is_empty() {
            return;
        }
        if !hevc {
            data = h264::rewrite_access_unit(&data).unwrap_or(data);
        }
        if key {
            g.gop.clear();
            g.key_in_gop = true;
        }
        if g.key_in_gop && g.gop.len() < GOP_CAP {
            g.gop.push(data.clone());
        }
        if let Some(tx) = &g.tx {
            // A full queue means the decoder fell behind: drop the session, it reopens from the GOP.
            if tx
                .try_send(Record::Frame {
                    key,
                    rebuild: false,
                    data,
                })
                .is_err()
            {
                g.tx = None;
            }
        }
    }

    fn audio(&self, pcm: &[u8]) {
        let mut g = SHARED.lock().unwrap();
        if g.phase != "streaming" || !crate::apple::audio_to_pc() {
            return;
        }
        if g.player.is_none() {
            match zenlink_audio::Player::start() {
                Ok(p) => {
                    p.set_jitter_ms(0.0); // the cable: lowest latency
                    g.player = Some(p);
                }
                Err(e) => {
                    log::warn!("usb direct: no sound output ({e}); picture only");
                    return;
                }
            }
        }
        let (volume, muted) = crate::apple::audio_level();
        let p = g.player.as_mut().unwrap();
        p.set_volume(volume);
        p.set_muted(muted);
        p.feed(pcm);
    }
}

/// Called with each backend state: remembers the mode and the device list, and lays the
/// session's state over the backend's while Studio over the cable is the mode.
pub fn overlay(st: &mut Value) {
    let conn = |k: &str| {
        st.pointer(&format!("/connection/{k}"))
            .and_then(Value::as_str)
    };
    let wanted = conn("active") == Some("usbdirect") && conn("link") != Some("wireless");
    if WANTED.swap(wanted, Relaxed) && !wanted {
        // The mode changed away. Off this (state poll) thread: stop waits up to 10 s.
        std::thread::spawn(stop);
    }
    if !wanted {
        return;
    }
    *DEVICES.lock().unwrap() = (
        st.get("devices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        st.get("selected").and_then(Value::as_str).map(String::from),
    );
    if let Some(c) = SHARED.lock().unwrap().control.as_ref() {
        c.audio_to_pc.store(crate::apple::audio_to_pc(), Relaxed);
    }
    let g = SHARED.lock().unwrap();
    if !g.running && g.error.is_none() && g.phase == "idle" {
        return;
    }
    let streaming = g.phase == "streaming";
    let codec = matches!(g.info.as_ref().map(|i| &i.codec), Some(Codec::Hevc));
    let extra = json!({
        "phase": if g.error.is_some() { "error" } else { g.phase },
        "detail": g.detail,
        "error": g.error,
        "backend": g.running.then_some("usbdirect"),
        "device": g.device,
        "rotation": null,
        "control": g.running.then(|| json!({ "level": "mirroring", "video": streaming, "control": false,
                                              "experimental": false, "note": "" })),
        "capture": g.running.then(|| json!({ "backend": "usbdirect", "codec": if codec { "HEVC" } else { "H.264" },
                                              "width": g.size.0, "height": g.size.1 })),
    });
    drop(g);
    if let (Some(dst), Value::Object(src)) = (st.as_object_mut(), extra) {
        dst.extend(src);
    }
}

/// Short advice for errors the user can act on (castmanager.py _friendly_error).
fn friendly(text: &str) -> String {
    let low = text.to_lowercase();
    if ["developer mode", "libusb0", "settings >"]
        .iter()
        .any(|k| low.contains(k))
    {
        text.into()
    } else if low.contains("pair") || low.contains("trust") {
        "Unlock the device and tap Trust, then try again.".into()
    } else if ["not connected", "not found", "no device"]
        .iter()
        .any(|k| low.contains(k))
    {
        "The device was disconnected. Plug it back in and try again.".into()
    } else if ["in use", "busy", "access"].iter().any(|k| low.contains(k)) {
        "Another app is using the device's screen (Xcode, QuickTime, TikTok LIVE Studio). Close it and try again."
            .into()
    } else {
        text.into()
    }
}

fn fail(msg: &str) {
    let mut g = SHARED.lock().unwrap();
    g.phase = "error";
    g.error = Some(msg.into());
}

/// Start button. `false`: not Studio over the cable, the caller handles it.
pub fn start() -> bool {
    if !WANTED.load(Relaxed) {
        return false;
    }
    if running() {
        return true;
    }
    let (devices, selected) = DEVICES.lock().unwrap().clone();
    let id = |d: &Value| d.get("id").and_then(Value::as_str).map(String::from);
    let target = devices
        .iter()
        .find(|d| id(d).is_some() && id(d) == selected)
        .or_else(|| {
            devices
                .iter()
                .find(|d| d.get("ready").and_then(Value::as_bool) == Some(true))
        })
        .or_else(|| devices.first());
    let Some(target) = target.cloned() else {
        fail("Connect an iPhone or iPad with a USB cable.");
        return true;
    };
    if let Some(need) = target.get("need").and_then(Value::as_str) {
        let msg = NEED
            .iter()
            .find(|(k, _)| *k == need)
            .map_or("Unlock the device and tap Trust, then try again.", |m| m.1);
        fail(msg);
        return true;
    }
    let serial = id(&target).unwrap_or_default();
    let (s, control) = session::Session::new(
        &serial,
        Arc::new(Feed),
        DISPLAY,
        crate::apple::audio_to_pc(),
    );
    {
        let mut g = SHARED.lock().unwrap();
        *g = Inner::new();
        g.running = true;
        g.control = Some(control);
        g.phase = "connecting";
        g.detail = "Opening the device through libusb0".into();
        g.device = json!({ "id": serial, "name": target.get("name"), "wifi": false, "family": target.get("family") });
    }
    std::thread::Builder::new()
        .name("usbdirect".into())
        .spawn(move || {
            let r = s.run();
            log::info!("usb direct: session ended ({r:?})");
            let mut g = SHARED.lock().unwrap();
            g.running = false;
            g.control = None;
            g.player = None;
            g.tx = None;
            match r {
                Ok(()) => (g.phase, g.detail) = ("idle", "Casting stopped".into()),
                Err(e) => (g.phase, g.error) = ("error", Some(friendly(&e))),
            }
        })
        .expect("usbdirect thread");
    true
}

/// Stop button. `false`: no session here. Waits up to 10 s for the device to be restored.
pub fn stop() -> bool {
    let Some(c) = SHARED.lock().unwrap().control.clone() else {
        return false;
    };
    c.stop.store(true, Relaxed);
    let t = Instant::now();
    while running() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

pub struct UsbSource;

impl VideoSource for UsbSource {
    fn open(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                let mut g = SHARED.lock().unwrap();
                if let Some(info) = g.info.clone().filter(|_| g.key_in_gop) {
                    let (tx, rx) = sync_channel(600);
                    g.tx = Some(tx);
                    let pending = g
                        .gop
                        .iter()
                        .enumerate()
                        .map(|(i, d)| Record::Frame {
                            key: i == 0,
                            rebuild: false,
                            data: d.clone(),
                        })
                        .collect();
                    return Some((info, Box::new(UsbRecords { pending, rx })));
                }
            }
            if !running() || Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn finished(&self) -> bool {
        !running()
    }
}

struct UsbRecords {
    pending: VecDeque<Record>,
    rx: Receiver<Record>,
}

impl RecordStream for UsbRecords {
    fn next_record(&mut self) -> Option<Record> {
        if let Some(r) = self.pending.pop_front() {
            return Some(r);
        }
        loop {
            match self.rx.recv_timeout(Duration::from_secs(1)) {
                Ok(r) => return Some(r),
                Err(RecvTimeoutError::Timeout) if running() => continue,
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::friendly;

    #[test]
    fn errors_get_short_advice() {
        assert_eq!(
            friendly("InvalidHostID: pairing gone"),
            "Unlock the device and tap Trust, then try again."
        );
        assert!(friendly("usb: no device").starts_with("The device was disconnected"));
        assert!(friendly("resource busy").starts_with("Another app"));
        let own = "Install libusb0 first (Settings > Connection & Drivers).";
        assert_eq!(friendly(own), own);
        assert_eq!(friendly("odd"), "odd");
    }
}
