//! Android mirroring glue (phase A3): turns the scrcpy video stream into the
//! decoder records `zenlink-video` already plays, so Android and Apple share
//! one decoder, one renderer and the OBS-safe `VideoView`.
//!
//! - [`DeviceSource`]: a real device through the ADB server (scrcpy server).
//! - [`FixtureSource`]: a recorded scrcpy stream (`ZL_ANDROID_FIXTURE`), for
//!   tests without a device. It is paced by the recorded timestamps.
//!
//! Control (phase A4): the scrcpy control socket carries touch, scroll,
//! keys, text and the PC clipboard (Ctrl+V); see [`Shared::pointer`],
//! [`Shared::action`] and [`key_input`].
//!
//! Sound (phase A4): raw PCM from the scrcpy audio socket goes through the
//! v2.2.31 jitter buffer (`zenlink-audio`) to WASAPI. Low-latency mode:
//! sound plays as it arrives, the picture as it decodes (the Apple cable
//! path works the same way).

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zenlink_android::adb::{AdbClient, Target};
use zenlink_android::control::{
    self, keycode, meta, Control, ACTION_DOWN, ACTION_MOVE, ACTION_UP, POINTER_ID_GENERIC_FINGER,
};
/// Pointer ids of the two pinch fingers (not the mouse's -1 or the single finger's -2).
const PINCH_POINTERS: [i64; 2] = [10, 11];

use zenlink_android::encoders::{self, Attempt, Decoders, Encoder, Preference};
use zenlink_android::mirror::{
    codec_name, read_device_meta, Mirror, Packet, Session, Stopper, VideoOptions, VideoReader,
    CODEC_H264, CODEC_H265,
};
use zenlink_android::scrcpy::{SERVER_FILE, SERVER_SIZE};
use zenlink_core::api::{device_point, Codec, CodecInfo};
use zenlink_core::nal::{annexb_to_lengths, avcc_from_annexb, hvcc_from_annexb};
use zenlink_core::source::{RecordStream, VideoSource};
use zenlink_core::stream::Record;
use zenlink_video::PointerEvent;

/// After this many failed starts in a row the session gives up and shows the error.
const MAX_FAILURES: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Connecting,
    Streaming,
    Error,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Connecting => "connecting",
            Phase::Streaming => "streaming",
            Phase::Error => "error",
        }
    }
}

/// What the window shows about the Android session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    /// Name the scrcpy server reported (device meta), or the list's name.
    pub device: String,
    /// Last error, in English (logged; the UI shows its own bilingual text).
    pub error: String,
    /// Codec and encoder of the running (or last tried) start attempt;
    /// encoder empty = the server's default.
    pub codec: String,
    pub encoder: String,
    /// The attempt is not the first of the plan (Smart Encoder fallback).
    pub fell_back: bool,
    /// Xiaomi / Redmi / POCO (MIUI, HyperOS) with "USB debugging (Security settings)" off:
    /// the phone ignores mouse and keyboard input from adb.
    pub input_blocked: bool,
}

/// Whether the output of [`MI_INPUT_PROPS`] says the phone blocks adb input: a MIUI or
/// HyperOS version is set, and `persist.security.adbinput` is not 1.
fn mi_input_blocked(out: &str) -> bool {
    let l: Vec<&str> = out.lines().map(str::trim).collect();
    let get = |i: usize| l.get(i).copied().unwrap_or("");
    (!get(0).is_empty() || !get(1).is_empty()) && get(2) != "1"
}

const MI_INPUT_PROPS: &str = "getprop ro.miui.ui.version.name; getprop ro.mi.os.version.name; \
                              getprop persist.security.adbinput";

/// Shared between the decoder thread (source) and the window.
#[derive(Default)]
pub struct Shared {
    status: Mutex<Status>,
    stop: AtomicBool,
    stopper: Mutex<Option<Stopper>>,
    /// Sound latency (queue + output) in ms; 0 = no sound.
    audio_ms: AtomicU32,
    /// Write end of the control socket while a session streams.
    control: Mutex<Option<TcpStream>>,
    /// Video frame size from the last session packet (touch coordinates).
    frame_w: AtomicU32,
    frame_h: AtomicU32,
    /// Panel the bottom bar opened ("notifications" / "quick-settings"):
    /// pressing the same button again closes it, like the Apple bar.
    panel: Mutex<Option<&'static str>>,
    /// The phone's encoder list (Smart Encoder Detection), once read.
    encoders: Mutex<Option<Vec<Encoder>>>,
    /// Off screen (another device is shown): the session keeps running, its picture and
    /// sound are read and dropped, so coming back needs no reconnect.
    parked: AtomicBool,
    /// The running stream while parked, handed back on return (see [`Shared::park`]).
    held: Mutex<Option<(CodecInfo, Box<dyn RecordStream>)>>,
    /// Set by a return from parking, for the next open.
    resuming: AtomicBool,
    /// adb and serial of the running session (to open settings on the phone).
    adb: Mutex<Option<(AdbClient, String)>>,
}

/// The session the window's mouse and keyboard go to.
static ACTIVE: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

pub fn set_active(s: Option<Arc<Shared>>) {
    *ACTIVE.lock().unwrap() = s;
}

/// The Android session that takes input, if one streams with control.
pub fn active() -> Option<Arc<Shared>> {
    ACTIVE
        .lock()
        .unwrap()
        .clone()
        .filter(|s| !s.parked.load(Relaxed) && s.control.lock().unwrap().is_some())
}

/// Sound settings from the window (Settings › Audio, the mute button).
static AUDIO_TO_PC: AtomicBool = AtomicBool::new(true);
static AUDIO_MUTED: AtomicBool = AtomicBool::new(false);
/// f32 bits, 0.0..=1.0.
static AUDIO_VOLUME: AtomicU32 = AtomicU32::new(0x3f80_0000);

/// Called whenever the window pushes its sound settings. "Device only"
/// (`to_pc == false`) restarts a running session without audio capture (the
/// window does that), so the phone keeps its own sound.
pub fn set_audio(to_pc: bool, volume: f32, muted: bool) {
    AUDIO_TO_PC.store(to_pc, Relaxed);
    AUDIO_VOLUME.store(volume.clamp(0.0, 1.0).to_bits(), Relaxed);
    AUDIO_MUTED.store(muted, Relaxed);
}

impl Shared {
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// Sound latency in ms (queue + output), `None` without sound.
    pub fn audio_ms(&self) -> Option<u32> {
        Some(self.audio_ms.load(Relaxed)).filter(|&v| v > 0)
    }

    /// The encoder list read at session start, `None` until read or when
    /// the phone could not list them.
    pub fn encoders(&self) -> Option<Vec<Encoder>> {
        self.encoders.lock().unwrap().clone()
    }

    fn set(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    /// Sends control messages; a broken socket disables control until the
    /// next session. Never blocks longer than the socket's write timeout.
    pub fn send(&self, msgs: &[Control]) {
        let mut c = self.control.lock().unwrap();
        let Some(s) = c.as_mut() else { return };
        let mut buf = Vec::with_capacity(32 * msgs.len());
        for m in msgs {
            buf.extend_from_slice(&m.encode());
        }
        if let Err(e) = s.write_all(&buf) {
            log::info!("android: control socket closed: {e}");
            *c = None;
        }
    }

    fn frame(&self) -> Option<(u16, u16)> {
        let (w, h) = (self.frame_w.load(Relaxed), self.frame_h.load(Relaxed));
        (w > 0 && h > 0).then(|| (w.min(0xffff) as u16, h.min(0xffff) as u16))
    }

    /// Video frame size from the last session packet (0 before one).
    pub fn frame_size(&self) -> (u32, u32) {
        (self.frame_w.load(Relaxed), self.frame_h.load(Relaxed))
    }

    fn set_frame(&self, w: u32, h: u32) {
        self.frame_w.store(w, Relaxed);
        self.frame_h.store(h, Relaxed);
    }

    /// Mouse on the picture. `u, v` are as the user sees the picture;
    /// `rotation` is the rotation the window shows it with.
    pub fn pointer(self: &Arc<Self>, e: PointerEvent, rotation: u32) {
        if let PointerEvent::Pinch(u, v, d) = e {
            let sh = Arc::clone(self);
            crate::pinch::zoom(u, v, d, Arc::new(move |p, f| sh.fingers(p, f, rotation)));
            return;
        }
        let Some((w, h)) = self.frame() else { return };
        let at = |u: f32, v: f32| {
            let (x, y) = device_point(u, v, rotation);
            control::position(x as f32 / 65535.0, y as f32 / 65535.0, w, h)
        };
        let touch = |action, pos, pressure| Control::Touch {
            action,
            pointer: POINTER_ID_GENERIC_FINGER,
            pos,
            pressure,
            action_button: 0,
            buttons: 0,
        };
        match e {
            PointerEvent::Down(u, v) => {
                self.forget_panel();
                self.send(&[touch(ACTION_DOWN, at(u, v), 1.0)])
            }
            PointerEvent::Move(u, v) => self.send(&[touch(ACTION_MOVE, at(u, v), 1.0)]),
            PointerEvent::Up(u, v) => self.send(&[touch(ACTION_UP, at(u, v), 0.0)]),
            PointerEvent::Back | PointerEvent::Secondary => {
                self.forget_panel();
                self.back()
            }
            PointerEvent::Middle => self.send(&Control::key_tap(keycode::HOME, 0)),
            PointerEvent::Pinch(..) => {}
            PointerEvent::Wheel(u, v, notches) => self.send(&[Control::Scroll {
                pos: at(u, v),
                h: 0.0,
                v: notches,
                buttons: 0,
            }]),
        }
    }

    /// Two fingers of a pinch ([`crate::pinch`]). scrcpy turns the second DOWN / first UP
    /// into POINTER_DOWN / POINTER_UP itself.
    fn fingers(&self, phase: crate::pinch::Phase, f: crate::pinch::Fingers, rotation: u32) {
        use crate::pinch::Phase;
        let Some((w, h)) = self.frame() else { return };
        let touch = |i: usize| {
            let (x, y) = device_point(f[i].0, f[i].1, rotation);
            let (action, pressure) = match phase {
                Phase::Down => (ACTION_DOWN, 1.0),
                Phase::Move => (ACTION_MOVE, 1.0),
                Phase::Up => (ACTION_UP, 0.0),
            };
            Control::Touch {
                action,
                pointer: PINCH_POINTERS[i],
                pos: control::position(x as f32 / 65535.0, y as f32 / 65535.0, w, h),
                pressure,
                action_button: 0,
                buttons: 0,
            }
        };
        if phase == Phase::Down {
            self.forget_panel();
        }
        self.send(&if phase == Phase::Up {
            [touch(1), touch(0)]
        } else {
            [touch(0), touch(1)]
        });
    }

    /// The phone closes a panel by itself on a touch, BACK or HOME.
    fn forget_panel(&self) {
        *self.panel.lock().unwrap() = None;
    }

    /// BACK (or screen on), press and release.
    fn back(&self) {
        self.send(&[
            Control::BackOrScreenOn {
                action: ACTION_DOWN,
            },
            Control::BackOrScreenOn { action: ACTION_UP },
        ]);
    }

    /// A shortcut-bar button. `false` when it is not an Android action
    /// (the window handles it, e.g. mute).
    pub fn action(&self, name: &str) -> bool {
        match name {
            "back" => {
                self.forget_panel();
                self.back()
            }
            "home" => {
                self.forget_panel();
                self.send(&Control::key_tap(keycode::HOME, 0))
            }
            "switcher" => {
                self.forget_panel();
                self.send(&Control::key_tap(keycode::APP_SWITCH, 0))
            }
            "notifications" | "quick-settings" => {
                let name = if name == "notifications" {
                    "notifications"
                } else {
                    "quick-settings"
                };
                let mut open = self.panel.lock().unwrap();
                if *open == Some(name) {
                    *open = None;
                    drop(open);
                    self.send(&[Control::CollapsePanels]);
                } else {
                    *open = Some(name);
                    drop(open);
                    self.send(&[if name == "notifications" {
                        Control::ExpandNotifications
                    } else {
                        Control::ExpandSettings
                    }]);
                }
            }
            "vol-down" => self.send(&Control::key_tap(keycode::VOLUME_DOWN, 0)),
            "vol-up" => self.send(&Control::key_tap(keycode::VOLUME_UP, 0)),
            "lock" => self.send(&Control::key_tap(keycode::POWER, 0)),
            "rotate-device" => self.send(&[Control::RotateDevice]),
            _ => return false,
        }
        true
    }

    /// Opens Developer options on the phone (for "USB debugging (Security settings)").
    pub fn open_developer_options(&self) {
        let Some((client, serial)) = self.adb.lock().unwrap().clone() else {
            return;
        };
        std::thread::spawn(move || {
            let cmd = "am start -a android.settings.APPLICATION_DEVELOPMENT_SETTINGS";
            if let Err(e) = client.shell(&Target::Serial(&serial), cmd, 4096) {
                log::warn!("android: open developer options: {e}");
            }
        });
    }

    /// Pastes the PC clipboard on the device (sets its clipboard, then pastes).
    pub fn paste(&self, text: String) {
        self.send(&[Control::SetClipboard {
            sequence: 0, // SEQUENCE_INVALID: no ack wanted
            paste: true,
            text,
        }]);
    }

    /// Takes the session off screen (`true`) or back (`false`). The decoder ends while
    /// parked; the stream it read waits in `held` for the next [`DeviceSource::open`].
    pub fn park(&self, parked: bool) {
        self.parked.store(parked, Relaxed);
        if !parked {
            *self.panel.lock().unwrap() = None;
            self.resuming.store(true, Relaxed);
        }
    }

    pub fn parked(&self) -> bool {
        self.parked.load(Relaxed)
    }

    /// Ends the session: the decoder's blocked read returns at once.
    pub fn stop(&self) {
        self.stop.store(true, Relaxed);
        *self.control.lock().unwrap() = None;
        if let Some(s) = self.stopper.lock().unwrap().take() {
            s.stop();
        }
        self.set(|s| s.phase = Phase::Idle);
    }
}

/// Converts scrcpy packets to decoder records. Packets are Annex B; the
/// decoder takes 4-byte-length samples and an avcC (H.264) or hvcC (H.265)
/// record.
fn to_record(codec: u32, p: Packet) -> Option<Record> {
    match p {
        // The size comes from the decoder; a rotation is followed by a new config.
        Packet::Session { .. } => None,
        Packet::Config(c) => Some(Record::Config(codec_info(codec, &c)?)),
        Packet::Media { key, data, .. } => Some(Record::Frame {
            key,
            rebuild: false,
            data: annexb_to_lengths(&data),
        }),
    }
}

fn codec_info(codec: u32, config: &[u8]) -> Option<CodecInfo> {
    let (codec, description) = match codec {
        CODEC_H264 => (Codec::H264, avcc_from_annexb(config)?),
        CODEC_H265 => (Codec::Hevc, hvcc_from_annexb(config)?),
        _ => return None,
    };
    Some(CodecInfo {
        codec,
        description,
        // RESET_VIDEO (scrcpy 3.0+); MediaCodec also sends one every 10 s.
        keyframes_on_request: true,
        display: None,
    })
}

/// Reads up to the first config packet; media before it cannot be decoded.
/// The session packet before it carries the video size, which touch input
/// needs (the server ignores touches made for another size).
fn first_config<R: Read>(v: &mut VideoReader<R>, shared: &Shared) -> io::Result<CodecInfo> {
    if v.codec != CODEC_H264 && v.codec != CODEC_H265 {
        return Err(io::Error::other(format!(
            "codec {} is not supported yet",
            codec_name(v.codec)
        )));
    }
    for _ in 0..64 {
        match v.next_packet()? {
            Packet::Session { width, height, .. } => shared.set_frame(width, height),
            Packet::Config(c) => {
                return codec_info(v.codec, &c).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "no parameter sets")
                });
            }
            Packet::Media { .. } => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "no config packet",
    ))
}

/// Plays timestamps in real time (recordings only; a live device paces itself).
struct Pacer {
    start: Instant,
    pts0: Option<u64>,
}

impl Pacer {
    fn wait(&mut self, pts: u64, stop: &AtomicBool) {
        let pts0 = *self.pts0.get_or_insert(pts);
        let due = self.start + Duration::from_micros(pts.saturating_sub(pts0));
        while !stop.load(Relaxed) {
            let now = Instant::now();
            if now >= due {
                return;
            }
            std::thread::sleep((due - now).min(Duration::from_millis(20)));
        }
    }
}

struct Records<R: Read + Send + 'static> {
    /// `None` only after a drop handed it to the parking thread.
    video: Option<VideoReader<R>>,
    pacer: Option<Pacer>,
    shared: Arc<Shared>,
    /// Keeps the server (shell, sockets) alive while it streams.
    _session: Option<Session>,
    /// The last config, for a return from parking.
    info: Option<CodecInfo>,
}

impl<R: Read + Send + 'static> RecordStream for Records<R> {
    fn next_record(&mut self) -> Option<Record> {
        loop {
            if self.shared.stop.load(Relaxed) || self.shared.parked.load(Relaxed) {
                return None;
            }
            let p = match self.video.as_mut()?.next_packet() {
                Ok(p) => p,
                Err(e) => {
                    if !self.shared.stop.load(Relaxed) {
                        log::info!("android: stream ended: {e}");
                    }
                    return None;
                }
            };
            if let (Some(pacer), Packet::Media { pts, .. }) = (self.pacer.as_mut(), &p) {
                pacer.wait(*pts, &self.shared.stop);
            }
            if let Packet::Session { width, height, .. } = p {
                self.shared.set_frame(width, height);
            }
            let codec = self.video.as_ref()?.codec;
            if let Some(r) = to_record(codec, p) {
                if let Record::Config(i) = &r {
                    self.info = Some(i.clone());
                }
                return Some(r);
            }
        }
    }
}

impl<R: Read + Send + 'static> Drop for Records<R> {
    /// Parked: a thread keeps reading (and dropping) the stream so the phone never blocks,
    /// until the session comes back on screen (the stream goes to `held`) or stops.
    fn drop(&mut self) {
        let s = self.shared.clone();
        if !s.parked.load(Relaxed) || s.stop.load(Relaxed) {
            return;
        }
        let (Some(mut video), Some(mut info)) = (self.video.take(), self.info.take()) else {
            return;
        };
        let session = self._session.take();
        let pacer = self.pacer.take();
        log::info!("android: off screen; the session keeps running");
        let _ = std::thread::Builder::new()
            .name("android-parked".into())
            .spawn(move || {
                while s.parked.load(Relaxed) && !s.stop.load(Relaxed) {
                    match video.next_packet() {
                        Ok(Packet::Config(c)) => {
                            if let Some(i) = codec_info(video.codec, &c) {
                                info = i;
                            }
                        }
                        Ok(Packet::Session { width, height, .. }) => s.set_frame(width, height),
                        Ok(_) => {}
                        Err(e) => {
                            log::info!("android: parked stream ended: {e}");
                            return;
                        }
                    }
                }
                if s.stop.load(Relaxed) {
                    return;
                }
                let back = Records {
                    video: Some(video),
                    pacer,
                    shared: s.clone(),
                    _session: session,
                    info: Some(info.clone()),
                };
                *s.held.lock().unwrap() = Some((info, Box::new(back)));
            });
    }
}

/// Keeps a source alive across decoder threads (a parked session comes back to the same
/// [`DeviceSource`], with its encoder plan and failure count).
pub struct Kept(pub Arc<Mutex<Box<dyn VideoSource>>>);

impl VideoSource for Kept {
    fn open(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        self.0.lock().unwrap().open()
    }

    fn request_keyframe(&self) {
        self.0.lock().unwrap().request_keyframe()
    }

    fn finished(&self) -> bool {
        self.0.lock().unwrap().finished()
    }
}

/// Smart Encoder Detection inputs for a session start: the user's choice,
/// what the PC decodes, and an encoder list read earlier (skips the query).
pub struct Smart {
    pub pref: Preference,
    pub dec: Decoders,
    pub known: Option<Vec<Encoder>>,
}

/// A real device: starts the scrcpy server through the ADB server.
pub struct DeviceSource {
    pub client: AdbClient,
    pub serial: String,
    pub transport_id: Option<u64>,
    pub server: Arc<Vec<u8>>,
    pub options: VideoOptions,
    pub shared: Arc<Shared>,
    failures: u32,
    /// Smart Encoder Detection: what the user chose, what the PC decodes,
    /// an encoder list known from earlier (skips the query), the plan and
    /// the attempt in use.
    pref: Preference,
    dec: Decoders,
    known: Option<Vec<Encoder>>,
    plan: Vec<Attempt>,
    step: usize,
    /// App windows (v2.5.1): the package started on the new virtual display
    /// after every (re)connect; the display closes with its apps.
    pub start_app: Option<String>,
}

impl DeviceSource {
    /// `options` come from the window (Settings › Android); `audio` must
    /// already be false on Android 10 and older and in "Device only" mode.
    pub fn new(
        serial: String,
        transport_id: Option<u64>,
        name: String,
        server: Arc<Vec<u8>>,
        shared: Arc<Shared>,
        options: VideoOptions,
        smart: Smart,
    ) -> Self {
        shared.set(|s| {
            *s = Status {
                phase: Phase::Connecting,
                device: name,
                ..Status::default()
            }
        });
        Self {
            client: AdbClient::default(),
            serial,
            transport_id,
            server,
            options,
            shared,
            failures: 0,
            pref: smart.pref,
            dec: smart.dec,
            known: smart.known,
            plan: Vec::new(),
            step: 0,
            start_app: None,
        }
    }

    /// Reads the encoder list (once per session) and plans the attempts.
    fn ensure_plan(&mut self, target: &Target) {
        if !self.plan.is_empty() {
            return;
        }
        let list = match self.known.take() {
            Some(l) => Some(l),
            None => match encoders::query(&self.client, target, &self.server) {
                Ok(l) if !l.is_empty() => Some(l),
                Ok(_) => {
                    log::warn!("android: the server listed no encoders");
                    None
                }
                Err(e) => {
                    log::warn!("android: encoder list failed: {e}");
                    None
                }
            },
        };
        if let Some(l) = &list {
            let names: Vec<String> = l
                .iter()
                .map(|e| format!("{}:{}({})", e.codec, e.name, e.accel.as_str()))
                .collect();
            log::info!("android: encoders {}", names.join(" "));
        }
        self.plan = encoders::plan(&self.pref, list.as_deref(), self.dec);
        log::info!("android: encoder plan {:?}", self.plan);
        *self.shared.encoders.lock().unwrap() = list;
    }

    /// The attempt to start with now.
    fn attempt(&self) -> Attempt {
        self.plan
            .get(self.step)
            .or(self.plan.last())
            .cloned()
            .unwrap_or(Attempt {
                codec: "h264",
                encoder: None,
            })
    }

    /// Consecutive failures that end the session: every planned attempt
    /// gets its chance, and at least MAX_FAILURES (cable hiccups).
    fn max_failures(&self) -> u32 {
        MAX_FAILURES.max(self.plan.len() as u32)
    }

    fn fail(&mut self, e: String) {
        log::warn!("android: {e}");
        // A replugged phone gets a new transport id: retry by serial.
        self.transport_id = None;
        // Bounded fallback (master prompt section 5): the next attempt of
        // the plan, e.g. another hardware encoder, then H.264 with the
        // server's default encoder. The user's preference is kept for the
        // next session.
        if self.step + 1 < self.plan.len() {
            let from = self.attempt();
            self.step += 1;
            let to = self.attempt();
            log::warn!("android: {from:?} failed; trying {to:?}");
        }
        self.failures += 1;
        let gave_up = self.failures >= self.max_failures();
        self.shared.set(|s| {
            s.error = e;
            if gave_up {
                s.phase = Phase::Error;
            }
        });
    }
}

impl VideoSource for DeviceSource {
    fn open(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        if self.finished() {
            return None;
        }
        if let Some(back) = self.resume() {
            return Some(back);
        }
        self.shared.set(|s| s.phase = Phase::Connecting);
        let serial = self.serial.clone();
        let target = match self.transport_id {
            Some(id) => Target::TransportId(id),
            None => Target::Serial(&serial),
        };
        self.ensure_plan(&target);
        let a = self.attempt();
        self.options.codec = a.codec;
        self.options.encoder = a.encoder.clone();
        let fell_back = self.step > 0;
        self.shared.set(|s| {
            s.codec = a.codec.to_string();
            s.encoder = a.encoder.clone().unwrap_or_default();
            s.fell_back = fell_back;
        });
        let mut m = match Mirror::start(&self.client, &target, &self.server, &self.options) {
            Ok(m) => m,
            Err(e) => {
                self.fail(e.to_string());
                return None;
            }
        };
        if let Ok(s) = m.stopper() {
            *self.shared.stopper.lock().unwrap() = Some(s);
        }
        if self.shared.stop.load(Relaxed) {
            return None;
        }
        let info = match first_config(&mut m.video, &self.shared) {
            Ok(i) => i,
            Err(e) => {
                self.fail(e.to_string());
                return None;
            }
        };
        self.failures = 0;
        let name = m.device_name.clone();
        self.shared.set(|s| {
            s.phase = Phase::Streaming;
            // The list's name (marketing name from ro.product.*) wins; the
            // scrcpy header only carries ro.product.model ("TECNO LI7" for a
            // TECNO POVA 6), so it is a fallback.
            if s.device.is_empty() && !name.is_empty() {
                s.device = name;
            }
            s.error.clear();
            s.input_blocked = false;
        });
        *self.shared.adb.lock().unwrap() = Some((self.client.clone(), self.serial.clone()));
        if self.options.control {
            let (client, serial, sh) = (
                self.client.clone(),
                self.serial.clone(),
                self.shared.clone(),
            );
            std::thread::spawn(move || {
                let out = client
                    .shell(&Target::Serial(&serial), MI_INPUT_PROPS, 512)
                    .unwrap_or_default();
                if mi_input_blocked(&String::from_utf8_lossy(&out)) {
                    log::warn!("android: MIUI/HyperOS with USB debugging (Security settings) off: input is blocked");
                    sh.set(|s| s.input_blocked = true);
                }
            });
        }
        let Mirror {
            video,
            audio,
            control,
            session,
            ..
        } = m;
        *self.shared.control.lock().unwrap() = control;
        if let Some(pkg) = &self.start_app {
            // The server waits up to a second for the virtual display's id.
            self.shared.send(&[Control::StartApp(pkg.clone())]);
        }
        if let Some(a) = audio {
            let wifi = zenlink_android::devices::connection_for(&self.serial).is_wireless();
            play_audio(a, self.shared.clone(), wifi);
        }
        Some((
            info.clone(),
            Box::new(Records {
                video: Some(video),
                pacer: None,
                shared: self.shared.clone(),
                _session: Some(session),
                info: Some(info),
            }),
        ))
    }

    fn request_keyframe(&self) {
        self.shared.send(&[Control::ResetVideo]);
    }

    fn finished(&self) -> bool {
        let s = &self.shared;
        s.stop.load(Relaxed) || s.parked.load(Relaxed) || self.failures >= self.max_failures()
    }
}

impl DeviceSource {
    /// Back from parking: the running stream, with a fresh keyframe asked for. `None` when
    /// the session was never parked or its stream ended meanwhile (then it reconnects).
    fn resume(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        if !self.shared.resuming.swap(false, Relaxed) {
            return None;
        }
        let at = Instant::now();
        // The parking thread hands the stream over at its next packet (the encoder repeats
        // frames at least every 100 ms); the reset makes one come at once.
        let mut asked = false;
        while at.elapsed() < Duration::from_secs(2) && !self.shared.stop.load(Relaxed) {
            if let Some(back) = self.shared.held.lock().unwrap().take() {
                log::info!(
                    "android: back on screen after {} ms",
                    at.elapsed().as_millis()
                );
                return Some(back);
            }
            if !asked {
                asked = true;
                self.shared.send(&[Control::ResetVideo]);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }
}

/// What a key press in the window does on the device.
#[derive(Debug, PartialEq)]
pub enum KeyInput {
    Send(Vec<Control>),
    /// Ctrl+V: paste the PC clipboard.
    Paste,
    /// Not for the device (the window keeps it).
    Ignore,
}

/// Maps a Slint key press (its `text` and modifiers) like scrcpy's default
/// keyboard: printable text is injected as text, editing and navigation
/// keys as Android key codes, Ctrl+letter as a key with Ctrl (copy, cut,
/// select all, undo), Ctrl+V pastes the PC clipboard.
pub fn key_input(text: &str, ctrl: bool, shift: bool, alt: bool) -> KeyInput {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return KeyInput::Ignore;
    };
    let mut m = 0;
    if ctrl {
        m |= meta::CTRL_ON;
    }
    if shift {
        m |= meta::SHIFT_ON;
    }
    if alt {
        m |= meta::ALT_ON;
    }
    // Slint's key codes: control characters and private-use code points.
    let code = match c {
        '\u{8}' => Some(keycode::DEL),
        '\u{7f}' => Some(keycode::FORWARD_DEL),
        '\t' => Some(keycode::TAB),
        '\n' | '\r' => Some(keycode::ENTER),
        '\u{1b}' => Some(keycode::ESCAPE),
        '\u{F700}' => Some(keycode::DPAD_UP),
        '\u{F701}' => Some(keycode::DPAD_DOWN),
        '\u{F702}' => Some(keycode::DPAD_LEFT),
        '\u{F703}' => Some(keycode::DPAD_RIGHT),
        '\u{F729}' => Some(keycode::MOVE_HOME),
        '\u{F72B}' => Some(keycode::MOVE_END),
        '\u{F72C}' => Some(keycode::PAGE_UP),
        '\u{F72D}' => Some(keycode::PAGE_DOWN),
        _ => None,
    };
    if let Some(k) = code {
        return KeyInput::Send(Control::key_tap(k, m).to_vec());
    }
    if c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c) {
        return KeyInput::Ignore; // modifiers alone, F-keys, ...
    }
    if ctrl {
        let l = c.to_ascii_lowercase();
        if l == 'v' {
            return KeyInput::Paste;
        }
        // Ctrl+C / Ctrl+X: copy / cut on the phone, and its clipboard comes to the PC.
        if (l == 'c' || l == 'x') && !shift && !alt {
            let copy_key = if l == 'c' {
                control::COPY_KEY_COPY
            } else {
                control::COPY_KEY_CUT
            };
            return KeyInput::Send(vec![Control::GetClipboard { copy_key }]);
        }
        if l.is_ascii_lowercase() {
            let k = keycode::A + (l as i32 - 'a' as i32);
            return KeyInput::Send(Control::key_tap(k, m).to_vec());
        }
        return KeyInput::Ignore;
    }
    if alt {
        return KeyInput::Ignore;
    }
    KeyInput::Send(vec![Control::Text(c.to_string())])
}

/// Plays the audio socket until it closes (stop, unplug). Without a working
/// output the video goes on alone.
///
/// The sound shares the ADB link with the video and arrives in bursts; the
/// phone's clock also drifts from the PC's. Measured on a TECNO (USB, raw
/// PCM): with the Apple cable settings (10 ms margin, chunk drops) there
/// were 1-3 gaps and up to 267 ms dropped every 5 s, heard as crackling. So
/// Android keeps 40 ms queued (60 ms over Wi-Fi; scrcpy's own default is 50
/// ms) and corrects drift by playing up to 0.5% faster or slower.
fn play_audio(mut r: VideoReader<std::net::TcpStream>, shared: Arc<Shared>, wifi: bool) {
    let _ = std::thread::Builder::new()
        .name("android-audio".into())
        .spawn(move || {
            let mut player = match zenlink_audio::Player::start() {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("android: no sound output ({e}); video only");
                    return;
                }
            };
            player.set_smooth(true);
            player.set_jitter_ms(if wifi { 60.0 } else { 40.0 });
            let mut stats_at = Instant::now();
            while !shared.stop.load(Relaxed) {
                match r.next_packet() {
                    // Off screen: the sound is read and dropped (only one device is heard).
                    Ok(Packet::Media { .. }) if shared.parked.load(Relaxed) => {}
                    Ok(Packet::Media { data, .. }) => {
                        player.set_muted(AUDIO_MUTED.load(Relaxed));
                        player.set_volume(f32::from_bits(AUDIO_VOLUME.load(Relaxed)));
                        player.feed(&data);
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
                if stats_at.elapsed() >= Duration::from_secs(1) {
                    stats_at = Instant::now();
                    let s = player.stats();
                    let ms = (s.queue_ms + s.output_ms).round().max(1.0) as u32;
                    shared.audio_ms.store(ms, Relaxed);
                    if std::env::var_os("ZL_STATS_LOG").is_some() {
                        log::info!(
                            "sound stats queue={:.0} output={:.1} underruns={} dropped={:.0}",
                            s.queue_ms,
                            s.output_ms,
                            s.underruns,
                            s.dropped_ms
                        );
                    }
                }
            }
            shared.audio_ms.store(0, Relaxed);
        });
}

/// A recorded stream (`ZL_ANDROID_FIXTURE`), looped and paced by its PTS.
pub struct FixtureSource {
    pub path: PathBuf,
    pub shared: Arc<Shared>,
    data: Option<Arc<Vec<u8>>>,
}

impl FixtureSource {
    pub fn new(path: PathBuf, shared: Arc<Shared>) -> Self {
        shared.set(|s| {
            *s = Status {
                phase: Phase::Connecting,
                ..Status::default()
            }
        });
        Self {
            path,
            shared,
            data: None,
        }
    }
}

/// `Read` over a shared buffer, so each loop of a recording does not copy it.
struct SharedCursor {
    data: Arc<Vec<u8>>,
    pos: usize,
}

impl Read for SharedCursor {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rest = &self.data[self.pos.min(self.data.len())..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl VideoSource for FixtureSource {
    fn open(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        if self.data.is_none() {
            match std::fs::read(&self.path) {
                Ok(d) => self.data = Some(Arc::new(d)),
                Err(e) => {
                    self.shared.set(|s| {
                        s.phase = Phase::Error;
                        s.error = format!("{}: {e}", self.path.display());
                    });
                    return None;
                }
            }
        }
        let mut r = SharedCursor {
            data: self.data.clone()?,
            pos: 0,
        };
        let opened = read_device_meta(&mut r).and_then(|name| {
            let mut v = VideoReader::new(r)?;
            let info = first_config(&mut v, &self.shared)?;
            Ok((name, v, info))
        });
        let (name, video, info) = match opened {
            Ok(x) => x,
            Err(e) => {
                self.shared.set(|s| {
                    s.phase = Phase::Error;
                    s.error = e.to_string();
                });
                return None;
            }
        };
        self.shared.set(|s| {
            s.phase = Phase::Streaming;
            s.device = name;
        });
        Some((
            info,
            Box::new(Records {
                video: Some(video),
                info: None,
                pacer: Some(Pacer {
                    start: Instant::now(),
                    pts0: None,
                }),
                shared: self.shared.clone(),
                _session: None,
            }),
        ))
    }

    fn finished(&self) -> bool {
        self.shared.stop.load(Relaxed)
            || (self.data.is_none() && self.shared.status().phase == Phase::Error)
    }
}

/// The pinned scrcpy server, built into ZenLink.exe (B-28): the installer,
/// Portable.exe and the source zip all carry it without a separate file.
/// `scrcpy.rs` pins the name and size; a test checks this copy against them.
static BUILTIN_SERVER: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../ZenshiroDev/resources/third_party/scrcpy/scrcpy-server-v5.0.1"
));

/// The scrcpy server: `ZL_SCRCPY_SERVER` (tests, manual upgrades), else the
/// copy built into the exe. The size must match the pinned release.
pub fn load_server() -> Result<Vec<u8>, String> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("ZL_SCRCPY_SERVER") {
        candidates.push(PathBuf::from(p));
    }
    if candidates.is_empty() && BUILTIN_SERVER.len() as u64 == SERVER_SIZE {
        return Ok(BUILTIN_SERVER.to_vec());
    }
    for p in &candidates {
        if let Ok(d) = std::fs::read(p) {
            if d.len() as u64 == SERVER_SIZE {
                return Ok(d);
            }
            log::warn!(
                "android: {} has the wrong size ({} bytes)",
                p.display(),
                d.len()
            );
        }
    }
    Err(format!("{SERVER_FILE} not found"))
}

/// Google's platform-tools for Windows, pinned: ZenLink does not ship adb,
/// it downloads this exact zip on request (the same revision and SHA-256 as
/// `ZL_ADB_REV`/`ZL_ADB_SHA256` in Start ZenLink.bat). The hash pins the bytes
/// Google published; nothing else is accepted.
pub const ADB_ZIP_URL: &str =
    "https://dl.google.com/android/repository/platform-tools_r37.0.1-win.zip";
pub const ADB_ZIP_SIZE: u64 = 8_044_989;
pub const ADB_ZIP_SHA256: &str = "45f4d63113e895ebde0c90f194099a4676b6ac653bd28d54314a9e022bbc1a99";
/// The files ZenLink keeps from the zip, unmodified.
const ADB_FILES: [&str; 4] = ["adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll", "NOTICE.txt"];

/// Where a downloaded adb lives: `platform-tools` in the local data folder.
pub fn installed_adb_dir() -> Option<PathBuf> {
    Some(zenlink_core::local_data_dir()?.join("platform-tools"))
}

/// Downloads Google's platform-tools zip (checked size and SHA-256), then
/// extracts the four adb files to `installed_adb_dir()`. `progress(done,
/// total)` reports the download. On any error nothing half-written stays.
pub fn install_adb(progress: &dyn Fn(u64, u64)) -> Result<PathBuf, String> {
    let dir = installed_adb_dir().ok_or("no local data folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let zip = dir.join("platform-tools.zip");
    let asset = zenlink_core::update::Asset {
        name: "platform-tools.zip".into(),
        url: ADB_ZIP_URL.into(),
        size: ADB_ZIP_SIZE,
        sha256: ADB_ZIP_SHA256.into(),
    };
    crate::updater::download(&asset, &zip, progress)?;
    let result = (|| {
        let bytes = std::fs::read(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
        for name in ADB_FILES {
            let data = zip_entry(&bytes, &format!("platform-tools/{name}"))?;
            let path = dir.join(name);
            let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, &data).map_err(|e| format!("{}: {e}", tmp.display()))?;
            if let Err(e) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                // A running adb server keeps adb.exe locked; same size = same file.
                let same = std::fs::metadata(&path)
                    .map(|m| m.len() == data.len() as u64)
                    .unwrap_or(false);
                if !same {
                    return Err(format!("{}: {e}", path.display()));
                }
            }
        }
        Ok(dir.join("adb.exe"))
    })();
    let _ = std::fs::remove_file(&zip);
    result
}

/// One file from a zip archive (stored or deflated), found through the
/// central directory. Enough for Google's platform-tools zip; no zip64.
fn zip_entry(zip: &[u8], wanted: &str) -> Result<Vec<u8>, String> {
    let u16_at = |o: usize| -> Option<usize> {
        zip.get(o..o + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let u32_at = |o: usize| -> Option<usize> {
        zip.get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    // End of central directory: the last "PK\x05\x06" (comment up to 64 KB).
    let low = zip.len().saturating_sub(22 + 65_535);
    let eocd = (low..zip.len().saturating_sub(21))
        .rev()
        .find(|&i| zip[i..].starts_with(b"PK\x05\x06"))
        .ok_or("not a zip file")?;
    let count = u16_at(eocd + 10).ok_or("bad zip")?;
    let mut at = u32_at(eocd + 16).ok_or("bad zip")?;
    for _ in 0..count {
        if !zip.get(at..).is_some_and(|b| b.starts_with(b"PK\x01\x02")) {
            return Err("bad zip directory".into());
        }
        let method = u16_at(at + 10).ok_or("bad zip")?;
        let csize = u32_at(at + 20).ok_or("bad zip")?;
        let usize_ = u32_at(at + 24).ok_or("bad zip")?;
        let nlen = u16_at(at + 28).ok_or("bad zip")?;
        let elen = u16_at(at + 30).ok_or("bad zip")?;
        let clen = u16_at(at + 32).ok_or("bad zip")?;
        let local = u32_at(at + 42).ok_or("bad zip")?;
        let name = zip.get(at + 46..at + 46 + nlen).ok_or("bad zip")?;
        if name == wanted.as_bytes() {
            let lnlen = u16_at(local + 26).ok_or("bad zip")?;
            let lelen = u16_at(local + 28).ok_or("bad zip")?;
            let data_at = local + 30 + lnlen + lelen;
            let data = zip.get(data_at..data_at + csize).ok_or("bad zip")?;
            let out = match method {
                0 => data.to_vec(),
                8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, usize_)
                    .map_err(|e| format!("{wanted}: inflate {e:?}"))?,
                m => return Err(format!("{wanted}: zip method {m}")),
            };
            if out.len() != usize_ {
                return Err(format!("{wanted}: size mismatch"));
            }
            return Ok(out);
        }
        at += 46 + nlen + elen + clen;
    }
    Err(format!("{wanted} is missing from the zip"))
}

/// The `adb` program to start the ADB server with, when none runs.
/// `ZL_ADB=none` disables it (tests); `ZL_ADB=<path>` picks one.
/// Otherwise: a `third_party\platform-tools` next to the exe (older
/// installs), the copy ZenLink downloaded (`install_adb`), the Android SDK
/// (`ANDROID_HOME`, `ANDROID_SDK_ROOT`, Android Studio's default folder), PATH.
pub fn find_adb() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    if let Some(v) = std::env::var_os("ZL_ADB") {
        if v == "none" {
            return None;
        }
        return Some(PathBuf::from(v)).filter(|p| p.is_file());
    }
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        for dir in me.ancestors().skip(1).take(5) {
            c.push(dir.join("third_party").join("platform-tools").join(exe));
            // Source checkout: Start ZenLink.bat downloads adb here.
            c.push(
                dir.join("ZenshiroDev")
                    .join("resources")
                    .join("third_party")
                    .join("platform-tools")
                    .join(exe),
            );
            c.push(dir.join("platform-tools").join(exe));
        }
    }
    if let Some(dir) = installed_adb_dir() {
        c.push(dir.join(exe));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(var) {
            c.push(Path::new(&root).join("platform-tools").join(exe));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        c.push(
            Path::new(&local)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(exe),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        c.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    c.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zenlink_android::mirror::{write_device_meta, write_packet};

    /// A minimal zip: (name, data, deflate?) entries, local headers + central
    /// directory + end record, the way zip tools write them.
    fn make_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in entries {
            let body = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let offset = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 8]); // time, date, crc (not checked)
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]); // extra, comment, disk, attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn zip_entry_reads_stored_and_deflated_files() {
        let big = vec![b'a'; 10_000];
        let zip = make_zip(&[
            ("platform-tools/NOTICE.txt", b"notice", false),
            ("platform-tools/adb.exe", &big, true),
        ]);
        assert_eq!(
            zip_entry(&zip, "platform-tools/NOTICE.txt").unwrap(),
            b"notice"
        );
        assert_eq!(zip_entry(&zip, "platform-tools/adb.exe").unwrap(), big);
        assert!(zip_entry(&zip, "platform-tools/missing")
            .unwrap_err()
            .contains("missing"));
        assert!(zip_entry(b"not a zip", "x").is_err());
        assert!(
            zip_entry(&zip[..zip.len() - 30], "platform-tools/adb.exe").is_err(),
            "truncated zip"
        );
    }

    /// Real download from dl.google.com into a temporary LOCALAPPDATA:
    /// `cargo test -p zenlink-app --release install_adb_from_google -- --ignored`.
    #[test]
    #[ignore]
    fn install_adb_from_google() {
        let tmp = std::env::temp_dir().join(format!("zl-adb-test-{}", std::process::id()));
        std::env::set_var("LOCALAPPDATA", &tmp);
        let adb = install_adb(&|_, _| {}).expect("install");
        assert!(adb.is_file());
        let out = std::process::Command::new(&adb)
            .arg("version")
            .output()
            .expect("run adb");
        assert!(String::from_utf8_lossy(&out.stdout).contains("37.0.1"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn wire(packets: &[Packet]) -> Vec<u8> {
        let mut w = Vec::new();
        write_device_meta(&mut w, "Test").unwrap();
        w.extend_from_slice(&CODEC_H264.to_be_bytes());
        for p in packets {
            write_packet(&mut w, p).unwrap();
        }
        w
    }

    const CONFIG: [u8; 13] = [0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 1, 0x68, 0xce];

    #[test]
    fn packets_become_decoder_records() {
        assert!(to_record(
            CODEC_H264,
            Packet::Session {
                width: 1,
                height: 2,
                client_resized: false
            }
        )
        .is_none());
        match to_record(CODEC_H264, Packet::Config(CONFIG.to_vec())) {
            Some(Record::Config(c)) => {
                assert_eq!(c.codec, Codec::H264);
                assert_eq!(&c.description[..4], &[1, 0x42, 0xc0, 0x1f]);
                assert!(c.keyframes_on_request, "RESET_VIDEO");
            }
            r => panic!("{r:?}"),
        }
        match to_record(
            CODEC_H264,
            Packet::Media {
                pts: 0,
                key: true,
                data: vec![0, 0, 0, 1, 0x65, 1, 2],
            },
        ) {
            Some(Record::Frame { key, rebuild, data }) => {
                assert!(key && !rebuild);
                assert_eq!(data, [0, 0, 0, 3, 0x65, 1, 2]);
            }
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn fixture_source_plays_and_loops() {
        let path = std::env::temp_dir().join(format!("zl-fixture-{}.scrcpy", std::process::id()));
        std::fs::write(
            &path,
            wire(&[
                Packet::Session {
                    width: 16,
                    height: 16,
                    client_resized: false,
                },
                Packet::Config(CONFIG.to_vec()),
                Packet::Media {
                    pts: 0,
                    key: true,
                    data: vec![0, 0, 0, 1, 0x65, 9],
                },
                Packet::Media {
                    pts: 30_000,
                    key: false,
                    data: vec![0, 0, 0, 1, 0x41, 9],
                },
            ]),
        )
        .unwrap();
        let shared = Arc::new(Shared::default());
        let mut src = FixtureSource::new(path.clone(), shared.clone());
        for _ in 0..2 {
            let (info, mut s) = src.open().unwrap();
            assert_eq!(info.codec, Codec::H264);
            assert_eq!(shared.status().phase, Phase::Streaming);
            assert_eq!(shared.status().device, "Test");
            let t = Instant::now();
            assert!(matches!(
                s.next_record(),
                Some(Record::Frame { key: true, .. })
            ));
            assert!(matches!(
                s.next_record(),
                Some(Record::Frame { key: false, .. })
            ));
            assert!(t.elapsed() >= Duration::from_millis(25), "paced by PTS");
            assert!(s.next_record().is_none(), "end of recording");
        }
        shared.stop();
        assert!(src.finished());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn committed_fixture_is_a_valid_scrcpy_stream() {
        // apps/desktop/tests/fixtures/android: made by ZenshiroDev/tooling/dev/make_android_fixture.py.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/android/synthetic-h264-360x640.scrcpy");
        let data = std::fs::read(&path).unwrap();
        let mut r = &data[..];
        assert_eq!(read_device_meta(&mut r).unwrap(), "Synthetic test stream");
        let mut v = VideoReader::new(r).unwrap();
        let info = first_config(&mut v, &Shared::default()).unwrap();
        assert_eq!(info.codec, Codec::H264);
        let (mut frames, mut keys, mut last) = (0, 0, 0);
        while let Ok(p) = v.next_packet() {
            if let Packet::Media { pts, key, .. } = p {
                assert!(frames == 0 || pts > last, "PTS increase");
                last = pts;
                frames += 1;
                keys += key as u32;
            }
        }
        assert_eq!(frames, 120);
        assert!(keys >= 1);
        assert!(data.len() < 150_000, "keep the fixture small");
    }

    #[test]
    #[ignore = "needs a sound device (plays silence, muted)"]
    fn audio_socket_plays_through_the_output() {
        use std::io::Write;
        use zenlink_android::mirror::CODEC_RAW;
        set_audio(true, 1.0, true);
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let device = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&CODEC_RAW.to_be_bytes()).unwrap();
            // 1.5 s of 10 ms PCM packets, in real time like a device.
            for i in 0..150u64 {
                let mut w = Vec::new();
                write_packet(
                    &mut w,
                    &Packet::Media {
                        pts: i * 10_000,
                        key: false,
                        data: vec![0; 480 * 4],
                    },
                )
                .unwrap();
                s.write_all(&w).unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let r =
            VideoReader::new(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()).unwrap();
        let shared = Arc::new(Shared::default());
        play_audio(r, shared.clone(), false);
        std::thread::sleep(Duration::from_millis(1300));
        let ms = shared.audio_ms().expect("sound latency reported");
        println!("sound latency {ms} ms");
        assert!(ms < 150, "{ms} ms");
        device.join().unwrap();
    }

    #[test]
    fn missing_fixture_is_an_error() {
        let shared = Arc::new(Shared::default());
        let mut src = FixtureSource::new(PathBuf::from("Z:/does/not/exist.scrcpy"), shared.clone());
        assert!(src.open().is_none());
        assert_eq!(shared.status().phase, Phase::Error);
        assert!(src.finished());
    }

    #[test]
    fn keys_map_like_scrcpy() {
        assert_eq!(
            key_input("a", false, false, false),
            KeyInput::Send(vec![Control::Text("a".into())])
        );
        assert_eq!(
            key_input("A", false, true, false),
            KeyInput::Send(vec![Control::Text("A".into())])
        );
        assert_eq!(
            key_input("\u{8}", false, false, false),
            KeyInput::Send(Control::key_tap(keycode::DEL, 0).to_vec())
        );
        assert_eq!(
            key_input("\n", false, false, false),
            KeyInput::Send(Control::key_tap(keycode::ENTER, 0).to_vec())
        );
        assert_eq!(
            key_input("\u{F702}", false, true, false),
            KeyInput::Send(Control::key_tap(keycode::DPAD_LEFT, meta::SHIFT_ON).to_vec())
        );
        assert_eq!(
            key_input("c", true, false, false),
            KeyInput::Send(vec![Control::GetClipboard {
                copy_key: control::COPY_KEY_COPY
            }])
        );
        assert_eq!(
            key_input("a", true, false, false),
            KeyInput::Send(Control::key_tap(keycode::A, meta::CTRL_ON).to_vec())
        );
        assert_eq!(key_input("v", true, false, false), KeyInput::Paste);
        assert_eq!(
            key_input("\u{10}", false, true, false),
            KeyInput::Ignore,
            "Shift alone"
        );
        assert_eq!(
            key_input("\u{F704}", false, false, false),
            KeyInput::Ignore,
            "F1"
        );
        assert_eq!(key_input("", false, false, false), KeyInput::Ignore);
    }

    #[test]
    fn xiaomi_input_check() {
        assert!(
            mi_input_blocked("\nOS2.0\n\n"),
            "HyperOS, security setting off"
        );
        assert!(mi_input_blocked("V140\n\n0\n"), "MIUI, off");
        assert!(!mi_input_blocked("V140\n\n1\n"), "MIUI, on");
        assert!(!mi_input_blocked("\n\n\n"), "not Xiaomi");
        assert!(!mi_input_blocked(""), "no answer");
    }

    #[test]
    fn control_goes_to_the_socket_in_frame_pixels() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tx = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut rx, _) = l.accept().unwrap();
        let shared = Arc::new(Shared::default());
        *shared.control.lock().unwrap() = Some(tx);
        shared.pointer(PointerEvent::Down(0.5, 0.5), 0);
        // No frame size yet: nothing is sent.
        shared.set_frame(1080, 1920);
        shared.pointer(PointerEvent::Down(0.5, 0.25), 0);
        assert!(shared.action("home"));
        assert!(!shared.action("mute"));
        let mut b = vec![0u8; 32 + 2 * 14];
        rx.read_exact(&mut b).unwrap();
        assert_eq!(&b[..2], &[2, 0], "touch down");
        assert_eq!(&b[10..14], &540i32.to_be_bytes());
        assert_eq!(&b[14..18], &480i32.to_be_bytes());
        assert_eq!(&b[32..34], &[0, 0], "HOME key down");
        assert_eq!(&b[36..38], &[0, 3]);
        // 90 degrees: the same mapping as the Apple touch path (device_point).
        let mut b = vec![0u8; 32];
        shared.pointer(PointerEvent::Up(1.0, 0.5), 90);
        rx.read_exact(&mut b).unwrap();
        assert_eq!(&b[10..14], &540i32.to_be_bytes());
        assert_eq!(&b[14..18], &0i32.to_be_bytes());
        shared.stop();
        shared.pointer(PointerEvent::Down(0.1, 0.1), 0);
        assert!(shared.control.lock().unwrap().is_none());
    }

    #[test]
    fn session_size_before_the_config_is_kept_for_touch() {
        // scrcpy sends the session packet (video size) before the config;
        // first_config used to swallow it, so every touch was dropped.
        let w = wire(&[
            Packet::Session {
                width: 720,
                height: 1600,
                client_resized: false,
            },
            Packet::Config(CONFIG.to_vec()),
        ]);
        let mut r = &w[..];
        read_device_meta(&mut r).unwrap();
        let mut v = VideoReader::new(r).unwrap();
        let shared = Arc::new(Shared::default());
        first_config(&mut v, &shared).unwrap();
        assert_eq!(shared.frame(), Some((720, 1600)));
    }

    #[test]
    fn panel_buttons_toggle() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tx = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut rx, _) = l.accept().unwrap();
        let shared = Arc::new(Shared::default());
        *shared.control.lock().unwrap() = Some(tx);
        shared.action("notifications");
        shared.action("notifications");
        shared.action("quick-settings");
        shared.action("notifications");
        shared.set_frame(100, 100);
        shared.pointer(PointerEvent::Down(0.5, 0.5), 0); // touching the phone forgets the panel
        shared.action("quick-settings");
        let mut b = [0u8; 5];
        rx.read_exact(&mut b).unwrap();
        assert_eq!(
            b,
            [5, 7, 6, 5, 2],
            "open, close, settings, switch to notifications, touch"
        );
        let mut rest = [0u8; 32];
        rx.read_exact(&mut rest[..31]).unwrap();
        let mut last = [0u8; 1];
        rx.read_exact(&mut last).unwrap();
        assert_eq!(last, [6], "opens again after a touch");
    }

    #[test]
    fn av1_is_refused_for_now() {
        let mut w = Vec::new();
        w.extend_from_slice(&zenlink_android::mirror::CODEC_AV1.to_be_bytes());
        let mut v = VideoReader::new(&w[..]).unwrap();
        assert!(first_config(&mut v, &Shared::default()).is_err());
    }

    #[test]
    fn h265_config_becomes_an_hvcc_record() {
        let mut config = vec![0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01];
        config.extend_from_slice(&[
            0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0x5d, 0xa0,
        ]);
        config.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xc1, 0x72]);
        let mut w = Vec::new();
        w.extend_from_slice(&CODEC_H265.to_be_bytes());
        write_packet(&mut w, &Packet::Config(config)).unwrap();
        let mut v = VideoReader::new(&w[..]).unwrap();
        let info = first_config(&mut v, &Shared::default()).unwrap();
        assert_eq!(info.codec, Codec::Hevc);
        assert_eq!(info.description[22], 3, "VPS, SPS, PPS arrays");
    }

    #[test]
    fn failed_h265_falls_back_to_h264() {
        let shared = Arc::new(Shared::default());
        let mut src = DeviceSource::new(
            "x".into(),
            None,
            "x".into(),
            Arc::new(Vec::new()),
            shared,
            VideoOptions::default(),
            Smart {
            pref: Preference {
                codec: "h265".into(),
                encoder: String::new(),
            },
            dec: Decoders {
                h265: true,
                av1: false,
            },
            // The phone listed an H.265 and an H.264 hardware encoder.
            known: Some(encoders::parse_list(
                "    --video-codec=h265 --video-encoder=c2.x.hevc.encoder (hw)\n    --video-codec=h264 --video-encoder=c2.x.avc.encoder (hw)",
            )),
            },
        );
        src.ensure_plan(&Target::Serial("x"));
        assert_eq!(src.attempt().codec, "h265");
        assert_eq!(src.attempt().encoder.as_deref(), Some("c2.x.hevc.encoder"));
        assert_eq!(src.shared.encoders().unwrap().len(), 2);
        // Every planned attempt gets a turn before the session stops.
        let n = src.plan.len();
        assert!(n <= encoders::MAX_ATTEMPTS);
        for _ in 1..n {
            src.fail("encoder".into());
            assert!(!src.finished());
        }
        assert_eq!(src.attempt().codec, "h264");
        assert_eq!(src.attempt().encoder, None);
        while !src.finished() {
            src.fail("encoder".into());
        }
        assert_eq!(src.failures, src.max_failures());
        assert_eq!(src.shared.status().phase, Phase::Error);
    }

    #[test]
    fn builtin_server_matches_the_pinned_release() {
        assert_eq!(BUILTIN_SERVER.len() as u64, SERVER_SIZE);
        assert_eq!(SERVER_FILE, "scrcpy-server-v5.0.1");
    }

    #[test]
    fn bundled_server_is_found_from_the_repo() {
        std::env::set_var(
            "ZL_SCRCPY_SERVER",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../ZenshiroDev/resources/third_party/scrcpy")
                .join(SERVER_FILE),
        );
        assert_eq!(load_server().unwrap().len() as u64, SERVER_SIZE);
    }

    #[test]
    fn adb_can_be_disabled() {
        std::env::set_var("ZL_ADB", "none");
        assert!(find_adb().is_none());
        std::env::remove_var("ZL_ADB");
    }
}
