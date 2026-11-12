//! Screen mirroring with the scrcpy server (phase A3: video only).
//!
//! Protocol notes: ZenshiroDev/docs/android/SCRCPY.md (pinned v5.0.1). In short:
//!
//! 1. push the server to [`DEVICE_SERVER_PATH`];
//! 2. tunnel `localabstract:scrcpy_<scid>`: `adb reverse` (the device connects
//!    to us), or `adb forward` when reverse is unavailable (we connect to it);
//! 3. start it with `app_process` and the client version as first argument;
//! 4. read the device meta (64-byte name) and the codec id, then packets:
//!    session packets (new capture size) and media packets (config or frame).
//!
//! Everything here is portable std Rust. The packets are raw `MediaCodec`
//! output (Annex B for H.264/H.265); the app converts them for its decoder.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::adb::{AdbClient, AdbError, Target};
use crate::scrcpy::{DEVICE_SERVER_PATH, SCRCPY_VERSION};

/// Video codec ids (`u32` big-endian, the codec's FourCC-like name).
pub const CODEC_H264: u32 = 0x6832_3634;
pub const CODEC_H265: u32 = 0x6832_3635;
pub const CODEC_AV1: u32 = 0x0061_7631;

/// Audio codec ids (scrcpy `AudioCodec.java`).
pub const CODEC_OPUS: u32 = 0x6f70_7573;
pub const CODEC_AAC: u32 = 0x0061_6163;
pub const CODEC_FLAC: u32 = 0x666c_6163;
/// PCM s16le, 48 kHz, stereo (`AudioConfig.java`): no decoder needed.
pub const CODEC_RAW: u32 = 0x0072_6177;

/// Codec ids that are not codecs (`Streamer.writeDisableStream`): the device
/// disabled the stream (e.g. audio capture not possible), or a
/// configuration error happened.
pub const STREAM_DISABLED: u32 = 0;
pub const STREAM_ERROR: u32 = 1;

/// Size of the device meta (the device name, NUL-padded UTF-8).
pub const DEVICE_NAME_LEN: usize = 64;

const FLAG_SESSION: u64 = 1 << 63;
const FLAG_CONFIG: u64 = 1 << 62;
const FLAG_KEY_FRAME: u64 = 1 << 61;
const PTS_MASK: u64 = FLAG_KEY_FRAME - 1;

/// Largest packet ZenLink accepts. A 4K keyframe is a few MB at most.
const MAX_PACKET: usize = 32 << 20;

/// Name of a codec id, for logs and the UI.
pub fn codec_name(id: u32) -> &'static str {
    match id {
        CODEC_H264 => "h264",
        CODEC_H265 => "h265",
        CODEC_AV1 => "av1",
        CODEC_OPUS => "opus",
        CODEC_AAC => "aac",
        CODEC_FLAC => "flac",
        CODEC_RAW => "raw",
        STREAM_DISABLED => "disabled",
        STREAM_ERROR => "error",
        _ => "unknown",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    /// A new capture session (start, rotation, display resize).
    Session {
        width: u32,
        height: u32,
        client_resized: bool,
    },
    /// Codec configuration (SPS/PPS for H.264), Annex B.
    Config(Vec<u8>),
    /// One encoded frame, Annex B. `pts` is in microseconds.
    Media { pts: u64, key: bool, data: Vec<u8> },
}

/// Reads the device name sent first on the first socket.
pub fn read_device_meta<R: Read>(r: &mut R) -> io::Result<String> {
    let mut name = [0u8; DEVICE_NAME_LEN];
    r.read_exact(&mut name)?;
    let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
    Ok(String::from_utf8_lossy(&name[..end]).trim().to_string())
}

/// Writes a device meta block (tests and recordings).
pub fn write_device_meta<W: Write>(w: &mut W, name: &str) -> io::Result<()> {
    let mut block = [0u8; DEVICE_NAME_LEN];
    // Keep it NUL-terminated and cut on a character boundary.
    let mut n = name.len().min(DEVICE_NAME_LEN - 1);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    block[..n].copy_from_slice(&name.as_bytes()[..n]);
    w.write_all(&block)
}

/// The video socket after the device meta: codec id, then packets.
pub struct VideoReader<R> {
    r: R,
    pub codec: u32,
}

impl<R: Read> VideoReader<R> {
    /// Reads the codec id. `0` means the device could not encode (scrcpy
    /// sends it when the stream is disabled or the encoder failed).
    pub fn new(mut r: R) -> io::Result<Self> {
        let mut id = [0u8; 4];
        r.read_exact(&mut id)?;
        Ok(Self {
            r,
            codec: u32::from_be_bytes(id),
        })
    }

    /// Next packet. An error (including EOF) ends the stream.
    pub fn next_packet(&mut self) -> io::Result<Packet> {
        let mut head = [0u8; 12];
        self.r.read_exact(&mut head)?;
        let word = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        if head[0] & 0x80 != 0 {
            return Ok(Packet::Session {
                width: word(4),
                height: word(8),
                client_resized: head[3] & 1 != 0,
            });
        }
        let flags = u64::from_be_bytes(head[..8].try_into().unwrap());
        let len = word(8) as usize;
        if len == 0 || len > MAX_PACKET {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad packet size {len}"),
            ));
        }
        let mut data = vec![0u8; len];
        self.r.read_exact(&mut data)?;
        if flags & FLAG_CONFIG != 0 {
            return Ok(Packet::Config(data));
        }
        Ok(Packet::Media {
            pts: flags & PTS_MASK,
            key: flags & FLAG_KEY_FRAME != 0,
            data,
        })
    }

    pub fn into_inner(self) -> R {
        self.r
    }
}

/// Writes one packet in the server's wire format (tests and recordings).
pub fn write_packet<W: Write>(w: &mut W, p: &Packet) -> io::Result<()> {
    match p {
        Packet::Session {
            width,
            height,
            client_resized,
        } => {
            let flags = (FLAG_SESSION >> 32) as u32 | u32::from(*client_resized);
            w.write_all(&flags.to_be_bytes())?;
            w.write_all(&width.to_be_bytes())?;
            w.write_all(&height.to_be_bytes())
        }
        Packet::Config(d) => {
            w.write_all(&FLAG_CONFIG.to_be_bytes())?;
            w.write_all(&(d.len() as u32).to_be_bytes())?;
            w.write_all(d)
        }
        Packet::Media { pts, key, data } => {
            let flags = (pts & PTS_MASK) | if *key { FLAG_KEY_FRAME } else { 0 };
            w.write_all(&flags.to_be_bytes())?;
            w.write_all(&(data.len() as u32).to_be_bytes())?;
            w.write_all(data)
        }
    }
}

/// A random 31-bit session id; it names the device socket `scrcpy_<scid>`.
pub fn random_scid() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (h.finish() as u32) & 0x7fff_ffff
}

/// Device socket name for a session id (the server formats it with `%08x`).
pub fn socket_name(scid: u32) -> String {
    format!("scrcpy_{scid:08x}")
}

/// Server options (phases A3-A4). Later phases add control and the
/// per-device profiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoOptions {
    /// Longest side in pixels, 0 = native.
    pub max_size: u32,
    /// Bits per second, 0 = server default (8 Mbps).
    pub bit_rate: u32,
    /// Frames per second cap, 0 = no cap.
    pub max_fps: u32,
    /// `h264` (the default; `h265`/`av1` only after Smart Encoder Detection).
    pub codec: &'static str,
    /// A MediaCodec encoder name from the encoder list (`None` = the
    /// server's default for the codec).
    pub encoder: Option<String>,
    /// Device sound as raw PCM (Android 11+). Raw needs no decoder on the
    /// PC: 1.5 Mbps, small next to the video.
    pub audio: bool,
    /// Keep the sound playing on the device too (`audio_dup`, Android 13+;
    /// the server then captures the playback, not the output).
    pub audio_dup: bool,
    /// Control socket (touch, keys, clipboard from the PC).
    pub control: bool,
    /// Mirror a new virtual display instead of the device screen
    /// (`new_display`, Android 10+). Apps are started on it with
    /// `Control::StartApp`; it is destroyed with its apps when the session ends.
    pub new_display: Option<NewDisplay>,
}

/// A virtual display's size and density; 0 = the server's default
/// (the main display's size and density).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NewDisplay {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    /// Resizable with `Control::ResizeDisplay` (`flex_display`), e.g. to
    /// follow the window. Not with `max_size` cropping.
    pub flex: bool,
}

impl NewDisplay {
    /// The `new_display` value: `WxH/dpi`, `WxH`, `/dpi` or empty
    /// (`Options.parseNewDisplay`).
    pub fn arg(&self) -> String {
        let size = if self.width > 0 && self.height > 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            String::new()
        };
        if self.dpi > 0 {
            format!("{size}/{}", self.dpi)
        } else {
            size
        }
    }
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            max_size: 1920,
            bit_rate: 8_000_000,
            max_fps: 60,
            codec: "h264",
            encoder: None,
            audio: false,
            audio_dup: false,
            control: false,
            new_display: None,
        }
    }
}

/// A MediaCodec name safe to put in a shell command.
pub fn is_encoder_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The `adb shell` command that starts the server.
pub fn server_command(scid: u32, tunnel_forward: bool, o: &VideoOptions) -> String {
    let mut cmd = format!(
        "CLASSPATH={DEVICE_SERVER_PATH} app_process / com.genymobile.scrcpy.Server {SCRCPY_VERSION} \
         scid={scid:08x} log_level=info video_codec={} {} {}",
        o.codec,
        if o.audio && o.audio_dup {
            "audio=true audio_codec=raw audio_source=playback audio_dup=true"
        } else if o.audio {
            "audio=true audio_codec=raw"
        } else {
            "audio=false"
        },
        // No automatic clipboard sync: the PC sends its clipboard only on
        // Ctrl+V (the device's copies are drained, not used yet).
        if o.control {
            "control=true clipboard_autosync=false"
        } else {
            "control=false"
        }
    );
    if tunnel_forward {
        cmd.push_str(" tunnel_forward=true");
    }
    // Names come from the server's own list; anything else is not passed
    // to the shell.
    if let Some(e) = o.encoder.as_deref().filter(|e| is_encoder_name(e)) {
        cmd.push_str(&format!(" video_encoder={e}"));
    }
    if o.max_size > 0 {
        cmd.push_str(&format!(" max_size={}", o.max_size));
    }
    if o.bit_rate > 0 {
        cmd.push_str(&format!(" video_bit_rate={}", o.bit_rate));
    }
    if o.max_fps > 0 {
        cmd.push_str(&format!(" max_fps={}", o.max_fps));
    }
    if let Some(d) = &o.new_display {
        // Digits, 'x' and '/' only: safe in the shell command.
        cmd.push_str(&format!(" new_display={}", d.arg()));
        if d.flex {
            cmd.push_str(" flex_display=true");
        }
    }
    cmd
}

/// How long the server gets to connect after it was started.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A running mirror session: the video socket and the server's shell.
/// Destructure it to keep `video` and `session` apart; dropping the
/// session ends the server.
pub struct Mirror {
    pub device_name: String,
    pub video: VideoReader<TcpStream>,
    /// Raw PCM packets; `None` when audio was not asked for, or the device
    /// disabled it (video-only fallback).
    pub audio: Option<VideoReader<TcpStream>>,
    /// Write end of the control socket (`None` without `control`).
    pub control: Option<TcpStream>,
    pub session: Session,
}

/// The server's shell and handles on the media sockets.
pub struct Session {
    /// Closing the shell socket ends `app_process` on the device.
    shell: TcpStream,
    sockets: Vec<TcpStream>,
}

impl Mirror {
    /// Pushes `server` (the bundled jar), starts it and connects the video
    /// socket. Blocks for at most a few seconds.
    pub fn start(
        client: &AdbClient,
        target: &Target,
        server: &[u8],
        opts: &VideoOptions,
    ) -> Result<Self, AdbError> {
        client.push(target, server, DEVICE_SERVER_PATH, 0o100644)?;
        let scid = random_scid();
        let remote = format!("localabstract:{}", socket_name(scid));

        // Prefer reverse: no port has to be free on the device side, and the
        // server does not need to send a dummy byte.
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let local = format!("tcp:{}", listener.local_addr()?.port());
        let forward = match client.reverse(target, &remote, &local) {
            Ok(()) => None,
            Err(e) => {
                log::info!("android: adb reverse failed ({e}), using forward");
                let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
                let local = format!("tcp:{port}");
                client.forward(target, &local, &remote)?;
                Some((port, local))
            }
        };

        let shell = client.open_service(
            target,
            &format!("shell:{}", server_command(scid, forward.is_some(), opts)),
        )?;
        shell.set_read_timeout(None)?;
        let ended = Arc::new(AtomicBool::new(false));
        log_shell(shell.try_clone()?, ended.clone());

        // The server opens its sockets in order: video, audio, control.
        // The tunnel stays until all are connected.
        let count = 1 + usize::from(opts.audio) + usize::from(opts.control);
        let mut sockets = Vec::with_capacity(count);
        let mut failed = None;
        for n in 0..count {
            let r = match &forward {
                None => accept(&listener, &ended),
                // Only the first forward socket carries the dummy byte.
                Some((port, _)) if n == 0 => connect_forward(*port, &ended),
                Some((port, _)) => TcpStream::connect(("127.0.0.1", *port)),
            };
            match r {
                Ok(s) => sockets.push(s),
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match &forward {
            None => {
                let _ = client.reverse_remove(target, &remote);
            }
            Some((_, local)) => {
                let _ = client.forward_remove(target, local);
            }
        }
        if let Some(e) = failed {
            let _ = shell.shutdown(Shutdown::Both);
            return Err(e.into());
        }
        let mut handles = Vec::with_capacity(count);
        for s in &sockets {
            // Bounded until the headers arrived: a server that connects but never writes them
            // must not block the start (Stop cannot interrupt it yet).
            s.set_read_timeout(Some(Duration::from_secs(15)))?;
            s.set_nodelay(true)?;
            handles.push(s.try_clone()?);
        }
        let session = Session {
            shell,
            sockets: handles,
        };
        let mut it = sockets.into_iter();
        let mut video = it.next().expect("one socket");
        let device_name = read_device_meta(&mut video)?;
        let video = VideoReader::new(video)?;
        let audio = match opts.audio {
            true => open_audio(it.next().expect("audio socket"))?,
            false => None,
        };
        for s in &session.sockets {
            s.set_read_timeout(None)?;
        }
        let control = match it.next() {
            Some(c) => {
                Self::drain_device_messages(c.try_clone()?);
                c.set_write_timeout(Some(Duration::from_millis(500)))?;
                Some(c)
            }
            None => None,
        };
        log::info!(
            "android: mirroring {device_name:?} video={} audio={}",
            codec_name(video.codec),
            audio.as_ref().map_or("off", |a| codec_name(a.codec))
        );
        Ok(Self {
            device_name,
            video,
            audio,
            control,
            session,
        })
    }

    /// Reads and drops what the device writes on the control socket
    /// (clipboard, acks, UHID output), so its buffer never fills and blocks the
    /// server. Ends when the socket closes.
    fn drain_device_messages(mut s: TcpStream) {
        let _ = std::thread::Builder::new()
            .name("android-control-rx".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
            });
    }

    /// A handle that ends the session from another thread.
    pub fn stopper(&self) -> io::Result<Stopper> {
        self.session.stopper()
    }
}

impl Session {
    pub fn stopper(&self) -> io::Result<Stopper> {
        Ok(Stopper {
            shell: self.shell.try_clone()?,
            sockets: self
                .sockets
                .iter()
                .map(TcpStream::try_clone)
                .collect::<io::Result<_>>()?,
        })
    }
}

/// Ends a [`Mirror`] from another thread: blocked reads return.
pub struct Stopper {
    shell: TcpStream,
    sockets: Vec<TcpStream>,
}

impl Stopper {
    pub fn stop(&self) {
        for s in &self.sockets {
            let _ = s.shutdown(Shutdown::Both);
        }
        let _ = self.shell.shutdown(Shutdown::Both);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for s in &self.sockets {
            let _ = s.shutdown(Shutdown::Both);
        }
        let _ = self.shell.shutdown(Shutdown::Both);
    }
}

/// Reads the audio codec id. A device that cannot capture sound (Android 10
/// or older, or capture refused) disables the stream: mirroring goes on
/// without sound.
fn open_audio(s: TcpStream) -> io::Result<Option<VideoReader<TcpStream>>> {
    // The server writes the id once its capture is set up.
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let r = VideoReader::new(s)?;
    match r.codec {
        CODEC_RAW => {
            r.r.set_read_timeout(None)?;
            Ok(Some(r))
        }
        STREAM_DISABLED => {
            log::info!("android: the device disabled audio; video only");
            Ok(None)
        }
        other => {
            log::warn!("android: audio stream {}; video only", codec_name(other));
            Ok(None)
        }
    }
}

/// Logs the server's output; sets `ended` when the shell closes.
fn log_shell(mut shell: TcpStream, ended: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("scrcpy-server-log".into())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            let mut line = Vec::new();
            loop {
                match shell.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if b == b'\n' {
                                log::info!("scrcpy: {}", String::from_utf8_lossy(&line).trim_end());
                                line.clear();
                            } else if line.len() < 4096 {
                                line.push(b);
                            }
                        }
                    }
                }
            }
            if !line.is_empty() {
                log::info!("scrcpy: {}", String::from_utf8_lossy(&line).trim_end());
            }
            ended.store(true, Relaxed);
        });
}

fn accept(listener: &TcpListener, ended: &AtomicBool) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nonblocking(false)?;
                return Ok(s);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        if ended.load(Relaxed) {
            return Err(io::Error::other(
                "the scrcpy server exited before connecting",
            ));
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the scrcpy server did not connect",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Forward tunnel: adb accepts our connection even before the server
/// listens, so a connection only counts once the server's dummy byte arrives.
fn connect_forward(port: u16, ended: &AtomicBool) -> io::Result<TcpStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut dummy = [0u8; 1];
            if s.read_exact(&mut dummy).is_ok() {
                return Ok(s);
            }
        }
        if ended.load(Relaxed) {
            return Err(io::Error::other(
                "the scrcpy server exited before connecting",
            ));
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "could not connect to the scrcpy server",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::adb::read_frame;
    use std::sync::Mutex;

    /// A stand-in for the adb server plus a device running the scrcpy server:
    /// takes the push, the reverse tunnel and the shell command, then
    /// connects back through the "tunnel" and streams `wire`.
    fn fake_device(wire: Vec<u8>) -> (u16, Arc<Mutex<Vec<String>>>) {
        fake_device_with_audio(wire, None)
    }

    fn fake_device_with_audio(
        wire: Vec<u8>,
        audio: Option<Vec<u8>>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            let tunnel = Arc::new(Mutex::new(0u16));
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let (log, tunnel, wire) = (log2.clone(), tunnel.clone(), wire.clone());
                let audio = audio.clone();
                std::thread::spawn(move || {
                    let t = String::from_utf8(read_frame(&mut c).unwrap()).unwrap();
                    assert_eq!(t, "host:transport:SER");
                    c.write_all(b"OKAY").unwrap();
                    let svc = String::from_utf8(read_frame(&mut c).unwrap()).unwrap();
                    log.lock().unwrap().push(svc.clone());
                    c.write_all(b"OKAY").unwrap();
                    if svc == "sync:" {
                        let mut total = 0usize;
                        loop {
                            let mut h = [0u8; 8];
                            c.read_exact(&mut h).unwrap();
                            let n = u32::from_le_bytes(h[4..].try_into().unwrap()) as usize;
                            match &h[..4] {
                                b"SEND" | b"DATA" => {
                                    let mut b = vec![0u8; n];
                                    c.read_exact(&mut b).unwrap();
                                    if &h[..4] == b"DATA" {
                                        total += n;
                                    }
                                }
                                b"DONE" => break,
                                _ => panic!("sync"),
                            }
                        }
                        log.lock().unwrap().push(format!("pushed {total}"));
                        c.write_all(b"OKAY\0\0\0\0").unwrap();
                    } else if let Some(rest) = svc.strip_prefix("reverse:forward:") {
                        *tunnel.lock().unwrap() =
                            rest.rsplit("tcp:").next().unwrap().parse().unwrap();
                        c.write_all(b"OKAY").unwrap();
                    } else if svc.starts_with("reverse:killforward:") {
                        c.write_all(b"OKAY").unwrap();
                    } else if svc.starts_with("shell:") {
                        let p = *tunnel.lock().unwrap();
                        let mut v = TcpStream::connect(("127.0.0.1", p)).unwrap();
                        c.write_all(b"[server] INFO: Device: [Google] Pixel 9\n")
                            .unwrap();
                        v.write_all(&wire).unwrap();
                        let _a = svc.contains("audio=true").then(|| {
                            let mut a = TcpStream::connect(("127.0.0.1", p)).unwrap();
                            a.write_all(audio.as_deref().unwrap_or(&[])).unwrap();
                            a
                        });
                        // Keep the shell open until the client closes it.
                        let mut sink = [0u8; 64];
                        while matches!(c.read(&mut sink), Ok(n) if n > 0) {}
                        log.lock().unwrap().push("shell closed".into());
                    }
                });
            }
        });
        (port, log)
    }

    #[test]
    fn start_pushes_tunnels_and_reads_the_stream() {
        let mut wire = Vec::new();
        write_device_meta(&mut wire, "Pixel 9").unwrap();
        wire.extend_from_slice(&CODEC_H264.to_be_bytes());
        let first = Packet::Session {
            width: 1080,
            height: 2424,
            client_resized: false,
        };
        let config = Packet::Config(vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f]);
        write_packet(&mut wire, &first).unwrap();
        write_packet(&mut wire, &config).unwrap();
        let (port, log) = fake_device(wire);
        let mut client = AdbClient::new(port);
        client.timeout = Duration::from_secs(2);
        let jar = vec![1u8; 100_000];
        let mut m = Mirror::start(
            &client,
            &Target::Serial("SER"),
            &jar,
            &VideoOptions::default(),
        )
        .unwrap();
        assert_eq!(m.device_name, "Pixel 9");
        assert_eq!(m.video.codec, CODEC_H264);
        assert_eq!(m.video.next_packet().unwrap(), first);
        assert_eq!(m.video.next_packet().unwrap(), config);
        m.stopper().unwrap().stop();
        assert!(m.video.next_packet().is_err(), "stop ends the stream");
        drop(m);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !log.lock().unwrap().iter().any(|l| l == "shell closed") {
            assert!(
                Instant::now() < deadline,
                "shell not closed: {:?}",
                log.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = log.lock().unwrap();
        assert_eq!(log[0], "sync:");
        assert_eq!(log[1], "pushed 100000");
        assert!(
            log[2].starts_with("reverse:forward:localabstract:scrcpy_"),
            "{}",
            log[2]
        );
        assert!(log[3].starts_with("shell:CLASSPATH=/data/local/tmp/scrcpy-server.jar"));
        assert!(log[4].starts_with("reverse:killforward:localabstract:scrcpy_"));
    }

    fn video_wire() -> Vec<u8> {
        let mut wire = Vec::new();
        write_device_meta(&mut wire, "Pixel 9").unwrap();
        wire.extend_from_slice(&CODEC_H264.to_be_bytes());
        write_packet(&mut wire, &Packet::Config(vec![0, 0, 0, 1, 0x67])).unwrap();
        wire
    }

    fn start_with(audio: Vec<u8>) -> Mirror {
        let (port, _log) = fake_device_with_audio(video_wire(), Some(audio));
        let mut client = AdbClient::new(port);
        client.timeout = Duration::from_secs(2);
        let opts = VideoOptions {
            audio: true,
            ..VideoOptions::default()
        };
        Mirror::start(&client, &Target::Serial("SER"), &[1, 2, 3], &opts).unwrap()
    }

    #[test]
    fn audio_socket_carries_raw_pcm() {
        let mut a = CODEC_RAW.to_be_bytes().to_vec();
        let pcm = Packet::Media {
            pts: 10_000,
            key: false,
            data: vec![1, 0, 2, 0, 3, 0, 4, 0],
        };
        write_packet(&mut a, &pcm).unwrap();
        let mut m = start_with(a);
        assert!(matches!(m.video.next_packet().unwrap(), Packet::Config(_)));
        let audio = m.audio.as_mut().expect("audio stream");
        assert_eq!(audio.codec, CODEC_RAW);
        assert_eq!(audio.next_packet().unwrap(), pcm);
        m.stopper().unwrap().stop();
        assert!(m.audio.as_mut().unwrap().next_packet().is_err());
    }

    #[test]
    fn device_without_audio_falls_back_to_video_only() {
        let m = start_with(STREAM_DISABLED.to_be_bytes().to_vec());
        assert!(m.audio.is_none());
        assert_eq!(m.device_name, "Pixel 9");
        let m = start_with(STREAM_ERROR.to_be_bytes().to_vec());
        assert!(m.audio.is_none());
    }

    #[test]
    fn audio_options_in_the_server_command() {
        let o = VideoOptions {
            audio: true,
            ..VideoOptions::default()
        };
        assert!(server_command(1, false, &o).contains(" audio=true audio_codec=raw control=false"));
        // Both: the phone keeps its sound (scrcpy audio_dup, playback source).
        let both = VideoOptions {
            audio: true,
            audio_dup: true,
            ..VideoOptions::default()
        };
        assert!(server_command(1, false, &both).contains(
            " audio=true audio_codec=raw audio_source=playback audio_dup=true control=false"
        ));
        assert_eq!(codec_name(CODEC_OPUS), "opus");
        assert_eq!(codec_name(0x0072_6177), "raw");
    }

    #[test]
    fn packets_round_trip() {
        let packets = [
            Packet::Session {
                width: 1080,
                height: 2400,
                client_resized: false,
            },
            Packet::Config(vec![0, 0, 0, 1, 0x67, 1]),
            Packet::Media {
                pts: 33_333,
                key: true,
                data: vec![0, 0, 0, 1, 0x65, 2],
            },
            Packet::Media {
                pts: (1 << 61) - 1,
                key: false,
                data: vec![0, 0, 1, 0x41],
            },
            Packet::Session {
                width: 2400,
                height: 1080,
                client_resized: true,
            },
        ];
        let mut wire = Vec::new();
        write_device_meta(&mut wire, "Pixel 9").unwrap();
        wire.extend_from_slice(&CODEC_H264.to_be_bytes());
        for p in &packets {
            write_packet(&mut wire, p).unwrap();
        }
        let mut r = &wire[..];
        assert_eq!(read_device_meta(&mut r).unwrap(), "Pixel 9");
        let mut v = VideoReader::new(r).unwrap();
        assert_eq!(codec_name(v.codec), "h264");
        for p in &packets {
            assert_eq!(&v.next_packet().unwrap(), p);
        }
        assert!(v.next_packet().is_err(), "EOF ends the stream");
    }

    #[test]
    fn wire_layout_matches_the_server() {
        // Values from scrcpy's Streamer.java: config = 1<<62, key frame = 1<<61.
        let mut w = Vec::new();
        write_packet(&mut w, &Packet::Config(vec![9])).unwrap();
        assert_eq!(w, [0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 9]);
        let mut w = Vec::new();
        write_packet(
            &mut w,
            &Packet::Media {
                pts: 1,
                key: true,
                data: vec![7],
            },
        )
        .unwrap();
        assert_eq!(w, [0x20, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 7]);
        let mut w = Vec::new();
        write_packet(
            &mut w,
            &Packet::Session {
                width: 2,
                height: 3,
                client_resized: true,
            },
        )
        .unwrap();
        assert_eq!(w, [0x80, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]);
    }

    #[test]
    fn rejects_bad_sizes() {
        let mut wire = CODEC_H264.to_be_bytes().to_vec();
        wire.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let mut v = VideoReader::new(&wire[..]).unwrap();
        assert!(v.next_packet().is_err(), "zero-size packet");
    }

    #[test]
    fn device_meta_is_nul_padded_and_cut_safely() {
        let mut w = Vec::new();
        write_device_meta(&mut w, &"é".repeat(40)).unwrap();
        assert_eq!(w.len(), DEVICE_NAME_LEN);
        let name = read_device_meta(&mut &w[..]).unwrap();
        assert_eq!(name, "é".repeat(31));
    }

    #[test]
    fn server_command_has_version_scid_and_options() {
        let cmd = server_command(0xabcd, false, &VideoOptions::default());
        assert!(cmd.starts_with(
            "CLASSPATH=/data/local/tmp/scrcpy-server.jar app_process / com.genymobile.scrcpy.Server 5.0.1 "
        ));
        assert!(cmd.contains(" scid=0000abcd "), "{cmd}");
        assert!(cmd.contains(" audio=false control=false"));
        assert!(cmd.contains(" video_codec=h264"));
        assert!(cmd.contains(" max_size=1920 video_bit_rate=8000000 max_fps=60"));
        assert!(!cmd.contains("tunnel_forward"));
        let fwd = server_command(
            1,
            true,
            &VideoOptions {
                max_size: 0,
                bit_rate: 0,
                max_fps: 0,
                codec: "h264",
                encoder: None,
                audio: false,
                audio_dup: false,
                control: true,
                new_display: None,
            },
        );
        assert!(
            fwd.ends_with("control=true clipboard_autosync=false tunnel_forward=true"),
            "{fwd}"
        );
    }

    #[test]
    fn new_display_in_the_server_command() {
        let mut o = VideoOptions {
            new_display: Some(NewDisplay {
                width: 1280,
                height: 720,
                dpi: 240,
                flex: true,
            }),
            ..VideoOptions::default()
        };
        let cmd = server_command(1, false, &o);
        assert!(
            cmd.ends_with(" new_display=1280x720/240 flex_display=true"),
            "{cmd}"
        );
        o.new_display = Some(NewDisplay::default());
        assert!(server_command(1, false, &o).ends_with(" new_display="));
        o.new_display = Some(NewDisplay {
            dpi: 160,
            ..NewDisplay::default()
        });
        assert!(server_command(1, false, &o).ends_with(" new_display=/160"));
        o.new_display = Some(NewDisplay {
            width: 800,
            height: 600,
            ..NewDisplay::default()
        });
        assert!(server_command(1, false, &o).ends_with(" new_display=800x600"));
        assert!(!server_command(1, false, &VideoOptions::default()).contains("new_display"));
    }
    #[test]
    fn encoder_name_is_passed_only_when_safe() {
        let mut o = VideoOptions {
            codec: "h265",
            encoder: Some("c2.qti.hevc.encoder".into()),
            ..VideoOptions::default()
        };
        let cmd = server_command(1, false, &o);
        assert!(cmd.contains(" video_codec=h265 "));
        assert!(cmd.contains(" video_encoder=c2.qti.hevc.encoder "), "{cmd}");
        o.encoder = Some("x; reboot".into());
        assert!(!server_command(1, false, &o).contains("video_encoder"));
        o.encoder = Some(String::new());
        assert!(!server_command(1, false, &o).contains("video_encoder"));
    }

    #[test]
    fn scid_is_31_bit_and_named_like_the_server() {
        for _ in 0..50 {
            assert!(random_scid() <= 0x7fff_ffff);
        }
        assert_eq!(socket_name(0x1a2b), "scrcpy_00001a2b");
    }
}
