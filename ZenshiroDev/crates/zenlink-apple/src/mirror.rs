//! The screen over CoreDevice's display service: HEVC over RTP through the link, handed out
//! as Annex B access units. The offer follows idevice's tools/src/coredevice_stream.rs
//! (MIT, Jackson Coxson) and what Device Hub sends.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use idevice::core_device::{
    build_frame_ack, build_keyframe_request, build_liveness, build_rctl, build_receiver_report,
    build_screen_audio_offer, build_screen_video_offer, build_sdes, build_start_audio_parameters,
    build_start_video_parameters, is_rtcp, negotiation::build_screen_media_blob, CallInfoBlob,
    DisplayServiceClient, ReportBlock, RtpPacket, SenderReport,
};
use idevice::RsdService as _;

use crate::link::Link;

/// What the controller supports (Device Hub sends 140), not the device's mask.
const CLIENT_SUPPORTED_FEATURES: u64 = 140;
/// No packet for this long: the device stopped sending (locked, cable out, Wi-Fi gone).
const SILENCE: Duration = Duration::from_secs(8);
/// Default video bitrate cap in idevice's offer (6 Mbps, used on USB).
const FULL_BITRATE: u32 = 6_000_000;
/// Lower cap on Wi-Fi so the TLS-PSK TCP tunnel does not build up head-of-line bursts.
const WIFI_BITRATE: u32 = 3_000_000;
/// Minimum interval between RTCP PLI+FIR retries while waiting for a keyframe.
const KEYFRAME_RETRY: Duration = Duration::from_millis(350);

/// Maximum out-of-order packets held before declaring a gap lost.
/// Over a TCP tunnel packets arrive in order; >8 packets past a gap is loss.
const MAX_REORDER: usize = 8;
/// Skip a stalled sequence gap after this long even on a static screen.
const REORDER_TIMEOUT: Duration = Duration::from_millis(60);
/// Hard cap for a single access unit so missing markers cannot grow unbounded.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const ANNEXB_START: [u8; 4] = [0, 0, 0, 1];
const MEDIA_BLOB_KEY: &str = "avcMediaStreamNegotiatorMediaBlob";

/// One RTP packet: sequence number, timestamp, payload, marker.
type Held = (u16, u32, Vec<u8>, bool);

/// Packets in a row a new SSRC needs before it replaces the running stream.
const SWITCH_AFTER: usize = 30;

/// Picks the screen's RTP stream by SSRC. The video port also gets a lone packet about once a
/// second from another SSRC (seq 12, a new SSRC each time; seen on iPadOS 27). Fed to the
/// depacketizer it can move the cursor to seq 12, after which every video packet is "behind"
/// and dropped: the picture freezes while packets keep coming.
#[derive(Default)]
struct Picker {
    main: Option<u32>,
    /// Packets of SSRCs not (yet) accepted, kept so the first keyframe is not lost.
    held: std::collections::HashMap<u32, Vec<Held>>,
}

impl Picker {
    /// `None`: ignore the packet (it was held). `Some(packets)`: feed these, in order; after a
    /// switch to a new stream they are its held packets and the depacketizer must start fresh.
    fn push(&mut self, ssrc: u32, p: Held) -> Option<(bool, Vec<Held>)> {
        if self.main == Some(ssrc) {
            return Some((false, vec![p]));
        }
        let need = if self.main.is_some() { SWITCH_AFTER } else { 2 };
        let h = self.held.entry(ssrc).or_default();
        h.push(p);
        if h.len() >= need {
            let h = self.held.remove(&ssrc).unwrap_or_default();
            self.held.clear();
            self.main = Some(ssrc);
            return Some((true, h));
        }
        if self.held.len() > 8 {
            self.held.retain(|_, v| v.len() > 1); // the lone packets
        }
        None
    }
}

/// RTP sequence-number ordering with 16-bit wraparound (RFC 1982).
#[inline]
fn seq_lt(a: u16, b: u16) -> bool {
    let diff = b.wrapping_sub(a);
    diff != 0 && diff < 0x8000
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssembledFrame {
    ts: u32,
    key: bool,
    annexb: Vec<u8>,
}

struct ReorderPkt {
    seq: u16,
    ts: u32,
    marker: bool,
    payload: Vec<u8>,
    at: Instant,
}

/// Loss-aware RFC 7798 HEVC depacketizer and access-unit assembler.
///
/// Unlike `idevice::HevcDepacketizer`, this bounds reordering to `MAX_REORDER` packets and
/// `REORDER_TIMEOUT`, handles 16-bit sequence wraparound cleanly, drops corrupted frames on loss,
/// splits access units on timestamp changes if a marker packet is lost, and preserves cached
/// parameter sets (VPS/SPS/PPS) across sequence resets.
#[derive(Default)]
struct HevcAssembler {
    next_seq: Option<u16>,
    reorder: Vec<ReorderPkt>,
    fu_buf: Vec<u8>,
    fu_active: bool,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    cur_ts: Option<u32>,
    cur_nals: Vec<u8>,
    cur_has_vcl: bool,
    cur_is_key: bool,
    cur_has_ps: [bool; 3],
    cur_corrupt: bool,
    lost: bool,
}

impl HevcAssembler {
    fn new() -> Self {
        Self::default()
    }

    fn reset_fu(&mut self) {
        self.fu_buf.clear();
        self.fu_active = false;
    }

    fn clear_au(&mut self) {
        self.cur_ts = None;
        self.cur_nals.clear();
        self.cur_has_vcl = false;
        self.cur_is_key = false;
        self.cur_has_ps = [false; 3];
        self.cur_corrupt = false;
    }

    /// Resets stream sequencing after a stall while keeping cached VPS/SPS/PPS.
    fn reset_sequence(&mut self) {
        self.next_seq = None;
        self.reorder.clear();
        self.reset_fu();
        self.clear_au();
    }

    /// Full reset when switching to a brand new RTP SSRC.
    fn reset_all(&mut self) {
        self.reset_sequence();
        self.vps = None;
        self.sps = None;
        self.pps = None;
        self.lost = false;
    }

    fn take_lost(&mut self) -> bool {
        std::mem::take(&mut self.lost)
    }

    fn push(
        &mut self,
        seq: u16,
        ts: u32,
        marker: bool,
        payload: Vec<u8>,
        now: Instant,
    ) -> Vec<AssembledFrame> {
        if self.next_seq.is_none() {
            self.next_seq = Some(seq);
        }
        if self.next_seq.is_some_and(|next| seq_lt(seq, next)) {
            return Vec::new();
        }
        if !self.reorder.iter().any(|p| p.seq == seq) {
            self.reorder.push(ReorderPkt {
                seq,
                ts,
                marker,
                payload,
                at: now,
            });
        }
        let mut out = Vec::new();
        self.drain_in_order(&mut out);
        self.skip_gaps(now, &mut out);
        out
    }

    fn flush_stale(&mut self, now: Instant) -> Vec<AssembledFrame> {
        let mut out = Vec::new();
        self.skip_gaps(now, &mut out);
        out
    }

    fn drain_in_order(&mut self, out: &mut Vec<AssembledFrame>) {
        while let Some(next) = self.next_seq {
            let Some(pos) = self.reorder.iter().position(|p| p.seq == next) else {
                break;
            };
            let pkt = self.reorder.swap_remove(pos);
            self.process_packet(pkt.ts, pkt.marker, &pkt.payload, out);
            self.next_seq = Some(next.wrapping_add(1));
        }
    }

    fn skip_gaps(&mut self, now: Instant, out: &mut Vec<AssembledFrame>) {
        loop {
            if self.reorder.is_empty() {
                break;
            }
            let oldest_at = self
                .reorder
                .iter()
                .map(|p| p.at)
                .min()
                .unwrap_or(now);
            let timed_out = now.saturating_duration_since(oldest_at) >= REORDER_TIMEOUT;
            if self.reorder.len() <= MAX_REORDER && !timed_out {
                break;
            }
            if self.cur_ts.is_some() || self.fu_active {
                self.cur_corrupt = true;
            }
            self.lost = true;
            self.reset_fu();
            // Pick the earliest pending sequence number in modular 16-bit order.
            let mut earliest = self.reorder[0].seq;
            for p in &self.reorder[1..] {
                if seq_lt(p.seq, earliest) {
                    earliest = p.seq;
                }
            }
            self.next_seq = Some(earliest);
            self.drain_in_order(out);
        }
    }

    fn process_packet(
        &mut self,
        ts: u32,
        marker: bool,
        payload: &[u8],
        out: &mut Vec<AssembledFrame>,
    ) {
        if self.cur_ts.is_some_and(|prev| prev != ts) {
            self.finish_frame(out);
        }
        if self.cur_ts.is_none() {
            self.cur_ts = Some(ts);
        }
        if payload.len() >= 2 {
            match (payload[0] >> 1) & 0x3f {
                48 => self.process_ap(payload),
                49 => self.process_fu(payload),
                _ => {
                    if self.fu_active {
                        self.reset_fu();
                        self.cur_corrupt = true;
                        self.lost = true;
                    }
                    self.emit_nal(payload);
                }
            }
        }
        if marker {
            self.finish_frame(out);
        }
    }

    fn process_ap(&mut self, payload: &[u8]) {
        if self.fu_active {
            self.reset_fu();
            self.cur_corrupt = true;
            self.lost = true;
        }
        let mut off = 2;
        while off + 2 <= payload.len() {
            let size = u16::from_be_bytes([payload[off], payload[off + 1]]) as usize;
            off += 2;
            let Some(nal) = payload.get(off..off + size) else {
                self.cur_corrupt = true;
                self.lost = true;
                break;
            };
            self.emit_nal(nal);
            off += size;
        }
    }

    fn process_fu(&mut self, payload: &[u8]) {
        if payload.len() < 3 {
            self.cur_corrupt = true;
            self.lost = true;
            return;
        }
        let fu_header = payload[2];
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        let fu_type = fu_header & 0x3f;
        let frag = &payload[3..];
        if start {
            if self.fu_active {
                self.cur_corrupt = true;
                self.lost = true;
            }
            let b0 = (payload[0] & 0x81) | (fu_type << 1);
            let b1 = payload[1];
            self.fu_buf.clear();
            self.fu_buf.push(b0);
            self.fu_buf.push(b1);
            self.fu_buf.extend_from_slice(frag);
            self.fu_active = true;
        } else if self.fu_active {
            self.fu_buf.extend_from_slice(frag);
        } else {
            self.cur_corrupt = true;
            self.lost = true;
            return;
        }
        if end && self.fu_active {
            let nal = std::mem::take(&mut self.fu_buf);
            self.fu_active = false;
            self.emit_nal(&nal);
        }
    }

    fn emit_nal(&mut self, nal: &[u8]) {
        if nal.len() < 2 {
            return;
        }
        let t = (nal[0] >> 1) & 0x3f;
        if t == 35 {
            return; // AU delimiter: frames are already split per access unit
        }
        match t {
            32 => {
                self.vps = Some(nal.to_vec());
                self.cur_has_ps[0] = true;
            }
            33 => {
                self.sps = Some(nal.to_vec());
                self.cur_has_ps[1] = true;
            }
            34 => {
                self.pps = Some(nal.to_vec());
                self.cur_has_ps[2] = true;
            }
            0..=31 => {
                self.cur_has_vcl = true;
                if (16..=23).contains(&t) {
                    self.cur_is_key = true;
                }
            }
            _ => {}
        }
        if self.cur_nals.len() + ANNEXB_START.len() + nal.len() > MAX_FRAME_BYTES {
            self.cur_corrupt = true;
            self.lost = true;
            self.cur_nals.clear();
            return;
        }
        self.cur_nals.extend_from_slice(&ANNEXB_START);
        self.cur_nals.extend_from_slice(nal);
    }

    fn finish_frame(&mut self, out: &mut Vec<AssembledFrame>) {
        if self.fu_active {
            self.reset_fu();
            self.cur_corrupt = true;
            self.lost = true;
        }
        let ts = self.cur_ts.take();
        let mut data = std::mem::take(&mut self.cur_nals);
        let has_vcl = std::mem::take(&mut self.cur_has_vcl);
        let is_key = std::mem::take(&mut self.cur_is_key);
        let has_ps = std::mem::take(&mut self.cur_has_ps);
        let corrupt = std::mem::take(&mut self.cur_corrupt);
        if corrupt || !has_vcl {
            return;
        }
        if is_key && has_ps != [true; 3] {
            if let (Some(vps), Some(sps), Some(pps)) = (&self.vps, &self.sps, &self.pps) {
                let mut prefix = Vec::with_capacity(
                    data.len() + vps.len() + sps.len() + pps.len() + 12,
                );
                for (present, ps) in [
                    (has_ps[0], vps.as_slice()),
                    (has_ps[1], sps.as_slice()),
                    (has_ps[2], pps.as_slice()),
                ] {
                    if !present {
                        prefix.extend_from_slice(&ANNEXB_START);
                        prefix.extend_from_slice(ps);
                    }
                }
                prefix.extend_from_slice(&data);
                data = prefix;
            }
        }
        out.push(AssembledFrame {
            ts: ts.unwrap_or(0),
            key: is_key,
            annexb: data,
        });
    }
}

/// The sound's AudioSpecificConfig: AAC-ELD, 48 kHz stereo, 480-sample frames (pymobiledevice3's
/// AAC_ELD_ASC_48K_STEREO_480 with frameLengthFlag set, as ZenLink 2.5.x decoded it).
pub const AUDIO_ASC: [u8; 4] = [0xF8, 0xE6, 0x50, 0x00];

/// Where receiver reports for the sound go: the device's RTCP port, our SSRC, the device's SSRC.
/// Without them the device ends the sound session after ~20 s (RTCPTimeoutInterval).
#[derive(Clone, Copy, Debug)]
struct Rtcp {
    port: u16,
    ours: u32,
    theirs: u32,
}

/// `streamConfig` of a startmediastream answer. Its SSRC names are the device's view:
/// LocalSSRC is the device's, RemoteSSRC ours.
fn rtcp_of(answer: &plist::Value) -> Option<Rtcp> {
    fn find<'a>(v: &'a plist::Value, key: &str) -> Option<&'a plist::Value> {
        match v {
            plist::Value::Dictionary(d) => {
                d.get(key).or_else(|| d.values().find_map(|x| find(x, key)))
            }
            plist::Value::Array(a) => a.iter().find_map(|x| find(x, key)),
            _ => None,
        }
    }
    let cfg = find(answer, "streamConfig")?;
    let num = |k: &str| {
        let v = find(cfg, k)?;
        v.as_unsigned_integer()
            .or_else(|| v.as_signed_integer().map(|i| i as u64))
    };
    Some(Rtcp {
        port: num("SourcePort")? as u16,
        ours: num("RemoteSSRC")? as u32,
        theirs: num("LocalSSRC")? as u32,
    })
}

/// The extended highest sequence number (RFC 3550) after `seq`.
fn extend_seq(highest: u32, seq: u16) -> u32 {
    if highest == 0 {
        return u32::from(seq);
    }
    let mut cycles = highest >> 16;
    let last = (highest & 0xFFFF) as u16;
    if seq < last && last - seq > 0x8000 {
        cycles = cycles.wrapping_add(1);
    }
    let ext = (cycles << 16) | u32::from(seq);
    if ext.wrapping_sub(highest) < 0x8000_0000 {
        ext
    } else {
        highest
    }
}

/// The device refuses screen streaming during a call (CoreDevice 9022). The app translates it.
pub const IN_CALL: &str =
    "The iPhone or iPad is in a phone or video call. End the call, then try again.";

/// A short reason for a CoreDevice error, instead of its full dump (kilobytes of plist bytes).
fn device_error(dump: &str) -> String {
    if dump.contains("Integer(9022)") || dump.contains("call is currently in progress") {
        return IN_CALL.into();
    }
    if let Some(i) = dump.find("NSLocalizedDescription") {
        if let Some(text) = dump[i..]
            .split("String(\\\"")
            .nth(1)
            .and_then(|t| t.split("\\\"").next())
        {
            return text.into();
        }
    }
    dump.chars().take(200).collect()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string().to_uppercase()
}

fn wifi_bitrate() -> u32 {
    std::env::var("ZL_WIFI_BITRATE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.clamp(500_000, 20_000_000))
        .unwrap_or(WIFI_BITRATE)
}

fn ntp_now() -> u64 {
    const UNIX_TO_NTP: u64 = 2_208_988_800;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + UNIX_TO_NTP;
    let frac = (u64::from(now.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

fn bitrate_param(value: u32) -> Vec<u8> {
    fn varint(o: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            o.push((v as u8) | 0x80);
            v >>= 7;
        }
        o.push(v as u8);
    }
    let mut m = Vec::new();
    for (field, v) in [(1u64, 0u64), (2, u64::from(value)), (3, 0x20000)] {
        varint(&mut m, field << 3);
        varint(&mut m, v);
    }
    let mut o = Vec::new();
    varint(&mut o, (9 << 3) | 2);
    varint(&mut o, m.len() as u64);
    o.extend(m);
    o
}

fn patch_media_blob_bitrate(blob: &[u8], max_bitrate: u32) -> Option<Vec<u8>> {
    let old = bitrate_param(FULL_BITRATE);
    let at = blob.windows(old.len()).position(|w| w == old.as_slice())?;
    let mut out = blob[..at].to_vec();
    out.extend(bitrate_param(max_bitrate));
    out.extend_from_slice(&blob[at + old.len()..]);
    Some(out)
}

/// Wraps `data` in RFC 1950 zlib stored blocks so no extra deflate dependency is needed.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = data.chunks(0xffff).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(!n).to_le_bytes());
        z.extend_from_slice(c);
    }
    if chunks.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

fn build_video_offer(
    call_id: &str,
    call_info: &CallInfoBlob,
    ssrc: u32,
    max_bitrate: u32,
) -> Result<Vec<u8>, String> {
    let base = build_screen_video_offer(call_id, call_info, ssrc)
        .map_err(|e| format!("video offer: {e:?}"))?;
    if max_bitrate == FULL_BITRATE {
        return Ok(base);
    }
    let blob = build_screen_media_blob(ssrc, ntp_now());
    let Some(patched_blob) = patch_media_blob_bitrate(&blob, max_bitrate) else {
        log::warn!("screen: bitrate cap parameter not found in offer; using default");
        return Ok(base);
    };
    let Ok(mut dict) = plist::from_bytes::<plist::Dictionary>(&base) else {
        log::warn!("screen: offer plist parse failed; using default");
        return Ok(base);
    };
    dict.insert(
        MEDIA_BLOB_KEY.into(),
        plist::Value::Data(zlib_stored(&patched_blob)),
    );
    let mut out = Vec::new();
    if let Err(e) = plist::to_writer_binary(&mut out, &plist::Value::Dictionary(dict)) {
        log::warn!("screen: offer plist encode failed ({e}); using default");
        return Ok(base);
    }
    Ok(out)
}

fn lsr_dlsr(last_sr: Option<(u32, Instant)>) -> (u32, u32) {
    match last_sr {
        Some((ntp, at)) => {
            let dlsr = (at.elapsed().as_secs_f64() * 65536.0).min(u32::MAX as f64) as u32;
            (ntp, dlsr)
        }
        None => (0, 0),
    }
}

/// Streams until `on_frame` returns `false` or the stream ends. Each call of `on_frame(au, rebuild)`
/// gets one access unit in Annex B (the first ones carry VPS/SPS/PPS; `rebuild` is `true` on a
/// keyframe recovering from packet loss or a stream stall); each call of `on_audio` gets one
/// AAC-ELD access unit (see [`AUDIO_ASC`]). While `key` is set, keyframe requests (RTCP PLI + FIR)
/// go to the device until the caller clears it on keyframe arrival.
/// `gate_only`: iOS 17-26 accept the stream but send no picture; it is held open only so the
/// HID services take input (Hybrid), so silence does not end it.
pub async fn stream(
    link: &mut Link,
    key: &AtomicBool,
    gate_only: bool,
    mut on_frame: impl FnMut(Vec<u8>, bool) -> bool,
    mut on_audio: impl FnMut(&[u8]),
) -> Result<(), String> {
    let mut client = DisplayServiceClient::connect_rsd(&mut link.adapter, &mut link.rsd)
        .await
        .map_err(|e| format!("display service: {e:?}"))?;
    let udp = |e| format!("udp: {e:?}");
    let audio_udp = link.adapter.bind_udp(0).await.map_err(udp)?;
    let video_udp = link.adapter.bind_udp(0).await.map_err(udp)?;
    let host = link.adapter.host_ip().to_string();
    let peer = link.adapter.peer_ip().to_string();
    let call_info = CallInfoBlob {
        call_id: 0,
        client_version: 1,
        device_type: "Mac17,7".into(),
        framework_version: "2205.3.1".into(),
        os_version: "25F71".into(),
        device_name: None,
        audio_device_uid: None,
    };
    let session = uuid::Uuid::new_v4();
    let audio_offer = build_screen_audio_offer(&new_id(), &call_info)
        .map_err(|e| format!("audio offer: {e:?}"))?;
    // The device refuses the sound during a phone or VoIP call (CoreDevice 9022): mirror
    // without sound instead of failing.
    let rtcp = match client
        .start_media_stream(build_start_audio_parameters(
            &host,
            audio_udp.local_port(),
            &peer,
            50000,
            audio_offer,
            CLIENT_SUPPORTED_FEATURES,
            session,
        ))
        .await
    {
        Ok(answer) => {
            let rtcp = rtcp_of(&answer);
            if rtcp.is_none() {
                log::warn!(
                    "sound: no RTCP target in the answer; the device may stop the sound after 20 s"
                );
            }
            rtcp
        }
        Err(e) => {
            log::warn!(
                "sound: not started, mirroring without sound: {}",
                device_error(&format!("{e:?}"))
            );
            None
        }
    };
    let our_video_ssrc = uuid::Uuid::new_v4().as_u128() as u32;
    let max_bitrate = if link.wifi {
        wifi_bitrate()
    } else {
        FULL_BITRATE
    };
    if link.wifi {
        log::info!(
            "screen: Wi-Fi video bitrate cap {:.1} Mbps",
            max_bitrate as f64 / 1e6
        );
    }
    let video_offer = build_video_offer(&new_id(), &call_info, our_video_ssrc, max_bitrate)?;
    let video_answer = client
        .start_media_stream(build_start_video_parameters(
            &host,
            video_udp.local_port(),
            &peer,
            50001,
            video_offer,
            CLIENT_SUPPORTED_FEATURES,
            1,
            session,
        ))
        .await
        .map_err(|e| match device_error(&format!("{e:?}")) {
            m if m == IN_CALL => m,
            m => format!("start video: {m}"),
        })?;
    let video_rtcp = rtcp_of(&video_answer);
    if video_rtcp.is_none() {
        log::warn!("screen: no RTCP target in the answer; keyframes cannot be requested");
    }

    let mut assembler = HevcAssembler::new();
    let mut picker = Picker::default();
    let mut frame_at = Instant::now();
    let mut video_at = Instant::now();
    let started_at = Instant::now();
    let mut audio_seq = 0u32;
    let mut video_seq = 0u32;
    let mut video_first_seq: Option<u16> = None;
    let mut video_last_seq = 0u16;
    let mut video_frames = 0u16;
    let mut video_peer_port: Option<u16> = video_rtcp.map(|r| r.port);
    let video_ours = video_rtcp.map(|r| r.ours).unwrap_or(our_video_ssrc);
    let mut last_video_sr: Option<(u32, Instant)> = None;
    let mut need_keyframe = false;
    let mut need_rebuild = false;
    let mut last_key_req: Option<Instant> = None;
    let mut fir_seq = 0u8;

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rctl_tick = tokio::time::interval(Duration::from_millis(50));
    rctl_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let send_keyframe_req = |port: u16,
                             media: u32,
                             highest_seq: u32,
                             last_sr: Option<(u32, Instant)>,
                             fir: &mut u8| {
        let (lsr, dlsr) = lsr_dlsr(last_sr);
        let block = ReportBlock {
            source_ssrc: media,
            highest_seq,
            lsr,
            dlsr,
            ..ReportBlock::default()
        };
        *fir = fir.wrapping_add(1);
        let req = build_keyframe_request(video_ours, "zenlink", media, &[block], *fir);
        video_udp.send_to(port, req)
    };

    let result = loop {
        tokio::select! {
            dg = video_udp.recv() => {
                let dg = match dg {
                    Ok(dg) => dg,
                    Err(e) => break Err(format!("video: {e:?}")),
                };
                video_at = Instant::now();
                if is_rtcp(&dg.data) {
                    if let Some(sr) = SenderReport::parse_first(&dg.data) {
                        last_video_sr = Some((sr.ntp_middle, Instant::now()));
                    }
                    continue;
                }
                let Some(pkt) = RtpPacket::parse(&dg.data) else {
                    continue;
                };
                let held = (pkt.sequence_number, pkt.timestamp, pkt.payload.to_vec(), pkt.marker);
                let Some((fresh, packets)) = picker.push(pkt.ssrc, held) else {
                    continue;
                };
                video_peer_port = Some(dg.source_port);
                if fresh {
                    log::info!("screen: RTP stream {:08x}", pkt.ssrc);
                    assembler.reset_all();
                    video_first_seq = None;
                    video_seq = 0;
                    need_keyframe = false;
                    need_rebuild = false;
                }
                let now = Instant::now();
                let mut done = false;
                for (seq, ts, payload, marker) in packets {
                    video_seq = extend_seq(video_seq, seq);
                    video_first_seq.get_or_insert(seq);
                    video_last_seq = seq;
                    let frames = assembler.push(seq, ts, marker, payload, now);
                    if assembler.take_lost() {
                        if !need_keyframe {
                            log::info!(
                                "screen: RTP gap/loss (ssrc {:08x}, seq {}); requesting keyframe",
                                pkt.ssrc,
                                seq
                            );
                        }
                        need_keyframe = true;
                        need_rebuild = true;
                        key.store(true, Relaxed);
                    }
                    for f in frames {
                        video_frames = video_frames.wrapping_add(1);
                        if let Some(port) = video_peer_port {
                            let _ = video_udp
                                .send_to(port, build_frame_ack(video_ours, f.ts))
                                .await;
                        }
                        if need_keyframe {
                            if !f.key {
                                continue;
                            }
                            need_keyframe = false;
                        }
                        let rebuild = f.key && std::mem::take(&mut need_rebuild);
                        frame_at = Instant::now();
                        if !on_frame(f.annexb, rebuild) {
                            done = true;
                            break;
                        }
                    }
                    if done {
                        break;
                    }
                }
                if done {
                    break Ok(());
                }
                if key.load(Relaxed)
                    && last_key_req.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY)
                {
                    if let (Some(port), Some(media)) = (
                        video_peer_port,
                        picker.main.or_else(|| video_rtcp.map(|r| r.theirs)),
                    ) {
                        last_key_req = Some(Instant::now());
                        let _ = send_keyframe_req(
                            port,
                            media,
                            video_seq,
                            last_video_sr,
                            &mut fir_seq,
                        )
                        .await;
                    }
                }
            }
            dg = audio_udp.recv() => {
                let Ok(dg) = dg else { continue };
                if is_rtcp(&dg.data) {
                    continue;
                }
                if let Some(pkt) = RtpPacket::parse(&dg.data) {
                    audio_seq = extend_seq(audio_seq, pkt.sequence_number);
                    if !pkt.payload.is_empty() {
                        on_audio(pkt.payload);
                    }
                }
            }
            _ = rctl_tick.tick() => {
                let frames = assembler.flush_stale(Instant::now());
                if assembler.take_lost() {
                    if !need_keyframe {
                        log::info!(
                            "screen: RTP gap timed out (ssrc {:08x}, seq {}); requesting keyframe",
                            picker.main.unwrap_or(0),
                            video_last_seq
                        );
                    }
                    need_keyframe = true;
                    need_rebuild = true;
                    key.store(true, Relaxed);
                }
                let mut done = false;
                for f in frames {
                    video_frames = video_frames.wrapping_add(1);
                    if let Some(port) = video_peer_port {
                        let _ = video_udp
                            .send_to(port, build_frame_ack(video_ours, f.ts))
                            .await;
                    }
                    if need_keyframe {
                        if !f.key {
                            continue;
                        }
                        need_keyframe = false;
                    }
                    let rebuild = f.key && std::mem::take(&mut need_rebuild);
                    frame_at = Instant::now();
                    if !on_frame(f.annexb, rebuild) {
                        done = true;
                        break;
                    }
                }
                if done {
                    break Ok(());
                }
                if let (Some(port), Some(first)) = (video_peer_port, video_first_seq) {
                    let clock = (started_at.elapsed().as_millis() & 0xffff) as u16;
                    let rel = video_last_seq.wrapping_sub(first);
                    let _ = video_udp
                        .send_to(port, build_rctl(video_ours, clock, video_frames, rel))
                        .await;
                }
                if key.load(Relaxed)
                    && last_key_req.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY)
                {
                    if let (Some(port), Some(media)) = (
                        video_peer_port,
                        picker.main.or_else(|| video_rtcp.map(|r| r.theirs)),
                    ) {
                        last_key_req = Some(Instant::now());
                        let _ = send_keyframe_req(
                            port,
                            media,
                            video_seq,
                            last_video_sr,
                            &mut fir_seq,
                        )
                        .await;
                    }
                }
            }
            _ = tick.tick() => {
                if !gate_only && video_at.elapsed() > SILENCE {
                    break Err("the device stopped sending the screen".into());
                }
                if let Some(r) = rtcp {
                    let block = ReportBlock {
                        source_ssrc: r.theirs,
                        highest_seq: audio_seq,
                        ..ReportBlock::default()
                    };
                    let mut rr = build_receiver_report(r.ours, &[block]);
                    rr.extend(build_sdes(r.ours, "zenlink"));
                    let _ = audio_udp.send_to(r.port, rr).await;
                }
                if let (Some(port), Some(media)) = (
                    video_peer_port,
                    picker.main.or_else(|| video_rtcp.map(|r| r.theirs)),
                ) {
                    let (lsr, dlsr) = lsr_dlsr(last_video_sr);
                    let block = ReportBlock {
                        source_ssrc: media,
                        highest_seq: video_seq,
                        lsr,
                        dlsr,
                        ..ReportBlock::default()
                    };
                    let _ = video_udp
                        .send_to(port, build_liveness(video_ours, "zenlink", &[block]))
                        .await;
                }
                if !gate_only && picker.main.is_some() && frame_at.elapsed() > Duration::from_secs(3) {
                    frame_at = Instant::now();
                    assembler.reset_sequence();
                    need_keyframe = true;
                    need_rebuild = true;
                    key.store(true, Relaxed);
                    log::info!(
                        "screen: no frame for 3 s (ssrc {:08x}, seq {}); requesting keyframe",
                        picker.main.unwrap_or(0),
                        video_last_seq
                    );
                    if let (Some(port), Some(media)) = (
                        video_peer_port,
                        picker.main.or_else(|| video_rtcp.map(|r| r.theirs)),
                    ) {
                        last_key_req = Some(Instant::now());
                        let _ = send_keyframe_req(
                            port,
                            media,
                            video_seq,
                            last_video_sr,
                            &mut fir_seq,
                        )
                        .await;
                    }
                }
            }
        }
    };
    // The link may be dead already (cable out, Wi-Fi gone): never wait on it for long.
    let _ = tokio::time::timeout(Duration::from_secs(3), client.stop_media_stream()).await;
    drop(audio_udp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::core_device::MediaNegotiationBlob;

    #[test]
    fn device_errors_are_short() {
        let call = r#"CoreDevice(DeviceError("Dictionary({\"code\": Integer(9022), \"userInfo\": Dictionary({\"NSLocalizedDescription\": String(\"A phone or VoIP call is currently in progress on the device.\")})})"))"#;
        assert_eq!(device_error(call), IN_CALL);
        let other = r#"Dictionary({\"code\": Integer(1), \"userInfo\": Dictionary({\"NSLocalizedDescription\": String(\"Busy.\")})})"#;
        assert_eq!(device_error(other), "Busy.");
        assert_eq!(device_error(&"x".repeat(500)).len(), 200);
    }

    fn p(seq: u16) -> Held {
        (seq, 0, vec![], false)
    }

    #[test]
    fn extended_sequence_numbers() {
        assert_eq!(extend_seq(0, 5), 5);
        assert_eq!(extend_seq(5, 6), 6);
        assert_eq!(extend_seq(6, 4), 6); // late packet
        assert_eq!(extend_seq(65535, 2), 65536 + 2); // wraps into the next cycle
    }

    #[test]
    fn rtcp_target_from_the_answer() {
        let mut cfg = plist::Dictionary::new();
        cfg.insert("SourcePort".into(), 50000u64.into());
        cfg.insert("LocalSSRC".into(), 7u64.into());
        cfg.insert("RemoteSSRC".into(), 9u64.into());
        let mut conn = plist::Dictionary::new();
        conn.insert("streamConfig".into(), plist::Value::Dictionary(cfg));
        let mut top = plist::Dictionary::new();
        top.insert("connection".into(), plist::Value::Dictionary(conn));
        let r = rtcp_of(&plist::Value::Dictionary(top)).unwrap();
        assert_eq!((r.port, r.ours, r.theirs), (50000, 9, 7));
        assert!(rtcp_of(&plist::Value::Boolean(true)).is_none());
    }

    #[test]
    fn lone_packets_of_other_ssrcs_are_ignored() {
        let mut k = Picker::default();
        assert!(k.push(7, p(100)).is_none()); // held, not lost
        assert!(k.push(50, p(12)).is_none()); // the lone packet
        let (fresh, got) = k.push(7, p(101)).unwrap();
        assert!(fresh);
        assert_eq!(got.iter().map(|h| h.0).collect::<Vec<_>>(), [100, 101]);
        assert_eq!(k.push(7, p(102)).unwrap(), (false, vec![p(102)]));
        for i in 0..20 {
            assert!(k.push(60 + i, p(12)).is_none()); // one per second, new SSRC each
        }
        assert!(k.held.len() <= 9);
        // A real new stream takes over after SWITCH_AFTER packets in a row.
        for s in 0..SWITCH_AFTER as u16 - 1 {
            assert!(k.push(9, p(s)).is_none());
        }
        let (fresh, got) = k.push(9, p(99)).unwrap();
        assert!(fresh && got.len() == SWITCH_AFTER);
    }

    fn nal(t: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![t << 1, 1];
        v.extend_from_slice(body);
        v
    }

    fn fu_start(t: u8, frag: &[u8]) -> Vec<u8> {
        let mut v = vec![49 << 1, 1, 0x80 | (t & 0x3f)];
        v.extend_from_slice(frag);
        v
    }

    fn fu_end(t: u8, frag: &[u8]) -> Vec<u8> {
        let mut v = vec![49 << 1, 1, 0x40 | (t & 0x3f)];
        v.extend_from_slice(frag);
        v
    }

    #[test]
    fn hevc_assembler_skips_gap_after_max_reorder_and_signals_loss() {
        let mut a = HevcAssembler::new();
        let t0 = Instant::now();
        // Initial keyframe with VPS/SPS/PPS.
        assert!(a.push(1, 100, false, nal(32, &[1]), t0).is_empty());
        assert!(a.push(2, 100, false, nal(33, &[2]), t0).is_empty());
        assert!(a.push(3, 100, false, nal(34, &[3]), t0).is_empty());
        let f1 = a.push(4, 100, true, nal(19, &[4]), t0);
        assert_eq!(f1.len(), 1);
        assert!(f1[0].key);
        assert!(!a.take_lost());

        // Frame 2 starts with seq 5 (FU start), seq 6 (FU end) is LOST, then seq 7..=15 arrive.
        assert!(a.push(5, 200, false, fu_start(1, &[10]), t0).is_empty());
        let mut emitted = Vec::new();
        for s in 7..=14u16 {
            emitted.extend(a.push(s, 300 + u32::from(s) * 10, true, nal(1, &[s as u8]), t0));
        }
        // seq 15..=16 form a fragmented bare recovery IDR (FU start + FU end).
        emitted.extend(a.push(15, 500, false, fu_start(19, &[99, 1]), t0));
        emitted.extend(a.push(16, 500, true, fu_end(19, &[2, 3]), t0));
        assert!(a.take_lost());
        // Corrupted frame 2 must not be emitted; subsequent complete frames + bare IDR are.
        assert!(emitted.iter().all(|f| f.ts != 200));
        let idr = emitted.last().unwrap();
        assert!(idr.key);
        // Cached VPS/SPS/PPS were prepended before the bare recovery IDR.
        assert!(idr
            .annexb
            .windows(6)
            .any(|w| w == [0, 0, 0, 1, 32 << 1, 1]));
        assert!(idr
            .annexb
            .windows(6)
            .any(|w| w == [0, 0, 0, 1, 33 << 1, 1]));
        assert!(idr
            .annexb
            .windows(6)
            .any(|w| w == [0, 0, 0, 1, 34 << 1, 1]));
    }

    #[test]
    fn hevc_assembler_skips_gap_on_timeout() {
        let mut a = HevcAssembler::new();
        let t0 = Instant::now();
        assert_eq!(a.push(10, 100, true, nal(19, &[1]), t0).len(), 1);
        // seq 11 lost; seq 12 and 13 arrive (below MAX_REORDER).
        assert!(a.push(12, 200, true, nal(1, &[2]), t0).is_empty());
        assert!(a.push(13, 300, true, nal(1, &[3]), t0).is_empty());
        assert!(a.flush_stale(t0 + Duration::from_millis(20)).is_empty());
        let out = a.flush_stale(t0 + Duration::from_millis(70));
        assert!(a.take_lost());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ts, 200);
        assert_eq!(out[1].ts, 300);
    }

    #[test]
    fn hevc_assembler_handles_seq_wraparound() {
        let mut a = HevcAssembler::new();
        let t0 = Instant::now();
        assert_eq!(a.push(65534, 100, true, nal(19, &[1]), t0).len(), 1);
        // 65535 dropped; 0..=8 arrive across the wrap boundary.
        let mut emitted = Vec::new();
        for i in 0..=8u16 {
            emitted.extend(a.push(i, 200 + u32::from(i), true, nal(1, &[i as u8]), t0));
        }
        assert!(a.take_lost());
        assert_eq!(emitted.len(), 9);
        assert_eq!(emitted[0].ts, 200);
        assert_eq!(emitted[8].ts, 208);
        // Late packet from previous cycle is ignored, not accepted.
        assert!(a.push(65535, 150, true, nal(1, &[9]), t0).is_empty());
    }

    #[test]
    fn hevc_assembler_splits_on_timestamp_when_marker_lost() {
        let mut a = HevcAssembler::new();
        let t0 = Instant::now();
        // Frame 1 has marker=false (simulating lost end-of-frame marker packet).
        assert!(a.push(1, 100, false, nal(19, &[1]), t0).is_empty());
        // Frame 2 arrives with new timestamp and marker=true -> must emit both separately.
        let out = a.push(2, 200, true, nal(1, &[2]), t0);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].ts, out[0].key), (100, true));
        assert_eq!((out[1].ts, out[1].key), (200, false));
    }

    #[test]
    fn wifi_video_offer_overrides_bitrate_and_parses() {
        let info = CallInfoBlob {
            device_type: "Mac17,7".into(),
            ..Default::default()
        };
        let offer = build_video_offer("TEST-CALL", &info, 0x1234_5678, WIFI_BITRATE).unwrap();
        let dict: plist::Dictionary = plist::from_bytes(&offer).unwrap();
        let z = dict[MEDIA_BLOB_KEY].as_data().unwrap();
        assert!(MediaNegotiationBlob::from_negotiator_offer(z).is_ok());
        assert!(z.len() > 11);
        let z_payload = &z[7..z.len() - 4];
        assert!(z_payload
            .windows(bitrate_param(WIFI_BITRATE).len())
            .any(|w| w == bitrate_param(WIFI_BITRATE)));
        assert!(!z_payload
            .windows(bitrate_param(FULL_BITRATE).len())
            .any(|w| w == bitrate_param(FULL_BITRATE)));
        let raw_default = build_screen_media_blob(0x1234_5678, 1);
        let patched = patch_media_blob_bitrate(&raw_default, WIFI_BITRATE).unwrap();
        assert!(patched
            .windows(bitrate_param(WIFI_BITRATE).len())
            .any(|w| w == bitrate_param(WIFI_BITRATE)));
        assert!(!patched
            .windows(bitrate_param(FULL_BITRATE).len())
            .any(|w| w == bitrate_param(FULL_BITRATE)));
    }
}
