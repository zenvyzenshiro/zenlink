//! Media Foundation decoder thread: stream.bin records in, NV12 frames out.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use zenlink_core::api::{Codec, CodecInfo};
use zenlink_core::nal::{hevc_visible, parse_config, to_annexb, ParamSets};
use zenlink_core::source::{RecordStream, VideoSource};
use zenlink_core::stream::Record;

use crate::d3d::{Gpu, SharedRing, Slot};
use crate::stats::Stats;

const MF_VERSION: u32 = 0x0002_0070;
const FRAME_100NS: i64 = 166_667;

pub struct Mft {
    t: IMFTransform,
    hardware: bool,
    provides_samples: bool,
    out_size: u32,
    width: u32,
    height: u32,
    src: RECT,
    /// The SPS's visible picture, used when the decoder reports no display aperture.
    visible: Option<[u32; 4]>,
    /// The source's picture size, when neither the decoder nor the SPS give one.
    display: Option<(u32, u32)>,
}

fn subtype(c: &Codec) -> GUID {
    match c {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

impl Mft {
    pub fn new(gpu: &Gpu, codec: &Codec) -> Result<Self> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: subtype(codec),
            };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut list,
                &mut count,
            )?;
            // No decoder at all: MFTEnumEx may leave the list null.
            if list.is_null() || count == 0 {
                if !list.is_null() {
                    CoTaskMemFree(Some(list as _));
                }
                return Err(windows::core::Error::from(MF_E_TOPO_CODEC_NOT_FOUND));
            }
            let acts = std::slice::from_raw_parts_mut(list, count as usize);
            let mut found = None;
            for a in acts.iter_mut() {
                if let Some(a) = a.take() {
                    if found.is_none() {
                        if let Ok(t) = a.ActivateObject::<IMFTransform>() {
                            found = Some(t);
                        }
                    }
                }
            }
            CoTaskMemFree(Some(list as _));
            let t = found.ok_or_else(|| windows::core::Error::from(MF_E_TOPO_CODEC_NOT_FOUND))?;

            let mut hardware = false;
            if let Ok(attrs) = t.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0
                    && t.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                        .is_ok()
                {
                    hardware = true;
                }
            }

            let mt = MFCreateMediaType()?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
            // Microsoft's HEVC decoder lists no output type until the input
            // type has a frame size. Any size works: the real one arrives
            // with the first frame as MF_E_TRANSFORM_STREAM_CHANGE.
            mt.SetUINT64(&MF_MT_FRAME_SIZE, (1920u64 << 32) | 1080)?;
            mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.SetInputType(0, &mt, 0)
                .inspect_err(|e| log::warn!("decoder: set input type: {e}"))?;

            let mut me = Self {
                t,
                hardware,
                provides_samples: false,
                out_size: 0,
                width: 0,
                height: 0,
                src: RECT::default(),
                visible: None,
                display: None,
            };
            // Some decoders (Microsoft's HEVC MFT) list no complete output
            // type until they have parsed the first frame; they then signal
            // MF_E_TRANSFORM_STREAM_CHANGE and drain() picks it.
            if let Err(e) = me.pick_output() {
                log::info!("decoder: output type after the first frame ({e})");
            }
            me.t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .inspect_err(|e| log::warn!("decoder: begin streaming: {e}"))?;
            me.t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(me)
        }
    }

    /// Choose NV12 output and read the picture size (again after a stream change).
    fn pick_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0;
            loop {
                let mt = self.t.GetOutputAvailableType(0, i)?;
                i += 1;
                if mt.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.t.SetOutputType(0, &mt, 0)?;
                let size = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                self.width = (size >> 32) as u32;
                self.height = (size & 0xffff_ffff) as u32;
                self.src = RECT {
                    left: 0,
                    top: 0,
                    right: self.width as i32,
                    bottom: self.height as i32,
                };
                let mut area = MFVideoArea::default();
                let mut got = 0u32;
                if mt
                    .GetBlob(
                        &MF_MT_MINIMUM_DISPLAY_APERTURE,
                        std::slice::from_raw_parts_mut(
                            &mut area as *mut _ as *mut u8,
                            std::mem::size_of::<MFVideoArea>(),
                        ),
                        Some(&mut got),
                    )
                    .is_ok()
                    && area.Area.cx > 0
                    && area.Area.cy > 0
                {
                    let x = area.OffsetX.value as i32;
                    let y = area.OffsetY.value as i32;
                    self.src = RECT {
                        left: x,
                        top: y,
                        right: x + area.Area.cx,
                        bottom: y + area.Area.cy,
                    };
                }
                self.crop_to_visible();
                log::info!(
                    "decoder output {}x{}, aperture {}, sps window {:?}, shown {:?}",
                    self.width,
                    self.height,
                    if got > 0 {
                        format!(
                            "{}x{}+{}+{}",
                            area.Area.cx, area.Area.cy, area.OffsetX.value, area.OffsetY.value
                        )
                    } else {
                        "none".into()
                    },
                    self.visible,
                    (self.src.left, self.src.top, self.src.right, self.src.bottom)
                );
                let info = self.t.GetOutputStreamInfo(0)?;
                self.provides_samples =
                    info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
                self.out_size = info.cbSize;
                return Ok(());
            }
        }
    }

    /// Without a display aperture from the decoder, cut the encoder's padding off
    /// (the SPS conformance window).
    fn crop_to_visible(&mut self) {
        let full = self.src.left == 0
            && self.src.top == 0
            && self.src.right == self.width as i32
            && self.src.bottom == self.height as i32;
        if !full {
            return;
        }
        if let Some([l, t, r, b]) = self.visible {
            if r <= self.width && b <= self.height {
                self.src = RECT {
                    left: l as i32,
                    top: t as i32,
                    right: r as i32,
                    bottom: b as i32,
                };
            }
        } else if let Some((w, h)) = self.display {
            // Either way round (the picture turns with the device); only a small pad is cut.
            let fits = |w: u32, h: u32| {
                w <= self.width && h <= self.height && self.width - w < 64 && self.height - h < 64
            };
            let (w, h) = if fits(w, h) { (w, h) } else { (h, w) };
            if fits(w, h) {
                self.src = RECT {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                };
            }
        }
    }

    fn set_visible(&mut self, ps: &ParamSets, display: Option<(u32, u32)>) {
        self.visible = hevc_visible(&ps.annexb);
        self.display = display;
        self.crop_to_visible();
    }

    pub fn flush(&self) {
        unsafe {
            let _ = self.t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }

    /// Feed one Annex B access unit. Returns false if the decoder rejected it.
    fn input(
        &mut self,
        data: &[u8],
        ts: i64,
        sink: &mut dyn FnMut(&mut Self, IMFSample),
    ) -> Result<()> {
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(ts)?;
            sample.SetSampleDuration(FRAME_100NS)?;
            for _ in 0..4 {
                match self.t.ProcessInput(0, &sample, 0) {
                    Ok(()) => {
                        self.drain(sink)?;
                        return Ok(());
                    }
                    Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain(sink)?,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    fn drain(&mut self, sink: &mut dyn FnMut(&mut Self, IMFSample)) -> Result<()> {
        unsafe {
            loop {
                let own = if self.provides_samples {
                    None
                } else {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size.max(1))?)?;
                    Some(s)
                };
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(own),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.t.ProcessOutput(0, &mut out, &mut status);
                let sample = ManuallyDrop::take(&mut out[0].pSample);
                drop(ManuallyDrop::take(&mut out[0].pEvents));
                match r {
                    Ok(()) => {
                        if let Some(s) = sample {
                            sink(self, s);
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.pick_output()?,
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

/// Copy a decoded sample into a free ring slot and publish it.
fn publish(
    gpu: &Gpu,
    ring: &SharedRing,
    stats: &Stats,
    mft: &Mft,
    sample: &IMFSample,
    upload: &mut Option<(ID3D11Texture2D, u32, u32)>,
) -> Result<()> {
    unsafe {
        let (w, h) = (mft.width, mft.height);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let idx = {
            let mut r = ring.lock().unwrap();
            let (idx, dropped) = r.free_slot();
            if dropped > 0 {
                stats.replaced.fetch_add(dropped, Relaxed);
            }
            let ok = matches!(&r.slots[idx], Some(s) if s.w == w && s.h == h);
            if !ok {
                r.slots[idx] = Some(Slot {
                    tex: gpu.nv12_texture(w, h, false)?,
                    w,
                    h,
                    src: mft.src,
                });
            }
            if let Some(s) = r.slots[idx].as_mut() {
                s.src = mft.src;
            }
            idx
        };
        let dst = ring.lock().unwrap().slots[idx]
            .as_ref()
            .unwrap()
            .tex
            .clone();
        let buffer = sample.GetBufferByIndex(0)?;
        if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
            let src = ID3D11Texture2D::from_raw(raw);
            let sub = dxgi.GetSubresourceIndex()?;
            gpu.context
                .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, sub, None);
        } else {
            // Software decoder: system-memory NV12, uploaded through a dynamic texture.
            if !matches!(upload, Some((_, uw, uh)) if *uw == w && *uh == h) {
                *upload = Some((gpu.nv12_texture(w, h, true)?, w, h));
            }
            let up = &upload.as_ref().unwrap().0;
            let b2: IMF2DBuffer = buffer.cast()?;
            let mut src = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2.Lock2D(&mut src, &mut pitch)?;
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if gpu
                .context
                .Map(up, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))
                .is_ok()
            {
                let rows = h as usize + h as usize / 2;
                for y in 0..rows {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * pitch as usize),
                        (m.pData as *mut u8).add(y * m.RowPitch as usize),
                        w as usize,
                    );
                }
                gpu.context.Unmap(up, 0);
                gpu.context.CopyResource(&dst, up);
            }
            b2.Unlock2D()?;
        }
        let mut r = ring.lock().unwrap();
        let dropped = r.push(idx);
        if dropped > 0 {
            stats.replaced.fetch_add(dropped, Relaxed);
        }
        stats
            .width
            .store((mft.src.right - mft.src.left) as u32, Relaxed);
        stats
            .height
            .store((mft.src.bottom - mft.src.top) as u32, Relaxed);
        Ok(())
    }
}

/// Whether Windows has a decoder for `codec` (HEVC needs the "HEVC Video
/// Extensions" or a GPU driver that ships one).
pub fn decoder_available(codec: &Codec) -> bool {
    if !media_foundation_present() {
        return false;
    }
    startup();
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype(codec),
        };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
        .is_err()
        {
            return false;
        }
        if list.is_null() {
            return false;
        }
        for a in std::slice::from_raw_parts_mut(list, count as usize) {
            drop(a.take());
        }
        CoTaskMemFree(Some(list as _));
        count > 0
    }
}

/// Whether Windows has Media Foundation (`mfplat.dll`). Windows 10/11 "N"
/// and "KN" editions ship without it until the Media Feature Pack is
/// installed (B-27). ZenLink.exe delay-loads `mfplat.dll` (build.rs), so
/// this check must come before any Media Foundation call.
pub fn media_foundation_present() -> bool {
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT.get_or_init(|| unsafe {
        match LoadLibraryExW(
            windows::core::w!("mfplat.dll"),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        ) {
            Ok(h) => {
                let _ = FreeLibrary(h);
                true
            }
            Err(e) => {
                log::warn!("mfplat.dll not found ({e}): no Media Foundation (Windows N without the Media Feature Pack?)");
                false
            }
        }
    })
}

pub fn startup() {
    if !media_foundation_present() {
        return;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
    }
}

/// Decode until `stop` is set. Reopens the source when a session ends.
pub fn run(
    gpu: Arc<Gpu>,
    ring: SharedRing,
    stats: Arc<Stats>,
    stop: Arc<AtomicBool>,
    mut source: Box<dyn VideoSource>,
) {
    startup();
    while !stop.load(Relaxed) && !source.finished() {
        let Some((info, mut stream)) = source.open() else {
            std::thread::sleep(Duration::from_millis(400));
            continue;
        };
        if let Err(e) = session(&gpu, &ring, &stats, &stop, &*source, info, &mut *stream) {
            log::warn!("decoder: {e}");
            stats.errors.fetch_add(1, Relaxed);
        }
        if !stop.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn session(
    gpu: &Gpu,
    ring: &SharedRing,
    stats: &Stats,
    stop: &AtomicBool,
    source: &dyn VideoSource,
    mut info: CodecInfo,
    stream: &mut dyn RecordStream,
) -> Result<()> {
    let mut ps: ParamSets = parse_config(&info.codec, &info.description).unwrap_or_default();
    let mut mft = Mft::new(gpu, &info.codec)?;
    mft.set_visible(&ps, info.display);
    stats.hardware.store(mft.hardware, Relaxed);
    log::info!(
        "decoder {:?} hardware={} provides_samples={} {}x{}",
        info.codec,
        mft.hardware,
        mft.provides_samples,
        mft.width,
        mft.height
    );
    let mut ts: i64 = 0;
    let mut got_key = false;
    let mut pending: VecDeque<(i64, Instant)> = VecDeque::new();
    let mut upload = None;
    while !stop.load(Relaxed) {
        let Some(rec) = stream.next_record() else {
            return Ok(());
        };
        match rec {
            Record::Config(c) => {
                info = c;
                ps = parse_config(&info.codec, &info.description).unwrap_or_default();
                mft = Mft::new(gpu, &info.codec)?;
                mft.set_visible(&ps, info.display);
                stats.hardware.store(mft.hardware, Relaxed);
                got_key = false;
            }
            Record::Frame { key, rebuild, data } => {
                if rebuild {
                    mft.flush();
                }
                if key {
                    got_key = true;
                }
                if !got_key {
                    continue;
                }
                stats.bytes.fetch_add(data.len() as u64, Relaxed);
                ts += FRAME_100NS;
                pending.push_back((ts, Instant::now()));
                if pending.len() > 64 {
                    pending.pop_front();
                }
                let au = to_annexb(&data, &ps, key);
                let mut sink = |m: &mut Mft, s: IMFSample| {
                    let t = unsafe { s.GetSampleTime().unwrap_or(0) };
                    if let Some(pos) = pending.iter().position(|(pts, _)| *pts == t) {
                        let (_, at) = pending[pos];
                        stats
                            .decode_us
                            .store(at.elapsed().as_micros() as u32, Relaxed);
                        pending.drain(..=pos);
                    }
                    match publish(gpu, ring, stats, m, &s, &mut upload) {
                        Ok(()) => {
                            stats.decoded.fetch_add(1, Relaxed);
                        }
                        Err(e) => {
                            if stats.errors.fetch_add(1, Relaxed) < 5 {
                                log::warn!("publish: {e}");
                            }
                        }
                    }
                };
                if let Err(e) = mft.input(&au, ts, &mut sink) {
                    log::warn!("decode: {e}");
                    stats.errors.fetch_add(1, Relaxed);
                    mft.flush();
                    got_key = false;
                    if info.keyframes_on_request {
                        source.request_keyframe();
                    }
                }
            }
        }
    }
    Ok(())
}
