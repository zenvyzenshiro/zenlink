//! The screen over CoreDevice's display service: HEVC over RTP through the link, handed out
//! as Annex B access units. The offer follows idevice's tools/src/coredevice_stream.rs
//! (MIT, Jackson Coxson) and what Device Hub sends.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use idevice::core_device::{
    build_keyframe_request, build_receiver_report, build_screen_audio_offer,
    build_screen_video_offer, build_sdes, build_start_audio_parameters,
    build_start_video_parameters, is_rtcp, CallInfoBlob, DisplayServiceClient, HevcDepacketizer,
    ReportBlock, RtpPacket,
};
use idevice::RsdService as _;

use crate::link::Link;

/// What the controller supports (Device Hub sends 140), not the device's mask.
const CLIENT_SUPPORTED_FEATURES: u64 = 140;
/// No packet for this long: the device stopped sending (locked, cable out, Wi-Fi gone).
const SILENCE: Duration = Duration::from_secs(8);

/// One RTP packet: sequence number, timestamp, payload, marker.
type Held = (u16, u32, Vec<u8>, bool);

/// Packets in a row a new SSRC needs before it replaces the running stream.
const SWITCH_AFTER: usize = 30;

/// Picks the screen's RTP stream by SSRC. The video port also gets a lone packet about once a
/// second from another SSRC (seq 12, a new SSRC each time; seen on iPadOS 27). Fed to idevice's
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

/// Streams until `on_frame` returns `false` or the stream ends. Each call of `on_frame` gets
/// one access unit in Annex B (the first ones carry VPS/SPS/PPS); each call of `on_audio` one
/// AAC-ELD access unit (see [`AUDIO_ASC`]). While `key` is set, a keyframe request (RTCP PLI +
/// FIR) goes to the device every 200 ms; the caller clears it when a keyframe arrives.
/// `gate_only`: iOS 17-26 accept the stream but send no picture; it is held open only so the
/// HID services take input (Hybrid), so silence does not end it.
pub async fn stream(
    link: &mut Link,
    key: &AtomicBool,
    gate_only: bool,
    mut on_frame: impl FnMut(Vec<u8>) -> bool,
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
    let video_offer =
        build_screen_video_offer(&new_id(), &call_info, uuid::Uuid::new_v4().as_u128() as u32)
            .map_err(|e| format!("video offer: {e:?}"))?;
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

    let mut depack = HevcDepacketizer::new();
    let mut au = Vec::new();
    let mut picker = Picker::default();
    let mut frame_at = std::time::Instant::now();
    let mut video_at = std::time::Instant::now();
    let mut audio_seq = 0u32;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut ask = tokio::time::interval(Duration::from_millis(200));
    let mut fir_seq = 0u8;
    let mut video_seq = 0u32;
    let result = loop {
        tokio::select! {
            dg = video_udp.recv() => {
                let dg = match dg {
                    Ok(dg) => dg,
                    Err(e) => break Err(format!("video: {e:?}")),
                };
                video_at = std::time::Instant::now();
                if is_rtcp(&dg.data) {
                    continue; // the device's sender reports
                }
                let Some(pkt) = RtpPacket::parse(&dg.data) else {
                    continue;
                };
                if picker.main == Some(pkt.ssrc) {
                    video_seq = extend_seq(video_seq, pkt.sequence_number);
                }
                let held = (pkt.sequence_number, pkt.timestamp, pkt.payload.to_vec(), pkt.marker);
                let Some((fresh, packets)) = picker.push(pkt.ssrc, held) else {
                    continue;
                };
                if fresh {
                    log::info!("screen: RTP stream {:08x}", pkt.ssrc);
                    depack = HevcDepacketizer::new();
                    au.clear();
                }
                let mut done = false;
                for (seq, ts, payload, marker) in packets {
                    depack.push(seq, ts, &payload);
                    au.extend_from_slice(&depack.take_output());
                    if marker && !au.is_empty() {
                        frame_at = std::time::Instant::now();
                        if !on_frame(std::mem::take(&mut au)) {
                            done = true;
                            break;
                        }
                    }
                }
                if done {
                    break Ok(());
                }
                if frame_at.elapsed() > Duration::from_secs(3) {
                    // Packets but no picture: say what arrives, once per stall.
                    frame_at = std::time::Instant::now();
                    log::info!(
                        "screen: packets but no frame for 3 s (ssrc {:08x}, seq {}, pt {})",
                        pkt.ssrc,
                        pkt.sequence_number,
                        pkt.payload_type
                    );
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
            _ = ask.tick() => {
                if let (true, Some(r)) = (key.load(Relaxed), video_rtcp) {
                    let media = picker.main.unwrap_or(r.theirs);
                    let block = ReportBlock {
                        source_ssrc: media,
                        highest_seq: video_seq,
                        ..ReportBlock::default()
                    };
                    fir_seq = fir_seq.wrapping_add(1);
                    let req = build_keyframe_request(r.ours, "zenlink", media, &[block], fir_seq);
                    let _ = video_udp.send_to(r.port, req).await;
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
}
