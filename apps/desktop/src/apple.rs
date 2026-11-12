//! Apple Driver mode in Rust (v2.5.12, stage 1 of the Rust transition): the device list,
//! the tunnel, the Developer Disk Image and the screen, all in ZenLink.exe on `zenlink-apple`
//! (idevice). It replaces the Python backend's Apple session; ZenLink Studio still runs there.
//!
//! iOS 17-26 has no screen service: the backend still runs Hybrid (AirPlay picture + touch)
//! for those devices.
//!
//! The window reads the same state JSON the backend sent (`snapshot`), so `on_state` and the
//! device list stay as they are: `main.rs` lays this state over the backend's while the
//! connection mode is Apple Driver.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zenlink_apple::{hid, link, mirror, models, wifi};
use zenlink_core::api::{Codec, CodecInfo};
use zenlink_core::nal::{hvcc_from_annexb, split_annexb};
use zenlink_core::source::{RecordStream, VideoSource};
use zenlink_core::stream::Record;

/// USB list refresh.
const USB_EVERY: Duration = Duration::from_secs(2);
/// Wi-Fi browse (each takes `WIFI_WAIT`).
const WIFI_EVERY: Duration = Duration::from_secs(12);
const WIFI_WAIT: Duration = Duration::from_secs(4);
/// A Wi-Fi device not seen for this long leaves the list.
const WIFI_FORGET: Duration = Duration::from_secs(45);

#[derive(Clone, Debug)]
struct Dev {
    udid: String,
    /// Wi-Fi address (RemotePairing); `None` = USB.
    addr: Option<SocketAddr>,
    /// USB: this PC has a pair record (Trust done). Wi-Fi: always (the advert matched one).
    paired: bool,
    seen: Instant,
}

#[derive(Default)]
struct State {
    phase: &'static str,
    detail: String,
    error: Option<String>,
    /// The device the session runs on.
    device: Option<(String, bool)>,
    /// Clockwise degrees that show the picture upright (SpringBoard orientation).
    rotation: u32,
    /// iOS 17-26: the picture comes over AirPlay (airplay.rs), this session only does input.
    hybrid: bool,
}

/// One device's session. Several run at once; only the one on screen streams its picture,
/// the others keep their tunnel, input and lock watch until Stop.
struct Sess {
    state: Mutex<State>,
    /// Set to end the session.
    stop: AtomicBool,
    /// Touches, keys and buttons into the session.
    input: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Input>>>,
    /// Ask the device for a keyframe (cleared when one arrives).
    key: AtomicBool,
    /// The screen's size (portrait): the stream pads it and does not say so.
    screen: Mutex<Option<(u32, u32)>>,
}

impl Sess {
    fn set(&self, phase: &'static str, detail: &str, error: Option<String>) {
        let mut s = self.state.lock().unwrap();
        s.phase = phase;
        s.detail = detail.to_string();
        s.error = error;
    }

    fn live(&self) -> bool {
        matches!(
            self.state.lock().unwrap().phase,
            "connecting" | "activating" | "streaming"
        )
    }

    fn send(&self, i: Input) {
        if let Some(tx) = self.input.lock().unwrap().as_ref() {
            let _ = tx.send(i);
        }
    }
}

pub struct Engine {
    rt: tokio::runtime::Runtime,
    devices: Mutex<Vec<Dev>>,
    info: Mutex<HashMap<String, link::Info>>,
    /// Sessions by UDID.
    sessions: Mutex<HashMap<String, Arc<Sess>>>,
    /// The device on screen (the list's pick, or the one Start fell back to).
    view: Mutex<Option<String>>,
    /// Why the last Start found no device (shown while no session is on screen).
    fail: Mutex<Option<String>>,
    /// Access units of the session on screen (its UDID), taken by the decoder's source.
    frames: Mutex<Option<(String, Receiver<Vec<u8>>)>>,
    watching: AtomicBool,
    /// Wi-Fi devices whose names were asked for this run (one tunnel each, once).
    asked: Mutex<HashSet<String>>,
    /// Automatic Wi-Fi setup: when each cable device was last tried.
    wifi_tried: Mutex<HashMap<String, Instant>>,
}

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

/// The connection mode is Apple Driver: this engine serves the Apple workspace.
static IN_USE: AtomicBool = AtomicBool::new(false);
/// From the backend's state: the device picked in the list, and Wi-Fi (wireless) vs USB.
static PICKED: Mutex<Option<String>> = Mutex::new(None);
static WIRELESS: AtomicBool = AtomicBool::new(false);
/// Settings > unlock the device again when it locks while mirroring.
static UNLOCK_ON_LOCK: AtomicBool = AtomicBool::new(false);
/// Settings > limit automatic unlocks (3 in 30 s).
static UNLOCK_LIMIT: AtomicBool = AtomicBool::new(true);

/// Settings > Audio: sound on the PC (not "device only"), its volume (f32 bits) and mute.
static AUDIO_TO_PC: AtomicBool = AtomicBool::new(true);
static AUDIO_VOLUME: AtomicU32 = AtomicU32::new(0x3f80_0000);
static AUDIO_MUTED: AtomicBool = AtomicBool::new(false);
/// Presses that take the device from full volume to 0 (ZenLink 2.5.x VOLUME_STEPS).
const VOLUME_STEPS: usize = 20;

/// Called whenever the window pushes its sound settings. A route change while mirroring
/// moves the device volume (down to 0 for "PC only", back to half when leaving it).
pub fn set_audio(to_pc: bool, volume: f32, muted: bool) {
    AUDIO_VOLUME.store(volume.clamp(0.0, 1.0).to_bits(), Relaxed);
    AUDIO_MUTED.store(muted, Relaxed);
    if AUDIO_TO_PC.swap(to_pc, Relaxed) != to_pc {
        if let Some(e) = ENGINE.get() {
            for s in e.sessions.lock().unwrap().values() {
                s.send(Input::Volume);
            }
        }
    }
}

/// Settings > Audio: sound on the PC (not "device only").
pub fn audio_to_pc() -> bool {
    AUDIO_TO_PC.load(Relaxed)
}

/// Settings > Audio: volume 0..1 and mute.
pub fn audio_level() -> (f32, bool) {
    (
        f32::from_bits(AUDIO_VOLUME.load(Relaxed)),
        AUDIO_MUTED.load(Relaxed),
    )
}

/// The button that turns the volume `down` (or up). iPadOS swaps the two at rotation 270
/// (verified in ZenLink 2.5.x).
fn volume_button(down: bool, ipad: bool, rotation: u32) -> &'static str {
    if down != (ipad && rotation == 270) {
        "volume-down"
    } else {
        "volume-up"
    }
}

/// Plays the device's sound (AAC-ELD access units of `asc` at `rate`) on the PC, on a thread
/// of its own like Android's; ends when the session drops the sender. AirPlay uses it too.
pub fn play_sound(rx: Receiver<Vec<u8>>, wifi: bool, asc: &'static [u8], rate: i32) {
    let _ = std::thread::Builder::new()
        .name("apple-audio".into())
        .spawn(move || {
            let Ok(first) = rx.recv() else { return };
            let mut dec = match crate::aac::Aac::new(asc, rate) {
                Ok(d) => d,
                Err(e) => return log::warn!("apple: no sound ({e}); picture only"),
            };
            let mut player = match zenlink_audio::Player::start() {
                Ok(p) => p,
                Err(e) => return log::warn!("apple: no sound output ({e}); picture only"),
            };
            player.set_smooth(true);
            player.set_jitter_ms(if wifi { 60.0 } else { 40.0 });
            for au in std::iter::once(first).chain(rx.iter()) {
                player.set_muted(AUDIO_MUTED.load(Relaxed));
                player.set_volume(f32::from_bits(AUDIO_VOLUME.load(Relaxed)));
                player.feed(dec.decode(&au));
            }
        });
}

pub fn set_unlock_on_lock(on: bool, limit: bool) {
    UNLOCK_ON_LOCK.store(on, Relaxed);
    UNLOCK_LIMIT.store(limit, Relaxed);
}

/// What the session's HID task runs, in order.
enum Input {
    Hid(hid::Cmd),
    /// SpringBoard reported a lock, at this time.
    Locked(Instant),
    /// The sound route changed, or the stream now takes buttons: match the device volume.
    Volume,
}

/// Whether a lock reported `at` gets an automatic unlock. Locks that arrived while the last
/// unlock ran, or within 1 s after it (SpringBoard reports the lock it just woke from late),
/// are skipped; with `limit`, at most 3 unlocks in 30 s, so a wrong passcode cannot loop.
fn relock_allowed(
    at: Instant,
    unlocked_at: Option<Instant>,
    recent: &mut Vec<Instant>,
    limit: bool,
) -> bool {
    if unlocked_at.is_some_and(|u| at < u + Duration::from_secs(1)) {
        return false;
    }
    recent.retain(|t| at.duration_since(*t) < Duration::from_secs(30));
    if limit && recent.len() >= 3 {
        return false;
    }
    recent.push(at);
    true
}

/// Apple Driver mode: input and video go to this engine.
pub fn in_use() -> bool {
    IN_USE.load(Relaxed)
}

/// The device to send files to: the one on screen, else the picked one, else the first trusted.
pub fn file_device() -> Option<String> {
    let e = engine();
    let shown = e.view.lock().unwrap().clone();
    shown
        .or_else(|| PICKED.lock().unwrap().clone())
        .or_else(|| {
            e.devices
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.paired)
                .map(|d| d.udid.clone())
        })
}

/// One-time Wi-Fi setup over the cable (Settings), in the backend's reply shape
/// {ok, message, device}. Off the UI thread.
pub fn prepare_wifi() -> Value {
    let fail = |m: &str| json!({ "ok": false, "message": m });
    if !IN_USE.load(Relaxed) {
        return fail("Wi-Fi needs Apple Driver mode.");
    }
    let e = engine();
    if e.sessions.lock().unwrap().values().any(|s| s.live()) {
        return fail("Stop casting first.");
    }
    let usb = match block_on(link::usb_devices()) {
        Ok(u) => u,
        Err(err) => return fail(&format!("Wi-Fi setup failed: {err}")),
    };
    let dir = zenlink_apple::wifi::record_dir();
    let has_record = |u: &str| {
        dir.as_ref()
            .is_some_and(|d| d.join(format!("remote_{u}.plist")).is_file())
    };
    let name = |u: &str| {
        custom_name(u).or_else(|| {
            let n = e.info.lock().unwrap().get(u)?.name.clone();
            (!n.is_empty()).then_some(n)
        })
    };
    let mut ready: Vec<String> = usb
        .iter()
        .filter(|(_, p)| *p)
        .map(|(u, _)| u.clone())
        .collect();
    if ready.is_empty() {
        if !usb.is_empty() {
            return fail("Unlock the device and tap Trust, then try again.");
        }
        let wifi = e
            .devices
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.addr.is_some())
            .map(|d| d.udid.clone());
        return match wifi {
            Some(u) => {
                json!({ "ok": true, "message": "Wi-Fi is already set up.", "device": name(&u) })
            }
            None => fail("Connect the device with the cable, unlock it and tap Trust first."),
        };
    }
    ready.sort_by_key(|u| has_record(u)); // a device with no Wi-Fi record yet first
    let udid = &ready[0];
    match block_on(zenlink_apple::wifi::prepare(udid)) {
        Ok(paired) => {
            log::info!("wifi: prepared {udid} (RemotePairing record: {paired})");
            json!({ "ok": true, "wifiConnections": true, "remotePairing": paired, "device": name(udid) })
        }
        Err(err) => fail(&format!("Wi-Fi setup failed: {err}")),
    }
}

/// Runs `f` on the engine's runtime; call from a plain thread, never from the runtime.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    engine().rt.block_on(f)
}

/// Lays the engine's state over the backend's while the mode is Apple Driver.
/// The backend keeps the mode, the picked device, the saved devices and the drivers.
pub fn overlay(st: &mut Value) {
    let conn = |k: &str| {
        st.pointer(&format!("/connection/{k}"))
            .and_then(Value::as_str)
    };
    let apple = conn("active") == Some("apple");
    WIRELESS.store(conn("link") == Some("wireless"), Relaxed);
    let picked = st
        .get("selected")
        .and_then(Value::as_str)
        .map(str::to_string);
    let changed = {
        let mut p = PICKED.lock().unwrap();
        let changed = *p != picked;
        *p = picked.clone();
        changed
    };
    if IN_USE.swap(apple, Relaxed) && !apple {
        engine().stop_all(); // the mode changed away from Apple Driver
    }
    if apple && changed && picked.is_some() {
        engine().show(picked); // a pick in the list switches the screen to that device
    }
    if !apple {
        return;
    }
    let mut snap = engine().snapshot();
    if snap["devices"].as_array().is_some_and(Vec::is_empty) {
        // Apple's service sees nothing: keep the backend's devices whose driver is wrong.
        snap.as_object_mut().map(|o| o.remove("devices"));
    }
    if let (Some(dst), Value::Object(src)) = (st.as_object_mut(), snap) {
        dst.extend(src);
    }
}

/// Start in the Apple workspace (the window's Start button). `false`: not Apple Driver mode.
pub fn start() -> bool {
    if !IN_USE.load(Relaxed) {
        return false;
    }
    let picked = PICKED.lock().unwrap().clone();
    engine().start(picked, WIRELESS.load(Relaxed));
    true
}

/// iOS major version 17-26: no screen service, so Hybrid (picture over AirPlay).
fn hybrid_version(version: &str) -> bool {
    version
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .is_some_and(|m| m < 27)
}

pub fn engine() -> &'static Arc<Engine> {
    ENGINE.get_or_init(|| {
        Arc::new(Engine {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("zenlink-apple")
                .enable_all()
                .build()
                .expect("tokio runtime"),
            devices: Mutex::new(Vec::new()),
            info: Mutex::new(load_info()),
            sessions: Mutex::new(HashMap::new()),
            view: Mutex::new(None),
            fail: Mutex::new(None),
            frames: Mutex::new(None),
            watching: AtomicBool::new(false),
            asked: Mutex::new(HashSet::new()),
            wifi_tried: Mutex::new(HashMap::new()),
        })
    })
}

fn info_file() -> Option<std::path::PathBuf> {
    zenlink_core::local_data_dir().map(|d| d.join("apple-devices.json"))
}

/// Names learned over USB, so a Wi-Fi-only device keeps its name.
fn load_info() -> HashMap<String, link::Info> {
    let Some(v) = info_file()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return HashMap::new();
    };
    let s = |d: &Value, k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(udid, d)| {
            (
                udid.clone(),
                link::Info {
                    name: s(d, "name"),
                    product_type: s(d, "productType"),
                    version: s(d, "version"),
                    device_class: s(d, "deviceClass"),
                },
            )
        })
        .collect()
}

fn save_info(all: &HashMap<String, link::Info>) {
    let v: serde_json::Map<String, Value> = all
        .iter()
        .map(|(u, i)| {
            (
                u.clone(),
                json!({ "name": i.name, "productType": i.product_type,
                        "version": i.version, "deviceClass": i.device_class }),
            )
        })
        .collect();
    if let Some(p) = info_file() {
        let _ = std::fs::write(
            p,
            serde_json::to_vec_pretty(&Value::Object(v)).unwrap_or_default(),
        );
    }
}

/// A name the user gave the device in ZenLink (the backend's devices.json).
fn custom_name(udid: &str) -> Option<String> {
    let p = crate::backend_data_dir()?.join("devices.json");
    let v: Value = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    v.get(udid.replace('-', ""))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// secrets.json: small secrets sealed with DPAPI (only this Windows user on this PC opens
/// them), base64 per name. Shared with the backend's secret.py.
fn secrets() -> Option<(std::path::PathBuf, serde_json::Map<String, Value>)> {
    let p = crate::backend_data_dir()?.join("secrets.json");
    let m = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    Some((p, m))
}

/// DPAPI seal (`protect`) or open, no UI.
fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `data`, alive for the call; `out` is freed with LocalFree.
    unsafe {
        let r = if protect {
            CryptProtectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        r.ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        Some(v)
    }
}

/// The saved passcode.
fn passcode() -> String {
    use base64::Engine as _;
    secrets()
        .and_then(|(_, m)| m.get("passcode")?.as_str().map(str::to_string))
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .and_then(|b| dpapi(&b, false))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Whether a passcode is saved.
pub fn has_passcode() -> bool {
    secrets().is_some_and(|(_, m)| {
        m.get("passcode")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    })
}

/// Saves the passcode (cleaned to what can be typed; empty removes it). `Some(saved)`,
/// or `None` when it could not be written.
pub fn save_passcode(raw: &str) -> Option<bool> {
    use base64::Engine as _;
    let code = zenlink_apple::hid::clean_passcode(raw);
    let (path, mut m) = secrets()?;
    if code.is_empty() {
        m.remove("passcode");
    } else {
        let sealed = dpapi(code.as_bytes(), true)?;
        m.insert(
            "passcode".into(),
            base64::engine::general_purpose::STANDARD
                .encode(sealed)
                .into(),
        );
    }
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(path, Value::Object(m).to_string()).ok()?;
    Some(!code.is_empty())
}

impl Engine {
    /// A touch, key or button for the session on screen (dropped when none runs).
    pub fn send(&self, c: hid::Cmd) {
        if let Some(s) = self.shown() {
            s.send(Input::Hid(c));
        }
    }

    fn shown(&self) -> Option<Arc<Sess>> {
        let v = self.view.lock().unwrap().clone()?;
        self.sessions.lock().unwrap().get(&v).cloned()
    }

    fn is_shown(&self, udid: &str) -> bool {
        self.view.lock().unwrap().as_deref() == Some(udid)
    }

    /// Puts `udid` on screen (`None`: no Apple device, e.g. an Android phone is shown).
    /// Sessions off screen keep streaming but are neither drawn nor heard.
    pub fn show(&self, udid: Option<String>) {
        *self.view.lock().unwrap() = udid;
    }

    fn watch(self: &Arc<Self>) {
        if self.watching.swap(true, Relaxed) {
            return;
        }
        let me = self.clone();
        self.rt.spawn(async move {
            let mut wifi_at = Instant::now()
                .checked_sub(WIFI_EVERY)
                .unwrap_or_else(Instant::now);
            loop {
                me.refresh_usb().await;
                if wifi_at.elapsed() >= WIFI_EVERY {
                    wifi_at = Instant::now();
                    let me2 = me.clone();
                    tokio::spawn(async move { me2.refresh_wifi().await });
                }
                tokio::time::sleep(USB_EVERY).await;
            }
        });
    }

    async fn refresh_usb(&self) {
        // A hung usbmuxd/lockdown call must not stall the device list for good.
        let list = tokio::time::timeout(Duration::from_secs(10), link::usb_devices())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let mut usb = Vec::new();
        for (udid, paired) in list {
            if paired && !self.info.lock().unwrap().contains_key(&udid) {
                if let Ok(Ok(i)) =
                    tokio::time::timeout(Duration::from_secs(10), link::info(&udid)).await
                {
                    let mut all = self.info.lock().unwrap();
                    all.insert(udid.clone(), i);
                    save_info(&all);
                }
            }
            usb.push(Dev {
                udid,
                addr: None,
                paired,
                seen: Instant::now(),
            });
        }
        self.auto_wifi(&usb);
        let mut devs = self.devices.lock().unwrap();
        devs.retain(|d| d.addr.is_some());
        devs.extend(usb);
    }

    /// Like PhoneLink V1: a trusted device on the cable gets Wi-Fi set up once (at most one try
    /// every 10 minutes, only in Apple Driver mode, never while casting), so it can later cast
    /// without the cable. A failed RemotePairing (Trust declined, old iOS) writes no record, so a
    /// short retry would ask the device again and again.
    fn auto_wifi(&self, usb: &[Dev]) {
        if !in_use() || self.sessions.lock().unwrap().values().any(|s| s.live()) {
            return;
        }
        let Some(dir) = wifi::record_dir() else {
            return;
        };
        let mut tried = self.wifi_tried.lock().unwrap();
        for d in usb.iter().filter(|d| d.paired) {
            let due = tried
                .get(&d.udid)
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(600));
            if !due || dir.join(format!("remote_{}.plist", d.udid)).is_file() {
                continue;
            }
            tried.insert(d.udid.clone(), Instant::now());
            let udid = d.udid.clone();
            tokio::spawn(async move {
                match wifi::prepare(&udid).await {
                    Ok(paired) => log::info!(
                        "wifi: auto setup of {udid} done (RemotePairing record: {paired})"
                    ),
                    Err(e) => log::warn!("wifi: auto setup of {udid} failed: {e}"),
                }
            });
        }
    }

    async fn refresh_wifi(self: &Arc<Self>) {
        let Some(dir) = wifi::record_dir() else {
            return;
        };
        let records = wifi::records(&dir);
        if records.is_empty() {
            return;
        }
        let found = wifi::browse(&records, WIFI_WAIT).await.unwrap_or_default();
        for f in &found {
            let known = self.info.lock().unwrap().contains_key(&f.udid);
            if !known && self.asked.lock().unwrap().insert(f.udid.clone()) {
                self.learn_info(f.udid.clone(), f.addr);
            }
        }
        let mut devs = self.devices.lock().unwrap();
        for f in found {
            devs.retain(|d| !(d.addr.is_some() && d.udid == f.udid));
            devs.push(Dev {
                udid: f.udid,
                addr: Some(f.addr),
                paired: true,
                seen: Instant::now(),
            });
        }
        devs.retain(|d| d.addr.is_none() || d.seen.elapsed() < WIFI_FORGET);
    }

    /// A device first seen on Wi-Fi gets its name and model over a short tunnel of its own,
    /// so the list shows it before it is ever started.
    fn learn_info(self: &Arc<Self>, udid: String, addr: SocketAddr) {
        let me = self.clone();
        let _ = std::thread::Builder::new()
            .name("zenlink-apple-info".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let ask = async {
                    let dir = wifi::record_dir().ok_or("no user folder")?;
                    let r = wifi::records(&dir)
                        .into_iter()
                        .find(|r| r.udid == udid)
                        .ok_or("no Wi-Fi pairing record")?;
                    link::Link::wifi(addr, &r).await?.info().await
                };
                // The timer must be made inside the runtime: built outside it, it panics
                // ("no reactor running") and, with panic = abort, ends ZenLink.
                match rt
                    .block_on(async { tokio::time::timeout(Duration::from_secs(20), ask).await })
                {
                    Ok(Ok(i)) if !i.product_type.is_empty() => {
                        log::info!("apple: learned {} ({}) over Wi-Fi", i.name, i.product_type);
                        let mut all = me.info.lock().unwrap();
                        all.insert(udid, i);
                        save_info(&all);
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => log::info!("apple: device info over Wi-Fi: {e}"),
                    Err(_) => log::info!("apple: device info over Wi-Fi: timed out"),
                }
            });
    }

    fn entry(&self, d: &Dev) -> Value {
        let info = self
            .info
            .lock()
            .unwrap()
            .get(&d.udid)
            .cloned()
            .unwrap_or_default();
        let family = match info.device_class.as_str() {
            "iPad" | "iPhone" => info.device_class.as_str(),
            _ => "iOS device",
        };
        let model = custom_name(&d.udid)
            .or_else(|| models::product_name(&info.product_type).map(str::to_string))
            .unwrap_or_else(|| models::series_name(&d.udid, family));
        json!({
            "id": d.udid,
            "name": model,
            "deviceName": (!info.name.is_empty()).then_some(info.name),
            "os": (!info.version.is_empty()).then_some(info.version),
            "ready": d.paired,
            "wifi": d.addr.is_some(),
            "need": if d.paired { Value::Null } else { "trust".into() },
            "live": self.sessions.lock().unwrap().get(&d.udid).is_some_and(|s| s.live()),
        })
    }

    /// State for the window, in the backend's shape. USB entries come before Wi-Fi ones;
    /// a device on both is listed twice (the window keeps the first).
    pub fn snapshot(self: &Arc<Self>) -> Value {
        self.watch();
        let mut devs = self.devices.lock().unwrap().clone();
        // Stable order: each Wi-Fi scan re-adds devices in mDNS answer order.
        devs.sort_by(|a, b| (a.addr.is_some(), &a.udid).cmp(&(b.addr.is_some(), &b.udid)));
        let devices: Vec<Value> = devs.iter().map(|d| self.entry(d)).collect();
        let shown = self.shown();
        let idle = State {
            phase: if self.fail.lock().unwrap().is_some() {
                "error"
            } else {
                "idle"
            },
            error: self.fail.lock().unwrap().clone(),
            ..State::default()
        };
        let guard = shown.as_ref().map(|x| x.state.lock().unwrap());
        let s = guard.as_deref().unwrap_or(&idle);
        let live = matches!(s.phase, "connecting" | "activating" | "streaming");
        let mut v = json!({
            "phase": s.phase,
            "detail": s.detail,
            "error": s.error,
            "devices": devices,
            "device": s.device.as_ref().map(|(n, w)| json!({ "name": n, "wifi": w })),
            "capture": live.then(|| if s.hybrid {
                json!({ "backend": "hybrid", "codec": "H.264",
                        "source": "AirPlay (Wi-Fi) + touch over the Apple driver" })
            } else {
                json!({ "backend": "apple", "codec": "HEVC", "source": "CoreDevice DisplayService" })
            }),
            // Hybrid: AirPlay's picture is upright already.
            "rotation": if s.hybrid { 0 } else { s.rotation },
        });
        if let (Some(_), Some(u)) = (&shown, self.view.lock().unwrap().as_ref()) {
            v["selected"] = u.as_str().into();
        }
        v
    }

    /// Starts the picked device (when listed), else the first ready one; USB before Wi-Fi
    /// unless `wireless`. `false`: the device runs iOS 17-26, left to the backend's Hybrid.
    pub fn start(self: &Arc<Self>, picked: Option<String>, wireless: bool) {
        *self.fail.lock().unwrap() = None;
        let devs = self.devices.lock().unwrap().clone();
        let ready = |d: &&Dev| d.paired;
        let on_link = |d: &&Dev| d.addr.is_some() == wireless;
        let dev = picked
            .as_ref()
            .and_then(|p| {
                devs.iter()
                    .filter(|d| &d.udid == p)
                    .filter(ready)
                    .min_by_key(|d| d.addr.is_some() != wireless)
            })
            .or_else(|| devs.iter().filter(ready).find(on_link))
            .or_else(|| devs.iter().find(ready))
            .cloned();
        let Some(dev) = dev else {
            let msg = if devs.is_empty() {
                "No iPhone or iPad found. Connect it with a cable, unlock it and tap Trust."
            } else {
                "Unlock the device and tap Trust, then press Start again."
            };
            *self.fail.lock().unwrap() = Some(msg.into());
            self.show(None);
            return;
        };
        let version = self
            .info
            .lock()
            .unwrap()
            .get(&dev.udid)
            .map(|i| i.version.clone());
        let hybrid = version.is_some_and(|v| hybrid_version(&v));
        let name = self
            .entry(&dev)
            .get("deviceName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "iPhone/iPad".into());
        let sess = Arc::new(Sess {
            state: Mutex::new(State {
                device: Some((name, dev.addr.is_some())),
                hybrid,
                ..State::default()
            }),
            stop: AtomicBool::new(false),
            input: Mutex::new(None),
            key: AtomicBool::new(false),
            screen: Mutex::new(None),
        });
        // Start again on a running device: the old session ends first.
        if let Some(old) = self
            .sessions
            .lock()
            .unwrap()
            .insert(dev.udid.clone(), sess.clone())
        {
            old.stop.store(true, Relaxed);
        }
        self.show(Some(dev.udid.clone()));
        sess.set(
            "connecting",
            if dev.addr.is_some() {
                "Opening a secure Wi-Fi tunnel to the device"
            } else {
                "Opening the USB tunnel to the device"
            },
            None,
        );
        let me = self.clone();
        let sess = sess.clone();
        // A thread of its own: idevice's display-service futures are not `Send`.
        let spawned = std::thread::Builder::new()
            .name("zenlink-apple-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return sess.set("error", "", Some(format!("runtime: {e}"))),
                };
                let r = rt.block_on(me.session(&sess, &dev));
                if sess.stop.load(Relaxed) {
                    return; // stopped by the user: stop() already removed it
                }
                match r {
                    Ok(()) => sess.set("idle", "", None),
                    Err(e) => {
                        log::warn!("apple: {e}");
                        sess.set("error", "", Some(e));
                    }
                }
            });
        if let Err(e) = spawned {
            *self.fail.lock().unwrap() = Some(format!("thread: {e}"));
        }
    }

    async fn session(&self, sess: &Sess, dev: &Dev) -> Result<(), String> {
        if !sess.state.lock().unwrap().hybrid {
            return self.session_inner(sess, dev, false).await;
        }
        let _picture = crate::airplay::Hybrid::start()?;
        if let Err(e) = self.session_inner(sess, dev, true).await {
            // hybrid.py: the AirPlay picture keeps running without touch control.
            log::warn!("hybrid: control path failed: {e}; picture only");
            while !sess.stop.load(Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        Ok(())
    }

    async fn session_inner(&self, sess: &Sess, dev: &Dev, hybrid: bool) -> Result<(), String> {
        let stop = &sess.stop;
        let version = self
            .info
            .lock()
            .unwrap()
            .get(&dev.udid)
            .map(|i| i.version.clone())
            .unwrap_or_default();
        let mut l = match dev.addr {
            Some(addr) => {
                let dir = wifi::record_dir().ok_or("no user folder")?;
                let r = wifi::records(&dir)
                    .into_iter()
                    .find(|r| r.udid == dev.udid)
                    .ok_or("the Wi-Fi pairing record is gone: connect the cable once")?;
                link::Link::wifi(addr, &r).await?
            }
            None => link::Link::usb(&dev.udid).await?,
        };
        if l.wifi && !self.info.lock().unwrap().contains_key(&dev.udid) {
            match l.info().await {
                Ok(i) if !i.product_type.is_empty() => {
                    if !i.name.is_empty() {
                        sess.state.lock().unwrap().device = Some((i.name.clone(), true));
                    }
                    let mut all = self.info.lock().unwrap();
                    all.insert(dev.udid.clone(), i);
                    save_info(&all);
                }
                Ok(_) => {}
                Err(e) => log::info!("apple: device info over Wi-Fi: {e}"),
            }
        }
        let screen = l.screen().await;
        log::info!("apple: screen {screen:?}");
        *sess.screen.lock().unwrap() = screen;
        if !l.display_offered() {
            sess.set(
                "activating",
                "Preparing the device (first time downloads a small image)",
                None,
            );
            l.ensure_ddi(&version, |_| {}).await?;
        }
        let watch_rot = zenlink_apple::watch::orientation(l.adapter.clone(), l.rsd.clone(), |r| {
            sess.state.lock().unwrap().rotation = r;
        });
        let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel();
        *sess.input.lock().unwrap() = Some(itx.clone());
        let vtx = itx.clone();
        let watch_lock = zenlink_apple::watch::lock(l.adapter.clone(), l.rsd.clone(), move || {
            log::info!("apple: device locked");
            let _ = itx.send(Input::Locked(Instant::now()));
        });
        let ipad = self
            .info
            .lock()
            .unwrap()
            .get(&dev.udid)
            .is_some_and(|i| i.device_class == "iPad");
        let mut hid = hid::Hid::new(l.adapter.clone(), l.rsd.clone(), ipad);
        let (atx, arx) = sync_channel::<Vec<u8>>(64);
        play_sound(arx, l.wifi, &mirror::AUDIO_ASC, 48000);
        // ZenLink turned the device volume down ("PC only").
        let quiet = std::cell::Cell::new(false);
        let input = async {
            let mut unlocked_at: Option<Instant> = None;
            let mut recent = Vec::new();
            while let Some(i) = irx.recv().await {
                let cmd = match i {
                    Input::Hid(hid::Cmd::Action { name, .. }) if name == "wake" => {
                        hid::Cmd::Action {
                            name,
                            text: passcode(),
                        }
                    }
                    // AirPlay shows the screen already turned: map back to the portrait touch space.
                    Input::Hid(hid::Cmd::Touch { contact, x, y }) if hybrid => {
                        let rot = sess.state.lock().unwrap().rotation;
                        let (x, y) = zenlink_core::api::device_point(
                            x as f32 / 65535.0,
                            y as f32 / 65535.0,
                            rot,
                        );
                        hid::Cmd::Touch { contact, x, y }
                    }
                    Input::Hid(hid::Cmd::Fingers { touching, points }) if hybrid => {
                        let rot = sess.state.lock().unwrap().rotation;
                        let points = points.map(|(x, y)| {
                            zenlink_core::api::device_point(
                                x as f32 / 65535.0,
                                y as f32 / 65535.0,
                                rot,
                            )
                        });
                        hid::Cmd::Fingers { touching, points }
                    }
                    Input::Hid(c) => c,
                    Input::Volume => {
                        let down = AUDIO_TO_PC.load(Relaxed);
                        if down == quiet.get() {
                            continue;
                        }
                        let rot = sess.state.lock().unwrap().rotation;
                        let times = if down { VOLUME_STEPS } else { VOLUME_STEPS / 2 };
                        match press(&mut hid, volume_button(down, ipad, rot), times).await {
                            Ok(()) => {
                                quiet.set(down);
                                log::info!(
                                    "apple: device volume {}",
                                    if down {
                                        "down (PC only)"
                                    } else {
                                        "back to half"
                                    }
                                );
                            }
                            Err(e) => log::warn!("apple: device volume: {e}"),
                        }
                        continue;
                    }
                    // Wake and type the saved passcode.
                    Input::Locked(at) => {
                        if !UNLOCK_ON_LOCK.load(Relaxed)
                            || sess.state.lock().unwrap().phase != "streaming"
                        {
                            continue;
                        }
                        if !relock_allowed(at, unlocked_at, &mut recent, UNLOCK_LIMIT.load(Relaxed))
                        {
                            log::info!("apple: lock not unlocked (just unlocked, or 3 in 30 s)");
                            continue;
                        }
                        tokio::time::sleep(Duration::from_millis(800)).await; // let the lock finish
                        hid::Cmd::Action {
                            name: "wake".into(),
                            text: passcode(),
                        }
                    }
                };
                let wake = matches!(&cmd, hid::Cmd::Action { name, .. } if name == "wake");
                match hid.run(cmd).await {
                    Ok(note) if !note.is_empty() => log::info!("apple input: {note}"),
                    Ok(_) => {}
                    Err(e) => log::warn!("apple input: {e}"),
                }
                if wake {
                    unlocked_at = Some(Instant::now());
                }
            }
        };
        // The stream runs for the whole session; off screen its frames are dropped. Coming
        // back asks the device for a keyframe (iOS sends none on its own) and shows from it.
        let pic = std::cell::RefCell::new(Pic::default());
        let restart = std::cell::Cell::new(false);
        let video = async {
            let mut ever = false;
            loop {
                if stop.load(Relaxed) {
                    return Ok(());
                }
                restart.set(false);
                mirror::stream(
                    &mut l,
                    &sess.key,
                    hybrid,
                    |au| {
                        if stop.load(Relaxed) || restart.get() {
                            return false;
                        }
                        if !ever {
                            ever = true;
                            sess.set("streaming", "", None);
                            // Presses sent while the stream settles are dropped (ZenLink 2.5.x: 3 s).
                            let vtx = vtx.clone();
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_secs(3));
                                let _ = vtx.send(Input::Volume);
                            });
                        }
                        if pic.borrow_mut().push(au) {
                            sess.key.store(false, Relaxed);
                        }
                        true
                    },
                    |au| {
                        if AUDIO_TO_PC.load(Relaxed) && pic.borrow().tx.is_some() {
                            let _ = atx.try_send(au.to_vec());
                        }
                    },
                )
                .await?;
            }
        };
        // Hands the picture to the decoder while the device is on screen.
        let screen = async {
            loop {
                let shown = self.is_shown(&dev.udid);
                let mut p = pic.borrow_mut();
                if shown && p.tx.is_none() {
                    let (tx, rx) = sync_channel(120);
                    p.tx = Some(tx);
                    p.wait = Some(Instant::now());
                    sess.key.store(true, Relaxed);
                    *self.frames.lock().unwrap() = Some((dev.udid.clone(), rx));
                } else if !shown && p.tx.is_some() {
                    p.tx = None;
                    p.wait = None;
                    sess.key.store(false, Relaxed);
                    log::info!("apple: {} off screen, picture paused", dev.udid);
                } else if p.wait.is_some_and(|w| w.elapsed() > KEY_WAIT) && p.started {
                    // The device ignored the requests: a new stream opens with a keyframe.
                    log::info!("apple: no keyframe after {KEY_WAIT:?}; restarting the stream");
                    p.started = false;
                    p.wait = Some(Instant::now());
                    restart.set(true);
                }
                drop(p);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        // The watchers never end; the session ends with the stream.
        // Stop also ends a Hybrid session, whose gate-only stream never calls back (no picture).
        let stopped = async {
            while !stop.load(Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        let r: Result<(), String> = tokio::select! {
            r = video => r,
            _ = stopped => Ok(()),
            _ = watch_rot => Ok(()),
            _ = watch_lock => Ok(()),
            _ = input => Ok(()),
            _ = screen => Ok(()),
        };
        if quiet.get() {
            // Give the device its sound back (half: the old level cannot be read).
            let rot = sess.state.lock().unwrap().rotation;
            let up = press(&mut hid, volume_button(false, ipad, rot), VOLUME_STEPS / 2);
            if !matches!(
                tokio::time::timeout(Duration::from_secs(5), up).await,
                Ok(Ok(()))
            ) {
                log::info!("apple: device volume not restored (link gone)");
            }
        }
        r
    }

    /// Stop: ends the session on screen; the others keep running.
    pub fn stop(&self) {
        *self.fail.lock().unwrap() = None;
        let Some(v) = self.view.lock().unwrap().clone() else {
            return;
        };
        if let Some(s) = self.sessions.lock().unwrap().remove(&v) {
            s.stop.store(true, Relaxed);
        }
        *self.frames.lock().unwrap() = None;
    }

    /// Leaving Apple Driver mode: ends every session.
    pub fn stop_all(&self) {
        *self.fail.lock().unwrap() = None;
        for (_, s) in self.sessions.lock().unwrap().drain() {
            s.stop.store(true, Relaxed);
        }
        *self.frames.lock().unwrap() = None;
    }

    pub fn source(self: &Arc<Self>) -> Box<dyn VideoSource> {
        Box::new(AppleSource {
            engine: self.clone(),
        })
    }
}

/// The decoder's view of the Rust session.
struct AppleSource {
    engine: Arc<Engine>,
}

impl VideoSource for AppleSource {
    fn open(&mut self) -> Option<(CodecInfo, Box<dyn RecordStream>)> {
        let (udid, rx) = self.engine.frames.lock().unwrap().take()?;
        let screen = self
            .engine
            .sessions
            .lock()
            .unwrap()
            .get(&udid)
            .and_then(|s| *s.screen.lock().unwrap());
        // A reopen (after a decode error) needs a fresh keyframe too.
        self.request_keyframe();
        // The stream opens with VPS/SPS/PPS; frames before them cannot be decoded.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let au = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(au) => au,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Keep the channel for the next try (slow Wi-Fi), else the picture stays dead
                    // until the device is taken off screen and back.
                    let mut slot = self.engine.frames.lock().unwrap();
                    if slot.is_none() && self.engine.is_shown(&udid) {
                        *slot = Some((udid, rx));
                    }
                    return None;
                }
                Err(_) => return None,
            };
            if let Some(info) = codec_info(&au, screen) {
                let pending = frame(&au);
                return Some((
                    info.clone(),
                    Box::new(AppleRecords {
                        udid,
                        rx: Some(rx),
                        engine: self.engine.clone(),
                        config: info.description,
                        pending,
                        screen,
                    }),
                ));
            }
        }
    }

    fn request_keyframe(&self) {
        if let Some(s) = self.engine.shown() {
            s.key.store(true, Relaxed);
        }
    }

    fn finished(&self) -> bool {
        !self.engine.shown().is_some_and(|s| s.live())
    }
}

struct AppleRecords {
    udid: String,
    /// Handed back to the engine on drop, so the decoder can reopen after a decode error.
    rx: Option<Receiver<Vec<u8>>>,
    engine: Arc<Engine>,
    config: Vec<u8>,
    pending: Option<Record>,
    screen: Option<(u32, u32)>,
}

impl RecordStream for AppleRecords {
    fn next_record(&mut self) -> Option<Record> {
        if let Some(r) = self.pending.take() {
            return Some(r);
        }
        loop {
            let au = match self.rx.as_ref()?.recv_timeout(Duration::from_secs(1)) {
                Ok(au) => au,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return None,
            };
            // New parameter sets (rotation, size change): reconfigure first.
            if let Some(info) =
                codec_info(&au, self.screen).filter(|i| i.description != self.config)
            {
                self.config = info.description.clone();
                self.pending = frame(&au);
                return Some(Record::Config(info));
            }
            if let Some(f) = frame(&au) {
                return Some(f);
            }
        }
    }
}

impl Drop for AppleRecords {
    fn drop(&mut self) {
        let mut slot = self.engine.frames.lock().unwrap();
        // Only while this device is still on screen and nothing newer took the slot.
        if slot.is_none() && self.engine.is_shown(&self.udid) {
            *slot = self.rx.take().map(|rx| (self.udid.clone(), rx));
        }
    }
}

fn nal_type(n: &[u8]) -> u8 {
    (n[0] >> 1) & 0x3f
}

fn codec_info(au: &[u8], screen: Option<(u32, u32)>) -> Option<CodecInfo> {
    Some(CodecInfo {
        codec: Codec::Hevc,
        description: hvcc_from_annexb(au)?,
        // RTCP PLI + FIR (see `Sess::key`).
        keyframes_on_request: true,
        display: screen,
    })
}

/// One decoder sample: the picture NAL units with 4-byte lengths; VPS/SPS/PPS go in the config.
fn frame(au: &[u8]) -> Option<Record> {
    let mut data = Vec::with_capacity(au.len());
    let mut key = false;
    for n in split_annexb(au).into_iter().filter(|n| n.len() > 2) {
        let t = nal_type(n);
        if (32..=34).contains(&t) {
            continue;
        }
        key |= (16..=21).contains(&t);
        data.extend_from_slice(&(n.len() as u32).to_be_bytes());
        data.extend_from_slice(n);
    }
    (!data.is_empty()).then_some(Record::Frame {
        key,
        rebuild: false,
        data,
    })
}

/// How long a return to the screen waits for the requested keyframe before restarting.
const KEY_WAIT: Duration = Duration::from_millis(2500);

/// The picture of one session: the decoder's channel while on screen, and the parameter sets
/// that a keyframe without them is sent with.
#[derive(Default)]
struct Pic {
    tx: Option<SyncSender<Vec<u8>>>,
    ps: Vec<u8>,
    /// Waiting for a keyframe since then: frames before it cannot be decoded.
    wait: Option<Instant>,
    /// The stream has sent frames (a restart waits for this).
    started: bool,
}

impl Pic {
    /// `true` for a keyframe.
    fn push(&mut self, au: Vec<u8>) -> bool {
        self.started = true;
        let mut ps = Vec::new();
        let mut key = false;
        for n in split_annexb(&au).into_iter().filter(|n| n.len() > 2) {
            match nal_type(n) {
                32..=34 => {
                    ps.extend_from_slice(&[0, 0, 0, 1]);
                    ps.extend_from_slice(n);
                }
                16..=21 => key = true,
                _ => {}
            }
        }
        let bare = ps.is_empty();
        if !bare {
            self.ps = ps;
        }
        let Some(tx) = &self.tx else { return key };
        if let Some(w) = self.wait {
            if !key {
                return false;
            }
            log::info!(
                "apple: keyframe {} ms after the request",
                w.elapsed().as_millis()
            );
            self.wait = None;
        }
        let au = if key && bare && !self.ps.is_empty() {
            [self.ps.as_slice(), &au].concat()
        } else {
            au
        };
        // A full queue means the decoder stalls: drop the frame, the next keyframe heals it.
        let _ = tx.try_send(au);
        key
    }
}

/// `times` presses of one button, 80 ms apart (ZenLink 2.5.x).
async fn press(hid: &mut hid::Hid, name: &'static str, times: usize) -> Result<(), String> {
    for i in 0..times {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        hid.run(hid::Cmd::Button(name)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_ends_only_the_device_on_screen() {
        let e = engine();
        let sess = || {
            Arc::new(Sess {
                state: Mutex::new(State {
                    phase: "streaming",
                    ..State::default()
                }),
                stop: AtomicBool::new(false),
                input: Mutex::new(None),
                key: AtomicBool::new(false),
                screen: Mutex::new(None),
            })
        };
        let (a, b) = (sess(), sess());
        e.sessions.lock().unwrap().insert("A".into(), a.clone());
        e.sessions.lock().unwrap().insert("B".into(), b.clone());
        e.show(Some("A".into()));
        assert!(e.snapshot()["selected"] == "A");
        e.show(Some("B".into()));
        e.stop();
        assert!(b.stop.load(Relaxed) && !a.stop.load(Relaxed));
        assert!(e.sessions.lock().unwrap().contains_key("A"));
        e.stop_all();
        assert!(a.stop.load(Relaxed));
    }

    #[test]
    fn pic_shows_from_a_keyframe_with_its_parameter_sets() {
        let mut p = Pic::default();
        let mut key = nal(32, &[1, 2, 3]);
        key.extend(nal(33, &[4; 16]));
        key.extend(nal(34, &[5, 6]));
        key.extend(nal(19, &[2]));
        assert!(p.push(key)); // off screen: only the parameter sets are kept
        let (tx, rx) = sync_channel(8);
        p.tx = Some(tx);
        p.wait = Some(Instant::now());
        assert!(!p.push(nal(1, &[0]))); // waiting: a delta frame is dropped
        assert!(p.push(nal(19, &[4]))); // a bare keyframe goes with the kept VPS/SPS/PPS
        assert!(codec_info(&rx.try_recv().unwrap(), None).is_some());
        p.push(nal(1, &[3]));
        assert_eq!(rx.try_recv().unwrap(), nal(1, &[3]));
        assert!(p.wait.is_none());
    }

    #[test]
    fn ipad_at_270_swaps_the_volume_buttons() {
        assert_eq!(volume_button(true, false, 270), "volume-down");
        assert_eq!(volume_button(true, true, 0), "volume-down");
        assert_eq!(volume_button(true, true, 270), "volume-up");
        assert_eq!(volume_button(false, true, 270), "volume-down");
    }

    /// NAL header bytes for type `t` (layer 0, temporal id 1).
    fn nal(t: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, t << 1, 1];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn frame_drops_parameter_sets_and_marks_keyframes() {
        let mut au = nal(32, &[1, 2, 3]);
        au.extend(nal(33, &[4; 16]));
        au.extend(nal(34, &[5, 6]));
        au.extend(nal(19, &[7, 8, 9])); // IDR_W_RADL
        let Some(Record::Frame { key, data, .. }) = frame(&au) else {
            panic!("no frame")
        };
        assert!(key);
        assert_eq!(data, [&[0, 0, 0, 5][..], &[19 << 1, 1, 7, 8, 9]].concat());
        assert!(codec_info(&au, None).is_some());

        let Some(Record::Frame { key, .. }) = frame(&nal(1, &[1, 2])) else {
            panic!("no frame")
        };
        assert!(!key);
        assert!(codec_info(&nal(1, &[1, 2]), None).is_none());
        assert!(frame(&nal(32, &[1, 2])).is_none());
    }

    #[test]
    fn relock_skips_its_own_echo_and_stops_a_loop() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_millis(n);
        let mut recent = Vec::new();
        assert!(relock_allowed(s(0), None, &mut recent, true));
        // The user locks again 1.2 s after the unlock finished (the bug report): unlock.
        assert!(relock_allowed(s(4_000), Some(s(2_800)), &mut recent, true));
        // Reported during the unlock, or right after it: skip.
        assert!(!relock_allowed(s(6_000), Some(s(6_500)), &mut recent, true));
        assert!(!relock_allowed(s(6_508), Some(s(6_500)), &mut recent, true));
        assert!(relock_allowed(s(9_000), Some(s(6_500)), &mut recent, true));
        // A 4th within 30 s: stop.
        assert!(!relock_allowed(
            s(12_000),
            Some(s(10_000)),
            &mut recent,
            true
        ));
        assert!(relock_allowed(
            s(40_000),
            Some(s(10_000)),
            &mut recent,
            true
        ));
        // Limit off: no cap.
        let mut recent = Vec::new();
        for n in 0..6 {
            assert!(relock_allowed(s(n * 2_000), None, &mut recent, false));
        }
    }

    #[test]
    fn ios_17_to_26_go_to_hybrid() {
        assert!(hybrid_version("17.0") && hybrid_version("26.1"));
        assert!(!hybrid_version("27.0") && !hybrid_version("28") && !hybrid_version(""));
    }
}
