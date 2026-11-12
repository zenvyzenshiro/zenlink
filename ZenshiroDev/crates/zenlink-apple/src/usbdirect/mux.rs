//! usbmux over raw USB (port of backend usbdirect/usbmux.py): talk to lockdownd (port 62078)
//! through the device's mux interface (class 0xFF, subclass 0xFE), the way usbmuxd does on macOS
//! and TikTok LIVE Studio's qcast does on Windows. Big-endian mux header (protocol, length, and for
//! version 2 a 0xFEEDFACE magic plus tx/rx counters), then a 20-byte TCP-like header.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::libusb0::{Open, TIMEOUT};

const PROTO_VERSION: u32 = 0;
const PROTO_SETUP: u32 = 2;
const PROTO_TCP: u32 = 6;
const TH_SYN: u8 = 0x02;
const TH_RST: u8 = 0x04;
const TH_ACK: u8 = 0x10;
const MAGIC: u32 = 0xFEED_FACE;
const DEVICE_MAGIC: u32 = 0xFACE_FACE;
const RX_WINDOW: u32 = 0x20000;
pub const LOCKDOWN_PORT: u16 = 62078;

struct Tx {
    version: u32,
    seq: u16,
    rx_seq: u16,
}

struct Shared {
    dev: Arc<Open>,
    ep_out: u8,
    max_packet: usize,
    tx: Mutex<Tx>,
    streams: Mutex<HashMap<u16, Arc<StreamState>>>,
    stop: AtomicBool,
}

impl Shared {
    fn send(&self, proto: u32, payload: &[u8]) -> Result<(), String> {
        let mut t = self.tx.lock().unwrap();
        let mut packet = Vec::with_capacity(payload.len() + 16);
        if t.version >= 2 {
            if proto == PROTO_SETUP {
                t.seq = 0;
                t.rx_seq = 0xFFFF;
            }
            packet.extend_from_slice(&proto.to_be_bytes());
            packet.extend_from_slice(&(16 + payload.len() as u32).to_be_bytes());
            packet.extend_from_slice(&MAGIC.to_be_bytes());
            packet.extend_from_slice(&t.seq.to_be_bytes());
            packet.extend_from_slice(&t.rx_seq.to_be_bytes());
            t.seq = t.seq.wrapping_add(1);
        } else {
            packet.extend_from_slice(&proto.to_be_bytes());
            packet.extend_from_slice(&(8 + payload.len() as u32).to_be_bytes());
        }
        packet.extend_from_slice(payload);
        self.dev
            .write(self.ep_out, &packet, 1000)
            .map_err(|e| format!("usbmux write: {e}"))?;
        if packet.len() % self.max_packet.max(1) == 0 {
            let _ = self.dev.write(self.ep_out, &[], 1000); // a zero-length packet ends the transfer
        }
        Ok(())
    }
}

struct StreamState {
    sport: u16,
    dport: u16,
    seqs: Mutex<(u32, u32)>,        // (tx_seq, tx_ack)
    connected: Mutex<Option<bool>>, // None: waiting; Some(false): refused / reset
    rx: Mutex<Option<Sender<Option<Vec<u8>>>>>,
}

impl StreamState {
    fn segment(&self, mux: &Shared, flags: u8, data: &[u8]) -> Result<(), String> {
        let (seq, ack) = *self.seqs.lock().unwrap();
        let mut tcp = Vec::with_capacity(20 + data.len());
        tcp.extend_from_slice(&self.sport.to_be_bytes());
        tcp.extend_from_slice(&self.dport.to_be_bytes());
        tcp.extend_from_slice(&seq.to_be_bytes());
        tcp.extend_from_slice(&ack.to_be_bytes());
        tcp.push(5 << 4);
        tcp.push(flags);
        tcp.extend_from_slice(&((RX_WINDOW >> 8) as u16).to_be_bytes());
        tcp.extend_from_slice(&[0; 4]);
        tcp.extend_from_slice(data);
        mux.send(PROTO_TCP, &tcp)
    }

    fn deliver(&self, item: Option<Vec<u8>>) {
        if let Some(tx) = self.rx.lock().unwrap().as_ref() {
            let _ = tx.send(item);
        }
    }

    fn on_segment(&self, mux: &Shared, flags: u8, seq: u32, data: &[u8]) {
        if flags & TH_RST != 0 {
            *self.connected.lock().unwrap() = Some(false);
            self.deliver(None);
            return;
        }
        let mut c = self.connected.lock().unwrap();
        if c.is_none() {
            if flags & TH_SYN != 0 && flags & TH_ACK != 0 {
                {
                    let mut s = self.seqs.lock().unwrap();
                    s.0 = s.0.wrapping_add(1);
                    s.1 = seq.wrapping_add(1);
                }
                let _ = self.segment(mux, TH_ACK, &[]);
                *c = Some(true);
            }
            return;
        }
        drop(c);
        if !data.is_empty() {
            self.seqs.lock().unwrap().1 = seq.wrapping_add(data.len() as u32);
            let _ = self.segment(mux, TH_ACK, &[]);
            self.deliver(Some(data.to_vec()));
        }
    }
}

/// One TCP-like connection to a device port. Blocking.
pub struct MuxStream {
    mux: Arc<Shared>,
    state: Arc<StreamState>,
    rx: Receiver<Option<Vec<u8>>>,
    closed: bool,
}

/// The sending half of a [`MuxStream`], usable from another thread while the stream blocks in
/// `recv` (sharing the stream behind one mutex starved the sender and lockdown timed out).
#[derive(Clone)]
pub struct MuxWriter {
    mux: Arc<Shared>,
    state: Arc<StreamState>,
}

impl MuxWriter {
    pub fn send(&self, data: &[u8]) -> Result<(), String> {
        if *self.state.connected.lock().unwrap() == Some(false) {
            return Err("connection closed by the device".into());
        }
        for chunk in data.chunks(16 * 1024) {
            self.state.segment(&self.mux, TH_ACK, chunk)?;
            let mut s = self.state.seqs.lock().unwrap();
            s.0 = s.0.wrapping_add(chunk.len() as u32);
        }
        Ok(())
    }
}

impl MuxStream {
    pub fn writer(&self) -> MuxWriter {
        MuxWriter {
            mux: self.mux.clone(),
            state: self.state.clone(),
        }
    }

    pub fn send(&self, data: &[u8]) -> Result<(), String> {
        if self.closed {
            return Err("connection closed by the device".into());
        }
        self.writer().send(data)
    }

    /// Next data from the device; `Ok(None)` when it closed the connection.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        if self.closed {
            return Ok(None);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(Some(d)) => Ok(Some(d)),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                Ok(None)
            }
            Err(RecvTimeoutError::Timeout) => Err("timed out waiting for the device".into()),
        }
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        let _ = self.state.segment(&self.mux, TH_RST, &[]);
        self.mux.streams.lock().unwrap().remove(&self.state.sport);
    }
}

/// usbmux on an already-claimed mux interface; the reader thread ends on drop.
pub struct UsbMux {
    shared: Arc<Shared>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl UsbMux {
    pub fn start(dev: Arc<Open>, ep_in: u8, ep_out: u8, max_packet: u16) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            dev,
            ep_out,
            max_packet: max_packet as usize,
            tx: Mutex::new(Tx {
                version: 0,
                seq: 0,
                rx_seq: 0xFFFF,
            }),
            streams: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });
        let (vtx, vrx) = channel();
        let s = shared.clone();
        let reader = std::thread::Builder::new()
            .name("usbmux".into())
            .spawn(move || read_loop(&s, ep_in, vtx))
            .map_err(|e| e.to_string())?;
        let mux = Self {
            shared,
            reader: Some(reader),
        };
        let mut hello = Vec::new();
        for n in [2u32, 0, 0] {
            hello.extend_from_slice(&n.to_be_bytes());
        }
        mux.shared.send(PROTO_VERSION, &hello)?;
        vrx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| "the device did not answer the usbmux handshake".to_string())?;
        mux.shared.send(PROTO_SETUP, &[7])?;
        Ok(mux)
    }

    pub fn connect(&self, port: u16) -> Result<MuxStream, String> {
        let sport = 0x4000 + (rand_u16() % 0xBFF0);
        let (tx, rx) = channel();
        let state = Arc::new(StreamState {
            sport,
            dport: port,
            seqs: Mutex::new((0, 0)),
            connected: Mutex::new(None),
            rx: Mutex::new(Some(tx)),
        });
        self.shared
            .streams
            .lock()
            .unwrap()
            .insert(sport, state.clone());
        state.segment(&self.shared, TH_SYN, &[])?;
        for _ in 0..50 {
            let c = *state.connected.lock().unwrap();
            match c {
                Some(true) => {
                    return Ok(MuxStream {
                        mux: self.shared.clone(),
                        state,
                        rx,
                        closed: false,
                    })
                }
                Some(false) => break,
                None => {}
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.shared.streams.lock().unwrap().remove(&sport);
        Err(format!("the device refused a connection to port {port}"))
    }
}

impl Drop for UsbMux {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

fn rand_u16() -> u16 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.subsec_nanos() ^ (t.as_secs() as u32).rotate_left(7)) as u16
}

fn read_loop(mux: &Shared, ep_in: u8, version: Sender<()>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    while !mux.stop.load(Relaxed) {
        match mux.dev.read(ep_in, &mut chunk, 200) {
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(TIMEOUT) => continue,
            Err(e) => {
                log::debug!("usbmux read stopped: {e}");
                break;
            }
        }
        while buf.len() >= 8 {
            let proto = u32::from_be_bytes(buf[0..4].try_into().unwrap());
            let len = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
            if !(8..=1 << 20).contains(&len) {
                // Not a usbmux packet: the stream cannot be followed, end it (closes the streams).
                log::warn!("usbmux: bad packet length {len}");
                buf.clear();
                mux.stop.store(true, Relaxed);
                break;
            }
            if buf.len() < len {
                break;
            }
            let packet: Vec<u8> = buf.drain(..len).collect();
            dispatch(mux, proto, &packet, &version);
        }
    }
    for s in mux.streams.lock().unwrap().values() {
        *s.connected.lock().unwrap() = Some(false);
        s.deliver(None);
    }
}

fn dispatch(mux: &Shared, proto: u32, p: &[u8], version: &Sender<()>) {
    let be32 = |at: usize| {
        p.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    if proto == PROTO_VERSION {
        let major = be32(8).unwrap_or(1);
        mux.tx.lock().unwrap().version = if major >= 2 { 2 } else { 1 };
        let _ = version.send(());
        return;
    }
    let body = if mux.tx.lock().unwrap().version >= 2 {
        // The host sends 0xFEEDFACE; the device answers with 0xFACEFACE.
        if p.len() < 16 || !matches!(be32(8), Some(MAGIC | DEVICE_MAGIC)) {
            return;
        }
        mux.tx.lock().unwrap().rx_seq = u16::from_be_bytes([p[12], p[13]]); // acknowledge its counter
        &p[16..]
    } else {
        &p[8..]
    };
    if proto != PROTO_TCP || body.len() < 20 {
        return;
    }
    let dport = u16::from_be_bytes([body[2], body[3]]);
    let seq = u32::from_be_bytes(body[4..8].try_into().unwrap());
    let off = ((body[12] >> 4) as usize) * 4;
    let flags = body[13];
    let state = mux.streams.lock().unwrap().get(&dport).cloned();
    if let (Some(s), Some(data)) = (state, body.get(off..)) {
        s.on_segment(mux, flags, seq, data);
    }
}
