//! avcC / hvcC records and AVCC -> Annex B conversion for Media Foundation.

use crate::api::Codec;

const START: [u8; 4] = [0, 0, 0, 1];

#[derive(Clone, Debug, Default)]
pub struct ParamSets {
    /// SPS/PPS (and VPS for HEVC) as Annex B.
    pub annexb: Vec<u8>,
    /// Size of each NAL length prefix in samples (1, 2 or 4).
    pub length_size: usize,
}

pub fn parse_config(codec: &Codec, rec: &[u8]) -> Option<ParamSets> {
    match codec {
        Codec::H264 => parse_avcc(rec),
        Codec::Hevc => parse_hvcc(rec),
    }
}

fn parse_avcc(r: &[u8]) -> Option<ParamSets> {
    if r.len() < 7 || r[0] != 1 {
        return None;
    }
    let length_size = (r[4] & 3) as usize + 1;
    let mut out = Vec::new();
    let mut i = 5;
    let nsps = (r[i] & 0x1f) as usize;
    i += 1;
    for _ in 0..nsps {
        i = push_ps(r, i, &mut out)?;
    }
    let npps = *r.get(i)? as usize;
    i += 1;
    for _ in 0..npps {
        i = push_ps(r, i, &mut out)?;
    }
    Some(ParamSets {
        annexb: out,
        length_size,
    })
}

fn parse_hvcc(r: &[u8]) -> Option<ParamSets> {
    if r.len() < 23 {
        return None;
    }
    let length_size = (r[21] & 3) as usize + 1;
    let arrays = r[22] as usize;
    let mut out = Vec::new();
    let mut i = 23;
    for _ in 0..arrays {
        i += 1; // array_completeness | NAL type
        let n = u16::from_be_bytes([*r.get(i)?, *r.get(i + 1)?]) as usize;
        i += 2;
        for _ in 0..n {
            i = push_ps(r, i, &mut out)?;
        }
    }
    Some(ParamSets {
        annexb: out,
        length_size,
    })
}

fn push_ps(r: &[u8], i: usize, out: &mut Vec<u8>) -> Option<usize> {
    let len = u16::from_be_bytes([*r.get(i)?, *r.get(i + 1)?]) as usize;
    let nal = r.get(i + 2..i + 2 + len)?;
    out.extend_from_slice(&START);
    out.extend_from_slice(nal);
    Some(i + 2 + len)
}

/// Convert one length-prefixed sample to Annex B. Keyframes get the parameter
/// sets in front, so a decoder can start (or restart) on any keyframe.
pub fn to_annexb(sample: &[u8], ps: &ParamSets, key: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len() + 64 + if key { ps.annexb.len() } else { 0 });
    if key {
        out.extend_from_slice(&ps.annexb);
    }
    let n = ps.length_size.max(1);
    let mut i = 0;
    while i + n <= sample.len() {
        let mut len = 0usize;
        for b in &sample[i..i + n] {
            len = (len << 8) | *b as usize;
        }
        i += n;
        if len == 0 || i + len > sample.len() {
            break;
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&sample[i..i + len]);
        i += len;
    }
    out
}

/// NAL units of an Annex B byte stream (3- or 4-byte start codes), without
/// the start codes. Android's MediaCodec (scrcpy) produces Annex B.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut e = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        // A 4-byte start code leaves one zero byte at the end of the NAL before it.
        while e > s && n + 1 < starts.len() && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Annex B access unit -> one sample with 4-byte NAL lengths (AVCC / HVCC).
pub fn annexb_to_lengths(data: &[u8]) -> Vec<u8> {
    let nals = split_annexb(data);
    let mut out = Vec::with_capacity(data.len() + 4);
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// avcC record (4-byte lengths) from an H.264 Annex B config packet that
/// holds the SPS (type 7) and PPS (type 8). `None` without an SPS and a PPS.
pub fn avcc_from_annexb(config: &[u8]) -> Option<Vec<u8>> {
    let nals = split_annexb(config);
    let sps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 7).collect();
    let pps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 8).collect();
    let first = sps.first()?;
    if pps.is_empty() || first.len() < 4 || sps.len() > 31 || pps.len() > 255 {
        return None;
    }
    let mut r = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xff,
        0xe0 | sps.len() as u8,
    ];
    for s in &sps {
        r.extend_from_slice(&(s.len() as u16).to_be_bytes());
        r.extend_from_slice(s);
    }
    r.push(pps.len() as u8);
    for p in &pps {
        r.extend_from_slice(&(p.len() as u16).to_be_bytes());
        r.extend_from_slice(p);
    }
    Some(r)
}

/// hvcC record (4-byte lengths) from an H.265 Annex B config packet with
/// the VPS (type 32), SPS (33) and PPS (34). The decoder only reads the NAL
/// arrays and the length size; profile, tier and level are copied from the
/// SPS's profile_tier_level so the record stays truthful. `None` without
/// all three.
pub fn hvcc_from_annexb(config: &[u8]) -> Option<Vec<u8>> {
    let nals = split_annexb(config);
    let kind = |n: &[u8]| (n[0] >> 1) & 0x3f;
    let of = |t: u8| -> Vec<&[u8]> {
        nals.iter()
            .copied()
            .filter(|n| n.len() > 2 && kind(n) == t)
            .collect()
    };
    let (vps, sps, pps) = (of(32), of(33), of(34));
    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
        return None;
    }
    let mut r = vec![0u8; 23];
    r[0] = 1; // configurationVersion
              // SPS: 2-byte NAL header, 1 byte (vps id, max sub layers, nesting), then
              // general profile_tier_level: 1 byte profile space/tier/idc, 4 bytes
              // compatibility flags, 6 bytes constraint flags, 1 byte level.
    if let Some(ptl) = sps[0].get(3..15) {
        r[1..13].copy_from_slice(ptl);
    }
    r[13] = 0xf0; // reserved + min_spatial_segmentation_idc (0)
    r[15] = 0xfc; // reserved + parallelismType (0)
    r[16] = 0xfd; // reserved + chroma_format_idc 4:2:0
    r[17] = 0xf8; // reserved + bit_depth_luma_minus8 0
    r[18] = 0xf8; // reserved + bit_depth_chroma_minus8 0
    r[21] = 0x0f; // constantFrameRate 0, numTemporalLayers 1, nesting 1, lengthSizeMinusOne 3
    r[22] = 3; // numOfArrays
    for (t, list) in [(32u8, &vps), (33, &sps), (34, &pps)] {
        r.push(0x80 | t); // array_completeness = 1
        r.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for n in list.iter() {
            r.extend_from_slice(&(n.len() as u16).to_be_bytes());
            r.extend_from_slice(n);
        }
    }
    Some(r)
}

/// Colour range and VUI parameters parsed from an H.264 / HEVC SPS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VuiColor {
    pub full_range: Option<bool>,
    pub primaries: Option<u8>,
    pub transfer: Option<u8>,
    pub matrix: Option<u8>,
}

/// Exp-Golomb bit reader over an RBSP (emulation prevention bytes removed).
struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let b = self.data.get(self.pos / 8)? >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        Some(u32::from(b))
    }

    fn u(&mut self, n: usize) -> Option<u32> {
        (0..n).try_fold(0, |v, _| Some(v << 1 | self.bit()?))
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<i32> {
        let v = self.ue()?;
        let mag = ((v + 1) >> 1) as i32;
        Some(if v & 1 == 1 { mag } else { -mag })
    }
}

fn skip_profile_tier_level(b: &mut Bits, sub_layers: usize) -> Option<()> {
    b.skip(96); // general profile_tier_level
    let mut present = Vec::with_capacity(sub_layers);
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers));
    }
    for (profile, level) in present {
        b.skip(88 * profile as usize + 8 * level as usize);
    }
    Some(())
}

fn skip_hevc_scaling_list(b: &mut Bits) -> Option<()> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        for _matrix_id in (0..6).step_by(step) {
            if b.bit()? == 0 {
                b.ue()?;
            } else {
                let n = (1usize << (4 + (size_id << 1))).min(64);
                if size_id > 1 {
                    b.se()?;
                }
                for _ in 0..n {
                    b.se()?;
                }
            }
        }
    }
    Some(())
}

fn parse_vui_color(b: &mut Bits) -> Option<VuiColor> {
    if b.bit()? == 1 && b.u(8)? == 255 {
        b.skip(32); // sar_width (16) + sar_height (16)
    }
    if b.bit()? == 1 {
        b.skip(1); // overscan_appropriate_flag
    }
    if b.bit()? == 0 {
        return None;
    }
    b.skip(3); // video_format
    let full_range = Some(b.bit()? != 0);
    let (mut primaries, mut transfer, mut matrix) = (None, None, None);
    if b.bit()? == 1 {
        primaries = Some(b.u(8)? as u8);
        transfer = Some(b.u(8)? as u8);
        matrix = Some(b.u(8)? as u8);
    }
    Some(VuiColor {
        full_range,
        primaries,
        transfer,
        matrix,
    })
}

/// Colour metadata from the stream's SPS VUI (`None` if absent or unparsed).
pub fn sps_color(codec: &Codec, annexb: &[u8]) -> Option<VuiColor> {
    match codec {
        Codec::Hevc => hevc_sps_color(annexb),
        Codec::H264 => h264_sps_color(annexb),
    }
}

fn hevc_sps_color(annexb: &[u8]) -> Option<VuiColor> {
    let sps = split_annexb(annexb)
        .into_iter()
        .find(|n| n.len() > 2 && (n[0] >> 1) & 0x3f == 33)?;
    let mut b = Bits::new(&sps[2..]);
    b.skip(4); // sps_video_parameter_set_id
    let sub_layers = b.u(3)? as usize;
    b.skip(1); // sps_temporal_id_nesting_flag
    skip_profile_tier_level(&mut b, sub_layers)?;
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.skip(1); // separate_colour_plane_flag
    }
    b.ue()?; // pic_width_in_luma_samples
    b.ue()?; // pic_height_in_luma_samples
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    b.ue()?; // bit_depth_luma_minus8
    b.ue()?; // bit_depth_chroma_minus8
    let log2_poc = b.ue()? + 4;
    if log2_poc > 16 {
        return None;
    }
    let sub_layer_ordering = b.bit()?;
    let start = if sub_layer_ordering != 0 { 0 } else { sub_layers };
    for _ in start..=sub_layers {
        b.ue()?;
        b.ue()?;
        b.ue()?;
    }
    for _ in 0..6 {
        b.ue()?;
    }
    if b.bit()? == 1 && b.bit()? == 1 {
        skip_hevc_scaling_list(&mut b)?;
    }
    b.skip(2); // amp_enabled_flag + sample_adaptive_offset_enabled_flag
    if b.bit()? == 1 {
        b.skip(8); // pcm_sample_bit_depth_luma/chroma_minus1
        b.ue()?;
        b.ue()?;
        b.skip(1); // pcm_loop_filter_disabled_flag
    }
    let num_st_rps = b.ue()? as usize;
    if num_st_rps > 64 {
        return None;
    }
    let mut num_delta_pocs = Vec::with_capacity(num_st_rps);
    for idx in 0..num_st_rps {
        let pred = if idx != 0 { b.bit()? } else { 0 };
        if pred == 1 {
            b.skip(1); // delta_rps_sign
            b.ue()?; // abs_delta_rps_minus1
            let ref_n = *num_delta_pocs.get(idx - 1)?;
            let mut count = 0usize;
            for _ in 0..=ref_n {
                let used = b.bit()?;
                let use_delta = if used == 1 { 1 } else { b.bit()? };
                if used == 1 || use_delta == 1 {
                    count += 1;
                }
            }
            num_delta_pocs.push(count);
        } else {
            let neg = b.ue()? as usize;
            let pos = b.ue()? as usize;
            if neg + pos > 64 {
                return None;
            }
            for _ in 0..(neg + pos) {
                b.ue()?;
                b.skip(1);
            }
            num_delta_pocs.push(neg + pos);
        }
    }
    if b.bit()? == 1 {
        let n = b.ue()? as usize;
        if n > 32 {
            return None;
        }
        for _ in 0..n {
            b.skip(log2_poc as usize);
            b.bit()?;
        }
    }
    b.skip(2); // sps_temporal_mvp_enabled_flag + strong_intra_smoothing_enabled_flag
    if b.bit()? == 0 {
        return None;
    }
    parse_vui_color(&mut b)
}

fn h264_sps_color(annexb: &[u8]) -> Option<VuiColor> {
    let sps = split_annexb(annexb)
        .into_iter()
        .find(|n| !n.is_empty() && n[0] & 0x1f == 7)?;
    let mut b = Bits::new(&sps[1..]);
    let profile_idc = b.u(8)?;
    b.skip(16); // constraint flags + level_idc
    b.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma = b.ue()?;
        if chroma == 3 {
            b.skip(1);
        }
        b.ue()?;
        b.ue()?;
        b.skip(1);
        if b.bit()? == 1 {
            let count = if chroma != 3 { 8 } else { 12 };
            for i in 0..count {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.skip(1);
            b.se()?;
            b.se()?;
            let n = b.ue()? as usize;
            if n > 256 {
                return None;
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1); // gaps_in_frame_num_value_allowed_flag
    b.ue()?; // pic_width_in_mbs_minus1
    b.ue()?; // pic_height_in_map_units_minus1
    if b.bit()? == 0 {
        b.skip(1); // mb_adaptive_frame_field_flag
    }
    b.skip(1); // direct_8x8_inference_flag
    if b.bit()? == 1 {
        for _ in 0..4 {
            b.ue()?;
        }
    }
    if b.bit()? == 0 {
        return None;
    }
    parse_vui_color(&mut b)
}

/// The visible picture of an HEVC stream (left, top, right, bottom in luma pixels) from the
/// SPS conformance window; `None` without one. Encoders pad the coded size to a multiple of
/// 16 or 32 (an iPhone 17's 1206x2622 screen comes as 1216x2656).
pub fn hevc_visible(annexb: &[u8]) -> Option<[u32; 4]> {
    let sps = split_annexb(annexb)
        .into_iter()
        .find(|n| n.len() > 2 && (n[0] >> 1) & 0x3f == 33)?;
    let mut b = Bits::new(&sps[2..]);
    b.skip(4); // sps_video_parameter_set_id
    let sub_layers = b.u(3)? as usize;
    b.skip(1);
    b.skip(96); // general profile_tier_level
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers));
    }
    for (profile, level) in present {
        b.skip(88 * profile as usize + 8 * level as usize);
    }
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.skip(1);
    }
    let (w, h) = (b.ue()?, b.ue()?);
    if b.bit()? == 0 {
        return None;
    }
    let (sx, sy) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let (l, r, t, bo) = (b.ue()? * sx, b.ue()? * sx, b.ue()? * sy, b.ue()? * sy);
    (l + r < w && t + bo < h).then_some([l, t, w - r, h - bo])
}

#[cfg(test)]
mod tests {
    #[test]
    fn hevc_visible_reads_the_conformance_window() {
        // A hand-built Main-profile SPS: 1216x2656, window right 5, bottom 17 (chroma units).
        let mut bits = String::new();
        let ue = |v: u32| {
            let n = 32 - (v + 1).leading_zeros() - 1;
            format!(
                "{}{:0width$b}",
                "0".repeat(n as usize),
                v + 1,
                width = n as usize + 1
            )
        };
        bits += "0000"; // vps id
        bits += "000"; // max_sub_layers_minus1
        bits += "1";
        bits += &"0".repeat(96);
        bits += &ue(0); // sps id
        bits += &ue(1); // 4:2:0
        bits += &ue(1216);
        bits += &ue(2656);
        bits += "1";
        bits += &(ue(0) + &ue(5) + &ue(0) + &ue(17));
        while bits.len() % 8 != 0 {
            bits.push('1');
        }
        let mut au = vec![0, 0, 0, 1, 33 << 1, 1];
        au.extend(
            bits.as_bytes()
                .chunks(8)
                .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap()),
        );
        assert_eq!(super::hevc_visible(&au), Some([0, 0, 1206, 2622]));
    }

    use super::*;

    #[test]
    fn hvcc_from_scrcpy_config_round_trips() {
        let mut config = vec![0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01];
        let sps = [
            0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0x5d, 0xa0,
        ];
        config.extend_from_slice(&[0, 0, 0, 1]);
        config.extend_from_slice(&sps);
        config.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xc1, 0x72]);
        let rec = hvcc_from_annexb(&config).unwrap();
        assert_eq!(rec[1], 0x01, "general profile Main");
        assert_eq!(rec[12], 0x5d, "level 3.1");
        let ps = parse_config(&Codec::Hevc, &rec).unwrap();
        assert_eq!(ps.length_size, 4);
        assert_eq!(ps.annexb, config);
        assert!(
            hvcc_from_annexb(&[0, 0, 0, 1, 0x42, 0x01, 1]).is_none(),
            "SPS only"
        );
    }

    #[test]
    fn annexb_split_and_lengths() {
        // 4-byte start, 3-byte start, 4-byte start.
        let au = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0x1f, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84,
        ];
        let nals = split_annexb(&au);
        assert_eq!(
            nals,
            vec![
                &[0x67, 0x42, 0, 0x1f][..],
                &[0x68, 0xce][..],
                &[0x65, 0x88, 0x84][..]
            ]
        );
        let s = annexb_to_lengths(&au);
        assert_eq!(&s[..8], &[0, 0, 0, 4, 0x67, 0x42, 0, 0x1f]);
        assert!(split_annexb(&[]).is_empty());
        assert!(split_annexb(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn avcc_from_scrcpy_config_round_trips() {
        let config = [
            0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80,
        ];
        let rec = avcc_from_annexb(&config).unwrap();
        assert_eq!(&rec[..6], &[1, 0x42, 0xc0, 0x1f, 0xff, 0xe1]);
        let ps = parse_config(&Codec::H264, &rec).unwrap();
        assert_eq!(ps.length_size, 4);
        assert_eq!(ps.annexb, config.to_vec());
        // A frame converted to lengths comes back unchanged through to_annexb.
        let frame = [0, 0, 0, 1, 0x41, 0x9a, 0x01];
        assert_eq!(
            to_annexb(&annexb_to_lengths(&frame), &ps, false),
            frame.to_vec()
        );
        assert!(avcc_from_annexb(&[0, 0, 0, 1, 0x68, 1]).is_none(), "no SPS");
    }

    #[test]
    fn avcc_roundtrip() {
        // version 1, profile, compat, level, 0xFF (4-byte lengths), 1 SPS, 1 PPS
        let rec = [1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 1, 0x68];
        let ps = parse_config(&Codec::H264, &rec).unwrap();
        assert_eq!(ps.length_size, 4);
        assert_eq!(ps.annexb, vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68]);
        let sample = [0, 0, 0, 2, 0x65, 9, 0, 0, 0, 1, 0x06];
        assert_eq!(
            to_annexb(&sample, &ps, false),
            vec![0, 0, 0, 1, 0x65, 9, 0, 0, 0, 1, 0x06]
        );
        assert_eq!(to_annexb(&sample, &ps, true).len(), ps.annexb.len() + 11);
    }

    #[test]
    fn hvcc_arrays() {
        let mut rec = vec![1u8; 21];
        rec.push(0x0f); // length size 4
        rec.push(1); // one array
        rec.extend([0x20, 0, 1, 0, 3, 0x40, 1, 2]); // VPS x1
        let ps = parse_config(&Codec::Hevc, &rec).unwrap();
        assert_eq!(ps.length_size, 4);
        assert_eq!(ps.annexb, vec![0, 0, 0, 1, 0x40, 1, 2]);
    }

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut s = bits.to_string();
        while s.len() % 8 != 0 {
            s.push('0');
        }
        s.as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect()
    }

    fn ue_bits(v: u32) -> String {
        let n = 32 - (v + 1).leading_zeros() - 1;
        format!(
            "{}{:0width$b}",
            "0".repeat(n as usize),
            v + 1,
            width = n as usize + 1
        )
    }

    #[test]
    fn hevc_sps_color_reads_vui_full_range_and_matrix() {
        let mut bits = String::new();
        bits += "0000"; // vps id
        bits += "000"; // max_sub_layers_minus1
        bits += "1"; // temporal_id_nesting
        bits += &"0".repeat(96);
        bits += &ue_bits(0); // sps id
        bits += &ue_bits(1); // 4:2:0
        bits += &ue_bits(1184);
        bits += &ue_bits(2576);
        bits += "0"; // conformance_window_flag
        bits += &ue_bits(0); // bit_depth_luma_minus8
        bits += &ue_bits(0); // bit_depth_chroma_minus8
        bits += &ue_bits(4); // log2_max_pic_order_cnt_lsb_minus4
        bits += "1"; // sps_sub_layer_ordering_info_present_flag
        bits += &(ue_bits(1) + &ue_bits(0) + &ue_bits(0));
        for _ in 0..6 {
            bits += &ue_bits(0);
        }
        bits += "0"; // scaling_list_enabled_flag
        bits += "000"; // amp + sao + pcm
        // 1 short_term_ref_pic_set with 1 neg delta poc
        bits += &ue_bits(1);
        bits += &ue_bits(1);
        bits += &ue_bits(0);
        bits += &(ue_bits(0) + "1");
        bits += "0"; // long_term_ref_pics_present_flag
        bits += "00"; // temporal_mvp + strong_intra_smoothing

        let mut no_vui = vec![0, 0, 0, 1, 33 << 1, 1];
        no_vui.extend(bits_to_bytes(&(bits.clone() + "0")));
        assert_eq!(sps_color(&Codec::Hevc, &no_vui), None);

        let mut vui_bits = bits;
        vui_bits += "1"; // vui_parameters_present_flag
        vui_bits += "00"; // aspect_ratio + overscan
        vui_bits += "1"; // video_signal_type_present_flag
        vui_bits += "101"; // video_format = 5 (unspecified)
        vui_bits += "1"; // video_full_range_flag = 1
        vui_bits += "1"; // colour_description_present_flag
        vui_bits += &format!("{:08b}{:08b}{:08b}", 1u8, 13u8, 1u8);
        let mut with_vui = vec![0, 0, 0, 1, 33 << 1, 1];
        with_vui.extend(bits_to_bytes(&vui_bits));
        assert_eq!(
            sps_color(&Codec::Hevc, &with_vui),
            Some(VuiColor {
                full_range: Some(true),
                primaries: Some(1),
                transfer: Some(13),
                matrix: Some(1),
            })
        );
    }

    #[test]
    fn h264_sps_color_reads_vui_full_range_and_matrix() {
        let mut bits = String::new();
        bits += &format!("{:08b}", 100u8); // High profile
        bits += "00000000"; // constraint flags
        bits += &format!("{:08b}", 40u8); // level 4.0
        bits += &ue_bits(0); // sps id
        bits += &ue_bits(1); // chroma_format_idc = 4:2:0
        bits += &ue_bits(0); // bit_depth_luma_minus8
        bits += &ue_bits(0); // bit_depth_chroma_minus8
        bits += "00"; // qpprime_y_zero_transform_bypass + seq_scaling_matrix_present_flag
        bits += &ue_bits(0); // log2_max_frame_num_minus4
        bits += &ue_bits(0); // pic_order_cnt_type = 0
        bits += &ue_bits(0); // log2_max_pic_order_cnt_lsb_minus4
        bits += &ue_bits(1); // max_num_ref_frames
        bits += "0"; // gaps_in_frame_num_value_allowed_flag
        bits += &ue_bits(119); // width in MBs - 1
        bits += &ue_bits(67); // height in map units - 1
        bits += "110"; // frame_mbs_only + direct_8x8_inference + frame_cropping_flag=0
        bits += "1"; // vui_parameters_present_flag
        bits += "001"; // aspect=0, overscan=0, video_signal_type=1
        bits += "101"; // video_format
        bits += "1"; // full_range = 1
        bits += "1"; // colour_description_present_flag
        bits += &format!("{:08b}{:08b}{:08b}", 1u8, 1u8, 1u8);
        let mut sps = vec![0, 0, 0, 1, 0x67];
        sps.extend(bits_to_bytes(&bits));
        assert_eq!(
            sps_color(&Codec::H264, &sps),
            Some(VuiColor {
                full_range: Some(true),
                primaries: Some(1),
                transfer: Some(1),
                matrix: Some(1),
            })
        );
    }
}
