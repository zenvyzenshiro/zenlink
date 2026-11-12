//! ZenLink Studio over Wi-Fi in the window: the Rust AirPlay receiver (zenlink-airplay) feeds
//! the decoder directly (the backend's Python receiver was removed in v2.5.12).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenlink_airplay::{server, Sink, State};
use zenlink_core::api::{Codec, CodecInfo};
use zenlink_core::source::{RecordStream, VideoSource};
use zenlink_core::stream::Record;

const NAME: &str = "ZenLinkStudio";
/// iOS sends a keyframe only when mirroring starts: a late viewer gets the whole GOP (hub.py).
const GOP_CAP: usize = 3600;

static SERVER: Mutex<Option<server::Server>> = Mutex::new(None);
static SHARED: Mutex<Inner> = Mutex::new(Inner::new());
/// Studio Wi-Fi is the chosen mode (last state poll).
static WANTED: AtomicBool = AtomicBool::new(false);
/// Stop pressed: stay off until Start (castmanager `_airplay_paused`).
static PAUSED: AtomicBool = AtomicBool::new(false);
/// The receiver serves an Apple Driver Hybrid session, not Studio.
static HYBRID: AtomicBool = AtomicBool::new(false);

pub fn running() -> bool {
    SERVER.lock().unwrap().is_some()
}

struct Inner {
    phase: &'static str,
    detail: String,
    error: Option<String>,
    info: Option<CodecInfo>,
    size: (u32, u32),
    gop: Vec<Vec<u8>>,
    key_in_gop: bool,
    tx: Option<SyncSender<Record>>,
    /// The sound thread's input; dropping it ends the thread.
    sound: Option<Sender<Vec<u8>>>,
}

impl Inner {
    const fn new() -> Self {
        Self {
            phase: "idle",
            detail: String::new(),
            error: None,
            info: None,
            size: (0, 0),
            gop: Vec::new(),
            key_in_gop: false,
            tx: None,
            sound: None,
        }
    }

    fn reset(&mut self) {
        *self = Self {
            phase: self.phase,
            detail: std::mem::take(&mut self.detail),
            ..Self::new()
        };
    }
}

struct Feed;

impl Sink for Feed {
    fn state(&self, s: State) {
        let mut g = SHARED.lock().unwrap();
        match s {
            State::Waiting => {
                g.reset(); // the device left: end the decoder's session
                g.phase = "activating";
                g.detail = format!(
                    "On your iPhone/iPad open Control Center, tap Screen Mirroring and choose \
                     “{NAME}”. Both must be on the same Wi-Fi network."
                );
            }
            State::Connected(name) => {
                g.phase = "activating";
                g.detail = format!("{name} is connecting over AirPlay…");
            }
            State::Streaming => g.phase = "streaming",
        }
    }

    fn audio(&self, au: Vec<u8>) {
        let mut g = SHARED.lock().unwrap();
        let sound = g.sound.get_or_insert_with(|| {
            let (tx, rx) = channel();
            crate::apple::play_sound(
                rx,
                true,
                &zenlink_airplay::audio::ASC,
                zenlink_airplay::audio::RATE,
            );
            tx
        });
        let _ = sound.send(au); // a thread that ended (no decoder / output) stays ended until Waiting
    }

    fn config(&self, description: Vec<u8>, hevc: bool, width: u32, height: u32) {
        let info = CodecInfo {
            codec: if hevc { Codec::Hevc } else { Codec::H264 },
            description,
            keyframes_on_request: false,
            display: None,
        };
        let mut g = SHARED.lock().unwrap();
        g.gop.clear();
        g.key_in_gop = false;
        g.size = (width, height);
        if let Some(tx) = &g.tx {
            if tx.try_send(Record::Config(info.clone())).is_err() {
                g.tx = None;
            }
        }
        g.info = Some(info);
    }

    fn frame(&self, au: Vec<u8>) {
        let mut g = SHARED.lock().unwrap();
        let hevc = matches!(g.info.as_ref().map(|i| &i.codec), Some(Codec::Hevc));
        let (data, key) = clean(&au, hevc);
        if data.is_empty() {
            return;
        }
        if key {
            g.gop.clear();
            g.key_in_gop = true;
        }
        if g.key_in_gop {
            if g.gop.len() < GOP_CAP {
                g.gop.push(data.clone());
            } else {
                // A cut GOP would replay as a frozen picture: wait for the next key frame.
                g.gop.clear();
                g.key_in_gop = false;
            }
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
}

/// Drops SEI and reports whether the access unit holds an IDR / IRAP picture.
/// shortcut: no H.264 SPS low-latency rewrite (h264.py); the decoder sets MF_LOW_LATENCY. Add it
/// if AirPlay shows a delay the backend's receiver did not.
pub(crate) fn clean(au: &[u8], hevc: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(au.len());
    let mut key = false;
    let mut i = 0;
    while i + 4 < au.len() {
        let n = u32::from_be_bytes(au[i..i + 4].try_into().unwrap()) as usize;
        let Some(nal) = au.get(i + 4..i + 4 + n).filter(|x| !x.is_empty()) else {
            break;
        };
        i += 4 + n;
        let (sei, irap) = if hevc {
            let t = (nal[0] >> 1) & 0x3F;
            (t == 39 || t == 40, (16..=21).contains(&t))
        } else {
            let t = nal[0] & 0x1F;
            (t == 6, t == 5)
        };
        if sei {
            continue;
        }
        key |= irap;
        out.extend_from_slice(&(n as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    (out, key)
}

fn start_server() -> Result<(), String> {
    let dll = crate::aac::find_native("playfair", "playfair.dll")
        .or_else(|| crate::aac::unpack_embedded("playfair", crate::aac::PLAYFAIR))
        .ok_or("playfair.dll not found")?;
    zenlink_airplay::fairplay::load(&dll)?;
    let cfg = server::Config {
        name: NAME.into(),
        hevc: false, // castmanager: Studio Wi-Fi asks for H.264
        data_dir: crate::backend_data_dir().ok_or("no data folder")?,
        port: server::DEFAULT_PORT,
        advertise: true,
    };
    let s = crate::apple::block_on(server::Server::start(cfg, Arc::new(Feed)))?;
    *SHARED.lock().unwrap() = Inner::new();
    Feed.state(State::Waiting);
    *SERVER.lock().unwrap() = Some(s);
    Ok(())
}

fn stop_server() {
    if let Some(s) = SERVER.lock().unwrap().take() {
        s.stop();
    }
    *SHARED.lock().unwrap() = Inner::new();
}

/// iOS 17-26 in Apple Driver mode: the picture and sound over AirPlay while the Apple session
/// (apple.rs) holds the touch link. Dropping it stops the receiver.
pub struct Hybrid;

impl Hybrid {
    /// Called from the session's own runtime: the server starts on a plain thread.
    pub fn start() -> Result<Self, String> {
        stop_server();
        std::thread::spawn(start_server)
            .join()
            .map_err(|_| "AirPlay start panicked".to_string())??;
        HYBRID.store(true, Relaxed);
        Ok(Self)
    }
}

impl Drop for Hybrid {
    fn drop(&mut self) {
        HYBRID.store(false, Relaxed);
        stop_server();
    }
}

/// Called with each backend state: runs the receiver while Studio Wi-Fi is the mode and lays
/// its state over the backend's (or over the Apple session's, in Hybrid).
pub fn overlay(st: &mut Value) {
    if HYBRID.load(Relaxed) {
        let g = SHARED.lock().unwrap();
        if st.get("phase").and_then(Value::as_str) != Some("error") {
            st["phase"] = g.phase.into();
            st["detail"] = g.detail.clone().into();
        }
        return;
    }
    let conn = |k: &str| {
        st.pointer(&format!("/connection/{k}"))
            .and_then(Value::as_str)
    };
    let wanted = conn("active") == Some("usbdirect") && conn("link") == Some("wireless");
    if !wanted && WANTED.load(Relaxed) {
        PAUSED.store(false, Relaxed); // a mode change clears Stop, as set_link does
    }
    WANTED.store(wanted, Relaxed);
    let on = wanted && !PAUSED.load(Relaxed);
    if on != running() {
        if on {
            if let Err(e) = start_server() {
                log::warn!("airplay: {e}");
                PAUSED.store(true, Relaxed);
                SHARED.lock().unwrap().error = Some(e);
            }
        } else {
            let error = SHARED.lock().unwrap().error.take();
            stop_server();
            SHARED.lock().unwrap().error = error;
        }
    }
    let g = SHARED.lock().unwrap();
    if !wanted || (!running() && g.error.is_none()) {
        return;
    }
    let live = matches!(g.phase, "activating" | "streaming");
    let extra = json!({
        "phase": if g.error.is_some() { "error" } else { g.phase },
        "detail": g.detail,
        "error": g.error,
        "backend": "usbdirect",
        "device": { "id": "airplay", "name": "AirPlay", "wifi": true },
        "capture": live.then(|| json!({ "backend": "usbdirect", "codec": "H.264", "source": "AirPlay (Wi-Fi)",
                                        "width": g.size.0, "height": g.size.1 })),
    });
    if let (Some(dst), Value::Object(src)) = (st.as_object_mut(), extra) {
        dst.extend(src);
    }
}

/// Start button. `false`: not Studio Wi-Fi in Rust, the caller handles it.
pub fn start() -> bool {
    if !WANTED.load(Relaxed) {
        return false;
    }
    PAUSED.store(false, Relaxed);
    SHARED.lock().unwrap().error = None; // the next state poll starts the receiver
    true
}

/// Stop button. `false`: the receiver is not running here.
pub fn stop() -> bool {
    if !running() || HYBRID.load(Relaxed) {
        return false; // Hybrid: the Apple session's Stop ends the receiver too
    }
    PAUSED.store(true, Relaxed);
    stop_server();
    true
}

pub struct AirSource;

impl VideoSource for AirSource {
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
                    return Some((info, Box::new(AirRecords { pending, rx })));
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

struct AirRecords {
    pending: VecDeque<Record>,
    rx: Receiver<Record>,
}

impl RecordStream for AirRecords {
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
    use super::clean;

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], n].concat())
            .collect()
    }

    #[test]
    fn strips_sei_and_finds_keyframes() {
        let (d, key) = clean(&au(&[&[0x06, 1], &[0x65, 2, 3]]), false);
        assert_eq!((d, key), (au(&[&[0x65, 2, 3]]), true));
        assert!(!clean(&au(&[&[0x41, 9]]), false).1);
        let (d, key) = clean(&au(&[&[39 << 1, 1, 0], &[19 << 1, 1, 7]]), true);
        assert_eq!((d, key), (au(&[&[19 << 1, 1, 7]]), true));
        assert_eq!(clean(&[0, 0, 0, 9, 1], false).0, Vec::<u8>::new()); // truncated
    }
}
