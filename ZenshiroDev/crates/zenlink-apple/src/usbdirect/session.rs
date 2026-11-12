//! One USB Direct capture session (port of backend usbdirect/session.py): Apple's QuickTime
//! screen-capture protocol over libusb0.
//!
//! 1. Switch the device to its capture configuration (vendor request 0x40/0x52, wIndex 2) and
//!    wait for it to re-enumerate.
//! 2. Prove trust the way TikTok LIVE Studio's qcast does: usbmux on the mux interface, a TLS
//!    lockdown session with the Windows pair record, stopped again. Without it the device stays
//!    silent.
//! 3. Claim the capture interface (class 0xFF, subclass 0x2A), clear both halts, answer the
//!    handshake (PING, CWPA/AFMT/CVRP/CLOK/TIME/SKEW/GO!) and receive FEED (video) and EAT! (PCM).
//! 4. On stop: HPA0/HPD0, the disable request (wIndex 0) and the normal configuration again.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::libusb0::{self, Config, Device, Open, TIMEOUT};
use super::mux::{UsbMux, LOCKDOWN_PORT};
use super::wire::{self, CmTime, Format, Sample};

const VENDOR: u8 = 0xFF;
const QT_SUBCLASS: u8 = 0x2A;
const MUX_SUBCLASS: u8 = 0xFE;
/// One libusb0 request per read, 100 ms timeout (qcast recv_thread_func).
const READ_SIZE: usize = 0x10000;
const READ_TIMEOUT_MS: u32 = 100;

/// What the session reports; called from its own threads.
pub trait Sink: Send + Sync {
    fn state(&self, phase: &'static str, detail: &str);
    /// A new video format (avcC / hvcC as the device sent it).
    fn format(&self, f: &Format);
    /// One access unit, length-prefixed NAL units.
    fn video(&self, au: Vec<u8>);
    /// 48 kHz stereo s16le.
    fn audio(&self, pcm: &[u8]);
    /// The device's ProductType, learned during the trust check.
    fn product(&self, _product_type: &str) {}
}

/// Rate of the device's audio clock against the host clock, for SKEW (session.py SkewMeter):
/// each 2 s window keeps its least-delayed arrival, the rate is the slope between the first
/// and newest window (6 s minimum), a jump in either clock starts over, and the result stays
/// within 0.2 % of nominal.
#[derive(Default)]
pub struct SkewMeter {
    scale: u32,
    last: Option<(f64, f64)>,
    first: Option<(f64, f64)>,
    win: Option<(f64, f64, f64)>,
    done: Option<(f64, f64)>,
}

impl SkewMeter {
    const WINDOW: f64 = 2.0;
    const MIN_SPAN: f64 = 6.0;
    const LIMIT: f64 = 0.002;
    const JUMP: f64 = 0.5;

    pub fn new() -> Self {
        Self {
            scale: 48000,
            ..Self::default()
        }
    }

    pub fn add(&mut self, pts: CmTime, local: CmTime) {
        if pts.scale != 0 {
            self.scale = pts.scale;
        }
        let (dev, loc) = (pts.seconds(), local.seconds());
        if let Some((ld, ll)) = self.last {
            let (dd, dl) = (dev - ld, loc - ll);
            if dd < 0.0 || dl < 0.0 || (dd - dl).abs() > Self::JUMP {
                *self = Self {
                    scale: self.scale,
                    ..Self::default()
                };
            }
        }
        self.last = Some((dev, loc));
        let off = loc - dev;
        match self.win {
            None => self.win = Some((dev, dev, off)),
            Some(w) if dev - w.0 >= Self::WINDOW => {
                if self.first.is_none() {
                    self.first = Some((w.1, w.2));
                } else {
                    self.done = Some((w.1, w.2));
                }
                self.win = Some((dev, dev, off));
            }
            Some(w) if off < w.2 => self.win = Some((w.0, dev, off)),
            _ => {}
        }
    }

    pub fn rate(&self) -> f64 {
        let nominal = if self.scale == 0 {
            48000.0
        } else {
            self.scale as f64
        };
        let (Some(f), Some(d)) = (self.first, self.done) else {
            return nominal;
        };
        let span = d.0 - f.0;
        if span < Self::MIN_SPAN {
            return nominal;
        }
        let r = nominal * (span + d.1 - f.1) / span;
        r.clamp(nominal * (1.0 - Self::LIMIT), nominal * (1.0 + Self::LIMIT))
    }
}

/// A CMClock running on host time, in nanoseconds.
struct HostClock {
    id: u64,
    start: Instant,
}

impl HostClock {
    fn new(id: u64) -> Self {
        Self {
            id,
            start: Instant::now(),
        }
    }

    fn now(&self) -> CmTime {
        CmTime::ns(self.start.elapsed().as_nanos() as u64)
    }
}

fn configs(dev: &Device) -> (Option<u8>, Option<u8>) {
    let has = |c: &Config, sub| {
        c.interfaces
            .iter()
            .any(|i| i.class == VENDOR && i.subclass == sub)
    };
    let qt = dev
        .configs
        .iter()
        .find(|c| has(c, QT_SUBCLASS))
        .map(|c| c.value);
    let mux = dev
        .configs
        .iter()
        .find(|c| !has(c, QT_SUBCLASS) && has(c, MUX_SUBCLASS))
        .map(|c| c.value);
    (mux, qt)
}

/// Where the session reads and writes once the capture interface is claimed.
struct Capture {
    dev: Arc<Open>,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
}

pub struct Session {
    serial: String,
    sink: Arc<dyn Sink>,
    display: (u32, u32),
    stop: Arc<AtomicBool>,
    audio_to_pc: Arc<AtomicBool>,
    mux_cfg: Option<u8>,
    cap: Option<Capture>,
    write_lock: Mutex<()>,
    // protocol state
    need: Vec<u8>,
    clock: Option<HostClock>,
    audio_clock: Option<HostClock>,
    device_audio_clock: u64,
    skew: SkewMeter,
    pinged: bool,
    streaming: bool,
    audio_on: bool,
    released: bool,
}

/// Handles another thread uses while the session runs.
#[derive(Clone)]
pub struct Control {
    pub stop: Arc<AtomicBool>,
    pub audio_to_pc: Arc<AtomicBool>,
}

impl Session {
    pub fn new(
        serial: &str,
        sink: Arc<dyn Sink>,
        display: (u32, u32),
        audio_to_pc: bool,
    ) -> (Self, Control) {
        let control = Control {
            stop: Arc::new(AtomicBool::new(false)),
            audio_to_pc: Arc::new(AtomicBool::new(audio_to_pc)),
        };
        let s = Self {
            serial: serial.replace('-', "").to_uppercase(),
            sink,
            display,
            stop: control.stop.clone(),
            audio_to_pc: control.audio_to_pc.clone(),
            mux_cfg: None,
            cap: None,
            write_lock: Mutex::new(()),
            need: Vec::new(),
            clock: None,
            audio_clock: None,
            device_audio_clock: 0,
            skew: SkewMeter::new(),
            pinged: false,
            streaming: false,
            audio_on: false,
            released: false,
        };
        (s, control)
    }

    /// Blocks until stopped or the stream fails.
    pub fn run(mut self) -> Result<(), String> {
        let r = self.activate().and_then(|_| self.receive());
        self.close_stream();
        self.deactivate();
        if self.stop.load(Relaxed) {
            Ok(())
        } else {
            r
        }
    }

    fn find(&self) -> Result<Option<Device>, String> {
        libusb0::find(&self.serial)
    }

    fn capture_request(&self, dev: &Device, enable: bool) {
        // The device drops off the bus right after, so an error here is normal.
        if let Ok(h) = dev.open() {
            let _ = h.control(0x40, 0x52, 0, if enable { 2 } else { 0 }, &mut [], 1000);
        }
    }

    fn wait_reconnect(&self, want_capture: bool, patience: Duration) -> Result<Device, String> {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(4) && !self.stop.load(Relaxed) {
            if self.find()?.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let t = Instant::now();
        while t.elapsed() < patience && !self.stop.load(Relaxed) {
            if let Some(d) = self.find()? {
                if configs(&d).1.is_some() == want_capture {
                    std::thread::sleep(Duration::from_millis(800)); // let Windows finish binding the driver
                    return Ok(self.find()?.unwrap_or(d));
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if self.stop.load(Relaxed) {
            return Err("Stopped.".into());
        }
        Err(if want_capture {
            "The device did not come back in screen-capture mode. Restart the iPhone/iPad (turn it off and on), \
             plug it in again, unlock it and retry."
        } else {
            "The device did not leave screen-capture mode. Unplug it, plug it in again and retry."
        }
        .into())
    }

    /// qcast's order: normal mode -> capture request -> reconnect with the capture configuration
    /// -> trust over usbmux -> capture interface. A device left in capture mode by an earlier
    /// attempt stays silent, so it is switched back to normal first.
    fn activate(&mut self) -> Result<(), String> {
        let not_attached = "The ZenLink Studio driver is installed, but Windows hasn't attached it to the device yet. \
                            Unplug the cable and plug it in again (restart Windows if that doesn't help).";
        let mut dev = self.find()?.ok_or(not_attached)?;
        let (mux, mut qt) = configs(&dev);
        self.mux_cfg = mux;
        self.sink
            .state("activating", "Switching the device to screen capture");
        let mut adopt = false;
        if qt.is_some() {
            log::info!(
                "usb direct: device is still in capture mode; switching it back to normal first"
            );
            self.capture_request(&dev, false);
            match self.wait_reconnect(false, Duration::from_secs(6)) {
                Ok(d) => dev = d,
                Err(_) if !self.stop.load(Relaxed) => {
                    log::info!("usb direct: device stayed in capture mode; adopting it");
                    adopt = true;
                    dev = self
                        .find()?
                        .ok_or("The device disappeared. Plug it in again and retry.")?;
                }
                Err(e) => return Err(e),
            }
            let (m, q) = configs(&dev);
            self.mux_cfg = self.mux_cfg.or(m);
            qt = q;
        }
        if !adopt {
            log::info!(
                "usb direct: requesting capture mode ({} configurations)",
                dev.configs.len()
            );
            self.capture_request(&dev, true);
            dev = self.wait_reconnect(true, Duration::from_secs(15))?;
            let (m, q) = configs(&dev);
            self.mux_cfg = self.mux_cfg.or(m);
            qt = q;
        }
        let qt = qt.ok_or(
            "The device did not offer its screen-capture configuration. Unlock it and try again. \
             If it keeps failing, restart the iPhone/iPad.",
        )?;
        log::info!(
            "usb direct: capture mode on ({} configurations, capture config {qt})",
            dev.configs.len()
        );

        // qcast QMirror_Init/Trust: capture configuration, lockdown session over usbmux.
        {
            let h = Arc::new(dev.open()?);
            // libusb0 refuses claim_interface (-22) on a handle that never selected a
            // configuration, even when the device already runs it (a device left in capture mode).
            if let Err(e) = h.set_configuration(qt) {
                if h.active_configuration() != Some(qt) {
                    return Err(format!(
                        "Trust check over USB failed ({e}). Unplug the device and retry."
                    ));
                }
            }
            self.prove_trust(&h, &dev, qt)?;
        }

        // qcast quicktime_libusb0_impl::start: a fresh handle, SET_CONFIGURATION(capture), claim the
        // capture interface, clear both halts, then read 16 KiB with a 100 ms timeout.
        let dev = self
            .find()?
            .ok_or("The device disappeared. Plug it in again and retry.")?;
        let h = Arc::new(dev.open()?);
        if let Err(e) = h.set_configuration(qt) {
            if h.active_configuration() != Some(qt) {
                return Err(format!(
                    "Windows refused to select the capture configuration ({e}). Unplug the device, plug it in again and retry."
                ));
            }
        }
        let cfg = dev
            .configs
            .iter()
            .find(|c| c.value == qt)
            .ok_or("capture configuration missing")?;
        let iface = cfg
            .interfaces
            .iter()
            .find(|i| i.class == VENDOR && i.subclass == QT_SUBCLASS)
            .ok_or("The capture interface is missing from the capture configuration.")?;
        h.claim(iface.number).map_err(|e| {
            format!(
                "Windows refused to open the capture interface ({e}). Unplug the device and retry."
            )
        })?;
        let (Some(ep_in), Some(ep_out)) = (iface.ep_in(), iface.ep_out()) else {
            return Err("The capture interface has no bulk endpoints.".into());
        };
        for ep in [ep_in.address, ep_out.address] {
            if let Err(e) = h.clear_halt(ep) {
                log::info!("usb direct: clear halt 0x{ep:02x}: {e}");
            }
        }
        log::info!(
            "usb direct: capture module started: config {qt}, interface {}, in=0x{:02x} out=0x{:02x}",
            iface.number,
            ep_in.address,
            ep_out.address
        );
        self.cap = Some(Capture {
            dev: h,
            iface: iface.number,
            ep_in: ep_in.address,
            ep_out: ep_out.address,
        });
        Ok(())
    }

    fn prove_trust(&self, h: &Arc<Open>, dev: &Device, qt: u8) -> Result<(), String> {
        let Some(iface) = dev
            .configs
            .iter()
            .find(|c| c.value == qt)
            .and_then(|c| {
                c.interfaces
                    .iter()
                    .find(|i| i.class == VENDOR && i.subclass == MUX_SUBCLASS)
            })
            .cloned()
        else {
            log::warn!("usb direct: no usbmux interface in the capture configuration");
            return Ok(());
        };
        self.sink
            .state("activating", "Checking that this PC is trusted");
        let (Some(ep_in), Some(ep_out)) = (iface.ep_in().cloned(), iface.ep_out().cloned()) else {
            return Ok(());
        };
        h.claim(iface.number).map_err(|e| {
            format!("Trust check over USB failed ({e}). Unplug the device and retry.")
        })?;
        let result = (|| {
            let mux = UsbMux::start(h.clone(), ep_in.address, ep_out.address, ep_out.max_packet)?;
            let stream = mux.connect(LOCKDOWN_PORT)?;
            let r = super::trust::check(stream, &self.serial);
            drop(mux);
            std::thread::sleep(Duration::from_millis(250)); // let the mux reader finish its last read
            r
        })();
        h.release(iface.number);
        match result {
            Ok(product) => {
                log::info!("usb direct: trust confirmed (lockdown session started and stopped)");
                if let Some(p) = product {
                    self.sink.product(&p);
                }
                Ok(())
            }
            Err(e) => {
                log::warn!("usb direct: lockdown session failed: {e}");
                if e.contains("PasswordProtected") || e.contains("DeviceLocked") {
                    Err("Unlock the device and try again.".into())
                } else if e == super::trust::NOT_TRUSTED {
                    Err(e)
                } else if e.contains("InvalidHostID") || e.contains("Pair") {
                    Err("The device doesn't trust this PC anymore. Use Apple mode once and tap Trust, then try again.".into())
                } else {
                    Ok(()) // session.py goes on; the device may still stream
                }
            }
        }
    }

    fn deactivate(&mut self) {
        let Some(c) = self.cap.take() else {
            // activate() failed after asking for capture mode (trust check, claim): put a
            // device left in capture mode back, or it stays silent until it is replugged.
            if let Ok(Some(d)) = self.find() {
                if configs(&d).1.is_some() {
                    self.capture_request(&d, false);
                    log::info!("usb direct: capture mode undone after a failed start");
                }
            }
            return;
        };
        c.dev.release(c.iface);
        let _ = c.dev.control(0x40, 0x52, 0, 0, &mut [], 1000);
        if let Some(cfg) = self.mux_cfg {
            let _ = c.dev.set_configuration(cfg);
        }
        log::info!("usb direct: device restored to its normal configuration");
    }

    /// Let go of the interface but leave the mode switch to activate().
    fn release_capture(&mut self) {
        if let Some(c) = self.cap.take() {
            c.dev.release(c.iface);
        }
    }

    fn write(&self, data: &[u8]) -> Result<(), String> {
        self.write_timeout(data, 2000)
            .map_err(|e| format!("USB connection lost: {e}"))
    }

    fn write_timeout(&self, data: &[u8], timeout: u32) -> Result<(), i32> {
        let c = self.cap.as_ref().ok_or(-1)?;
        let _g = self.write_lock.lock().unwrap();
        c.dev.write(c.ep_out, data, timeout).map(drop)
    }

    /// One thread only reads USB so a read is always pending (qcast's qt_recv); this one parses
    /// and answers. Parsing on the reading thread made frames arrive in bursts.
    fn start_reader(&self) -> (Receiver<Result<Vec<u8>, i32>>, Arc<AtomicBool>) {
        let c = self.cap.as_ref().unwrap();
        let (dev, ep) = (c.dev.clone(), c.ep_in);
        let (tx, rx) = sync_channel(4096);
        let on = Arc::new(AtomicBool::new(true));
        let (on2, stop) = (on.clone(), self.stop.clone());
        std::thread::Builder::new()
            .name("usbdirect-read".into())
            .spawn(move || {
                let mut buf = vec![0u8; READ_SIZE];
                while on2.load(Relaxed) && !stop.load(Relaxed) {
                    match dev.read(ep, &mut buf, READ_TIMEOUT_MS) {
                        Ok(n) => {
                            if tx.send(Ok(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(TIMEOUT) => {
                            let _ = tx.try_send(Err(TIMEOUT));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    }
                }
            })
            .expect("reader thread");
        (rx, on)
    }

    fn receive(&mut self) -> Result<(), String> {
        self.sink
            .state("waiting", "Unlock the device and keep it unlocked");
        let mut buf: Vec<u8> = Vec::new();
        let mut received = 0usize;
        let mut silent = Instant::now();
        let mut retried = false;
        // Handshake bytes alone are not a picture: an untrusted device answers them and then
        // stays silent (B-57), which used to sit at 0.0 fps forever.
        let (opened, mut seen_frame) = (Instant::now(), false);
        let (mut rx, mut reading) = self.start_reader();
        let (mut stats_at, mut stats_bytes, mut frames) = (Instant::now(), 0usize, 0u32);
        let result = loop {
            if self.stop.load(Relaxed) {
                break Ok(());
            }
            let item = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(i) => i,
                Err(RecvTimeoutError::Timeout) => Err(TIMEOUT),
                Err(RecvTimeoutError::Disconnected) => Err(-1),
            };
            if received > 0 && stats_at.elapsed() >= Duration::from_secs(5) {
                let secs = stats_at.elapsed().as_secs_f64();
                log::info!(
                    "usb direct: {:.1} fps, {:.1} Mbps, audio skew {:.1} Hz",
                    frames as f64 / secs,
                    (received - stats_bytes) as f64 * 8.0 / secs / 1e6,
                    self.skew.rate()
                );
                (stats_at, stats_bytes, frames) = (Instant::now(), received, 0);
            }
            if !seen_frame && opened.elapsed() > Duration::from_secs(30) {
                break Err(
                    "The device answered but sent no picture. Unlock it and keep it unlocked. If it \
                     still fails, this PC may not be trusted: use Apple mode once, tap Trust on the \
                     device, then switch back to ZenLink Studio."
                        .into(),
                );
            }
            let chunk = match item {
                Ok(c) => c,
                Err(TIMEOUT) => {
                    if received == 0 && !retried && silent.elapsed() > Duration::from_secs(4) {
                        // A device left in capture mode by an interrupted session stays silent:
                        // switch capture mode off and on once.
                        retried = true;
                        log::info!("usb direct: no data; switching capture mode off and on once");
                        reading.store(false, Relaxed);
                        self.release_capture();
                        if let Err(e) = self.activate() {
                            break Err(e);
                        }
                        self.sink
                            .state("waiting", "Unlock the device and keep it unlocked");
                        self.pinged = false;
                        silent = Instant::now();
                        (rx, reading) = self.start_reader();
                        continue;
                    }
                    if received == 0 && retried && silent.elapsed() > Duration::from_secs(10) {
                        break Err("The device opened screen capture but sent nothing. Restart the iPhone/iPad \
                                   (turn it off and on), unlock it, keep it on the home screen, then press Start \
                                   casting again."
                            .into());
                    }
                    if !self.pinged {
                        // qcast: one active PING on the first read timeout, never repeated.
                        let ok = self.write_timeout(&wire::ping(true), 100).is_ok();
                        self.pinged = true;
                        log::info!(
                            "usb direct: active PING {}",
                            if ok { "sent" } else { "not taken" }
                        );
                    }
                    continue;
                }
                Err(e) => break Err(format!("USB connection lost: {e}")),
            };
            if received == 0 {
                log::info!("usb direct: first {} bytes from the device", chunk.len());
            }
            received += chunk.len();
            buf.extend_from_slice(&chunk);
            let mut start = 0;
            let mut failed = None;
            while buf.len() - start >= 8 {
                let len = wire::u32_at(&buf, start).unwrap() as usize;
                if !(8..=64 << 20).contains(&len) {
                    failed = Some(format!("Corrupt packet length {len} from the device."));
                    break;
                }
                if buf.len() - start < len {
                    break;
                }
                let packet = buf[start + 4..start + len].to_vec();
                start += len;
                match self.handle(&packet) {
                    Ok(true) => {
                        frames += 1;
                        seen_frame = true;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            buf.drain(..start);
            if let Some(e) = failed {
                break Err(e);
            }
        };
        reading.store(false, Relaxed);
        result
    }

    /// Moves the device's sound to this PC (HPA1) or back to the device (HPA0).
    fn apply_audio_route(&mut self) {
        let on = self.audio_to_pc.load(Relaxed);
        if self.audio_clock.is_none() || on == self.audio_on {
            return;
        }
        let packet = if on {
            wire::hpa1(self.device_audio_clock)
        } else {
            wire::asyn(wire::HPA0, self.device_audio_clock, &[])
        };
        match self.write(&packet) {
            Ok(()) => {
                self.audio_on = on;
                log::info!(
                    "usb direct: sound {}",
                    if on {
                        "to this PC"
                    } else {
                        "back on the device"
                    }
                );
            }
            Err(e) => log::info!("usb direct: could not move the sound: {e}"),
        }
    }

    fn close_stream(&mut self) {
        if self.cap.is_none() {
            return;
        }
        let _ = self.write_timeout(&wire::asyn(wire::HPA0, self.device_audio_clock, &[]), 1000);
        let _ = self.write_timeout(&wire::asyn(wire::HPD0, wire::EMPTY_CF, &[]), 1000);
        if !self.released {
            // Wait up to 1 s for RELS (session.py waits on _released).
            let c = self.cap.as_ref().unwrap();
            let mut b = vec![0u8; READ_SIZE];
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(1) {
                match c.dev.read(c.ep_in, &mut b, 100) {
                    Ok(n) if b[..n].windows(4).any(|w| w == wire::RELS.to_le_bytes()) => break,
                    Ok(_) | Err(TIMEOUT) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = self.write_timeout(&wire::asyn(wire::HPD0, wire::EMPTY_CF, &[]), 1000);
    }

    /// One packet; `Ok(true)` for a video frame.
    fn handle(&mut self, p: &[u8]) -> Result<bool, String> {
        self.apply_audio_route();
        match wire::u32_at(p, 0) {
            Some(wire::PING) => {
                if !self.pinged {
                    self.write(&wire::ping(false))?;
                    self.pinged = true;
                }
                self.sink
                    .state("handshake", "Device answered, setting up the stream");
                Ok(false)
            }
            Some(wire::SYNC) => self.handle_sync(p).map(|_| false),
            Some(wire::ASYN) => self.handle_asyn(p),
            _ => Ok(false),
        }
    }

    fn handle_sync(&mut self, p: &[u8]) -> Result<(), String> {
        let (Some(sub), Some(corr)) = (wire::u32_at(p, 12), wire::u64_at(p, 16)) else {
            return Ok(());
        };
        let body_u64 = wire::u64_at(p, 24).unwrap_or(0);
        match sub {
            wire::CWPA => {
                self.device_audio_clock = body_u64;
                let clock = HostClock::new(body_u64 + 1000);
                self.skew = SkewMeter::new(); // new host clock: a new timeline
                let (w, h) = self.display;
                self.write(&wire::hpd1(w, h))?;
                self.write(&wire::hpd1(w, h))?;
                self.write(&wire::reply_clock(corr, clock.id))?;
                self.audio_clock = Some(clock);
                // Same handshake either way (known to work); "device only" hands the sound back
                // once the picture runs.
                self.write(&wire::hpa1(body_u64))?;
                self.audio_on = true;
            }
            wire::CVRP => {
                self.need = wire::asyn(wire::NEED, body_u64, &[]);
                self.write(&self.need.clone())?;
                self.write(&wire::reply_clock(corr, body_u64 + 0x1000AF))?;
            }
            wire::CLOK => {
                let clock_ref = wire::u64_at(p, 4).unwrap_or(0) + 0x10000;
                self.clock = Some(HostClock::new(clock_ref));
                self.write(&wire::reply_clock(corr, clock_ref))?;
            }
            wire::TIME => {
                let t = self.clock.as_ref().map(HostClock::now).unwrap_or_default();
                self.write(&wire::reply_time(corr, t))?;
            }
            wire::AFMT => self.write(&wire::reply_dict(corr, &[("Error", wire::Value::U32(0))]))?,
            wire::SKEW => self.write(&wire::reply_skew(corr, self.skew.rate()))?,
            wire::OG | wire::STOP => self.write(&wire::reply_empty(corr))?,
            _ => {}
        }
        Ok(())
    }

    fn handle_asyn(&mut self, p: &[u8]) -> Result<bool, String> {
        match wire::u32_at(p, 12) {
            Some(wire::FEED) => {
                // Ask for the next frame first: the device waits for NEED before sending another.
                if !self.need.is_empty() {
                    self.write(&self.need.clone())?;
                }
                let Some(s) = p.get(16..).and_then(Sample::parse) else {
                    return Ok(false);
                };
                if let Some(f) = s.format.as_ref().filter(|f| !f.config.is_empty()) {
                    self.sink.format(f);
                }
                if s.data.is_empty() {
                    return Ok(false);
                }
                self.sink.video(s.data);
                if !self.streaming {
                    self.streaming = true;
                    self.sink.state("streaming", "");
                }
                Ok(true)
            }
            Some(wire::EAT) => {
                let Some(s) = p.get(16..).and_then(Sample::parse) else {
                    return Ok(false);
                };
                if let Some(c) = &self.audio_clock {
                    self.skew.add(s.pts, c.now());
                }
                let fmt = s.format.as_ref().filter(|f| f.sound);
                if fmt.is_none_or(|f| f.channels == 2 && f.bits == 16) {
                    self.sink.audio(&s.data);
                }
                Ok(false)
            }
            Some(wire::RELS) => {
                self.released = true;
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// session.py SkewMeterTests, with a small deterministic jitter instead of random.
    fn feed(m: &mut SkewMeter, seconds: f64, rate: f64, jitter: f64, start: (f64, f64)) {
        let mut t = 0.0;
        let mut k = 0u32;
        while t < seconds {
            k = k.wrapping_mul(1103515245).wrapping_add(12345);
            let j = jitter * ((k >> 16) & 0x7FFF) as f64 / 32768.0;
            let dev = start.0 + t;
            let loc = start.1 + t * rate + 0.02 + j;
            m.add(
                CmTime {
                    value: (dev * 48000.0) as u64,
                    scale: 48000,
                    flags: 0,
                    epoch: 0,
                },
                CmTime::ns((loc * 1e9) as u64),
            );
            t += 0.02;
        }
    }

    #[test]
    fn skew_meter() {
        let mut m = SkewMeter::new();
        assert_eq!(m.rate(), 48000.0);
        feed(&mut m, 30.0, 1.0, 0.03, (0.0, 0.0));
        assert!((m.rate() - 48000.0).abs() < 24.0, "{}", m.rate());

        let mut m = SkewMeter::new();
        feed(&mut m, 60.0, 1.0005, 0.01, (0.0, 0.0));
        assert!((m.rate() - 48024.0).abs() < 6.0, "{}", m.rate());

        let mut m = SkewMeter::new();
        feed(&mut m, 20.0, 1.0, 0.0, (0.0, 0.0));
        feed(&mut m, 20.0, 1.0, 0.0, (3.0, 25.0)); // device audio timeline restarted
        assert!((m.rate() - 48000.0).abs() < 24.0, "{}", m.rate());
    }
}
