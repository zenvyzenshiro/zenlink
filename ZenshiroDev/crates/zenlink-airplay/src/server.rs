//! The receiver: RTSP on port 7100, the mDNS adverts, and per SETUP the mirror (TCP),
//! audio (UDP) and timing (UDP) ports. One device mirrors at a time; a second one replaces it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plist::{Dictionary, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::AbortHandle;

use crate::fairplay::FairPlay;
use crate::identity::Identity;
use crate::info::{Receiver, PORT};
use crate::pair::PairVerify;
use crate::{mirror, session_key, stream_key_iv, Sink, State};

pub struct Config {
    pub name: String,
    pub hevc: bool,
    /// Holds airplay.json.
    pub data_dir: PathBuf,
    /// 0 picks a free port (tests); the app uses [`PORT`] and advertises it.
    pub port: u16,
    pub advertise: bool,
}

/// Session keys from SETUP, for the audio stream.
#[derive(Clone, Copy)]
pub struct AudioKeys {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

#[derive(Default)]
struct Streams {
    /// The device mirroring now (its address).
    client: Option<String>,
    /// RTSP connections by peer, to end a replaced device's session.
    rtsp: HashMap<String, AbortHandle>,
    mirror: Option<AbortHandle>,
    video: Vec<AbortHandle>,
    audio: Vec<AbortHandle>,
    timing: Option<AbortHandle>,
}

impl Streams {
    fn close(&mut self) {
        // The listener and the live connections: a replaced device must not keep feeding frames.
        for h in self
            .mirror
            .take()
            .into_iter()
            .chain(self.video.drain(..))
            .chain(self.audio.drain(..))
        {
            h.abort();
        }
        if let Some(h) = self.timing.take() {
            h.abort();
        }
    }
}

struct Inner {
    id: Identity,
    cfg: Config,
    sink: Arc<dyn Sink>,
    streams: Mutex<Streams>,
}

/// A running receiver; dropping it (or [`Server::stop`]) ends everything.
pub struct Server {
    inner: Arc<Inner>,
    accept: AbortHandle,
    _advert: Option<crate::mdns::Advert>,
    pub port: u16,
}

impl Server {
    /// Binds the RTSP port and starts advertising. Call inside a tokio runtime.
    pub async fn start(cfg: Config, sink: Arc<dyn Sink>) -> Result<Self, String> {
        let listener = TcpListener::bind(("0.0.0.0", cfg.port))
            .await
            .map_err(|e| {
                format!(
                    "ZenLink couldn't open the AirPlay port ({e}). Close other AirPlay receivers \
                 (TikTok LIVE Studio, UxPlay, LonelyScreen) and try again."
                )
            })?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let inner = Arc::new(Inner {
            id: Identity::load(&cfg.data_dir),
            cfg,
            sink,
            streams: Mutex::new(Streams::default()),
        });
        let advert = if inner.cfg.advertise {
            Some(crate::mdns::Advert::start(&inner.receiver(), port)?)
        } else {
            None
        };
        let me = inner.clone();
        let accept = tokio::spawn(async move {
            loop {
                // A failed accept (out of handles, a reset) must not end the receiver.
                let (s, peer) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("airplay: accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let me2 = me.clone();
                let ip = peer.ip().to_string();
                let task = tokio::spawn(async move { me2.rtsp(s, peer).await });
                if let Some(old) = me
                    .streams
                    .lock()
                    .unwrap()
                    .rtsp
                    .insert(ip, task.abort_handle())
                {
                    old.abort(); // the same device reconnected
                }
            }
        })
        .abort_handle();
        log::info!("airplay: receiver \"{}\" on port {port}", inner.cfg.name);
        inner.sink.state(State::Waiting);
        Ok(Self {
            inner,
            accept,
            _advert: advert,
            port,
        })
    }

    pub fn stop(&self) {
        self.accept.abort();
        let mut s = self.inner.streams.lock().unwrap();
        s.close();
        for (_, h) in s.rtsp.drain() {
            h.abort();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Conn {
    peer: String,
    fairplay: FairPlay,
    pair: PairVerify,
    keys: Option<AudioKeys>,
}

fn bplist(d: Dictionary) -> Vec<u8> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(d)).expect("plist to memory");
    out
}

fn parse(body: &[u8]) -> Dictionary {
    Value::from_reader(std::io::Cursor::new(body))
        .ok()
        .and_then(Value::into_dictionary)
        .unwrap_or_default()
}

/// An 8-byte plist integer as iOS means it: unsigned.
fn unsigned(v: Option<&Value>) -> u64 {
    match v.and_then(Value::as_signed_integer) {
        Some(i) => i as u64,
        None => v.and_then(Value::as_unsigned_integer).unwrap_or(0),
    }
}

impl Inner {
    fn receiver(&self) -> Receiver<'_> {
        Receiver {
            id: &self.id,
            name: &self.cfg.name,
            hevc: self.cfg.hevc,
        }
    }

    async fn rtsp(self: Arc<Self>, stream: TcpStream, peer: SocketAddr) {
        let ip = peer.ip().to_string();
        log::info!("airplay: connection from {ip}");
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let mut conn = Conn {
            peer: ip.clone(),
            fairplay: FairPlay::default(),
            pair: PairVerify::default(),
            keys: None,
        };
        while let Some(req) = crate::rtsp::read(&mut r).await {
            let (status, extra, body) = self.dispatch(&mut conn, &req).await;
            if !(req.method == "POST" && req.url == "/feedback") {
                log::info!(
                    "airplay: {} {} ({} bytes) -> {status} ({} bytes)",
                    req.method,
                    req.url,
                    req.body.len(),
                    body.len()
                );
            }
            let cseq = req.headers.get("cseq").map(String::as_str).unwrap_or("0");
            if w.write_all(&crate::rtsp::reply(status, cseq, &extra, &body))
                .await
                .is_err()
            {
                break;
            }
            let idle = {
                let s = self.streams.lock().unwrap();
                s.mirror.is_none() && s.audio.is_empty()
            };
            if req.method == "TEARDOWN" && idle {
                break;
            }
        }
        log::info!("airplay: {ip} disconnected");
        let mut s = self.streams.lock().unwrap();
        if s.client.as_deref() == Some(ip.as_str()) {
            s.client = None;
            s.close();
            drop(s);
            self.sink.state(State::Waiting);
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        conn: &mut Conn,
        req: &crate::rtsp::Request,
    ) -> (&'static str, Vec<(&'static str, String)>, Vec<u8>) {
        const OK: &str = "200 OK";
        let bplist_type = || {
            vec![(
                "Content-Type",
                "application/x-apple-binary-plist".to_string(),
            )]
        };
        let octet = || vec![("Content-Type", "application/octet-stream".to_string())];
        match (req.method.as_str(), req.url.as_str()) {
            ("GET", "/info") => {
                let ct = req.headers.get("content-type").map(String::as_str);
                (OK, bplist_type(), self.receiver().info(ct, &req.body))
            }
            ("POST", "/pair-setup") => (OK, octet(), self.id.pk().to_vec()),
            ("POST", "/pair-verify") => (OK, octet(), conn.pair.handle(&self.id.key, &req.body)),
            ("POST", "/fp-setup") => match conn.fairplay.handle(&req.body) {
                Some(b) => (OK, octet(), b),
                None => ("501 Not Implemented", vec![], vec![]),
            },
            ("OPTIONS", _) => (
                OK,
                vec![(
                    "Public",
                    "SETUP, RECORD, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER".into(),
                )],
                vec![],
            ),
            ("SETUP", _) => (OK, bplist_type(), self.setup(conn, &req.body).await),
            ("GET_PARAMETER", _) if req.body.windows(6).any(|w| w == b"volume") => (
                OK,
                vec![("Content-Type", "text/parameters".into())],
                b"volume: 0.000000\r\n".to_vec(),
            ),
            ("RECORD", _) => (
                OK,
                vec![
                    ("Audio-Latency", "11025".into()),
                    ("Audio-Jack-Status", "connected; type=analog".into()),
                ],
                vec![],
            ),
            ("TEARDOWN", _) => {
                let d = parse(&req.body);
                let types: Vec<u64> = d
                    .get("streams")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|s| unsigned(s.as_dictionary().and_then(|d| d.get("type"))))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut s = self.streams.lock().unwrap();
                if types.is_empty() || types.contains(&110) {
                    if let Some(h) = s.mirror.take() {
                        h.abort();
                    }
                }
                if types.is_empty() || types.contains(&96) {
                    for h in s.audio.drain(..) {
                        h.abort();
                    }
                }
                drop(s);
                if types.is_empty() {
                    self.sink.state(State::Waiting);
                }
                (OK, vec![], vec![])
            }
            // SET_PARAMETER, FLUSH, POST /feedback, /audioMode, /command ...
            _ => (OK, vec![], vec![]),
        }
    }

    async fn setup(self: &Arc<Self>, conn: &mut Conn, body: &[u8]) -> Vec<u8> {
        let req = parse(body);
        let mut res = Dictionary::new();
        if let (Some(ekey), Some(eiv)) = (
            req.get("ekey").and_then(Value::as_data),
            req.get("eiv")
                .and_then(Value::as_data)
                .and_then(|d| <[u8; 16]>::try_from(d).ok()),
        ) {
            let fp = match conn.fairplay.decrypt(ekey) {
                Ok(k) => k,
                Err(e) => {
                    log::warn!("airplay: {e}");
                    return bplist(res);
                }
            };
            conn.keys = Some(AudioKeys {
                key: session_key(&fp, conn.pair.secret.as_ref()),
                iv: eiv,
            });
            {
                let mut s = self.streams.lock().unwrap();
                if let Some(old) = s.client.clone().filter(|c| *c != conn.peer) {
                    log::info!("airplay: {} replaces {old}", conn.peer);
                    if let Some(h) = s.rtsp.remove(&old) {
                        h.abort(); // the previous device's session ends for real
                    }
                }
                s.close();
                s.client = Some(conn.peer.clone());
            }
            let s = |k: &str| req.get(k).and_then(Value::as_string);
            let name = s("name")
                .or(s("model"))
                .unwrap_or("iPhone/iPad")
                .to_string();
            log::info!("airplay: {name} ({}) connected", s("model").unwrap_or(""));
            self.sink.state(State::Connected(name));
            if let Ok(sock) = UdpSocket::bind("0.0.0.0:0").await {
                let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
                res.insert("timingPort".into(), Value::Integer(port.into()));
                res.insert("eventPort".into(), Value::Integer(0.into()));
                let their = unsigned(req.get("timingPort")) as u16;
                let peer = conn.peer.clone();
                let task = tokio::spawn(async move {
                    if their == 0 {
                        return std::future::pending().await;
                    }
                    ntp_requests(sock, &peer, their).await
                });
                self.streams.lock().unwrap().timing = Some(task.abort_handle());
            }
        }
        let mut streams = Vec::new();
        for st in req
            .get("streams")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some((d, Some(keys))) = st.as_dictionary().map(|d| (d, conn.keys)) else {
                continue;
            };
            match unsigned(d.get("type")) {
                110 => {
                    let id = unsigned(d.get("streamConnectionID"));
                    log::info!("airplay: video stream id {id}");
                    let (key, iv) = stream_key_iv(&keys.key, id);
                    if let Some(port) = self.start_mirror(key, iv).await {
                        streams.push(Value::Dictionary(
                            [("dataPort", port as u64), ("type", 110)]
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), Value::Integer(v.into())))
                                .collect(),
                        ));
                    }
                }
                96 => {
                    let (Ok(data), Ok(control)) = (
                        UdpSocket::bind("0.0.0.0:0").await,
                        UdpSocket::bind("0.0.0.0:0").await,
                    ) else {
                        continue;
                    };
                    let ports = (
                        data.local_addr().map(|a| a.port()).unwrap_or(0),
                        control.local_addr().map(|a| a.port()).unwrap_or(0),
                    );
                    let ct = unsigned(d.get("ct"));
                    log::info!(
                        "airplay: audio stream ct={ct} spf={:?}",
                        d.get("spf").and_then(Value::as_unsigned_integer)
                    );
                    let sink = self.sink.clone();
                    let task = tokio::spawn(async move {
                        let _control = control; // shortcut: no RTCP/retransmit requests, as receiver.py
                        crate::audio::run(data, keys.key, keys.iv, ct, &*sink).await;
                    });
                    let mut s = self.streams.lock().unwrap();
                    for h in s.audio.drain(..) {
                        h.abort();
                    }
                    s.audio.push(task.abort_handle());
                    drop(s);
                    streams.push(Value::Dictionary(
                        [
                            ("dataPort", ports.0 as u64),
                            ("controlPort", ports.1 as u64),
                            ("type", 96),
                        ]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), Value::Integer(v.into())))
                        .collect(),
                    ));
                }
                _ => {}
            }
        }
        if !streams.is_empty() {
            res.insert("streams".into(), Value::Array(streams));
        }
        bplist(res)
    }

    async fn start_mirror(self: &Arc<Self>, key: [u8; 16], iv: [u8; 16]) -> Option<u16> {
        let listener = TcpListener::bind("0.0.0.0:0").await.ok()?;
        let port = listener.local_addr().ok()?.port();
        let sink = self.sink.clone();
        let streams = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let mut n = 0;
            loop {
                let (s, peer) = match listener.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("airplay: video accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                };
                n += 1;
                log::info!("airplay: video connection #{n} from {peer}");
                zenlink_core::quick_ack(&s);
                // Each video connection starts its own keystream (iOS opens it again sometimes).
                let sink = sink.clone();
                let t = tokio::spawn(async move { mirror::run(s, key, iv, &*sink).await });
                if let Some(me) = streams.upgrade() {
                    me.streams.lock().unwrap().video.push(t.abort_handle());
                }
            }
        });
        let mut s = self.streams.lock().unwrap();
        if let Some(h) = s.mirror.replace(task.abort_handle()) {
            h.abort();
        }
        Some(port)
    }
}

/// Asks the sender for its clock every 3 s, as UxPlay does (keeps it happy).
async fn ntp_requests(sock: UdpSocket, peer: &str, port: u16) {
    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            + 2_208_988_800.0;
        let mut pkt = vec![0x80, 0xD2, 0x00, 0x07];
        pkt.extend([0; 20]);
        pkt.extend((now as u32).to_be_bytes());
        pkt.extend((((now % 1.0) * 4_294_967_296.0) as u32).to_be_bytes());
        if sock.send_to(&pkt, (peer, port)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The default port, re-exported for the app.
pub const DEFAULT_PORT: u16 = PORT;
