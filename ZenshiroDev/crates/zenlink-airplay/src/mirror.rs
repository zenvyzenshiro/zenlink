//! The mirror stream: 128-byte headers + payloads on its own TCP connection.
//! Type 1 carries the decoder configuration (avcC, or an hvc1 sample entry) and the picture
//! size; type 0 an AES-CTR encrypted access unit of 4-byte length-prefixed NAL units;
//! types 2 and 5 are heartbeats and reports.

use std::time::Instant;

use aes::cipher::StreamCipher;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{ctr, Ctr, Sink, State};

/// A decrypted access unit is a chain of length-prefixed NAL units ending exactly at the end.
pub fn valid_au(data: &[u8]) -> bool {
    let n = data.len();
    if n < 5 {
        return false;
    }
    let mut pos = 0;
    while pos + 4 <= n {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        if size == 0 || pos + 4 + size > n || data[pos + 4] & 0x80 != 0 {
            return false;
        }
        pos += 4 + size;
    }
    pos == n
}

/// AES-CTR over the whole connection, with a guard: a frame that does not decrypt to valid NAL
/// units is retried with a fresh keystream (iOS restarted it); garbage never reaches the decoder.
pub struct Decryptor {
    key: [u8; 16],
    iv: [u8; 16],
    cipher: Ctr,
    pub bad: u32,
}

impl Decryptor {
    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self {
            key,
            iv,
            cipher: ctr(&key, &iv),
            bad: 0,
        }
    }

    pub fn decrypt(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        let mut data = payload.to_vec();
        self.cipher.apply_keystream(&mut data);
        if valid_au(&data) {
            return Some(data);
        }
        let mut fresh = ctr(&self.key, &self.iv);
        let mut retry = payload.to_vec();
        fresh.apply_keystream(&mut retry);
        if valid_au(&retry) {
            log::warn!("airplay: video keystream restarted by the device; re-synchronised");
            self.cipher = fresh;
            return Some(retry);
        }
        self.bad += 1;
        if matches!(self.bad, 1 | 30 | 300) {
            log::warn!(
                "airplay: {} video frame(s) didn't decrypt to valid data; skipped",
                self.bad
            );
        }
        None
    }
}

/// The hvcC record inside an hvc1 sample entry (the decoder's description).
pub fn hvcc_from_sample_entry(entry: &[u8]) -> Option<Vec<u8>> {
    let pos = entry.windows(4).position(|w| w == b"hvcC")?;
    if pos < 4 {
        return None;
    }
    let size = u32::from_be_bytes(entry[pos - 4..pos].try_into().unwrap()) as usize;
    if size < 31 || pos - 4 + size > entry.len() {
        return None;
    }
    Some(entry[pos + 4..pos - 4 + size].to_vec())
}

/// Reads one video connection until it ends.
pub async fn run<R: AsyncRead + Unpin>(mut r: R, key: [u8; 16], iv: [u8; 16], sink: &dyn Sink) {
    let mut cipher = Decryptor::new(key, iv);
    let mut streaming = false;
    let (mut t0, mut frames, mut bytes, mut gap, mut last) =
        (Instant::now(), 0u32, 0usize, 0f32, None::<Instant>);
    let mut header = [0u8; 128];
    loop {
        if r.read_exact(&mut header).await.is_err() {
            return;
        }
        let now = Instant::now();
        let kind = header[4];
        if kind == 0 {
            if let Some(l) = last {
                gap = gap.max((now - l).as_secs_f32());
            }
            last = Some(now);
        }
        let secs = (now - t0).as_secs_f32();
        if secs >= 5.0 {
            log::info!(
                "airplay: received {:.1} fps, {:.1} Mbps, longest gap {} ms",
                frames as f32 / secs,
                bytes as f32 * 8.0 / secs / 1e6,
                (gap * 1000.0) as u32
            );
            (t0, frames, bytes, gap) = (now, 0, 0, 0.0);
        }
        let size = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        // A 4K key frame is a few MiB; more is a broken or hostile stream.
        if size > 16 << 20 {
            log::warn!("airplay: video packet of {size} bytes; connection dropped");
            return;
        }
        let mut payload = vec![0; size];
        if r.read_exact(&mut payload).await.is_err() {
            return;
        }
        match kind {
            0 => {
                let Some(au) = cipher.decrypt(&payload) else {
                    continue;
                };
                frames += 1;
                bytes += au.len();
                sink.frame(au);
                if !streaming {
                    streaming = true;
                    sink.state(State::Streaming);
                }
            }
            1 if !payload.is_empty() => {
                let w = f32::from_le_bytes(header[56..60].try_into().unwrap()) as u32;
                let h = f32::from_le_bytes(header[60..64].try_into().unwrap()) as u32;
                let (desc, hevc) = if payload.get(4..8) == Some(b"hvc1") {
                    match hvcc_from_sample_entry(&payload) {
                        Some(c) => (c, true),
                        None => {
                            log::warn!("airplay: HEVC configuration not understood");
                            continue;
                        }
                    }
                } else {
                    (payload, false)
                };
                log::info!(
                    "airplay: video {w}x{h} {}",
                    if hevc { "HEVC" } else { "H.264" }
                );
                sink.config(desc, hevc, w, h);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_nal_chains() {
        assert!(valid_au(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41]));
        assert!(
            !valid_au(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 5, 0x41]),
            "runs past the end"
        );
        assert!(!valid_au(&[0, 0, 0, 2, 0xE5, 1]), "forbidden bit");
        assert!(!valid_au(&[0, 0, 0, 0, 0]), "empty NAL");
    }

    #[test]
    fn resyncs_when_the_keystream_restarts() {
        let (key, iv) = ([1u8; 16], [2u8; 16]);
        let au = [0u8, 0, 0, 3, 0x65, 9, 9];
        let enc = |c: &mut Ctr| {
            let mut d = au.to_vec();
            c.apply_keystream(&mut d);
            d
        };
        let mut sender = ctr(&key, &iv);
        let mut d = Decryptor::new(key, iv);
        assert_eq!(d.decrypt(&enc(&mut sender)).unwrap(), au);
        assert_eq!(d.decrypt(&enc(&mut sender)).unwrap(), au);
        let mut restarted = ctr(&key, &iv);
        assert_eq!(
            d.decrypt(&enc(&mut restarted)).unwrap(),
            au,
            "fresh keystream"
        );
        assert_eq!(
            d.decrypt(&enc(&mut restarted)).unwrap(),
            au,
            "stays in step"
        );
        assert!(d.decrypt(&[7; 7]).is_none());
    }

    #[test]
    fn finds_hvcc() {
        let mut e = vec![0, 0, 0, 0];
        e.extend(b"hvc1");
        e.extend([0; 10]);
        e.extend(39u32.to_be_bytes());
        e.extend(b"hvcC");
        e.extend([5; 31]);
        assert_eq!(hvcc_from_sample_entry(&e).unwrap(), vec![5; 31]);
    }
}
