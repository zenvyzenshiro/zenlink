//! Child HWND that shows the picture, plus its render thread.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use windows::core::{w, Interface, Result};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use zenlink_core::source::VideoSource;

use crate::d3d::{Gpu, Ring, SharedRing};
use crate::decoder;
use crate::stats::{RateMeter, Stats, StatsSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Whole picture, letterboxed.
    Contain = 0,
    /// Fill the view, cropping the edges.
    Cover = 1,
    /// Stretch.
    Fill = 2,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerEvent {
    /// u, v: position on the picture as the user sees it, 0..1.
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    /// Mouse back button.
    Back,
    /// Right click (Android: BACK, like scrcpy).
    Secondary,
    /// Middle click (Android: HOME, like scrcpy).
    Middle,
    /// Wheel notches (positive = away from the user).
    Wheel(f32, f32, f32),
    /// Ctrl + wheel, or a touchpad pinch (Windows sends it as Ctrl + wheel):
    /// notches, positive = zoom in.
    Pinch(f32, f32, f32),
}

type PointerCb = Box<dyn Fn(PointerEvent) + Send + 'static>;

struct Inner {
    gpu: Arc<Gpu>,
    ring: SharedRing,
    stats: Arc<Stats>,
    hwnd: isize,
    /// Off-screen top-level window with a copy of the picture (see `present_capture`); 0 = none.
    capture_hwnd: isize,
    stop_render: AtomicBool,
    rotation: AtomicU32,
    fit: AtomicU8,
    /// Letterbox colour, 0xRRGGBB.
    bg: AtomicU32,
    pointer: Mutex<Option<PointerCb>>,
    pressed: AtomicBool,
    /// Edge crop in source pixels: left, top, right, bottom.
    crop: [AtomicU32; 4],
    /// Size changes asked for by set_bounds / shown by the render thread
    /// (see `set_bounds`).
    resized: Mutex<(u64, u64)>,
    resized_cv: Condvar,
    /// Latest resize request (a copy of `resized.0`), read by the render
    /// thread while it holds the ring lock (no lock-order inversion).
    resize_asked: std::sync::atomic::AtomicU64,
    /// Picture snapshot (Alt+C, text from the screen): asked by `snapshot`,
    /// filled by the render thread from the picture it just drew.
    snap_asked: AtomicBool,
    snap: Mutex<Option<Snapshot>>,
    snap_cv: Condvar,
}

/// The picture as shown (rotated, cropped), 32-bit BGRA rows, top first.
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Inner {
    /// Visible source size after crop, and the crop's left/top offset.
    fn visible(&self) -> (u32, u32, u32, u32) {
        let (w, h) = (
            self.stats.width.load(Relaxed),
            self.stats.height.load(Relaxed),
        );
        let c: Vec<u32> = self.crop.iter().map(|a| a.load(Relaxed)).collect();
        let l = c[0].min(w / 2);
        let r = c[2].min(w / 2);
        let t = c[1].min(h / 2);
        let b = c[3].min(h / 2);
        (w.saturating_sub(l + r), h.saturating_sub(t + b), l, t)
    }

    /// View pixel -> (u, v) over the full (uncropped) picture, as shown.
    fn to_uv(&self, hwnd: HWND, x: f32, y: f32) -> (f32, f32) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let (vw, vh, l, t) = self.visible();
        let rot = self.rotation.load(Relaxed);
        let r = picture_rect(
            rc.right,
            rc.bottom,
            vw,
            vh,
            rot,
            fit_of(self.fit.load(Relaxed)),
        );
        let u = (x - r.left as f32) / (r.right - r.left).max(1) as f32;
        let v = (y - r.top as f32) / (r.bottom - r.top).max(1) as f32;
        let (fw, fh) = (
            self.stats.width.load(Relaxed).max(1) as f32,
            self.stats.height.load(Relaxed).max(1) as f32,
        );
        // Crop is in source orientation; the shown picture may be rotated.
        let (cw, ch, cl, ct, w, h) = if rot == 90 || rot == 270 {
            (vh as f32, vw as f32, t as f32, l as f32, fh, fw)
        } else {
            (vw as f32, vh as f32, l as f32, t as f32, fw, fh)
        };
        ((cl + u * cw) / w, (ct + v * ch) / h)
    }
}

pub struct VideoView {
    inner: Arc<Inner>,
    render: Option<JoinHandle<()>>,
    decode: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    meter: Mutex<RateMeter>,
    bounds: Mutex<Option<Bounds>>,
    /// Parent-window rectangles (x, y, w, h) the picture leaves open, so
    /// the window's toast and tooltips drawn there stay visible.
    holes: Mutex<Vec<Hole>>,
}

/// A rectangle in the parent window's client pixels: x, y, width, height.
pub type Hole = (i32, i32, i32, i32);

/// Last `set_bounds`: x, y, width, height, visible, corner radius.
type Bounds = (i32, i32, i32, i32, bool, i32);

/// Where the picture lands inside a `cw` x `ch` view (physical pixels).
pub fn picture_rect(cw: i32, ch: i32, sw: u32, sh: u32, rotation: u32, fit: Fit) -> RECT {
    let (mut pw, mut ph) = (sw as f32, sh as f32);
    if rotation == 90 || rotation == 270 {
        std::mem::swap(&mut pw, &mut ph);
    }
    if pw <= 0.0 || ph <= 0.0 || cw <= 0 || ch <= 0 || fit == Fit::Fill {
        return RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
    }
    let (sx, sy) = (cw as f32 / pw, ch as f32 / ph);
    let s = if fit == Fit::Cover {
        sx.max(sy)
    } else {
        sx.min(sy)
    };
    let (dw, dh) = ((pw * s).round() as i32, (ph * s).round() as i32);
    let (x, y) = ((cw - dw) / 2, (ch - dh) / 2);
    RECT {
        left: x,
        top: y,
        right: x + dw,
        bottom: y + dh,
    }
}

/// Clip `dest` to the `cw` x `ch` view and trim `src` by the same share of
/// the picture (Cover fit overflows the view; the video processor refuses a
/// destination outside its output). `rot` is clockwise, as D3D11 rotates.
fn clip_to_view(src: RECT, dest: RECT, cw: i32, ch: i32, rot: u32) -> (RECT, RECT) {
    let (dw, dh) = (
        (dest.right - dest.left).max(1) as f32,
        (dest.bottom - dest.top).max(1) as f32,
    );
    // Share of the shown picture cut from each side: left, top, right, bottom.
    let cut = [
        (-dest.left).max(0) as f32 / dw,
        (-dest.top).max(0) as f32 / dh,
        (dest.right - cw).max(0) as f32 / dw,
        (dest.bottom - ch).max(0) as f32 / dh,
    ];
    if cut.iter().all(|c| *c == 0.0) {
        return (src, dest);
    }
    // Shown side -> source side for each rotation.
    let [l, t, r, b] = cut;
    let (sl, st, sr, sb) = match rot {
        90 => (t, r, b, l),
        180 => (r, b, l, t),
        270 => (b, l, t, r),
        _ => (l, t, r, b),
    };
    let (sw, sh) = ((src.right - src.left) as f32, (src.bottom - src.top) as f32);
    let src2 = RECT {
        left: src.left + (sw * sl).round() as i32,
        top: src.top + (sh * st).round() as i32,
        right: src.right - (sw * sr).round() as i32,
        bottom: src.bottom - (sh * sb).round() as i32,
    };
    let dest2 = RECT {
        left: dest.left.max(0),
        top: dest.top.max(0),
        right: dest.right.min(cw),
        bottom: dest.bottom.min(ch),
    };
    if src2.right <= src2.left
        || src2.bottom <= src2.top
        || dest2.right <= dest2.left
        || dest2.bottom <= dest2.top
    {
        return (
            src,
            RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            },
        );
    }
    (src2, dest2)
}

fn fit_of(v: u8) -> Fit {
    match v {
        1 => Fit::Cover,
        2 => Fit::Fill,
        _ => Fit::Contain,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Inner;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let inner = &*ptr;
    let x = (lp.0 & 0xffff) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
    let uv = || inner.to_uv(hwnd, x, y);
    let send = |e: PointerEvent| {
        if let Some(cb) = inner.pointer.lock().unwrap().as_ref() {
            cb(e);
        }
    };
    match msg {
        WM_LBUTTONDOWN => {
            let (u, v) = uv();
            if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                SetCapture(hwnd);
                inner.pressed.store(true, Relaxed);
                send(PointerEvent::Down(u, v));
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if inner.pressed.load(Relaxed) {
                let (u, v) = uv();
                send(PointerEvent::Move(u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if inner.pressed.swap(false, Relaxed) {
                let _ = ReleaseCapture();
                let (u, v) = uv();
                send(PointerEvent::Up(u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)));
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            send(PointerEvent::Secondary);
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            send(PointerEvent::Middle);
            LRESULT(0)
        }
        WM_XBUTTONUP => {
            if (wp.0 >> 16) & 0xffff == 1 {
                send(PointerEvent::Back);
            }
            LRESULT(1)
        }
        WM_MOUSEWHEEL => {
            let d = ((wp.0 >> 16) & 0xffff) as i16 as f32 / 120.0;
            let mut pt = windows::Win32::Foundation::POINT {
                x: x as i32,
                y: y as i32,
            };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            let (u, v) = inner.to_uv(hwnd, pt.x as f32, pt.y as f32);
            const MK_CONTROL: usize = 0x0008;
            send(if wp.0 & MK_CONTROL != 0 {
                PointerEvent::Pinch(u, v, d)
            } else {
                PointerEvent::Wheel(u, v, d)
            });
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETCURSOR => {
            if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(c));
            }
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: w!("ZenLinkVideo"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        RegisterClassExW(&WNDCLASSEXW {
            lpfnWndProc: Some(capture_wndproc),
            lpszClassName: w!("ZenLinkCapture"),
            ..wc
        });
    });
}

unsafe extern "system" fn capture_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, msg, wp, lp) }
}

/// The window behind `present_capture`: owned by the main window (no taskbar or Alt+Tab
/// entry, hidden with it), shown but off every screen, never activated.
fn create_capture_window(parent: HWND) -> isize {
    unsafe {
        let root = GetAncestor(parent, GA_ROOT);
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN) - 4000;
        let Ok(module) = GetModuleHandleW(None) else {
            return 0;
        };
        CreateWindowExW(
            WS_EX_NOACTIVATE,
            w!("ZenLinkCapture"),
            w!("ZenLink Game Capture"),
            WS_POPUP | WS_VISIBLE,
            x,
            0,
            640,
            360,
            Some(root),
            None,
            Some(module.into()),
            None,
        )
        .inspect_err(|e| log::warn!("capture window: {e}"))
        .map_or(0, |h| h.0 as isize)
    }
}

impl VideoView {
    /// Create the view as a child of `parent` (an HWND as isize).
    pub fn new(parent: isize) -> Result<Self> {
        decoder::startup();
        register_class();
        let gpu = Arc::new(Gpu::create()?);
        let hwnd = unsafe {
            let parent = HWND(parent as _);
            let style = GetWindowLongPtrW(parent, GWL_STYLE);
            SetWindowLongPtrW(parent, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("ZenLinkVideo"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                16,
                16,
                Some(parent),
                None,
                Some(GetModuleHandleW(None)?.into()),
                None,
            )?
        };
        let inner = Arc::new(Inner {
            gpu,
            ring: Arc::new(Mutex::new(Ring::default())),
            stats: Arc::new(Stats::default()),
            hwnd: hwnd.0 as isize,
            capture_hwnd: create_capture_window(hwnd),
            stop_render: AtomicBool::new(false),
            rotation: AtomicU32::new(0),
            fit: AtomicU8::new(Fit::Contain as u8),
            bg: AtomicU32::new(0x000000),
            pointer: Mutex::new(None),
            pressed: AtomicBool::new(false),
            crop: Default::default(),
            resized: Mutex::new((0, 0)),
            resized_cv: Condvar::new(),
            resize_asked: std::sync::atomic::AtomicU64::new(0),
            snap_asked: AtomicBool::new(false),
            snap: Mutex::new(None),
            snap_cv: Condvar::new(),
        });
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&inner) as isize) };
        let meter = Mutex::new(RateMeter::new(&inner.stats));
        let r = inner.clone();
        let render = std::thread::Builder::new()
            .name("zenlink-render".into())
            .spawn(move || {
                if let Err(e) = render_loop(&r) {
                    log::error!("render: {e}");
                }
            })
            .ok();
        Ok(Self {
            inner,
            render,
            decode: None,
            meter,
            bounds: Mutex::new(None),
            holes: Mutex::new(Vec::new()),
        })
    }

    pub fn hwnd(&self) -> isize {
        self.inner.hwnd
    }

    /// The picture on screen right now, as shown (rotation and crop
    /// applied, at the view's size). Waits for the next drawn frame, at
    /// most `timeout`; `None` without a picture.
    pub fn snapshot(&self, timeout: std::time::Duration) -> Option<Snapshot> {
        let mut g = self.inner.snap.lock().unwrap();
        *g = None;
        self.inner.snap_asked.store(true, Relaxed);
        // Wake a render thread that waits for a new frame.
        self.inner.ring.lock().unwrap().arrived.notify_all();
        let t0 = std::time::Instant::now();
        while g.is_none() && t0.elapsed() < timeout {
            g = self
                .inner
                .snap_cv
                .wait_timeout(g, timeout.saturating_sub(t0.elapsed()))
                .unwrap()
                .0;
        }
        self.inner.snap_asked.store(false, Relaxed);
        g.take()
    }

    /// Place the view inside its parent (physical pixels).
    /// Place the view inside its parent (physical pixels), with rounded
    /// corners of `radius` px. Does nothing when nothing changed.
    pub fn set_bounds(&self, x: i32, y: i32, w: i32, h: i32, visible: bool, radius: i32) {
        let key = (x, y, w.max(1), h.max(1), visible, radius);
        let mut last = self.bounds.lock().unwrap();
        if *last == Some(key) {
            return;
        }
        let size_changed = last
            .map(|l| (l.2, l.3, l.5) != (key.2, key.3, key.5))
            .unwrap_or(true);
        let moved = last.map(|l| (l.0, l.1) != (x, y)).unwrap_or(true);
        *last = Some(key);
        let holes = self.holes.lock().unwrap().clone();
        unsafe {
            let hwnd = HWND(self.inner.hwnd as _);
            if size_changed || (moved && !holes.is_empty()) {
                apply_region(hwnd, key, &holes);
            }
            let flags = if visible {
                SWP_SHOWWINDOW
            } else {
                SWP_HIDEWINDOW
            };
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                x,
                y,
                key.2,
                key.3,
                flags | SWP_NOACTIVATE,
            );
        }
        drop(last);
        if size_changed && visible {
            self.wait_resized();
        }
    }

    /// Leaves `holes` (parent client pixels) open in the picture, e.g. a
    /// toast or a tooltip of the window that overlaps it. The picture is a
    /// child window: without a hole it covers what the window draws there.
    /// OBS still captures the full picture (the region only affects what
    /// Windows shows). Does nothing when nothing changed.
    pub fn set_holes(&self, holes: &[Hole]) {
        let mut h = self.holes.lock().unwrap();
        if h.as_slice() == holes {
            return;
        }
        *h = holes.to_vec();
        let holes = h.clone();
        drop(h);
        if let Some(key) = *self.bounds.lock().unwrap() {
            unsafe { apply_region(HWND(self.inner.hwnd as _), key, &holes) };
        }
    }

    /// OBS Game Capture hooks both the picture (D3D11) and the window
    /// around it (OpenGL). ResizeBuffers drops its D3D capture, and the
    /// next Present or SwapBuffers in the process takes it over. When that
    /// was the window's OpenGL frame, OBS showed nothing until the source
    /// was added again. So the UI thread waits here (it is the one that
    /// draws OpenGL) until the render thread has resized and presented.
    fn wait_resized(&self) {
        let mut g = self.inner.resized.lock().unwrap();
        g.0 += 1;
        let want = g.0;
        drop(g);
        self.inner.resize_asked.store(want, Relaxed);
        // Wake a render thread that waits for a frame (take the ring lock so
        // the wake-up cannot fall between its check and its wait).
        {
            let r = self.inner.ring.lock().unwrap();
            r.arrived.notify_all();
        }
        let mut g = self.inner.resized.lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(120);
        let asked_at = std::time::Instant::now();
        while g.1 < want {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                log::warn!("view: resize not presented within 120 ms (OBS may lose the capture)");
                break;
            }
            g = self.inner.resized_cv.wait_timeout(g, left).unwrap().0;
        }
        if std::env::var_os("ZL_STATS_LOG").is_some() {
            log::info!(
                "view: resize handshake {:.1} ms",
                asked_at.elapsed().as_secs_f32() * 1000.0
            );
        }
    }

    pub fn set_rotation(&self, deg: u32) {
        self.inner.rotation.store(deg % 360, Relaxed);
    }

    pub fn set_fit(&self, fit: Fit) {
        self.inner.fit.store(fit as u8, Relaxed);
    }

    /// Edge crop in source pixels (left, top, right, bottom).
    pub fn set_crop(&self, l: u32, t: u32, r: u32, b: u32) {
        for (a, v) in self.inner.crop.iter().zip([l, t, r, b]) {
            a.store(v, Relaxed);
        }
    }

    /// Picture size after crop, before rotation. 0 x 0 before the first frame.
    pub fn source_size(&self) -> (u32, u32) {
        let (w, h, _, _) = self.inner.visible();
        (w, h)
    }

    /// Picture size as shown: after crop and rotation. 0 x 0 before the first frame.
    pub fn shown_size(&self) -> (u32, u32) {
        let (w, h, _, _) = self.inner.visible();
        if matches!(self.inner.rotation.load(Relaxed), 90 | 270) {
            (h, w)
        } else {
            (w, h)
        }
    }

    pub fn set_background(&self, rgb: u32) {
        self.inner.bg.store(rgb, Relaxed);
    }

    pub fn on_pointer(&self, cb: impl Fn(PointerEvent) + Send + 'static) {
        *self.inner.pointer.lock().unwrap() = Some(Box::new(cb));
    }

    /// Size of the picture as decoded (before rotation), 0 x 0 before the first frame.
    pub fn frame_size(&self) -> (u32, u32) {
        (
            self.inner.stats.width.load(Relaxed),
            self.inner.stats.height.load(Relaxed),
        )
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.meter.lock().unwrap().sample(&self.inner.stats)
    }

    /// Decode any video source (Android, a recording).
    pub fn start_source(&mut self, source: Box<dyn VideoSource>) {
        self.stop();
        let stop = Arc::new(AtomicBool::new(false));
        let (g, r, s, st) = (
            self.inner.gpu.clone(),
            self.inner.ring.clone(),
            self.inner.stats.clone(),
            stop.clone(),
        );
        if let Ok(h) = std::thread::Builder::new()
            .name("zenlink-decode".into())
            .spawn(move || decoder::run(g, r, s, st, source))
        {
            self.decode = Some((stop, h));
        }
    }

    /// Stop decoding. The thread exits after its current read returns (the
    /// backend closes the stream when casting stops).
    pub fn stop(&mut self) {
        if let Some((stop, _h)) = self.decode.take() {
            stop.store(true, Relaxed);
        }
        let mut r = self.inner.ring.lock().unwrap();
        r.clear();
        self.inner.stats.width.store(0, Relaxed);
        self.inner.stats.height.store(0, Relaxed);
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        self.stop();
        self.inner.stop_render.store(true, Relaxed);
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
        unsafe {
            let hwnd = HWND(self.inner.hwnd as _);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
            if self.inner.capture_hwnd != 0 {
                let _ = DestroyWindow(HWND(self.inner.capture_hwnd as _));
            }
        }
    }
}

struct Processor {
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    key: (u32, u32, u32, u32),
}

fn make_processor(gpu: &Gpu, iw: u32, ih: u32, ow: u32, oh: u32) -> Result<Processor> {
    unsafe {
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: iw,
            InputHeight: ih,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr = gpu.video_device.CreateVideoProcessorEnumerator(&desc)?;
        let vp = gpu.video_device.CreateVideoProcessor(&enumr, 0)?;
        let vc = &gpu.video_context;
        vc.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        vc.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
        // BT.709, studio range in; full-range RGB out.
        vc.VideoProcessorSetStreamColorSpace(
            &vp,
            0,
            &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 },
        );
        vc.VideoProcessorSetOutputColorSpace(
            &vp,
            &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
        );
        Ok(Processor {
            enumr,
            vp,
            key: (iw, ih, ow, oh),
        })
    }
}

/// Present pacing probe (ZL_STATS_LOG): what an OBS Game Capture hook sees,
/// since the hook copies the back buffer only inside `Present`.
#[derive(Default)]
struct PacingProbe {
    on: bool,
    since: Option<std::time::Instant>,
    last_present: Option<std::time::Instant>,
    last_new: Option<std::time::Instant>,
    /// µs between Present calls returning.
    gaps: Vec<u32>,
    /// µs between presents that showed a new picture.
    new_gaps: Vec<u32>,
    wait_max: u32,
    work_max: u32,
    present_max: u32,
    /// Every present: (time, frame number shown).
    shown: Vec<(std::time::Instant, u64)>,
    /// Every frame: (frame number, decode-ready time), first time shown.
    ready: Vec<(u64, std::time::Instant)>,
    /// µs from decode-ready to its first present.
    delay: Vec<u32>,
}

/// OBS Game Capture model, from obs-studio `graphics-hook.h`: the hook
/// copies the back buffer inside a Present when `frame_ready(interval)`
/// allows it (default interval = half the OBS frame time), and OBS renders
/// the latest copy on its own clock. Returns (repeated, skipped) frames for
/// an OBS output at `obs_fps`, given presents `(time, frame)`, and the same
/// for a capture that saw every frame the moment it was decoded. OBS's clock
/// has no fixed phase to the video, so the counts are averaged over 32
/// phases (x10, to keep one decimal).
fn obs_model(
    shown: &[(std::time::Instant, u64)],
    ready: &[(u64, std::time::Instant)],
    obs_fps: f64,
) -> ((u32, u32), (u32, u32)) {
    let Some(&(t0, _)) = shown.first() else {
        return ((0, 0), (0, 0));
    };
    let tick = 1.0 / obs_fps;
    let interval = tick / 2.0;
    let secs = |t: std::time::Instant| t.saturating_duration_since(t0).as_secs_f64();
    // Copies the hook makes: (time, frame).
    let mut copies = Vec::new();
    let mut last = f64::NEG_INFINITY;
    for &(t, f) in shown {
        let t = secs(t);
        let elapsed = t - last;
        if elapsed >= interval {
            last = if elapsed > interval * 2.0 {
                t
            } else {
                last + interval
            };
            copies.push((t, f));
        }
    }
    let ideal: Vec<(f64, u64)> = ready.iter().map(|&(f, t)| (secs(t), f)).collect();
    let count_at = |events: &[(f64, u64)], phase: f64| -> (u32, u32) {
        let end = events.last().map_or(0.0, |e| e.0);
        let (mut rep, mut skip) = (0, 0);
        let mut prev: Option<u64> = None;
        let mut i = 0;
        let mut cur = None;
        let mut k = 1.0;
        while k * tick + phase <= end {
            let at = k * tick + phase;
            while i < events.len() && events[i].0 <= at {
                cur = Some(events[i].1);
                i += 1;
            }
            if let (Some(p), Some(c)) = (prev, cur) {
                if c == p {
                    rep += 1;
                } else if c > p + 1 {
                    skip += (c - p - 1) as u32;
                }
            }
            prev = cur;
            k += 1.0;
        }
        (rep, skip)
    };
    let count = |events: &[(f64, u64)]| -> (u32, u32) {
        let (mut r, mut s) = (0, 0);
        for n in 0..32 {
            let (a, b) = count_at(events, tick * n as f64 / 32.0);
            r += a;
            s += b;
        }
        (r * 10 / 32, s * 10 / 32)
    };
    (count(&copies), count(&ideal))
}

impl PacingProbe {
    fn new() -> Self {
        Self {
            on: std::env::var_os("ZL_STATS_LOG").is_some(),
            ..Default::default()
        }
    }

    fn record(
        &mut self,
        wait_us: u32,
        work_us: u32,
        present_us: u32,
        new_frame: bool,
        frame: (u64, Option<std::time::Instant>),
    ) {
        if !self.on {
            return;
        }
        let now = std::time::Instant::now();
        if frame.0 > 0 {
            self.shown.push((now, frame.0));
            if new_frame {
                if let Some(r) = frame.1 {
                    self.ready.push((frame.0, r));
                    self.delay.push((now - r).as_micros() as u32);
                }
            }
        }
        if let Some(l) = self.last_present.replace(now) {
            self.gaps.push((now - l).as_micros() as u32);
        }
        if new_frame {
            if let Some(l) = self.last_new.replace(now) {
                self.new_gaps.push((now - l).as_micros() as u32);
            }
        }
        self.wait_max = self.wait_max.max(wait_us);
        self.work_max = self.work_max.max(work_us);
        self.present_max = self.present_max.max(present_us);
        let since = *self.since.get_or_insert(now);
        if now - since < std::time::Duration::from_secs(1) || self.gaps.is_empty() {
            return;
        }
        let pct = |v: &mut Vec<u32>, q: f32| -> u32 {
            if v.is_empty() {
                return 0;
            }
            v.sort_unstable();
            v[((v.len() - 1) as f32 * q) as usize]
        };
        let p50 = pct(&mut self.gaps, 0.5);
        let long = self.gaps.iter().filter(|&&g| g * 2 > p50 * 3).count();
        let n50 = pct(&mut self.new_gaps, 0.5);
        let nlong = self.new_gaps.iter().filter(|&&g| g * 2 > n50 * 3).count();
        let ((rep, skip), (irep, iskip)) = obs_model(&self.shown, &self.ready, 60.0);
        log::info!(
            "obs-model 60fps (avg of 32 phases): capture repeated={:.1} skipped={:.1} | frames at decode time repeated={:.1} skipped={:.1} | decode-to-present p50={:.2}ms max={:.2}ms",
            rep as f32 / 10.0,
            skip as f32 / 10.0,
            irep as f32 / 10.0,
            iskip as f32 / 10.0,
            pct(&mut self.delay, 0.5) as f32 / 1000.0,
            pct(&mut self.delay, 1.0) as f32 / 1000.0,
        );
        self.shown.clear();
        self.ready.clear();
        self.delay.clear();
        log::info!(
            "pacing presents={} gap p50={:.2}ms p99={:.2}ms max={:.2}ms long={} | new frames={} gap p50={:.2}ms max={:.2}ms long={} | wait max={:.2}ms work max={:.2}ms present max={:.2}ms",
            self.gaps.len(),
            p50 as f32 / 1000.0,
            pct(&mut self.gaps, 0.99) as f32 / 1000.0,
            pct(&mut self.gaps, 1.0) as f32 / 1000.0,
            long,
            self.new_gaps.len(),
            n50 as f32 / 1000.0,
            pct(&mut self.new_gaps, 1.0) as f32 / 1000.0,
            nlong,
            self.wait_max as f32 / 1000.0,
            self.work_max as f32 / 1000.0,
            self.present_max as f32 / 1000.0,
        );
        self.since = Some(now);
        self.gaps.clear();
        self.new_gaps.clear();
        self.wait_max = 0;
        self.work_max = 0;
        self.present_max = 0;
    }
}

/// Copies `dest` (the picture's place in the back buffer) to the CPU.
unsafe fn read_back(
    gpu: &Gpu,
    back: &ID3D11Texture2D,
    dest: RECT,
    cw: u32,
    ch: u32,
) -> Option<Snapshot> {
    let l = dest.left.clamp(0, cw as i32) as u32;
    let t = dest.top.clamp(0, ch as i32) as u32;
    let r = dest.right.clamp(0, cw as i32) as u32;
    let b = dest.bottom.clamp(0, ch as i32) as u32;
    let (w, h) = (r.checked_sub(l)?, b.checked_sub(t)?);
    if w == 0 || h == 0 {
        return None;
    }
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    back.GetDesc(&mut desc);
    desc.Width = w;
    desc.Height = h;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    desc.MiscFlags = 0;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc.Count = 1;
    desc.SampleDesc.Quality = 0;
    let mut staging = None;
    gpu.device
        .CreateTexture2D(&desc, None, Some(&mut staging))
        .ok()?;
    let staging = staging?;
    let bx = D3D11_BOX {
        left: l,
        top: t,
        front: 0,
        right: r,
        bottom: b,
        back: 1,
    };
    gpu.context
        .CopySubresourceRegion(&staging, 0, 0, 0, 0, back, 0, Some(&bx));
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    gpu.context
        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
        .ok()?;
    let row = (w * 4) as usize;
    let mut bgra = Vec::with_capacity(row * h as usize);
    for y in 0..h as usize {
        let p = (m.pData as *const u8).add(y * m.RowPitch as usize);
        bgra.extend_from_slice(std::slice::from_raw_parts(p, row));
    }
    gpu.context.Unmap(&staging, 0);
    Some(Snapshot {
        width: w,
        height: h,
        bgra,
    })
}

fn render_loop(inner: &Inner) -> Result<()> {
    // A failed swap chain / processor step used to end this thread, which
    // froze the picture (seen after changing fit or rotation). Start over
    // instead, a few times a second at most.
    let mut errors = 0u32;
    while !inner.stop_render.load(Relaxed) {
        match render_session(inner) {
            Ok(()) => return Ok(()),
            Err(e) => {
                errors += 1;
                if errors <= 20 || errors.is_multiple_of(100) {
                    log::warn!("render: {e}; starting the swap chain again ({errors})");
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

fn render_session(inner: &Inner) -> Result<()> {
    let gpu = &inner.gpu;
    let hwnd = HWND(inner.hwnd as _);
    unsafe {
        let factory: IDXGIFactory2 = CreateDXGIFactory1()?;
        let (mut cw, mut ch) = (16u32, 16u32);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: cw,
            Height: ch,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            ..Default::default()
        };
        let sc1 = factory
            .CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)
            .inspect_err(|e| log::error!("swapchain: {e}"))?;
        let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        let sc: IDXGISwapChain2 = sc1.cast()?;
        sc.SetMaximumFrameLatency(1)?;
        let waitable: HANDLE = sc.GetFrameLatencyWaitableObject();
        let mut proc_: Option<Processor> = None;
        let mut shown_seq = u64::MAX;
        let mut probe = PacingProbe::new();
        // ZL_PRESENT=vblank: the v2.2.31 behaviour (present every refresh,
        // new frames only at a refresh), for A/B measurements.
        let on_arrival = std::env::var("ZL_PRESENT").map_or(true, |v| v != "vblank");
        let mut handled_resize = 0u64;
        let mut capture_hwnd = HWND(inner.capture_hwnd as _);
        let mut capture: Option<Capture> = None;

        while !inner.stop_render.load(Relaxed) {
            let t_wait = std::time::Instant::now();
            WaitForSingleObjectEx(waitable, 100, true);
            if on_arrival {
                // The swap chain has room for one present. Present the next
                // frame the moment it is decoded rather than at the next
                // refresh: the screen still shows at most one new frame per
                // refresh (the waitable above), but whatever copies the
                // back buffer at Present (OBS Game Capture) gets each frame
                // at its real time instead of rounded to the refresh. Idle:
                // present at least every 50 ms (resize handshake, OBS hook).
                let mut r = inner.ring.lock().unwrap();
                let cv = r.arrived.clone();
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
                while r.queue.is_empty()
                    && !inner.stop_render.load(Relaxed)
                    && inner.resize_asked.load(Relaxed) == handled_resize
                {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    r = cv.wait_timeout(r, left).unwrap().0;
                }
                handled_resize = inner.resize_asked.load(Relaxed);
            }
            let t_work = std::time::Instant::now();
            let mut new_frame = false;
            let mut shown_frame: (u64, Option<std::time::Instant>) = (0, None);
            // Size changes asked for up to now (read before the size, so a
            // newer one is not reported as shown).
            let asked = inner.resized.lock().unwrap().0;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (nw, nh) = (rc.right.max(1) as u32, rc.bottom.max(1) as u32);
            if (nw, nh) != (cw, ch) {
                proc_ = None;
                sc.ResizeBuffers(
                    0,
                    nw,
                    nh,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(desc.Flags as i32),
                )
                .inspect_err(|e| log::error!("resize {nw}x{nh}: {e}"))?;
                (cw, ch) = (nw, nh);
                shown_seq = u64::MAX;
            }
            if !IsWindowVisible(hwnd).as_bool() {
                std::thread::sleep(std::time::Duration::from_millis(30));
                continue;
            }
            let frame = {
                // One queued frame per refresh (see Ring): no frame is
                // thrown away just because two arrived close together.
                let mut r = inner.ring.lock().unwrap();
                let (slot, skipped) = r.next();
                if skipped > 0 {
                    inner.stats.replaced.fetch_add(skipped, Relaxed);
                }
                match slot.and_then(|i| r.slots[i].as_ref()) {
                    Some(s) => {
                        let (n, ready) = slot.map_or((0, None), |i| r.meta[i]);
                        shown_frame = (n, ready);
                        Some((s.tex.clone(), s.w, s.h, s.src, r.seq))
                    }
                    None => None,
                }
            };
            let back: ID3D11Texture2D = sc.GetBuffer(0)?;
            let bg = inner.bg.load(Relaxed);
            let (br, bgc, bb) = (
                ((bg >> 16) & 255) as f32 / 255.0,
                ((bg >> 8) & 255) as f32 / 255.0,
                (bg & 255) as f32 / 255.0,
            );
            let mut cap_frame = None;
            match frame {
                Some((tex, w, h, mut src, seq)) => {
                    let c: Vec<i32> = inner.crop.iter().map(|a| a.load(Relaxed) as i32).collect();
                    let (sw, sh) = (src.right - src.left, src.bottom - src.top);
                    src.left += c[0].min(sw / 2);
                    src.right -= c[2].min(sw / 2);
                    src.top += c[1].min(sh / 2);
                    src.bottom -= c[3].min(sh / 2);
                    // Presented as often as the picture (also when the frame repeats), so a
                    // capture never waits on a still screen.
                    cap_frame = Some((tex.clone(), w, h, src));
                    if proc_.as_ref().map(|p| p.key) != Some((w, h, cw, ch)) {
                        proc_ = match make_processor(gpu, w, h, cw, ch) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                log::warn!("processor {w}x{h} -> {cw}x{ch}: {e}");
                                drop(back);
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                continue;
                            }
                        };
                    }
                    let p = proc_.as_ref().unwrap();
                    let vc = &gpu.video_context;
                    let rot = inner.rotation.load(Relaxed);
                    let dest = picture_rect(
                        cw as i32,
                        ch as i32,
                        (src.right - src.left) as u32,
                        (src.bottom - src.top) as u32,
                        rot,
                        fit_of(inner.fit.load(Relaxed)),
                    );
                    let (src, dest) = clip_to_view(src, dest, cw as i32, ch as i32, rot);
                    let rotation = match rot {
                        90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
                        180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
                        270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
                        _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                    };
                    vc.VideoProcessorSetStreamRotation(&p.vp, 0, rot != 0, rotation);
                    vc.VideoProcessorSetStreamSourceRect(&p.vp, 0, true, Some(&src));
                    vc.VideoProcessorSetStreamDestRect(&p.vp, 0, true, Some(&dest));
                    let full = RECT {
                        left: 0,
                        top: 0,
                        right: cw as i32,
                        bottom: ch as i32,
                    };
                    vc.VideoProcessorSetOutputTargetRect(&p.vp, true, Some(&full));
                    let color = D3D11_VIDEO_COLOR {
                        Anonymous: D3D11_VIDEO_COLOR_0 {
                            RGBA: D3D11_VIDEO_COLOR_RGBA {
                                R: br,
                                G: bgc,
                                B: bb,
                                A: 1.0,
                            },
                        },
                    };
                    vc.VideoProcessorSetOutputBackgroundColor(&p.vp, false, &color);

                    let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: 0,
                            },
                        },
                    };
                    let mut iv = None;
                    if let Err(e) = gpu.video_device.CreateVideoProcessorInputView(
                        &tex,
                        &p.enumr,
                        &in_desc,
                        Some(&mut iv),
                    ) {
                        log::warn!("input view: {e}");
                        proc_ = None;
                        drop(back);
                        continue;
                    }
                    let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    };
                    let mut ov = None;
                    if let Err(e) = gpu.video_device.CreateVideoProcessorOutputView(
                        &back,
                        &p.enumr,
                        &out_desc,
                        Some(&mut ov),
                    ) {
                        log::warn!("output view: {e}");
                        proc_ = None;
                        drop(back);
                        continue;
                    }
                    let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                        Enable: true.into(),
                        pInputSurface: ManuallyDrop::new(iv),
                        ..Default::default()
                    };
                    let mut streams = [stream];
                    let r = vc.VideoProcessorBlt(&p.vp, ov.as_ref().unwrap(), 0, &streams);
                    ManuallyDrop::drop(&mut streams[0].pInputSurface);
                    if let Err(e) = r {
                        // Next frame builds a fresh processor.
                        log::warn!("blt: {e}");
                        proc_ = None;
                    }
                    if seq != shown_seq {
                        shown_seq = seq;
                        new_frame = true;
                    }
                    if inner.snap_asked.swap(false, Relaxed) {
                        let shot = read_back(gpu, &back, dest, cw, ch);
                        if shot.is_none() {
                            log::warn!("snapshot: read back failed");
                        }
                        *inner.snap.lock().unwrap() = shot;
                        inner.snap_cv.notify_all();
                    }
                }
                None => {
                    let mut rtv = None;
                    gpu.device
                        .CreateRenderTargetView(&back, None, Some(&mut rtv))
                        .inspect_err(|e| log::error!("rtv: {e}"))?;
                    gpu.context
                        .ClearRenderTargetView(rtv.as_ref().unwrap(), &[br, bgc, bb, 1.0]);
                }
            }
            if let (false, Some((tex, w, h, src))) = (capture_hwnd.is_invalid(), &cap_frame) {
                let rot = inner.rotation.load(Relaxed);
                if let Err(e) = present_capture(gpu, &factory, capture_hwnd, tex, *w, *h, *src, rot, &mut capture) {
                    // The main picture must not suffer for it: stop copying.
                    log::warn!("capture window: {e}");
                    capture_hwnd = HWND::default();
                    capture = None;
                }
            }
            drop(back);
            let t_present = std::time::Instant::now();
            if sc.Present(1, DXGI_PRESENT(0)).is_ok() {
                inner.stats.presented.fetch_add(1, Relaxed);
            }
            let us = |a: std::time::Instant, b: std::time::Instant| (b - a).as_micros() as u32;
            let t_end = std::time::Instant::now();
            probe.record(
                us(t_wait, t_work),
                us(t_work, t_present),
                us(t_present, t_end),
                new_frame,
                shown_frame,
            );
            let mut g = inner.resized.lock().unwrap();
            if g.1 < asked {
                g.1 = asked;
                inner.resized_cv.notify_all();
            }
        }
    }
    Ok(())
}

/// TikTok LIVE Studio's Game Capture only takes a swap chain that presents to the window
/// picked in its list, and the picture is a child window, so it took the OpenGL UI window
/// (black) instead. This copies each shown frame to a swap chain on a top-level window,
/// "ZenLink Game Capture", which TikTok can pick. OBS Game Capture keeps working either way.
unsafe fn present_capture(
    gpu: &Gpu,
    factory: &IDXGIFactory2,
    hwnd: HWND,
    tex: &ID3D11Texture2D,
    w: u32,
    h: u32,
    src: RECT,
    rot: u32,
    cap: &mut Option<Capture>,
) -> Result<()> {
    // 1:1 with the phone's own pixels (rotation and crop applied), not the view's size.
    let (sw, sh) = ((src.right - src.left).max(1) as u32, (src.bottom - src.top).max(1) as u32);
    let (ow, oh) = if rot == 90 || rot == 270 { (sh, sw) } else { (sw, sh) };
    unsafe {
        if cap.is_none() {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: ow,
                Height: oh,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let sc = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
            *cap = Some(Capture { sc, size: (ow, oh), proc_: None });
        }
        let c = cap.as_mut().unwrap();
        if c.size != (ow, oh) {
            c.proc_ = None;
            c.sc.ResizeBuffers(0, ow, oh, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
            c.size = (ow, oh);
        }
        if c.proc_.as_ref().map(|p| p.key) != Some((w, h, ow, oh)) {
            c.proc_ = Some(make_processor(gpu, w, h, ow, oh)?);
        }
        let p = c.proc_.as_ref().unwrap();
        let vc = &gpu.video_context;
        let rotation = match rot {
            90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
            180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
            270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
            _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
        };
        let full = RECT { left: 0, top: 0, right: ow as i32, bottom: oh as i32 };
        vc.VideoProcessorSetStreamRotation(&p.vp, 0, rot != 0, rotation);
        vc.VideoProcessorSetStreamSourceRect(&p.vp, 0, true, Some(&src));
        vc.VideoProcessorSetStreamDestRect(&p.vp, 0, true, Some(&full));
        vc.VideoProcessorSetOutputTargetRect(&p.vp, true, Some(&full));
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut iv = None;
        gpu.video_device.CreateVideoProcessorInputView(tex, &p.enumr, &in_desc, Some(&mut iv))?;
        let back: ID3D11Texture2D = c.sc.GetBuffer(0)?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut ov = None;
        gpu.video_device.CreateVideoProcessorOutputView(&back, &p.enumr, &out_desc, Some(&mut ov))?;
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(iv),
            ..Default::default()
        }];
        let r = vc.VideoProcessorBlt(&p.vp, ov.as_ref().unwrap(), 0, &streams);
        ManuallyDrop::drop(&mut streams[0].pInputSurface);
        drop(back);
        r?;
        // Never block the picture's render thread on this window (DWM shows none of it).
        let hr = c.sc.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
        if hr == DXGI_ERROR_WAS_STILL_DRAWING {
            return Ok(());
        }
        hr.ok()
    }
}

/// The capture window's swap chain and its own scaler (see `present_capture`).
struct Capture {
    sc: IDXGISwapChain1,
    size: (u32, u32),
    proc_: Option<Processor>,
}

/// Rounded rectangle of the view minus the holes (converted to the view's
/// own coordinates). The system owns the region after SetWindowRgn.
unsafe fn apply_region(hwnd: HWND, key: Bounds, holes: &[Hole]) {
    use windows::Win32::Graphics::Gdi::{
        CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, SetWindowRgn, RGN_DIFF,
    };
    let (x, y, w, h, _, radius) = key;
    unsafe {
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, radius * 2, radius * 2);
        for &(hx, hy, hw, hh) in holes {
            let (l, t) = (hx - x, hy - y);
            if hw <= 0 || hh <= 0 || l >= w || t >= h || l + hw <= 0 || t + hh <= 0 {
                continue;
            }
            let cut = CreateRectRgn(l, t, l + hw, t + hh);
            CombineRgn(Some(rgn), Some(rgn), Some(cut), RGN_DIFF);
            let _ = DeleteObject(cut.into());
        }
        SetWindowRgn(hwnd, Some(rgn), true);
    }
}
