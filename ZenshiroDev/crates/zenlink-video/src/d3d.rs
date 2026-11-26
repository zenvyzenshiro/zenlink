//! D3D11 device shared by the decoder (through the DXGI device manager) and
//! the renderer, plus the ring of NV12 textures that hands frames between them.

use std::sync::Mutex;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub video_device: ID3D11VideoDevice,
    pub video_context: ID3D11VideoContext,
    pub manager: IMFDXGIDeviceManager,
}

// The device is free-threaded and the immediate context is guarded by
// ID3D10Multithread (SetMultithreadProtected), so both threads may use them.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    pub fn create() -> Result<Self> {
        if !crate::decoder::media_foundation_present() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                "Media Foundation is missing (Media Feature Pack)",
            ));
        }
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.unwrap();
            let context: ID3D11DeviceContext = context.unwrap();
            let mt: ID3D10Multithread = context.cast()?;
            let _ = mt.SetMultithreadProtected(true);
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&device, token)?;
            Ok(Self {
                video_device: device.cast()?,
                video_context: context.cast()?,
                device,
                context,
                manager,
            })
        }
    }

    pub fn nv12_texture(&self, w: u32, h: u32, dynamic: bool) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if dynamic {
                D3D11_USAGE_DYNAMIC
            } else {
                D3D11_USAGE_DEFAULT
            },
            // Video-processor inputs must be render-target capable on some drivers (AMD).
            BindFlags: if dynamic {
                D3D11_BIND_SHADER_RESOURCE.0 as u32
            } else {
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
            },
            CPUAccessFlags: if dynamic {
                D3D11_CPU_ACCESS_WRITE.0 as u32
            } else {
                0
            },
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.unwrap())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt709,
    Bt601,
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTransfer {
    Sdr,
    Pq,
    Hlg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub full_range: bool,
    pub matrix: ColorMatrix,
    pub transfer: ColorTransfer,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            full_range: false,
            matrix: ColorMatrix::Bt709,
            transfer: ColorTransfer::Sdr,
        }
    }
}

pub struct Slot {
    pub tex: ID3D11Texture2D,
    pub w: u32,
    pub h: u32,
    /// Visible part of the coded picture (decoders pad to 16 / 32 lines).
    pub src: RECT,
    pub color: ColorInfo,
}

/// Frames handed from the decoder to the screen, oldest first.
///
/// A short queue instead of "newest wins": frames from the phone arrive with
/// network / backend jitter, so two can land within one screen refresh. Newest
/// wins dropped the first and the next refresh showed the same picture again,
/// a visible stutter every few seconds when the phone's 60 fps drifts against
/// a 60 Hz screen (and in OBS capture). The queue shows each frame once, one
/// per refresh, and only skips one when it falls more than a frame behind.
pub const SLOTS: usize = 6;
/// Frames that may wait for the screen (each is one refresh of added delay).
pub const MAX_QUEUE: usize = 3;
/// A backlog of two or more frames kept this long is cut by one frame.
pub const CATCH_UP: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Default)]
pub struct Ring {
    pub slots: [Option<Slot>; SLOTS],
    /// Decoded frames not shown yet, oldest first.
    pub queue: std::collections::VecDeque<usize>,
    /// The slot on screen (shown again until a newer frame is due).
    pub showing: Option<usize>,
    pub seq: u64,
    behind_since: Option<std::time::Instant>,
    /// Signalled on every push: the render thread presents a new frame as
    /// soon as it is decoded instead of at the next refresh (B-17).
    pub arrived: std::sync::Arc<std::sync::Condvar>,
    /// Per slot: push number and decode-ready time (pacing measurements).
    pub meta: [(u64, Option<std::time::Instant>); SLOTS],
}

unsafe impl Send for Ring {}

pub type SharedRing = std::sync::Arc<Mutex<Ring>>;

impl Ring {
    /// A slot that is neither waiting nor on screen. Returns how many waiting
    /// frames had to be dropped to free one (never, with SLOTS > MAX_QUEUE + 1).
    pub fn free_slot(&mut self) -> (usize, u64) {
        let mut dropped = 0;
        loop {
            if let Some(i) =
                (0..SLOTS).find(|i| !self.queue.contains(i) && Some(*i) != self.showing)
            {
                return (i, dropped);
            }
            if self.queue.pop_front().is_none() {
                return (0, dropped);
            }
            dropped += 1;
        }
    }

    /// Queue a decoded frame. Returns how many old frames were dropped.
    pub fn push(&mut self, idx: usize) -> u64 {
        self.queue.retain(|&i| i != idx);
        self.queue.push_back(idx);
        self.seq += 1;
        if let Some(m) = self.meta.get_mut(idx) {
            *m = (self.seq, Some(std::time::Instant::now()));
        }
        self.arrived.notify_all();
        let mut dropped = 0;
        while self.queue.len() > MAX_QUEUE {
            self.queue.pop_front();
            dropped += 1;
        }
        dropped
    }

    /// Frame for this screen refresh: the next waiting one, or the one already
    /// on screen. Returns (slot, frames skipped to catch up).
    pub fn next(&mut self) -> (Option<usize>, u64) {
        let mut skipped = 0;
        if let Some(i) = self.queue.pop_front() {
            self.showing = Some(i);
        }
        if self.queue.len() >= 2 {
            let since = *self
                .behind_since
                .get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= CATCH_UP {
                if let Some(i) = self.queue.pop_front() {
                    self.showing = Some(i);
                    skipped = 1;
                }
                self.behind_since = None;
            }
        } else {
            self.behind_since = None;
        }
        (self.showing, skipped)
    }

    /// Forget every frame (casting stopped).
    pub fn clear(&mut self) {
        self.queue.clear();
        self.showing = None;
        self.behind_since = None;
    }
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn shows_each_frame_once_in_order() {
        let mut r = Ring::default();
        let (a, _) = r.free_slot();
        r.push(a);
        let (b, _) = r.free_slot();
        assert_ne!(a, b);
        r.push(b); // two frames within one refresh: both are kept
        assert_eq!(r.next(), (Some(a), 0));
        assert_eq!(r.next(), (Some(b), 0));
        assert_eq!(r.next(), (Some(b), 0)); // nothing new: same picture again
    }

    #[test]
    fn free_slot_skips_waiting_and_showing() {
        let mut r = Ring::default();
        for _ in 0..MAX_QUEUE {
            let (i, d) = r.free_slot();
            assert_eq!(d, 0);
            r.push(i);
        }
        r.next();
        let (i, d) = r.free_slot();
        assert_eq!(d, 0);
        assert!(!r.queue.contains(&i) && Some(i) != r.showing);
    }

    #[test]
    fn queue_is_bounded() {
        let mut r = Ring::default();
        let mut dropped = 0;
        for _ in 0..10 {
            let (i, d) = r.free_slot();
            dropped += d + r.push(i);
        }
        assert_eq!(r.queue.len(), MAX_QUEUE);
        assert_eq!(dropped, 10 - MAX_QUEUE as u64);
    }

    #[test]
    fn catches_up_on_a_lasting_backlog() {
        let mut r = Ring::default();
        for _ in 0..3 {
            let (i, _) = r.free_slot();
            r.push(i);
        }
        assert_eq!(r.next().1, 0); // 2 left: backlog starts
        std::thread::sleep(CATCH_UP + std::time::Duration::from_millis(20));
        let (i, _) = r.free_slot();
        r.push(i); // 3 waiting
        let (_, skipped) = r.next();
        assert_eq!(skipped, 1);
        assert_eq!(r.queue.len(), 1);
    }

    #[test]
    fn push_wakes_a_waiting_renderer_and_records_the_frame() {
        let ring = std::sync::Arc::new(Mutex::new(Ring::default()));
        let r2 = ring.clone();
        let waiter = std::thread::spawn(move || {
            let mut g = r2.lock().unwrap();
            let cv = g.arrived.clone();
            let t = std::time::Instant::now();
            while g.queue.is_empty() && t.elapsed() < std::time::Duration::from_secs(2) {
                g = cv
                    .wait_timeout(g, std::time::Duration::from_secs(2))
                    .unwrap()
                    .0;
            }
            t.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut g = ring.lock().unwrap();
        let (i, _) = g.free_slot();
        g.push(i);
        assert_eq!(g.meta[i].0, 1);
        assert!(g.meta[i].1.is_some());
        drop(g);
        assert!(waiter.join().unwrap() < std::time::Duration::from_millis(1000));
    }

    #[test]
    fn clear_forgets_everything() {
        let mut r = Ring::default();
        let (i, _) = r.free_slot();
        r.push(i);
        r.next();
        r.clear();
        assert_eq!(r.next(), (None, 0));
    }
}
