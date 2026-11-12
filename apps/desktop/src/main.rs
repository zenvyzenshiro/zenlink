#![windows_subsystem = "windows"]
//! ZenLink (v2.5, generation "Next"): Rust shell (ADR-0001 Stage 1).

#[macro_use]
mod i18n;
mod aac;
mod airplay;
mod android;
mod apple;
mod appwin;
mod backdrop;
mod backend;
mod devprofiles;
mod dots;
mod drivers;
mod filedrop;
mod filewin;
mod flow;
mod ghost;
mod input;
mod ocr;
mod perf;
mod pinch;
mod prefs;
mod shortcuts;
mod tray;
mod updater;
mod usbdirect;
mod usbphones;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slint::winit_030::winit::platform::windows::{BackdropType, WindowAttributesExtWindows};
use slint::winit_030::winit::window::WindowLevel;
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, LogicalSize, Model};
use windows::Win32::Foundation::HWND;

use zenlink_core::api::Api;
use zenlink_video::{Fit, PointerEvent, VideoView};

use input::{Cmd, Input};

slint::include_modules!();

static DEMO_T0: OnceLock<Instant> = OnceLock::new();

/// Minimal file logger in `zenlink_core::log_dir()`. Each start begins a new
/// file; the previous run is kept as `<name>.prev.log` for bug reports.
struct FileLog(Mutex<std::fs::File>, Instant);

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        use std::io::Write;
        if self.enabled(r.metadata()) {
            let t = self.1.elapsed().as_secs_f64();
            let _ = writeln!(
                self.0.lock().unwrap(),
                "{t:>9.3}s [{}] {}: {}",
                r.level(),
                r.target(),
                r.args()
            );
        }
    }
    fn flush(&self) {}
}

/// Local wall-clock time "YYYY-MM-DD HH:MM:SS" (no date crate needed).
fn local_time() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn init_log() {
    init_log_named(zenlink_core::LOG_FILE_NAME);
}

/// App windows log to their own file, so they never truncate the main log.
fn init_log_named(name: &str) {
    use std::io::Write;
    let dir = zenlink_core::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    let _ = std::fs::rename(&path, path.with_extension("prev.log"));
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = writeln!(
            f,
            "ZenLink {} | started {} | {} | pid {}",
            env!("CARGO_PKG_VERSION"),
            local_time(),
            windows_version(),
            std::process::id()
        );
        let l: &'static FileLog = Box::leak(Box::new(FileLog(Mutex::new(f), Instant::now())));
        let _ = log::set_logger(l).map(|()| log::set_max_level(log::LevelFilter::Info));
    }
    // panic = "abort" ends the process without a word: put the reason in the log first.
    std::panic::set_hook(Box::new(|p| {
        let t = std::thread::current();
        log::error!("crash: thread '{}' panicked: {p}", t.name().unwrap_or("?"));
    }));
}

/// "Windows build 26200.1234 (25H2)" from the registry, for bug reports.
fn windows_version() -> String {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    let key = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let text = |name: PCWSTR| -> String {
        let mut buf = [0u16; 64];
        let mut len = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        }
        .is_ok();
        if !ok {
            return String::new();
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    };
    let mut ubr = 0u32;
    let mut len = 4u32;
    let _ = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut ubr as *mut u32).cast()),
            Some(&mut len),
        )
    };
    format!(
        "Windows build {}.{} ({})",
        text(w!("CurrentBuild")),
        ubr,
        text(w!("DisplayVersion"))
    )
}

#[derive(Default)]
struct App {
    view: Option<VideoView>,
    streaming: bool,
    device_rotation: u32,
    /// (shown w, shown h, settings open) the window was last fitted to.
    fitted: Option<(u32, u32, bool)>,
    /// Logical window size ZenLink itself last set while fitting; any other
    /// size while streaming means the user resized the window.
    fit_set: Option<(f32, f32)>,
    /// Picture size the user chose by resizing, relative to the default fit
    /// (0 = never resized). Kept when the picture turns or settings change.
    user_scale: f32,
    /// Picture prefs the window was fitted for (fit, rotation, crop, follow):
    /// only these may resize the window, not volume or other settings.
    fit_prefs: Option<String>,
    meter_at: Option<Instant>,
    /// Apple session went live at; cleared once a frame arrived or after the
    /// "no picture" hint (B-56).
    live_since: Option<Instant>,
    dark: bool,
    backdrop: Option<(bool, bool, i32, bool)>,
    on_top: Option<bool>,
    dots: Option<dots::Dots>,
    // Stage width (logical) to keep while the settings panel slides.
    hold_stage: Option<f32>,
    /// Settings slide: from, to, start, duration (B-22).
    slide: Option<(f32, f32, Instant, Duration)>,
    /// Device menu slide (v2.6): from, to, start, duration. Like Settings,
    /// the window widens on its right edge and the picture moves over.
    menu_slide: Option<(f32, f32, Instant, Duration)>,
    /// Side panel slide (Screen settings / Files), same shape.
    panel_slide: Option<(f32, f32, Instant, Duration)>,
    /// Device menu rows (v2.6): "a:<android key>" or "i:<apple id>", in
    /// row order, the selected one, and Android phones in a window of
    /// their own.
    rail_keys: Vec<String>,
    rail_sel: Option<String>,
    popped: Vec<String>,
    dots_at: Option<Instant>,
    /// A mode choice was just sent: keep the chooser closed until the
    /// backend's state catches up.
    mode_sent: Option<Instant>,
    /// A choice is waiting for the backend (it may still be starting).
    mode_pending: bool,
    /// Pending connection mode the "restart now" sheet was already shown
    /// for, so it asks once per change (from the chooser, Settings, a
    /// driver change or the API).
    restart_asked: Option<String>,
    /// Result of the last "Check drivers", shown under the service line.
    drivers_text: Option<String>,
    /// Device overlay ZenLink opened last ("control-center" / "notifications")
    /// and when. Forgotten after a click on the picture or [`PANEL_TTL`]:
    /// the user may have closed it on the device, and a stale "open" made
    /// the next press send the close swipe (swipe up from the bottom), which
    /// left the app instead.
    panel: Option<(&'static str, Instant)>,
    /// v2-style connect card: when Start was pressed (the backend may still
    /// say "idle" for a moment).
    flow_started: Option<Instant>,
    /// USB instance id of the device, for driver changes.
    drv_instance: Option<String>,
    /// What the open notice sheet is for ("drv:restore", "restart", "info").
    sheet: Option<String>,
    /// Full screen: where the window was before (position, size, maximised).
    full_restore: Option<(slint::PhysicalPosition, slint::PhysicalSize, bool)>,
    /// Startup navigation: platform screen, Apple or Android (phase A2).
    flow: Option<flow::Flow>,
    /// The backend's last "needsChoice" (None until it first answered).
    apple_needs: Option<bool>,
    /// Android session (phase A3); `None` when none runs.
    android: Option<Arc<android::Shared>>,
    /// Its source, kept across decoder threads so a parked session comes back as it was.
    android_src: Option<Arc<Mutex<Box<dyn zenlink_core::source::VideoSource>>>>,
    /// The Android picture is live (the window shows video, fits to it).
    android_live: bool,
    /// Android status last shown in the window.
    android_shown: Option<android::Status>,
    /// USB / Wi-Fi of the device being mirrored.
    android_link: String,
    /// Which list entry was started (-1 = first ready), for Restart.
    android_index: i32,
    /// Height the first sizing chose, and when (see clamp_to_work_area).
    start_h: Option<(f32, Instant)>,
    /// Apple phase seen last (auto unlock fires on the step to streaming).
    apple_phase: String,
    /// When to send the auto-unlock (Apple Driver mode, picture running).
    unlock_at: Option<Instant>,
    /// Whether the backend was asked if a passcode is saved.
    passcode_asked: bool,
    /// Link the user picked for a phone on both USB and Wi-Fi
    /// (Some(true) = Wi-Fi); kept for restarts.
    android_via: Option<bool>,
    /// Settings the running Android session started with (sound target,
    /// picture options); a change restarts it.
    android_key: Option<String>,
    /// A source waiting for the video view to exist.
    android_pending: Option<Box<dyn zenlink_core::source::VideoSource>>,
    /// Devices the Android workspace shows, in row order.
    android_list: Vec<zenlink_android::discovery::AndroidDevice>,
    /// App windows (v2.5.1): the device whose apps the picker shows, its
    /// apps (cached per device key) and the rows shown after the filter.
    apps_device: Option<zenlink_android::discovery::AndroidDevice>,
    apps_cache: std::collections::HashMap<String, Vec<zenlink_android::apps::DeviceApp>>,
    apps_shown: Vec<zenlink_android::apps::DeviceApp>,
    /// ZL_ANDROID_FIXTURE: started once when the Android workspace opens.
    fixture_started: bool,
    /// Per-device Android settings (phase A5), loaded on first use.
    android_store: Option<zenlink_android::profiles::Store>,
    /// Stable key of the device last started: Settings edits go to it.
    android_dev: Option<String>,
    /// Encoder lists read this run, per device key (skips the query on
    /// the next start of the same phone).
    android_encoders: std::collections::HashMap<String, Vec<zenlink_android::encoders::Encoder>>,
    /// This session's working configuration was saved (lastGood).
    android_good_saved: bool,
}

/// ZenLink's own full screen (the window covers the monitor). winit's full
/// screen left Windows' glass blur grey afterwards (light theme after dark)
/// until the app restarted.
fn is_full() -> bool {
    APP.with_borrow(|a| a.full_restore.is_some())
}

/// Physical bounds of the monitor the window is on.
/// The first time the window is on screen: 85% of the work area's height
/// and up to 55% of its width (the owner found the old start too small and
/// too narrow), kept inside the work area on small or high-DPI screens.
fn clamp_to_work_area(ui: &AppWindow) {
    let Some((wx, wy, ww, wh)) = work_area(ui) else {
        return;
    };
    let cur = ui.window().size().to_logical(ui.window().scale_factor());
    let h = cur.height.max(wh * 0.85).min(wh * 0.95);
    // Other resizes in the next ticks still read the old height (the size
    // arrives with a later event): they keep at least this one for 2 s.
    APP.with_borrow_mut(|a| a.start_h = Some((h, Instant::now())));
    // Room for the device panel and Settings side by side: 55% of the
    // screen width, at most 1.35 x the height.
    let w = cur.width.max((ww * 0.55).min(h * 1.35)).min(ww * 0.95);
    if (w, h) != (cur.width, cur.height) {
        ui.window().set_size(LogicalSize::new(w, h));
    }
    // Open in the middle of the screen (Windows' default put it at the top left).
    let (x, y) = centered((wx, wy, ww, wh), (w, h));
    ui.window().set_position(slint::LogicalPosition::new(x, y));
}

/// Top-left corner that centres a `w` x `h` window in the work area.
fn centered((wx, wy, ww, wh): (f32, f32, f32, f32), (w, h): (f32, f32)) -> (f32, f32) {
    (
        (wx + (ww - w) / 2.0).max(wx).round(),
        (wy + (wh - h) / 2.0).max(wy).round(),
    )
}

/// CSS `cubic-bezier(0.05, 0.7, 0.1, 1)` (the v2.2.31 settings slide curve)
/// at time `x` in 0..=1.
fn slide_ease(x: f32) -> f32 {
    let (x1, y1, x2, y2) = (0.05f32, 0.7f32, 0.1f32, 1.0f32);
    let bez = |t: f32, a: f32, b: f32| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    // x(t) is monotonic: bisection finds t for x.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if bez(mid, x1, x2) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bez((lo + hi) / 2.0, y1, y2)
}

pub(crate) fn monitor_rect(w: &slint::Window) -> Option<(i32, i32, u32, u32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let h = hwnd_of(w)?;
    unsafe {
        let m = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(m, &mut mi).as_bool() {
            return None;
        }
        let r = mi.rcMonitor;
        Some((
            r.left,
            r.top,
            (r.right - r.left) as u32,
            (r.bottom - r.top) as u32,
        ))
    }
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::default());
    static START: Instant = Instant::now();
}

fn hwnd_of(w: &slint::Window) -> Option<HWND> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    w.with_winit_window(|ww| match ww.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as _)),
        _ => None,
    })
    .flatten()
}

/// Windows "app mode" (Settings > Personalization > Colors).
fn system_dark() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut v = 1u32;
    let mut n = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut _ as _),
            Some(&mut n),
        );
    }
    v == 0
}

fn apply_look(ui: &AppWindow) {
    let p = ui.get_prefs();
    let dark = match p.theme.as_str() {
        "dark" => true,
        "system" => system_dark(),
        _ => false,
    };
    ui.global::<Theme>().set_dark(dark);
    // Slider -> thickness (prefs::glass_level); 100% = no glass. Clarity: 1.0 clear .. 0.25.
    let (level_on, thick) = prefs::glass_level(p.glass);
    let glass_on = p.glass_on && level_on;
    ui.global::<Theme>()
        .set_glass(if glass_on { 1.0 - 0.75 * thick } else { 0.0 });
    set_lang(&ui.global::<I18n>(), p.lang.as_str());
    let glass_k = thick;
    // Square corners in full screen / maximised (rounded ones show the desktop).
    let full = is_full();
    let round = !full && !ui.window().is_maximized();
    APP.with_borrow_mut(|a| {
        a.dark = dark;
        let glass_q = (glass_k * 20.0).round() as i32; // re-apply in 5% steps
                                                       // Full screen paints a solid background, so no blur there. A dark
                                                       // blur kept through full screen came back grey afterwards (also in
                                                       // light theme) until the app restarted; turning it off avoids that.
        let blur = glass_on && !full;
        if a.backdrop != Some((blur, dark, glass_q, round)) {
            if let Some(h) = hwnd_of(ui.window()) {
                let mode = if blur {
                    backdrop::Backdrop::Accent
                } else {
                    backdrop::Backdrop::None
                };
                backdrop::apply(h, mode, dark, glass_k, round);
                a.backdrop = Some((blur, dark, glass_q, round));
            }
        }
        if a.on_top != Some(p.on_top) {
            let lvl = if p.on_top {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            };
            ui.window().with_winit_window(|w| w.set_window_level(lvl));
            a.on_top = Some(p.on_top);
        }
    });
}

/// Whether a passcode is saved, or saves one (`Some`; an empty string removes it). Off the
/// UI thread.
fn passcode_query(ui: &AppWindow, save: Option<String>) {
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let set = match &save {
            Some(code) => apple::save_passcode(code),
            None => Some(apple::has_passcode()),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match set {
                Some(s) => {
                    ui.set_passcode_set(s);
                    if save.is_some() {
                        let msg = if s {
                            i18n::tr("Passcode saved (encrypted on this PC)")
                        } else {
                            i18n::tr("Passcode removed")
                        };
                        show_toast(&ui, msg);
                    }
                }
                None if save.is_some() => show_toast(&ui, i18n::tr("Couldn't save the passcode")),
                None => APP.with_borrow_mut(|a| a.passcode_asked = false),
            }
        });
    });
}

fn push_audio(p: &Prefs, muted: bool) {
    let to_pc = p.audio_out.as_str() != "device";
    android::set_audio(to_pc, p.volume / 100.0, muted);
    apple::set_audio(to_pc, p.volume / 100.0, muted);
    apple::set_unlock_on_lock(p.unlock_on_lock, p.unlock_limit);
}

fn str_of<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_str()
}

/// Loads language `code` and points a window's texts at it (each window has
/// its own I18n global). An unknown code falls back to English.
pub fn set_lang(g: &I18n, code: &str) {
    let langs = i18n::languages();
    let code = if langs.iter().any(|(c, _)| c == code) {
        code
    } else {
        "en"
    };
    i18n::set(code);
    let list = |f: fn(&(String, String)) -> &String| {
        let v: Vec<slint::SharedString> = langs.iter().map(|l| f(l).into()).collect();
        slint::ModelRc::new(slint::VecModel::from(v))
    };
    g.set_codes(list(|l| &l.0));
    g.set_names(list(|l| &l.1));
    g.on_lookup(|text, _lang, value| {
        let s = i18n::tr(&text);
        match (s.find('{'), s.find('}')) {
            (Some(a), Some(b)) if !value.is_empty() && a < b => {
                format!("{}{value}{}", &s[..a], &s[b + 1..])
            }
            _ => s,
        }
        .into()
    });
    g.set_index(langs.iter().position(|(c, _)| c == code).unwrap_or(0) as i32);
    g.set_lang(code.into());
}

/// Backend messages are English; the language file translates them.
fn tr_backend(en: &str) -> String {
    match en.strip_prefix("Wi-Fi setup failed: ") {
        Some(reason) => trf!("Wi-Fi setup failed: {reason}", reason = reason),
        None => i18n::tr(en),
    }
}

/// The Apple device list: connected devices first, then devices trusted
/// before that are not connected now. `current` is the one Start uses.
fn apple_rows(ui: &AppWindow, st: &Value, current: &str) -> Vec<AppleDev> {
    let mut rows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for d in st
        .get("devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = str_of(d, &["id"]) else {
            continue;
        };
        if id == "airplay" || !seen.insert(id.to_string()) {
            continue;
        }
        let model = str_of(d, &["name"]).unwrap_or("iPhone/iPad");
        let title = str_of(d, &["deviceName"]).unwrap_or(model);
        let wifi = d.get("wifi").and_then(Value::as_bool).unwrap_or(false);
        let ready = d.get("ready").and_then(Value::as_bool).unwrap_or(false);
        let need = str_of(d, &["need"]).unwrap_or("");
        let how = if wifi { "Wi-Fi" } else { "USB" };
        let status = match need {
            "" if ready => i18n::tr("ready"),
            "" | "trust" => i18n::tr("unlock + Trust"),
            "wifi" => i18n::tr("Wi-Fi not set up"),
            "mode" => i18n::tr("choose a mode"),
            "restore" => i18n::tr("ZenLink Studio driver"),
            _ => i18n::tr("needs a driver"),
        };
        let os = str_of(d, &["os"])
            .map(|v| format!(" · iOS {v}"))
            .unwrap_or_default();
        let sub = if title == model {
            format!("{how}{os} · {status}")
        } else {
            format!("{model} · {how}{os} · {status}")
        };
        rows.push(AppleDev {
            id: id.into(),
            title: title.into(),
            sub: sub.into(),
            state: if d.get("live").and_then(Value::as_bool) == Some(true) {
                "live" // a session runs, also off screen
            } else if ready && need.is_empty() {
                "ready"
            } else {
                "need"
            }
            .into(),
            wifi,
            selected: id == current,
        });
    }
    for d in st
        .get("known")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = str_of(d, &["id"]) else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        let model = str_of(d, &["name"]).unwrap_or("iPhone/iPad");
        let title = str_of(d, &["deviceName"]).unwrap_or(model);
        let where_ = if ui.get_mode_link().as_str() == "wireless" {
            i18n::tr("not seen on Wi-Fi")
        } else {
            i18n::tr("not connected")
        };
        let sub = if title == model {
            trf!("Saved · {where_}", where_ = where_)
        } else {
            trf!("{model} · saved · {where_}", model = model, where_ = where_)
        };
        rows.push(AppleDev {
            id: id.into(),
            title: title.into(),
            sub: sub.into(),
            state: "offline".into(),
            wifi: false,
            selected: id == str_of(st, &["selected"]).unwrap_or("") && current.is_empty(),
        });
    }
    rows
}

/// Apple Driver mode (apple.rs) and ZenLink Studio (airplay.rs, usbdirect.rs) run in Rust.
fn cast_start(api: &Api) {
    if !airplay::start() && !usbdirect::start() && !apple::start() {
        api.start_cast();
    }
}

fn cast_stop(api: &Api) {
    if airplay::stop() || usbdirect::stop() {
        return;
    }
    if apple::in_use() {
        apple::engine().stop();
    } else {
        api.stop_cast();
    }
}

fn start_apple_video(v: &mut VideoView) {
    if airplay::running() {
        v.start_source(Box::new(airplay::AirSource));
    } else if usbdirect::running() {
        v.start_source(Box::new(usbdirect::UsbSource));
    } else if apple::in_use() {
        v.start_source(apple::engine().source());
    }
}

/// Ends every session (a link change, a mode restart, quitting).
pub fn cast_stop_all() {
    airplay::stop();
    usbdirect::stop();
    if apple::in_use() {
        apple::engine().stop();
    }
}

/// Backend state -> UI. Runs on the UI thread.
fn on_state(ui: &AppWindow, st: &Value) {
    let phase = str_of(st, &["phase"]).unwrap_or("idle").to_string();
    // The backend starts the picked device when it is connected, otherwise
    // the first ready one; show that one.
    let list = st.get("devices").and_then(Value::as_array);
    let selected = str_of(st, &["selected"]).unwrap_or("");
    let first = list
        .and_then(|l| l.iter().find(|d| str_of(d, &["id"]) == Some(selected)))
        .or_else(|| {
            list.and_then(|l| {
                l.iter()
                    .find(|d| d.get("ready").and_then(Value::as_bool).unwrap_or(false))
            })
        })
        .or_else(|| list.and_then(|l| l.first()));
    let first_id = first.and_then(|d| str_of(d, &["id"])).unwrap_or("");
    let rows = apple_rows(ui, st, first_id);
    ui.set_apple_devices(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    rail_refresh(ui);
    let device = str_of(st, &["device", "name"])
        .or_else(|| first.and_then(|d| str_of(d, &["name"])))
        .unwrap_or("");
    let wifi = first
        .and_then(|d| d.get("wifi"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let backend = str_of(st, &["capture", "backend"])
        .or_else(|| str_of(st, &["connection", "active"]))
        .unwrap_or("");
    let link = match backend {
        "usbdirect" => "ZenLink Studio".to_string(),
        "apple" | "hybrid" => format!("Apple · {}", if wifi { "Wi-Fi" } else { "USB" }),
        _ if wifi => "Wi-Fi".into(),
        _ if !device.is_empty() => "USB".into(),
        _ => String::new(),
    };
    let message = if phase == "error" {
        tr_backend(str_of(st, &["error"]).unwrap_or(""))
    } else {
        String::new()
    };
    // Auto unlock: Apple Driver mode only (the backend refuses Studio),
    // once the picture runs, if a passcode is saved and the option is on.
    let was = APP.with_borrow_mut(|a| std::mem::replace(&mut a.apple_phase, phase.clone()));
    if phase == "streaming"
        && was != "streaming"
        && matches!(backend, "apple" | "hybrid")
        && ui.get_prefs().auto_unlock
        && ui.get_passcode_set()
    {
        APP.with_borrow_mut(|a| a.unlock_at = Some(Instant::now() + Duration::from_millis(1500)));
    }
    if !APP.with_borrow_mut(|a| std::mem::replace(&mut a.passcode_asked, true)) {
        passcode_query(ui, None);
    }
    ui.set_phase(phase.clone().into());
    ui.set_device(device.into());
    ui.set_link(link.into());
    ui.set_message(message.into());
    let conn = match str_of(st, &["connection", "active"]) {
        Some("apple") => i18n::tr("Apple Driver (Standard)"),
        Some("usbdirect") => "ZenLink Studio".into(),
        _ => i18n::tr("Not chosen yet"),
    };
    ui.set_connection(conn.into());
    // Connection mode chooser: nothing connects until a mode is chosen.
    let c = st.get("connection");
    let needs = c
        .and_then(|c| c.get("needsChoice"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    APP.with_borrow_mut(|a| a.apple_needs = Some(needs));
    ui.set_apple_detected(!device.is_empty());
    // Only the Apple workspace may show the Apple chooser (flow.rs).
    let on_apple = APP.with_borrow_mut(|a| match a.flow.as_mut() {
        Some(f) => {
            if needs {
                f.backend_needs_choice();
            }
            f.apple_active()
        }
        None => false,
    });
    ui.set_mode_active(str_of(st, &["connection", "active"]).unwrap_or("").into());
    ui.set_mode_link(
        str_of(st, &["connection", "link"])
            .unwrap_or("wired")
            .into(),
    );
    ui.set_mode_remember(
        c.and_then(|c| c.get("remember"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
            || needs,
    );
    let just_sent = APP.with_borrow(|a| {
        a.mode_pending
            || a.mode_sent
                .map(|t| t.elapsed() < Duration::from_secs(3))
                .unwrap_or(false)
    });
    if needs && !just_sent && on_apple && !ui.get_chooser_open() {
        ui.set_chooser_cancelable(false);
        open_chooser(ui);
    }
    // Opened at launch before the backend answered, but a remembered mode
    // is already active: nothing to choose.
    if !needs && !just_sent && ui.get_chooser_open() && !ui.get_chooser_cancelable() {
        ui.set_chooser_open(false);
    }
    let pending = str_of(st, &["connection", "pending"]);
    if pending.is_none() {
        APP.with_borrow_mut(|a| a.restart_asked = None);
    }
    if let Some(p) = pending {
        let name = if p == "apple" {
            "Apple Driver"
        } else {
            "ZenLink Studio"
        };
        // A new mode only applies after the service restarts: offer it once,
        // not just the note in the connection line.
        let ask = !ui.get_sheet_open()
            && !ui.get_chooser_open()
            && APP.with_borrow(|a| a.restart_asked.as_deref() != Some(p));
        if ask {
            APP.with_borrow_mut(|a| a.restart_asked = Some(p.to_string()));
            open_sheet(
                ui,
                "restart",
                i18n::tr("Restart to switch the mode"),
                trf!("{name} is used after the ZenLink service restarts. Mirroring that is running stops.", name = name),
                i18n::tr("Restart now"),
                i18n::tr("Later"),
            );
        }
        ui.set_connection(
            format!(
                "{} · {}",
                ui.get_connection(),
                trf!("{name} after an app restart", name = name)
            )
            .into(),
        );
    }
    let service = st.get("service").and_then(Value::as_bool).unwrap_or(true);
    let native_audio = st
        .get("nativeAudio")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let checked = APP.with_borrow(|a| {
        a.drivers_text
            .clone()
            .map(|t| format!("\n{t}"))
            .unwrap_or_default()
    });
    ui.set_driver_text(
        format!(
            "Apple Mobile Device Service: {}\n{}: {}{checked}",
            if service {
                i18n::tr("running")
            } else {
                i18n::tr("not running")
            },
            i18n::tr("Native audio"),
            if native_audio {
                i18n::tr("available")
            } else {
                i18n::tr("not available")
            },
        )
        .into(),
    );
    // Connect card (1-2-3-4) like v2. AirPlay / Hybrid wait for the device
    // to pick ZenLink in Screen Mirroring: no card then (the guide shows).
    let airplay = first
        .map(|d| {
            d.get("airplay").and_then(Value::as_bool).unwrap_or(false)
                || d.get("hybrid").and_then(Value::as_bool).unwrap_or(false)
        })
        .unwrap_or(false);
    let kind = if str_of(st, &["connection", "active"]) == Some("usbdirect") {
        "usbdirect"
    } else {
        "apple"
    };
    ui.set_flow_kind(kind.into());
    // ZenLink Studio over Wi-Fi: the receiver waits for the device to pick it.
    let receiver = first
        .and_then(|d| d.get("airplay"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let listening = receiver && matches!(phase.as_str(), "connecting" | "activating");
    ui.set_apple_listening(listening);
    // Waiting for AirPlay: say so when Windows Firewall keeps the device out (checked every 3 s).
    thread_local!(static FW: std::cell::Cell<Option<(Instant, bool)>> = const { std::cell::Cell::new(None) });
    let blocked = listening
        && FW.with(|c| match c.get() {
            Some((at, b)) if at.elapsed() < Duration::from_secs(3) => b,
            _ => {
                let b = drivers::firewall_blocked();
                c.set(Some((Instant::now(), b)));
                b
            }
        });
    ui.set_firewall_blocked(blocked);
    ui.set_device_wifi(
        wifi || str_of(st, &["device", "wifi"]).is_some()
            && st["device"]["wifi"].as_bool().unwrap_or(false),
    );
    // What the device still needs before Start can work ("" = ready).
    let need = first
        .map(|d| match d.get("need").and_then(Value::as_str) {
            Some(n) => n.to_string(),
            None if !d.get("ready").and_then(Value::as_bool).unwrap_or(true) => "trust".into(),
            None => String::new(),
        })
        .unwrap_or_default();
    ui.set_device_need(need.into());
    // Driver buttons follow the device's driver (like v2).
    let drv = st.get("drivers");
    let dev = drv.and_then(|d| d.get("devices")).and_then(|d| d.get(0));
    let lib = drv
        .and_then(|d| d.pointer("/libusb0/available"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unsafe_filter = dev
        .and_then(|d| d.get("unsafeFilter"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let dmode = dev.and_then(|d| str_of(d, &["mode"])).unwrap_or("");
    ui.set_drv_filter(dev.is_some() && unsafe_filter);
    ui.set_drv_restore(
        dev.is_some() && !unsafe_filter && (dmode == "replaced" || dmode == "unknown"),
    );
    ui.set_drv_bind(dev.is_some() && !unsafe_filter && dmode == "apple" && lib);
    APP.with_borrow_mut(|a| {
        a.drv_instance = dev.and_then(|d| str_of(d, &["instance"])).map(String::from)
    });
    let fresh = APP.with_borrow(|a| {
        a.flow_started
            .map(|t| t.elapsed() < Duration::from_secs(3))
            .unwrap_or(false)
    });
    let step = ui.get_flow_step();
    match phase.as_str() {
        "connecting" | "activating" if airplay => ui.set_flow_step(-1),
        "connecting" | "activating" => {
            let n = if phase == "connecting" { 1 } else { 2 };
            ui.set_flow_failed(false);
            ui.set_flow_step(step.max(n));
        }
        "streaming" => {
            ui.set_flow_step(-1);
            ui.set_flow_failed(false);
            APP.with_borrow_mut(|a| a.flow_started = None);
        }
        "error" if step >= 0 => ui.set_flow_failed(true),
        _ if step >= 0 && !fresh && !ui.get_flow_failed() => ui.set_flow_step(-1),
        _ => {}
    }
    let rot = st.get("rotation").and_then(Value::as_u64).unwrap_or(0) as u32;
    let live = phase == "streaming";
    // iOS 27 sends H.265 only: without a HEVC decoder the window stays black (B-58).
    let needs_hevc = live
        && str_of(st, &["capture", "codec"]).is_some_and(|c| c.eq_ignore_ascii_case("hevc"))
        && !APP.with_borrow(|a| a.streaming)
        && !hevc_ok();
    if needs_hevc {
        log::warn!("apple: the stream is H.265 but Windows has no HEVC decoder");
        open_sheet(
            ui,
            "hevc",
            i18n::tr("H.265 (HEVC) decoder missing"),
            i18n::tr("This iPhone/iPad sends its screen as H.265, and this Windows has no HEVC decoder, so the picture stays black. Install the free \"HEVC Video Extensions from Device Manufacturer\" from the Microsoft Store, then press Restart. Or use ZenLink Studio, which sends H.264."),
            i18n::tr("Open Microsoft Store"),
            i18n::tr("Later"),
        );
    }
    APP.with_borrow_mut(|a| {
        a.device_rotation = if [0, 90, 180, 270].contains(&rot) {
            rot
        } else {
            0
        };
        if live != a.streaming {
            a.streaming = live;
            a.fitted = None;
            if let Some(v) = a.view.as_mut() {
                if live {
                    start_apple_video(v);
                } else {
                    v.stop();
                }
            }
            a.live_since = live.then(Instant::now);
            if live {
                log::info!("apple: streaming ({backend}, device {device:?})");
                push_audio(&ui.get_prefs(), ui.get_muted());
            }
        }
    });
}

fn effective_rotation(p: &Prefs, shown: (u32, u32), device_rotation: u32) -> u32 {
    match p.rotation.as_str() {
        "auto" => {
            if shown.0 > shown.1 {
                0
            } else {
                device_rotation
            }
        }
        r => r.parse().unwrap_or(0),
    }
}

/// Logical work area of the monitor the window is on.
fn work_area(ui: &AppWindow) -> Option<(f32, f32, f32, f32)> {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let h = hwnd_of(ui.window())?;
    let sf = ui.window().scale_factor();
    unsafe {
        let m = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(m, &mut mi).as_bool() {
            return None;
        }
        let r = mi.rcWork;
        Some((
            r.left as f32 / sf,
            r.top as f32 / sf,
            (r.right - r.left) as f32 / sf,
            (r.bottom - r.top) as f32 / sf,
        ))
    }
}

/// Size the window around the picture (portrait or landscape).
/// Gap between the picture and the glass stage edge (logical px).
const PIC_INSET: f32 = 6.0;
/// Narrowest window that still fits the header (logo, name, buttons,
/// window controls).
const MIN_W: f32 = 470.0;
/// How long ZenLink trusts that an overlay it opened is still open.
const PANEL_TTL: Duration = Duration::from_secs(20);

/// Stage width (logical px) while there is no picture yet: the smallest
/// one; [`idle_stage_w`] widens it on larger screens.
const IDLE_STAGE_W: f32 = 440.0;

/// Idle stage width for this screen: 42% of the work area, 440-820 px
/// (the owner found 440 px next to Settings too narrow on a large screen,
/// and asked for a wider start again with the device menu, 2026-11-09).
fn idle_stage_w(ui: &AppWindow) -> f32 {
    work_area(ui)
        .map(|(_, _, ww, _)| (ww * 0.42).clamp(IDLE_STAGE_W, 820.0))
        .unwrap_or(IDLE_STAGE_W)
}

/// Picture prefs that change the window's fitted size.
fn fit_prefs_key(p: &Prefs) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        p.fit, p.rotation, p.fit_window, p.crop_l, p.crop_t, p.crop_r, p.crop_b
    )
}

/// Default fitted picture size (logical px, without chrome) for `shown`.
fn default_fit(ui: &AppWindow, shown: (u32, u32)) -> Option<(f32, f32)> {
    let (_, _, ww, wh) = work_area(ui)?;
    let chrome_w = ui.get_chrome_w() + 2.0 * PIC_INSET;
    let chrome_h = ui.get_chrome_h() + 2.0 * PIC_INSET;
    let aspect = shown.0 as f32 / shown.1.max(1) as f32;
    // The window takes up to 85% of the screen's height (was 62%: too small).
    let max_h = wh * 0.85 - chrome_h;
    let max_w = ww * 0.6 - chrome_w;
    let (mut sw, mut sh) = (max_h * aspect, max_h);
    if sw > max_w {
        sw = max_w;
        sh = sw / aspect;
    }
    Some((sw.max(1.0), sh.max(1.0)))
}

/// Fit the window to the picture. `scale` > 0: the size the user chose by
/// resizing, relative to the default fit (kept within the screen).
fn fit_window(ui: &AppWindow, shown: (u32, u32), scale: f32) -> Option<(f32, f32)> {
    let (wx, wy, ww, wh) = work_area(ui)?;
    let chrome_w = ui.get_chrome_w() + 2.0 * PIC_INSET;
    let chrome_h = ui.get_chrome_h() + 2.0 * PIC_INSET;
    let (mut sw, mut sh) = default_fit(ui, shown)?;
    if scale > 0.0 {
        let k = scale
            .min((ww - chrome_w) / sw)
            .min((wh - chrome_h) / sh)
            .max(0.2);
        sw *= k;
        sh *= k;
    }
    let size = LogicalSize::new((sw + chrome_w).max(MIN_W), (sh + chrome_h).max(440.0));
    ui.window().set_size(size);
    keep_on_screen(ui, (wx, wy, ww, wh), size);
    Some((size.width, size.height))
}

/// Show the connection mode chooser, widening the window so the sheet fits.
fn open_chooser(ui: &AppWindow) {
    ui.set_chooser_open(true);
    widen_for_sheet(ui);
}

/// Widens the window so a full-window sheet (mode chooser, platform screen) fits.
fn widen_for_sheet(ui: &AppWindow) {
    let Some((wx, wy, ww, wh)) = work_area(ui) else {
        return;
    };
    let cur = ui.window().size().to_logical(ui.window().scale_factor());
    // A slide still running (Home from a mirror with a panel open) would end on
    // the narrow mirror stage: let it end on the idle stage instead.
    let idle = idle_stage_w(ui);
    APP.with_borrow_mut(|a| {
        if let Some(s) = a.hold_stage.as_mut() {
            *s = s.max(idle);
        }
    });
    // Never narrower than the idle stage plus the open settings panel (the
    // window may not have taken that size yet when this runs at launch).
    let want = if ui.get_settings_open() || ui.get_menu_open() {
        idle_stage_w(ui) + 16.0 + 368.0 * ui.get_sp() + 216.0 * ui.get_mp()
    } else {
        600.0
    };
    let w = cur.width.max(want.max(600.0).min(ww - 16.0));
    let start = APP
        .with_borrow(|a| a.start_h)
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(2))
        .map_or(0.0, |(h, _)| h);
    let h = cur.height.max(start).max(820.0f32.min(wh - 16.0));
    if (w, h) != (cur.width, cur.height) {
        let size = LogicalSize::new(w, h);
        ui.window().set_size(size);
        keep_on_screen(ui, (wx, wy, ww, wh), size);
    }
}

/// The data folder (settings.json, devices.json): a source checkout uses <repo>\data (or
/// ZENLINK_DATA_DIR), a portable folder its own folder, an installed build
/// %LOCALAPPDATA%\ZenLink-Next.
pub fn backend_data_dir() -> Option<std::path::PathBuf> {
    // Source checkout started by Start ZenLink.bat: ZENLINK_SOURCE names it.
    let source = std::env::var_os("ZENLINK_SOURCE")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    let data = std::env::var_os("ZENLINK_DATA_DIR")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    if let Ok(exe) = std::env::current_exe() {
        let dirs = source
            .into_iter()
            .chain(exe.ancestors().skip(1).map(|d| d.to_path_buf()));
        for dir in dirs {
            if dir.join("apps").join("desktop").join("Cargo.toml").exists() {
                return Some(data.unwrap_or_else(|| dir.join("data")));
            }
            if dir.join("portable.txt").exists() {
                return Some(dir);
            }
        }
    }
    zenlink_core::local_data_dir()
}

/// True when the backend's settings.json remembers a connection mode.
fn mode_remembered() -> bool {
    backend_data_dir()
        .and_then(|d| std::fs::read(d.join("settings.json")).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| {
            v.get("rememberConnectionMode")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && matches!(
                    v.get("connectionMode").and_then(Value::as_str),
                    Some("apple" | "usbdirect")
                )
        })
}

/// Short message above the dock for 3.5 s.
fn show_toast(ui: &AppWindow, text: String) {
    ui.set_toast("".into());
    ui.set_toast(text.into());
    let w = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(3500), move || {
        if let Some(ui) = w.upgrade() {
            ui.set_toast("".into());
        }
    });
}

/// "Check drivers" result, one line per device.
fn drivers_summary(d: &Value) -> String {
    let mut lines = Vec::new();
    for dev in d
        .get("devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let fam = dev
            .get("family")
            .and_then(Value::as_str)
            .unwrap_or("iPhone");
        let mode = match dev.get("mode").and_then(Value::as_str).unwrap_or("") {
            "apple" => "Apple Driver",
            "replaced" => "ZenLink Studio (libusb0)",
            "coexist" => &i18n::tr("mixed, needs repair"),
            _ => &i18n::tr("unknown"),
        };
        lines.push(format!("{fam}: {mode}"));
    }
    if lines.is_empty() {
        lines.push(i18n::tr("No iPhone/iPad on USB"));
    }
    lines.join("\n")
}

/// B-27: without Media Foundation (Windows N/KN without the Media Feature
/// Pack) there is no picture. Say so and open the official place to add it.
fn check_media_foundation(ui: &AppWindow) {
    let ok = zenlink_video::media_foundation_present() && {
        // On its own thread: Media Foundation and COM, see hevc_ok.
        std::thread::spawn(|| zenlink_video::decoder_available(&zenlink_core::api::Codec::H264))
            .join()
            .unwrap_or(false)
    };
    if ok {
        return;
    }
    open_sheet(
        ui,
        "mfp",
        i18n::tr("The Media Feature Pack is missing"),
        i18n::tr("This Windows (an N or KN edition) has no Windows video components, so ZenLink cannot show the picture. Add the free \"Media Feature Pack\" from Microsoft: Settings › Apps › Optional features › View features › Media Feature Pack. Then restart the PC."),
        i18n::tr("Open Optional features"),
        i18n::tr("Later"),
    );
}

fn open_sheet(ui: &AppWindow, kind: &str, title: String, body: String, ok: String, cancel: String) {
    APP.with_borrow_mut(|a| a.sheet = Some(kind.into()));
    ui.set_sheet_title(title.into());
    ui.set_sheet_body(body.into());
    ui.set_sheet_ok(ok.into());
    ui.set_sheet_cancel(cancel.into());
    ui.set_sheet_open(true);
}

/// Ask before a driver change (it needs administrator approval).
fn confirm_driver(ui: &AppWindow, action: &str) {
    if matches!(
        ui.get_phase().as_str(),
        "connecting" | "activating" | "streaming"
    ) {
        show_toast(ui, i18n::tr("Stop casting before changing drivers."));
        return;
    }
    if APP.with_borrow(|a| a.drv_instance.is_none()) {
        show_toast(ui, i18n::tr("Connect the device first."));
        return;
    }
    let (title, body) = match action {
        "restore" => (
            i18n::tr("Restore the Apple driver?"),
            i18n::tr("Apple Driver mode can see the device again. ZenLink Studio stops until its driver is installed again. A backup is saved first. Windows will ask for administrator permission. Unplug and reconnect the cable afterwards."),
        ),
        "bind" => (
            i18n::tr("Install the ZenLink Studio driver?"),
            i18n::tr("Like TikTok LIVE Studio, ZenLink installs the libusb-win32 driver for this device. While it is installed, Apple mode and Apple apps can't see the device; press Restore Apple driver to switch back. If there is no driver package for this model yet, ZenLink builds one and signs it with a new certificate named \"ZenLink ZenLink Studio\", added to Windows' trusted certificates (its private key is deleted). A backup is saved first, and Apple's driver comes back automatically if the device doesn't start. Windows will ask for administrator permission. Unplug and reconnect the cable afterwards."),
        ),
        _ => (
            i18n::tr("Remove the libusb0 filter?"),
            i18n::tr("It is stacked on Apple's driver, which can crash Windows. Apple's driver stays. A backup is saved first. Windows will ask for administrator permission."),
        ),
    };
    open_sheet(
        ui,
        &format!("drv:{action}"),
        title,
        body,
        i18n::tr("Continue"),
        i18n::tr("Cancel"),
    );
}

/// Run a confirmed driver change, then offer to restart the service.
fn run_driver(ui: &AppWindow, api: &Api, action: String) {
    let Some(instance) = APP.with_borrow(|a| a.drv_instance.clone()) else {
        return;
    };
    open_sheet(
        ui,
        "busy",
        i18n::tr("Changing the device's driver…"),
        i18n::tr("Approve the Windows permission prompt. A PowerShell window shows each step; keep the cable plugged in until it finishes."),
        String::new(),
        String::new(),
    );
    let remember = ui.get_mode_remember();
    let (api, w) = (api.clone(), ui.as_weak());
    std::thread::spawn(move || {
        let r = api.post_slow(
            "/api/drivers/action",
            &json!({ "action": action, "instance": instance, "confirmed": true }),
            900,
        );
        let ok = r
            .as_ref()
            .and_then(|r| r.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if ok && action != "removefilter" {
            // Keep the connection mode in step with the driver now installed.
            let mode = if action == "bind" {
                "usbdirect"
            } else {
                "apple"
            };
            api.post(
                "/api/connection",
                &json!({ "mode": mode, "remember": remember }),
            );
        }
        let msg = r
            .as_ref()
            .and_then(|r| str_of(r, &["message"]))
            .unwrap_or("")
            .to_string();
        let rolled = r
            .as_ref()
            .and_then(|r| r.get("rolledBack"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            if ok {
                let target = if action == "bind" {
                    "ZenLink Studio"
                } else {
                    "Apple Driver"
                };
                // This sheet already offers the restart for the new mode.
                if action != "removefilter" {
                    let mode = if action == "bind" {
                        "usbdirect"
                    } else {
                        "apple"
                    };
                    APP.with_borrow_mut(|a| a.restart_asked = Some(mode.into()));
                }
                open_sheet(
                    &ui,
                    "restart",
                    i18n::tr("Driver changed"),
                    format!("{msg}

{}", trf!("The ZenLink service needs a restart to use {target} mode. Unplug the cable and plug it in again.", target = target)),
                    i18n::tr("Restart now"),
                    i18n::tr("Later"),
                );
            } else {
                let body = if msg.is_empty() {
                    i18n::tr("The backend did not answer.")
                } else {
                    tr_backend(&msg)
                };
                let body = if rolled {
                    format!("{body} {}", i18n::tr("The previous state was restored."))
                } else {
                    body
                };
                open_sheet(
                    &ui,
                    "info",
                    i18n::tr("Driver not changed"),
                    body,
                    "OK".into(),
                    String::new(),
                );
            }
        });
    });
}

/// Move the window back inside the work area if a resize pushed it out.
fn keep_on_screen(ui: &AppWindow, (wx, wy, ww, wh): (f32, f32, f32, f32), size: LogicalSize) {
    let sf = ui.window().scale_factor();
    let pos = ui.window().position().to_logical(sf);
    let (w, h) = (size.width, size.height); // frameless: client = window
    let x = pos.x.min(wx + ww - w).max(wx);
    let y = pos.y.min(wy + wh - h).max(wy);
    if (x - pos.x).abs() > 0.5 || (y - pos.y).abs() > 0.5 {
        ui.window().set_position(slint::LogicalPosition::new(x, y));
    }
}

/// Icon font: "Segoe Fluent Icons" ships with Windows 11 only; Windows 10
/// has "Segoe MDL2 Assets" with the same code points for every icon ZenLink
/// uses (checked against segmdl2.ttf 1.86). Without this, Windows 10 drew
/// every icon as an empty box (B-12).
fn icon_font() -> &'static str {
    // Test hook: ZL_ICON_FONT=mdl2 shows the Windows 10 icons on Windows 11.
    if std::env::var("ZL_ICON_FONT").as_deref() == Ok("mdl2") {
        return pick_icon_font(false);
    }
    let fonts = std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\Windows".into())
        .join("Fonts");
    pick_icon_font(fonts.join("SegoeIcons.ttf").exists())
}

fn pick_icon_font(has_fluent: bool) -> &'static str {
    if has_fluent {
        "Segoe Fluent Icons"
    } else {
        "Segoe MDL2 Assets"
    }
}

/// Mirrors the startup navigation into the window.
fn apply_flow(ui: &AppWindow, f: &flow::Flow) {
    ui.set_screen(f.screen.as_str().into());
    ANDROID_POLL.store(
        match f.screen {
            // 2 on Home too: the device menu shows phone names there.
            flow::Screen::Platform => 2,
            flow::Screen::Android => 2,
            flow::Screen::Apple => 2,
        },
        Relaxed,
    );
    if f.apple_chooser {
        ui.set_chooser_cancelable(false);
        open_chooser(ui);
    } else if f.screen != flow::Screen::Apple {
        ui.set_chooser_open(false);
    }
    if f.screen == flow::Screen::Platform {
        widen_for_sheet(ui);
    }
    // The menu highlight follows the page on screen.
    rail_refresh(ui);
}

/// What the Android poll does: 0 = nothing (Apple workspace), 1 = count
/// devices without names, 2 = list them with names (Home and both workspaces).
static ANDROID_POLL: AtomicU8 = AtomicU8::new(0);
/// Set after adb was installed: the poll tries to start the ADB server again.
static ADB_RETRY: AtomicBool = AtomicBool::new(false);
/// An adb download runs (one at a time).
static ADB_INSTALLING: AtomicBool = AtomicBool::new(false);
/// ZenLink started the running ADB server: it stops it again on exit.
static ADB_STARTED: AtomicBool = AtomicBool::new(false);

/// On exit: stop the ADB server only when ZenLink started it, so a server
/// another tool (Android Studio, scrcpy) started keeps running.
fn stop_own_adb_server() {
    if ADB_STARTED.swap(false, Relaxed) {
        let client = zenlink_android::adb::AdbClient {
            timeout: Duration::from_millis(500),
            ..zenlink_android::adb::AdbClient::default()
        };
        if let Err(e) = client.kill_server() {
            log::info!("android: stop the ADB server: {e}");
        }
    }
}

enum AdbView {
    /// No ADB server, and no `adb` program to start one with.
    NoAdb,
    /// No ADB server yet; ZenLink is starting it.
    Starting,
    /// No ADB server; starting it failed.
    NotRunning,
    /// Port 5037 answers, but not as an ADB server.
    Foreign,
    /// An ADB server runs; `other` = a different version than ZenLink's.
    Running { version: u32, other: bool },
}

/// Polls the ADB server while the platform screen or the Android workspace
/// shows. When none runs, it starts one with the `adb` it finds (once; it
/// never kills or replaces a running server, only its own one on exit). Properties are read once per
/// transport and cached.
fn start_android_poll(ui: &AppWindow) {
    use zenlink_android::adb::{check_server, AdbClient, ServerStatus, BUNDLED_SERVER_VERSION};
    use zenlink_android::{devices, discovery};
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let client = AdbClient {
            timeout: Duration::from_millis(500),
            ..AdbClient::default()
        };
        let mut cache: std::collections::HashMap<
            (String, Option<u64>),
            zenlink_android::props::Props,
        > = Default::default();
        let mut tried_start = false;
        loop {
            let mode = ANDROID_POLL.load(Relaxed);
            if ADB_RETRY.swap(false, Relaxed) {
                tried_start = false;
            }
            if mode == 0 {
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            let (view, list) = match check_server(&client, BUNDLED_SERVER_VERSION) {
                ServerStatus::NotRunning if !tried_start => {
                    tried_start = true;
                    match android::find_adb() {
                        None => (AdbView::NoAdb, Vec::new()),
                        Some(adb) => {
                            log::info!("android: starting the ADB server with {}", adb.display());
                            let port = client.addr.port();
                            let w = weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() {
                                    show_android(&ui, &AdbView::Starting, &[], false);
                                }
                            });
                            match zenlink_android::adb::start_server(&adb, port) {
                                Ok(s) if s.success() => {
                                    ADB_STARTED.store(true, Relaxed);
                                    // Next round lists the devices.
                                    continue;
                                }
                                Ok(s) => {
                                    log::warn!("android: adb start-server exited with {s}");
                                    (AdbView::NotRunning, Vec::new())
                                }
                                Err(e) => {
                                    log::warn!("android: adb start-server: {e}");
                                    (AdbView::NotRunning, Vec::new())
                                }
                            }
                        }
                    }
                }
                ServerStatus::NotRunning => (
                    if android::find_adb().is_some() {
                        AdbView::NotRunning
                    } else {
                        AdbView::NoAdb
                    },
                    Vec::new(),
                ),
                ServerStatus::Foreign(_) => (AdbView::Foreign, Vec::new()),
                s => {
                    let (version, other) = match s {
                        ServerStatus::Mismatch { found, .. } => (found, true),
                        ServerStatus::Compatible(v) => (v, false),
                        _ => (0, true),
                    };
                    let entries = client
                        .devices_l()
                        .map(|t| devices::parse_list(&t))
                        .unwrap_or_default();
                    cache.retain(|k, _| {
                        entries
                            .iter()
                            .any(|e| (e.serial.clone(), e.transport_id) == *k)
                    });
                    let list = discovery::build(entries, |d| {
                        if mode < 2 {
                            return None;
                        }
                        let key = (d.serial.clone(), d.transport_id);
                        if let Some(p) = cache.get(&key) {
                            return Some(p.clone());
                        }
                        let target = match d.transport_id {
                            Some(id) => zenlink_android::adb::Target::TransportId(id),
                            None => zenlink_android::adb::Target::Serial(&d.serial),
                        };
                        let p = client.getprop(&target).ok()?;
                        cache.insert(key, p.clone());
                        Some(p)
                    });
                    (AdbView::Running { version, other }, list)
                }
            };
            // B-15: nothing in ADB, but Windows sees a phone (MTP/PTP).
            let phone_seen = list.is_empty()
                && matches!(view, AdbView::Running { .. })
                && usbphones::phone_without_adb();
            let w = weak.clone();
            if slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    show_android(&ui, &view, &list, phone_seen);
                }
            })
            .is_err()
            {
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn show_android(
    ui: &AppWindow,
    view: &AdbView,
    list: &[zenlink_android::discovery::AndroidDevice],
    phone_seen: bool,
) {
    use zenlink_android::devices::Connection;
    use zenlink_android::discovery::Readiness;
    ui.set_android_count(list.len() as i32);
    ui.set_adb_missing(matches!(view, AdbView::NoAdb) || ADB_INSTALLING.load(Relaxed));
    let status = match view {
        AdbView::NoAdb => i18n::tr("adb is not installed yet."),
        AdbView::Starting => i18n::tr("Starting ADB…"),
        AdbView::NotRunning => {
            i18n::tr("ADB did not start. Close other apps that use ADB, then try again.")
        }
        AdbView::Foreign => {
            i18n::tr("ADB port 5037 is used by another program. ZenLink does not change it.")
        }
        AdbView::Running { version, other } => {
            let base = if list.is_empty() && phone_seen {
                i18n::tr("Windows sees a phone on USB, but ADB does not yet. Turn on USB debugging (steps below), then unplug and plug the cable in again.")
            } else if list.is_empty() {
                i18n::tr("No Android device yet. Follow the steps below.")
            } else {
                format!("{} {}", list.len(), i18n::tr("device(s) seen through ADB."))
            };
            if *other {
                format!(
                    "{base} {}",
                    trf!("Another app's ADB server (version {version}) is running; ZenLink does not stop it.", version = version)
                )
            } else {
                base
            }
        }
    };
    ui.set_android_status(status.into());
    let rows: Vec<AndroidRow> = list
        .iter()
        .map(|d| {
            let link = match d.primary.connection {
                Connection::Usb => "USB".to_string(),
                Connection::WifiTcp { .. } | Connection::WifiTls { .. } => "Wi-Fi".to_string(),
                Connection::Emulator => i18n::tr("Emulator"),
                Connection::Unknown => "?".to_string(),
            };
            let state = match &d.readiness {
                Readiness::Ready => i18n::tr("Ready"),
                Readiness::NeedsAuthorization => i18n::tr("Tap Allow on the phone"),
                Readiness::Offline => i18n::tr("Offline, reconnect the cable"),
                Readiness::NoPermission => i18n::tr("No USB permission or driver"),
                Readiness::NotAndroidSystem => i18n::tr("Recovery or bootloader mode"),
                Readiness::TooOld { sdk } => {
                    trf!("Android too old (API {sdk}, needs 21+)", sdk = sdk)
                }
                Readiness::Unknown => i18n::tr("Reading device info…"),
            };
            let both_links = d.transports.iter().any(|t| t.connection == Connection::Usb)
                && d.transports.iter().any(|t| t.connection.is_wireless());
            let mut sub = vec![if both_links {
                "USB + Wi-Fi".to_string()
            } else {
                link
            }];
            if let Some(r) = &d.android_release {
                sub.push(format!("Android {r}"));
            }
            sub.push(state);
            let on_wifi = d.transports.iter().any(|t| {
                matches!(
                    t.connection,
                    Connection::WifiTcp { .. } | Connection::WifiTls { .. }
                )
            });
            let on_usb = d.transports.iter().any(|t| t.connection == Connection::Usb);
            AndroidRow {
                selected: false,
                title: d.name.display.clone().into(),
                sub: sub.join(" · ").into(),
                ok: d.readiness == Readiness::Ready,
                both: d.readiness == Readiness::Ready && on_usb && on_wifi,
                wifi: d.readiness == Readiness::Ready
                    && d.primary.connection == Connection::Usb
                    && !on_wifi,
            }
        })
        .collect();
    ui.set_android_devices(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    APP.with_borrow_mut(|a| a.android_list = list.to_vec());
    rail_refresh(ui);
    android_mark_picked(ui);
    // Test hook: show the QR pairing card (no phone needed to see it).
    static QR_TEST: AtomicBool = AtomicBool::new(false);
    if std::env::var_os("ZL_QR_TEST").is_some() && !QR_TEST.swap(true, Relaxed) {
        android_wifi_qr(ui);
    }
    // Test hook: play a recorded scrcpy stream as soon as the workspace shows.
    let fixture = std::env::var_os("ZL_ANDROID_FIXTURE").is_some();
    if fixture && !APP.with_borrow(|a| a.fixture_started) {
        APP.with_borrow_mut(|a| a.fixture_started = true);
        android_start(ui, -1);
        // Test hook: a toast over the playing picture (the picture must not hide it).
        if let Some(t) = std::env::var_os("ZL_TOAST_TEST") {
            let (w, t) = (ui.as_weak(), t.to_string_lossy().into_owned());
            slint::Timer::single_shot(Duration::from_secs(4), move || {
                if let Some(ui) = w.upgrade() {
                    show_toast(&ui, t);
                }
            });
        }
    }
}

/// The bundled scrcpy server, read once.
/// Whether Windows can decode H.265 (checked once; Media Foundation lookup).
fn hevc_ok() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    // On its own thread: Media Foundation initialises COM as multithreaded,
    // and the UI thread must stay single-threaded for winit (OleInitialize
    // fails with RPC_E_CHANGED_MODE otherwise and the window panics).
    *OK.get_or_init(|| {
        std::thread::spawn(|| zenlink_video::decoder_available(&zenlink_core::api::Codec::Hevc))
            .join()
            .unwrap_or(false)
    })
}

/// scrcpy server options from Settings › Android video and Settings › Audio.
/// H.265 falls back to H.264 when Windows has no HEVC decoder.
/// The codec and encoder come from the Smart Encoder plan (`android.rs`).
fn android_options(
    p: &Prefs,
    audio_supported: bool,
    sdk: Option<u32>,
) -> zenlink_android::mirror::VideoOptions {
    let num = |s: &str, d: u32| s.parse::<u32>().unwrap_or(d);
    zenlink_android::mirror::VideoOptions {
        max_size: num(&p.android_max_size, 1920),
        bit_rate: num(&p.android_bitrate, 8).saturating_mul(1_000_000),
        max_fps: num(&p.android_fps, 60),
        codec: "h264",
        encoder: None,
        audio: audio_supported && p.audio_out.as_str() != "device",
        // Both: the phone keeps its sound (Android 13+); older phones go
        // silent while the PC plays it, as with "Windows".
        audio_dup: p.audio_out.as_str() == "both" && sdk.is_some_and(|s| s >= 33),
        control: true,
        new_display: None,
    }
}

/// What the user chose for codec and encoder.
fn android_pref(p: &Prefs) -> zenlink_android::encoders::Preference {
    zenlink_android::encoders::Preference {
        codec: p.android_codec.to_string(),
        encoder: p.android_encoder.to_string(),
    }
}

fn android_decoders() -> zenlink_android::encoders::Decoders {
    zenlink_android::encoders::Decoders {
        h265: hevc_ok(),
        av1: false,
    }
}

/// Runs `f` on the per-device store (loaded on first use) and saves it.
fn with_store<R>(f: impl FnOnce(&mut zenlink_android::profiles::Store) -> R) -> R {
    APP.with_borrow_mut(|a| {
        let s = a.android_store.get_or_insert_with(devprofiles::load);
        let r = f(s);
        devprofiles::save(s);
        r
    })
}

/// Loads a device's own settings into Settings before it starts. A phone
/// seen for the first time takes the current Settings as its own.
fn android_select_device(ui: &AppWindow, d: &zenlink_android::discovery::AndroidDevice) {
    use zenlink_android::devices::Connection;
    let key = d.id.key.clone();
    let mut p = ui.get_prefs();
    let known = with_store(|s| {
        let known = s.get(&key).is_some();
        let e = s.entry(&key);
        e.name = d.name.display.clone();
        e.manufacturer = d.manufacturer.clone().unwrap_or_default();
        e.model = d.model.clone().unwrap_or_default();
        e.android = d.android_release.clone().unwrap_or_default();
        e.connection = match d.primary.connection {
            Connection::WifiTcp { .. } | Connection::WifiTls { .. } => "wifi",
            _ => "usb",
        }
        .into();
        if known {
            devprofiles::apply(&mut p, e);
        } else {
            e.set_picture(devprofiles::picture_of(&p));
        }
        known
    });
    if known {
        prefs::save(&p);
        ui.set_prefs(p);
    }
    APP.with_borrow_mut(|a| a.android_dev = Some(key));
    refresh_encoder_ui(ui);
}

/// Settings changed: store them for the selected device.
fn android_store_prefs(p: &Prefs) {
    let Some(key) = APP.with_borrow(|a| a.android_dev.clone()) else {
        return;
    };
    with_store(|s| {
        let e = s.entry(&key);
        e.set_picture(devprofiles::picture_of(p));
    });
}

/// Short button label for an encoder: the vendor part of its name and how
/// it runs, e.g. `qti hw`, `android sw`.
fn encoder_label(e: &zenlink_android::encoders::Encoder) -> String {
    let parts: Vec<&str> = e.name.split('.').collect();
    let vendor = match parts.as_slice() {
        ["c2" | "OMX", v, ..] => v,
        [v, ..] => v,
        [] => "?",
    };
    let vendor: String = vendor.chars().take(10).collect();
    let accel = e.accel.as_str();
    if accel.is_empty() {
        vendor
    } else {
        format!("{vendor} {accel}")
    }
}

/// Fills the encoder choices and the detection summary in Settings.
fn refresh_encoder_ui(ui: &AppWindow) {
    use zenlink_android::encoders::{usable_codecs, video_encoders, Accel};
    let list = APP.with_borrow(|a| {
        a.android_dev
            .as_ref()
            .and_then(|k| a.android_encoders.get(k).cloned())
    });
    let g = ui.global::<AndroidEnc>();
    let Some(list) = list else {
        g.set_options(slint::ModelRc::new(slint::VecModel::from(vec!["".into()])));
        g.set_labels(slint::ModelRc::new(slint::VecModel::from(vec![
            slint::SharedString::from("Auto"),
        ])));
        g.set_codecs(slint::ModelRc::new(slint::VecModel::from(Vec::<
            slint::SharedString,
        >::new())));
        g.set_detected(slint::SharedString::new());
        return;
    };
    let p = ui.get_prefs();
    let codec = match p.android_codec.as_str() {
        "h265" => "h265",
        _ => "h264",
    };
    let encs = video_encoders(&list, codec);
    let mut options: Vec<slint::SharedString> = vec!["".into()];
    let mut labels: Vec<slint::SharedString> = vec!["Auto".into()];
    // Seg shows at most six options.
    for e in encs.iter().take(5) {
        options.push(e.name.as_str().into());
        labels.push(encoder_label(e).into());
    }
    g.set_options(slint::ModelRc::new(slint::VecModel::from(options)));
    g.set_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
    let codecs: Vec<slint::SharedString> = usable_codecs(&list, android_decoders())
        .into_iter()
        .map(Into::into)
        .collect();
    g.set_codecs(slint::ModelRc::new(slint::VecModel::from(codecs)));
    // "H.264: 2 (1 hw) · H.265: 1 (1 hw) · AV1: 1 (no PC decoder)"
    let mut parts = Vec::new();
    for (c, name) in [("h264", "H.264"), ("h265", "H.265"), ("av1", "AV1")] {
        let v = video_encoders(&list, c);
        if v.is_empty() {
            continue;
        }
        let hw = v.iter().filter(|e| e.accel == Accel::Hardware).count();
        let mut s = format!("{name}: {} ({hw} hw)", v.len());
        if c == "av1" || (c == "h265" && !hevc_ok()) {
            s.push_str(&i18n::tr(", no PC decoder"));
        }
        parts.push(s);
    }
    g.set_detected(parts.join(" · ").into());
}

/// Alt+C: reads the text on the picture with Windows OCR (through the
/// backend's /api/ocr, as in the classic window) and puts it on the PC
/// clipboard. Lines from earlier captures are stitched (Alt+R clears).
fn copy_screen_text(ui: &AppWindow) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    let live = APP.with_borrow(|a| a.streaming || a.android_live);
    if !live {
        show_toast(ui, i18n::tr("Start mirroring first."));
        return;
    }
    if BUSY.swap(true, Relaxed) {
        return;
    }
    let shot = APP.with_borrow(|a| a.view.as_ref()?.snapshot(Duration::from_millis(400)));
    let Some(shot) = shot else {
        BUSY.store(false, Relaxed);
        show_toast(ui, i18n::tr("No picture to read yet."));
        return;
    };
    show_toast(ui, i18n::tr("Reading the text on the screen…"));
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let r = Some(
            match ocr::add_capture(shot.width, shot.height, &shot.bgra) {
                Ok((text, added, total)) => {
                    json!({ "ok": true, "text": text, "added": added, "total": total })
                }
                Err(e) => {
                    log::warn!("ocr: {e}");
                    json!({ "ok": false })
                }
            },
        );
        let _ = slint::invoke_from_event_loop(move || {
            BUSY.store(false, Relaxed);
            let Some(ui) = weak.upgrade() else { return };
            let ok = r
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(Value::as_bool)
                == Some(true);
            let text = r
                .as_ref()
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let num = |k: &str| {
                r.as_ref()
                    .and_then(|v| v.get(k))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            };
            let msg = if !ok {
                i18n::tr("Text recognition failed (Windows OCR).")
            } else if text.is_empty() {
                i18n::tr("No text found on the screen.")
            } else if !set_pc_clipboard_text(&text) {
                i18n::tr("Text read, but Windows didn't allow copying it to the clipboard.")
            } else if num("added") == 0 {
                i18n::tr("Nothing new on this screen; the copied text is unchanged.")
            } else {
                let (n, total) = (num("added"), num("total"));
                trf!(
                    "Copied to the PC: {n} new line(s) ({total} in total). Alt+R starts over.",
                    n = n,
                    total = total
                )
            };
            show_toast(&ui, msg);
        });
    });
}

/// Puts `text` on the Windows clipboard. False when the clipboard is busy.
fn set_pc_clipboard_text(text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let g = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2).ok()?;
            let p = GlobalLock(g) as *mut u16;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(g);
            // The clipboard owns the memory after this.
            SetClipboardData(CF_UNICODETEXT, Some(HANDLE(g.0))).ok()?;
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

/// Text on the Windows clipboard (Ctrl+V to Android), `None` without text.
fn pc_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT).ok().and_then(|h| {
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(g);
            Some(s)
        });
        let _ = CloseClipboard();
        text.filter(|s| !s.is_empty())
    }
}

/// The settings a running Android session depends on.
fn android_key(p: &Prefs) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        p.audio_out,
        p.android_max_size,
        p.android_fps,
        p.android_bitrate,
        p.android_codec,
        p.android_encoder
    )
}

/// Log of the app windows (`--android-app`), next to the main log.
const APP_LOG_FILE_NAME: &str = "zenlink-next-v25-apps.log";

/// App picker rows for `filter` (name or package, any case), at most 80.
fn apps_matching<'a>(
    all: &'a [zenlink_android::apps::DeviceApp],
    filter: &str,
) -> Vec<&'a zenlink_android::apps::DeviceApp> {
    let f = filter.trim().to_lowercase();
    all.iter()
        .filter(|a| f.is_empty() || a.name.to_lowercase().contains(&f) || a.package.contains(&f))
        .take(80)
        .collect()
}

/// Launcher tile text: the first letter of the first two words ("Google
/// Maps" → "GM", "Chrome" → "C").
fn app_letters(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .chars()
        .take(2)
        .collect()
}

/// Launcher tile colour: one of 12 hues, fixed per package.
fn app_tint(package: &str) -> slint::Color {
    const HUES: [u32; 12] = [
        0xef4444, 0xf97316, 0xf59e0b, 0x84cc16, 0x22c55e, 0x14b8a6, 0x06b6d4, 0x3b82f6, 0x6366f1,
        0x8b5cf6, 0xd946ef, 0xec4899,
    ];
    // FNV-1a: stable across runs (std's hasher is randomly seeded).
    let h = package.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    });
    let c = HUES[(h % HUES.len() as u32) as usize];
    slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Shows the picker's rows for `filter`.
fn apps_show(ui: &AppWindow, filter: &str) {
    let rows = APP.with_borrow_mut(|a| {
        let key = a.apps_device.as_ref().map(|d| d.id.key.clone())?;
        let all = a.apps_cache.get(&key)?;
        a.apps_shown = apps_matching(all, filter).into_iter().cloned().collect();
        Some((a.apps_shown.clone(), all.len()))
    });
    let Some((shown, total)) = rows else { return };
    let model: Vec<AppRow> = shown
        .iter()
        .map(|a| AppRow {
            letter: app_letters(&a.name).into(),
            tint: app_tint(&a.package),
            name: a.name.clone().into(),
            sub: if a.system {
                format!("{} · {}", a.package, i18n::tr("system"))
            } else {
                a.package.clone()
            }
            .into(),
        })
        .collect();
    let status = if total == 0 {
        i18n::tr("The phone listed no apps.")
    } else if shown.is_empty() {
        i18n::tr("No app matches.")
    } else if shown.len() < total {
        trf!(
            "{_0} of {total} apps. Type to search.",
            _0 = shown.len(),
            total = total
        )
    } else {
        trf!("{total} apps.", total = total)
    };
    ui.set_android_apps_status(status.into());
    ui.set_android_apps(std::rc::Rc::new(slint::VecModel::from(model)).into());
}

/// Opens the app picker for list entry `index` and reads its apps (once per
/// device; about a second or two on the phone).
fn apps_open(ui: &AppWindow) {
    if ui.get_android_apps_open() {
        ui.set_android_apps_open(false);
        return;
    }
    // The phone being mirrored (set in android_start), on the link in use.
    let Some(d) = APP.with_borrow(|a| a.android.as_ref().and(a.apps_device.clone())) else {
        show_toast(ui, i18n::tr("Start mirroring an Android phone first."));
        return;
    };
    ui.set_android_apps_window_ok(d.sdk.unwrap_or(0) >= zenlink_android::apps::MIN_SDK_NEW_DISPLAY);
    ui.set_android_apps_title(trf!("Apps on {_0}", _0 = d.name.display).into());
    ui.set_android_apps_open(true);
    let cached = APP.with_borrow_mut(|a| {
        a.apps_device = Some(d.clone());
        a.apps_cache.contains_key(&d.id.key)
    });
    if cached {
        ui.set_android_apps_busy(false);
        apps_show(ui, "");
        return;
    }
    let server = match scrcpy_server() {
        Ok(s) => s,
        Err(e) => {
            ui.set_android_apps_status(e.into());
            return;
        }
    };
    ui.set_android_apps_busy(true);
    ui.set_android_apps(std::rc::Rc::new(slint::VecModel::<AppRow>::default()).into());
    ui.set_android_apps_status(i18n::tr("Reading the phone's apps…").into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let client = zenlink_android::adb::AdbClient::default();
        let got = zenlink_android::apps::query(&client, &d.target(), &server);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_android_apps_busy(false);
            match got {
                Ok(list) => {
                    log::info!("android: {} apps on {}", list.len(), d.name.display);
                    APP.with_borrow_mut(|a| {
                        a.apps_cache.insert(
                            d.id.key.clone(),
                            zenlink_android::apps::sorted_for_picker(list),
                        );
                    });
                    apps_show(&ui, "");
                }
                Err(e) => {
                    log::warn!("android: app list failed: {e}");
                    ui.set_android_apps_status(trf!("Could not read the apps: {e}", e = e).into());
                }
            }
        });
    });
}

/// Opens picker row `index` in its own window.
fn apps_launch(ui: &AppWindow, index: i32, place: &str) {
    let Some((d, app)) = APP.with_borrow(|a| {
        Some((
            a.apps_device.clone()?,
            a.apps_shown.get(index.max(0) as usize)?.clone(),
        ))
    }) else {
        return;
    };
    ui.set_android_apps_open(false);
    if place != "window" {
        // On the phone's own screen, in the picture being mirrored.
        match android::active() {
            Some(sh) => {
                sh.send(&[zenlink_android::control::Control::StartApp(
                    app.package.clone(),
                )]);
                log::info!("android: start {} on the phone", app.package);
                show_toast(ui, trf!("Opening {_0}…", _0 = app.name));
            }
            None => show_toast(ui, i18n::tr("Phone control is not ready.")),
        }
        return;
    }
    let args = appwin::AppArgs {
        serial: d.primary.serial.clone(),
        transport_id: d.primary.transport_id,
        package: app.package.clone(),
        name: app.name.clone(),
        device: d.name.display.clone(),
        screen: false,
    };
    match appwin::spawn(&args) {
        Ok(_) => show_toast(ui, trf!("Opening {_0}…", _0 = app.name)),
        Err(e) => {
            log::warn!("android: app window failed: {e}");
            show_toast(ui, trf!("The app window did not open: {e}", e = e));
        }
    }
}

fn scrcpy_server() -> Result<Arc<Vec<u8>>, String> {
    static SERVER: OnceLock<Result<Arc<Vec<u8>>, String>> = OnceLock::new();
    SERVER
        .get_or_init(|| android::load_server().map(Arc::new))
        .clone()
}

/// Starts mirroring list entry `index` (-1 = the first ready device), or the
/// recording in ZL_ANDROID_FIXTURE.
/// Runs one Wi-Fi step on a worker thread (the ADB calls block for
/// seconds) and shows its message. `Ok` is a success message.
/// Shows the current step of a Wi-Fi job under the device list.
type Step<'a> = &'a dyn Fn(String);

fn android_wifi_job(
    ui: &AppWindow,
    job: impl FnOnce(Step) -> Result<(String, String), (String, String)> + Send + 'static,
) {
    if ui.get_android_wifi_busy() {
        return;
    }
    ui.set_android_wifi_busy(true);
    ui.set_android_wifi_status("".into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let w = weak.clone();
        let step = move |s: String| {
            let w = w.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_android_wifi_status(s.into());
                }
            });
        };
        let r = job(&step);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_android_wifi_busy(false);
            ui.set_android_wifi_status("".into());
            let ((msg, detail), _ok) = match r {
                Ok(m) => (m, true),
                Err(m) => (m, false),
            };
            let tail = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            };
            show_toast(&ui, format!("{msg}{tail}"));
        });
    });
}

/// "Wi-Fi" on a USB device: read its Wi-Fi address, switch adbd to TCP
/// (`adb tcpip 5555`) and `adb connect` to it. The settings follow the
/// device, because its identity is the hardware serial.
fn android_wifi_device(ui: &AppWindow, index: i32) {
    use zenlink_android::adb::{AdbClient, Target};
    use zenlink_android::wireless::{Reply, TCPIP_PORT};
    let dev = APP.with_borrow(|a| a.android_list.get(index.max(0) as usize).cloned());
    let Some(d) = dev else { return };
    android_wifi_job(ui, move |step| {
        let c = AdbClient::default();
        let serial = d.primary.serial.clone();
        step(i18n::tr("1/3 · Reading the phone's Wi-Fi address…"));
        let target = match d.primary.transport_id {
            Some(id) => Target::TransportId(id),
            None => Target::Serial(&serial),
        };
        let ip = match c.wifi_address(&target) {
            Ok(Some(ip)) => ip,
            Ok(None) => return Err((i18n::tr("The phone is not on Wi-Fi"), String::new())),
            Err(e) => return Err((i18n::tr("Couldn't read the Wi-Fi address"), e.to_string())),
        };
        step(i18n::tr(
            "2/3 · Turning on ADB over Wi-Fi on the phone (its screen may flicker)…",
        ));
        if let Err(e) = c.tcpip(&target, TCPIP_PORT) {
            return Err((i18n::tr("Couldn't turn on TCP/IP mode"), e.to_string()));
        }
        // adbd restarts on the phone; give it a moment, then retry.
        step(i18n::tr("3/3 · Connecting over Wi-Fi…"));
        let mut last = String::new();
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(1200));
            match c.connect_device(&ip, TCPIP_PORT) {
                Ok(Reply::Ok(_)) => {
                    log::info!("android: Wi-Fi connected to {ip}:{TCPIP_PORT}");
                    return Ok((
                        i18n::tr("Connected over Wi-Fi. You can unplug the cable"),
                        format!("{ip}:{TCPIP_PORT}"),
                    ));
                }
                Ok(Reply::Failed(m)) => last = m,
                Err(e) => last = e.to_string(),
            }
        }
        log::warn!("android: Wi-Fi connect to {ip} failed: {last}");
        Err((
            i18n::tr(
                "Couldn't connect over Wi-Fi. The PC and the phone must be on the same network",
            ),
            last,
        ))
    });
}

/// "Connect": `adb connect IP:port`.
fn android_wifi_connect(ui: &AppWindow, addr: String) {
    use zenlink_android::adb::AdbClient;
    use zenlink_android::wireless::{parse_endpoint, Reply};
    let Some((host, port)) = parse_endpoint(&addr) else {
        show_toast(ui, i18n::tr("Invalid address. Example: 192.168.1.20:37099"));
        return;
    };
    android_wifi_job(ui, move |step| {
        step(i18n::tr("Connecting to the phone over Wi-Fi…"));
        match AdbClient::default().connect_device(&host, port) {
            Ok(Reply::Ok(_)) => Ok((i18n::tr("Connected over Wi-Fi"), format!("{host}:{port}"))),
            Ok(Reply::Failed(m)) => Err((i18n::tr("Couldn't connect"), m)),
            Err(e) => Err((i18n::tr("Couldn't connect"), e.to_string())),
        }
    });
}

/// "Pair": `adb pair IP:port CODE` (Android 11+ wireless debugging).
fn android_wifi_pair(ui: &AppWindow, addr: String, code: String) {
    use zenlink_android::adb::AdbClient;
    use zenlink_android::wireless::{parse_endpoint, valid_pair_code, Reply};
    let Some((host, port)) = parse_endpoint(&addr) else {
        show_toast(
            ui,
            i18n::tr("Enter the pairing address (IP:port) shown on the phone."),
        );
        return;
    };
    if !valid_pair_code(&code) {
        show_toast(ui, i18n::tr("The pairing code has 6 digits."));
        return;
    }
    android_wifi_job(ui, move |step| {
        step(i18n::tr("Pairing with the phone…"));
        match AdbClient::default().pair(&host, port, &code) {
        Ok(Reply::Ok(_)) => Ok((i18n::tr("Paired. Now enter the address on the Wireless debugging screen and press Connect"), String::new())),
        Ok(Reply::Failed(m)) => Err((i18n::tr("Pairing failed"), m)),
        Err(e) => Err((i18n::tr("Pairing failed"), e.to_string())),
    }
    });
}

/// Current QR pairing attempt; a new attempt or Cancel bumps it and the
/// old poll thread stops.
static QR_RUN: AtomicU64 = AtomicU64::new(0);

/// Black-on-white QR code with a 4-module quiet zone, 6 px per module.
fn qr_image(qr: &qrcodegen::QrCode) -> slint::Image {
    const SCALE: usize = 6;
    const QUIET: i32 = 4;
    let n = (qr.size() + QUIET * 2) as usize;
    let side = n * SCALE;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(side as u32, side as u32);
    let px = buf.make_mut_slice();
    for (i, p) in px.iter_mut().enumerate() {
        let (x, y) = ((i % side) / SCALE, (i / side) / SCALE);
        let dark = qr.get_module(x as i32 - QUIET, y as i32 - QUIET);
        let v = if dark { 0 } else { 255 };
        *p = slint::Rgba8Pixel {
            r: v,
            g: v,
            b: v,
            a: 255,
        };
    }
    slint::Image::from_rgba8(buf)
}

/// "Pair with QR" (Android 11+): show `WIFI:T:ADB;S:<name>;P:<password>;;`,
/// wait until the phone that scanned it announces `_adb-tls-pairing` with
/// that name over mDNS, pair with the password, then connect to the
/// phone's `_adb-tls-connect` service. Gives up after two minutes.
fn android_wifi_qr(ui: &AppWindow) {
    use zenlink_android::adb::AdbClient;
    use zenlink_android::wireless::{find_connect, find_pairing, qr_payload, random_token, Reply};
    let run = QR_RUN.fetch_add(1, Relaxed) + 1;
    let name = format!("zenlink-{}", random_token(8));
    let password = random_token(10);
    let Ok(qr) =
        qrcodegen::QrCode::encode_text(&qr_payload(&name, &password), qrcodegen::QrCodeEcc::Medium)
    else {
        return;
    };
    ui.set_android_qr(qr_image(&qr));
    ui.set_android_qr_status(i18n::tr("Waiting for the phone to scan…").into());
    ui.set_android_qr_on(true);
    let weak = ui.as_weak();
    // Status line (and a toast when the attempt ends) for this attempt only.
    let report = move |text: String, detail: String, end: bool| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if QR_RUN.load(Relaxed) != run {
                return;
            }
            let tail = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            };
            let msg = format!("{text}{tail}");
            if end {
                ui.set_android_qr_on(false);
                show_toast(&ui, msg);
            } else {
                ui.set_android_qr_status(msg.into());
            }
        });
    };
    std::thread::spawn(move || {
        let c = AdbClient::default();
        let live = || QR_RUN.load(Relaxed) == run;
        let t0 = Instant::now();
        let mut last_err = String::new();
        let found = loop {
            if !live() {
                return;
            }
            if t0.elapsed() > Duration::from_secs(120) {
                break None;
            }
            match c.mdns_services() {
                Ok(list) => {
                    if let Some(s) = find_pairing(&list, &name) {
                        break Some(s.clone());
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
            std::thread::sleep(Duration::from_millis(800));
        };
        let Some(svc) = found else {
            report(i18n::tr("The QR code was not scanned within 2 minutes. Try again, or use a pairing code"), last_err, true);
            return;
        };
        report(i18n::tr("Phone found, pairing…"), String::new(), false);
        match c.pair(&svc.host, svc.port, &password) {
            Ok(Reply::Ok(_)) => {}
            Ok(Reply::Failed(m)) => return report(i18n::tr("Pairing failed"), m, true),
            Err(e) => return report(i18n::tr("Pairing failed"), e.to_string(), true),
        }
        report(i18n::tr("Paired, connecting…"), String::new(), false);
        // The connect service may be announced a moment after pairing.
        let t1 = Instant::now();
        while live() && t1.elapsed() < Duration::from_secs(30) {
            if let Ok(list) = c.mdns_services() {
                if let Some(s) = find_connect(&list, &svc.host) {
                    match c.connect_device(&s.host, s.port) {
                        Ok(Reply::Ok(_)) => {
                            return report(
                                i18n::tr("Connected over Wi-Fi"),
                                format!("{}:{}", s.host, s.port),
                                true,
                            )
                        }
                        Ok(Reply::Failed(m)) => return report(i18n::tr("Connect failed"), m, true),
                        Err(e) => return report(i18n::tr("Connect failed"), e.to_string(), true),
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        if live() {
            report(
                i18n::tr(
                    "Paired. Enter the address on the Wireless debugging screen and press Connect",
                ),
                String::new(),
                true,
            );
        }
    });
}

/// The list row Start uses: the phone picked here or in the device menu,
/// else the first ready one (like the Apple list).
fn android_picked_index() -> Option<usize> {
    use zenlink_android::discovery::Readiness;
    APP.with_borrow(|a| {
        let picked = a.rail_sel.as_deref().and_then(|k| k.strip_prefix("a:"));
        picked
            .and_then(|k| a.android_list.iter().position(|d| d.id.key == k))
            .or_else(|| {
                a.android_list
                    .iter()
                    .position(|d| d.readiness == Readiness::Ready)
            })
    })
}

/// Highlights the picked row.
fn android_mark_picked(ui: &AppWindow) {
    use slint::Model;
    let sel = android_picked_index();
    let model = ui.get_android_devices();
    for i in 0..model.row_count() {
        if let Some(mut r) = model.row_data(i) {
            if r.selected != (sel == Some(i)) {
                r.selected = sel == Some(i);
                model.set_row_data(i, r);
            }
        }
    }
    ui.set_android_selected(sel.map_or(-1, |i| i as i32));
}

fn android_start(ui: &AppWindow, index: i32) {
    use zenlink_android::devices::Connection;
    use zenlink_android::discovery::Readiness;
    android_stop(ui);
    let shared = Arc::new(android::Shared::default());
    let (source, link): (Box<dyn zenlink_core::source::VideoSource>, String) = if let Some(f) =
        std::env::var_os("ZL_ANDROID_FIXTURE")
    {
        (
            Box::new(android::FixtureSource::new(f.into(), shared.clone())),
            i18n::tr("Test recording"),
        )
    } else {
        let dev = APP.with_borrow(|a| {
            let ready =
                |d: &&zenlink_android::discovery::AndroidDevice| d.readiness == Readiness::Ready;
            // -1: the picked row (title-bar Start).
            let i = usize::try_from(index).ok();
            i.or_else(android_picked_index)
                .and_then(|i| a.android_list.get(i))
                .filter(ready)
                .cloned()
        });
        // A phone on both USB and Wi-Fi: use the link the user picked.
        let via = APP.with_borrow(|a| a.android_via);
        let dev = dev.map(|mut d| {
            if let Some(t) = via.and_then(|w| {
                d.transports
                    .iter()
                    .find(|t| t.connection.is_wireless() == w)
                    .cloned()
            }) {
                d.primary = t;
            }
            d
        });
        let Some(d) = dev else {
            let msg = if android_picked_index().is_some() {
                i18n::tr(
                    "The picked device is not ready yet. Follow the steps below, then press Start.",
                )
            } else {
                i18n::tr("Pick a device first. Plug in the phone with USB debugging on, or connect it over Wi-Fi.")
            };
            show_toast(ui, msg);
            return;
        };
        let server = match scrcpy_server() {
            Ok(s) => s,
            Err(e) => {
                log::warn!("android: {e}");
                ui.set_android_message(
                    trf!(
                        "The scrcpy server file is missing ({e}). Reinstall ZenLink.",
                        e = e
                    )
                    .into(),
                );
                return;
            }
        };
        let link = match d.primary.connection {
            Connection::Usb => "USB",
            Connection::WifiTcp { .. } | Connection::WifiTls { .. } => "Wi-Fi",
            Connection::Emulator => "Emulator",
            Connection::Unknown => "",
        };
        android_select_device(ui, &d);
        APP.with_borrow_mut(|a| a.apps_device = Some(d.clone()));
        let known = APP.with_borrow(|a| a.android_encoders.get(&d.id.key).cloned());
        let p = ui.get_prefs();
        (
            Box::new(android::DeviceSource::new(
                d.primary.serial.clone(),
                d.primary.transport_id,
                d.name.display.clone(),
                server,
                shared.clone(),
                android_options(&p, d.audio_supported, d.sdk),
                android::Smart {
                    pref: android_pref(&p),
                    dec: android_decoders(),
                    known,
                },
            )),
            link.to_string(),
        )
    };
    ui.set_android_message("".into());
    APP.with_borrow_mut(|a| {
        android::set_active(Some(shared.clone()));
        a.android = Some(shared);
        let src = Arc::new(Mutex::new(source));
        let source: Box<dyn zenlink_core::source::VideoSource> =
            Box::new(android::Kept(src.clone()));
        a.android_src = Some(src);
        a.android_index = index;
        a.android_good_saved = false;
        a.android_key = Some(android_key(&ui.get_prefs()));
        a.android_link = link;
        a.android_shown = None;
        a.fitted = None;
        match a.view.as_mut() {
            Some(v) => v.start_source(source),
            None => a.android_pending = Some(source),
        }
    });
    android_tick(ui);
}

/// v2.6 device menu: Android phones (ADB) and iPhones/iPads (backend) in
/// one list, the selected one first in line for Start and Files.
fn rail_refresh(ui: &AppWindow) {
    use slint::Model;
    use zenlink_android::devices::Connection;
    use zenlink_android::discovery::Readiness;
    let live_name =
        (ui.get_android_phase().as_str() == "streaming").then(|| ui.get_android_device());
    let (list, popped, sel) =
        APP.with_borrow(|a| (a.android_list.clone(), a.popped.clone(), a.rail_sel.clone()));
    let mut rows = Vec::new();
    let mut keys = Vec::new();
    for d in &list {
        let key = format!("a:{}", d.id.key);
        let ready = d.readiness == Readiness::Ready;
        let pop = popped.contains(&key);
        let live = live_name.as_deref() == Some(d.name.display.as_str());
        let state = if pop {
            "window"
        } else if live {
            "live"
        } else if ready {
            "ready"
        } else {
            "need"
        };
        let link = match d.primary.connection {
            Connection::Usb => "USB",
            Connection::WifiTcp { .. } | Connection::WifiTls { .. } => "Wi-Fi",
            Connection::Emulator => "Emulator",
            Connection::Unknown => "",
        };
        let word = match state {
            "live" => "Live".to_string(),
            "ready" => i18n::tr("Ready"),
            _ => i18n::tr("Needs attention"),
        };
        rows.push(RailDev {
            kind: "android".into(),
            title: d.name.display.clone().into(),
            sub: format!("{link} · {word}").into(),
            state: state.into(),
            selected: false,
            files: ready && !pop,
            pop: ready && !pop,
        });
        keys.push(key);
    }
    let apple_live = ui.get_phase().as_str() == "streaming";
    let mut apple_default = None;
    let mut apple_streaming = None;
    for r in ui.get_apple_devices().iter() {
        let key = format!("i:{}", r.id);
        if r.selected {
            apple_default = Some(key.clone());
        }
        let state = match r.state.as_str() {
            "ready" if apple_live && r.selected => "live",
            "live" => "live",
            "ready" => "ready",
            "need" => "need",
            _ => "off",
        };
        // The engine's own "live" beats the guess from the selected device.
        if r.state.as_str() == "live" || (state == "live" && apple_streaming.is_none()) {
            apple_streaming = Some(key.clone());
        }
        rows.push(RailDev {
            kind: "apple".into(),
            title: r.title.clone(),
            sub: if state == "live" {
                "Live".into()
            } else {
                r.sub.clone()
            },
            state: state.into(),
            selected: false,
            files: false,
            pop: false,
        });
        keys.push(key);
    }
    // Keep the pick while it is listed; otherwise the first phone of the
    // workspace on screen.
    // A pick of the other family (Android while the Apple page shows) would
    // highlight a page that is not on screen.
    let want = match ui.get_screen().as_str() {
        "apple" => "i:",
        "android" => "a:",
        _ => "",
    };
    // The device on screen wins over an older pick (the picture is the iPad, so is the highlight).
    let sel = apple_streaming
        .filter(|_| want == "i:")
        .or(sel)
        .filter(|k| keys.contains(k) && k.starts_with(want))
        .or_else(|| {
            if ui.get_screen().as_str() == "apple" {
                apple_default.or_else(|| keys.iter().find(|k| k.starts_with("i:")).cloned())
            } else {
                keys.iter().find(|k| k.starts_with("a:")).cloned()
            }
        });
    // On Home only Home is highlighted; the pick is kept for coming back.
    let home = ui.get_screen().as_str() == "platform";
    for (r, k) in rows.iter_mut().zip(&keys) {
        r.selected = !home && sel.as_ref() == Some(k);
    }
    ui.set_rail_devices(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    APP.with_borrow_mut(|a| {
        a.rail_keys = keys;
        a.rail_sel = sel;
    });
}

/// The Android phone selected in the device menu, if it is one.
fn rail_android() -> Option<zenlink_android::discovery::AndroidDevice> {
    APP.with_borrow(|a| {
        let key = a.rail_sel.as_deref()?.strip_prefix("a:")?;
        a.android_list.iter().find(|d| d.id.key == key).cloned()
    })
}

fn rail_pick(ui: &AppWindow, i: i32) {
    let Some(key) = APP.with_borrow(|a| a.rail_keys.get(usize::try_from(i).ok()?).cloned()) else {
        return;
    };
    APP.with_borrow_mut(|a| a.rail_sel = Some(key.clone()));
    let remember = ui.get_prefs().remember_platform;
    if let Some(id) = key.strip_prefix("i:") {
        ui.set_drawer_files(false);
        ui.set_drawer_screen(false);
        ui.invoke_panel_toggled(false);
        if ui.get_screen().as_str() != "apple" {
            if !android_park(true) {
                android_stop(ui);
            }
            ui.invoke_choose_platform("apple".into(), remember);
        }
        // Straight to the device: its session (if any) shows at once.
        apple::engine().show(Some(id.to_string()));
        ui.invoke_apple_pick(id.into());
    } else {
        if ui.get_screen().as_str() != "android" {
            if apple::in_use() {
                apple::engine().show(None); // Apple sessions keep running off screen
            } else if matches!(
                ui.get_phase().as_str(),
                "connecting" | "activating" | "streaming"
            ) {
                ui.invoke_stop_cast(); // Hybrid / ZenLink Studio run in the backend
            }
            ui.invoke_choose_platform("android".into(), remember);
            android_park(false);
        }
        if ui.get_drawer_files() {
            files_show_selected(ui);
        }
    }
    rail_refresh(ui);
}

/// Shows the selected Android phone in the Files drawer; false (and a
/// toast) when no ready phone is selected.
fn files_show_selected(ui: &AppWindow) -> bool {
    use zenlink_android::discovery::Readiness;
    match rail_android().filter(|d| d.readiness == Readiness::Ready) {
        Some(d) => {
            filewin::show(
                ui,
                filewin::FileArgs {
                    serial: d.primary.serial.clone(),
                    transport_id: d.primary.transport_id,
                    device: d.name.display.clone(),
                },
            );
            true
        }
        None => {
            show_toast(ui, i18n::tr("Pick a ready Android phone first."));
            false
        }
    }
}

/// Moves menu row `i` (a ready Android phone) to a window of its own, like
/// dragging a Chrome tab out. The window's Merge button brings it back.
fn rail_pop(ui: &AppWindow, i: i32) {
    use zenlink_android::discovery::Readiness;
    let Some((key, d)) = APP.with_borrow(|a| {
        let key = a.rail_keys.get(usize::try_from(i).ok()?)?.clone();
        let id = key.strip_prefix("a:")?;
        let d = a.android_list.iter().find(|d| d.id.key == id)?.clone();
        Some((key, d))
    }) else {
        return;
    };
    if d.readiness != Readiness::Ready {
        return;
    }
    // One phone, one mirror: the main window lets go of it first.
    if ui.get_android_phase().as_str() != "idle"
        && ui.get_android_device() == d.name.display.as_str()
    {
        android_stop(ui);
    }
    let name = d.name.display.clone();
    let child = appwin::spawn(&appwin::AppArgs {
        serial: d.primary.serial.clone(),
        transport_id: d.primary.transport_id,
        package: String::new(),
        name: name.clone(),
        device: name.clone(),
        screen: true,
    });
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            log::warn!("android: window failed: {e}");
            show_toast(ui, trf!("The window did not open: {e}", e = e));
            return;
        }
    };
    log::info!("android: {name} moved to its own window");
    APP.with_borrow_mut(|a| a.popped.push(key.clone()));
    rail_refresh(ui);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let merge = child.wait().ok().and_then(|s| s.code()) == Some(appwin::MERGE_EXIT);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            APP.with_borrow_mut(|a| a.popped.retain(|k| *k != key));
            if merge {
                log::info!("android: {name} merged back into the main window");
                APP.with_borrow_mut(|a| a.rail_sel = Some(key.clone()));
                if ui.get_screen().as_str() != "android" {
                    let remember = ui.get_prefs().remember_platform;
                    ui.invoke_choose_platform("android".into(), remember);
                }
                android_start(&ui, -1);
            }
            rail_refresh(&ui);
        });
    });
}

/// Another device goes on screen (`true`): a live Android session keeps running, neither
/// drawn nor heard. `false` brings a parked one back. Returns whether a session moved.
fn android_park(park: bool) -> bool {
    APP.with_borrow_mut(|a| {
        let (Some(s), Some(src)) = (a.android.clone(), a.android_src.clone()) else {
            return false;
        };
        if park {
            if s.parked() || s.status().phase != android::Phase::Streaming {
                return false;
            }
            s.park(true);
            if let Some(v) = a.view.as_mut() {
                v.stop();
            }
        } else {
            if !s.parked() {
                return false;
            }
            // The Apple picture leaves first, so its state change does not stop this one.
            a.streaming = false;
            s.park(false);
            if let Some(v) = a.view.as_mut() {
                v.start_source(Box::new(android::Kept(src)));
            }
        }
        // `android_live` stays: it only counts on the Android screen.
        a.fitted = None;
        true
    })
}

/// Ends the Android session, if any.
fn android_stop(ui: &AppWindow) {
    ui.set_android_apps_open(false);
    let had = APP.with_borrow_mut(|a| {
        a.android_pending = None;
        a.android_src = None;
        let Some(s) = a.android.take() else {
            return false;
        };
        android::set_active(None);
        let parked = s.parked(); // then the view shows another device
        s.stop();
        if let Some(v) = a.view.as_mut().filter(|_| !parked) {
            v.stop();
        }
        a.android_live = false;
        a.android_shown = None;
        a.fitted = None;
        true
    });
    if had {
        ui.set_android_phase("idle".into());
        rail_refresh(ui);
    }
}

/// Mirrors the Android session's status into the window (every frame tick;
/// cheap when nothing changed).
fn android_tick(ui: &AppWindow) {
    let Some((st, link)) = APP.with_borrow(|a| {
        let st = a.android.as_ref()?.status();
        (a.android_shown.as_ref() != Some(&st)).then(|| (st, a.android_link.clone()))
    }) else {
        return;
    };
    let live = st.phase == android::Phase::Streaming;
    ui.set_android_phase(st.phase.as_str().into());
    ui.set_android_device(st.device.clone().into());
    rail_refresh(ui);
    ui.set_android_link(link.into());
    let message = match st.phase {
        android::Phase::Error => trf!("Mirroring failed: {_0}. Reconnect the cable or tap Allow on the phone, then Start again.", _0 = st.error),
        android::Phase::Connecting if !st.error.is_empty() => trf!("Retrying… ({_0})", _0 = st.error),
        _ => String::new(),
    };
    ui.set_android_message(message.into());
    let using = if st.encoder.is_empty() {
        st.codec.to_uppercase().replace("H26", "H.26")
    } else {
        format!(
            "{} · {}",
            st.codec.to_uppercase().replace("H26", "H.26"),
            st.encoder
        )
    };
    ui.global::<AndroidEnc>().set_using(if st.codec.is_empty() {
        "".into()
    } else {
        using.clone().into()
    });
    if live {
        android_session_ok(ui, &st, &using);
    }
    APP.with_borrow_mut(|a| {
        if live != a.android_live {
            a.android_live = live;
            a.fitted = None;
        }
        a.android_shown = Some(st);
    });
}

/// Ghost mode on / off (R58, see ghost.rs).
fn set_ghost(ui: &AppWindow, on: bool) {
    let Some(h) = hwnd_of(ui.window()) else {
        return;
    };
    let p = ui.get_prefs();
    if !on {
        ghost::leave(h, p.on_top);
        show_toast(ui, i18n::tr("Ghost mode off").to_string());
        return;
    }
    let weak = ui.as_weak();
    ghost::enter(h, &p.ghost_key, p.ghost_opacity, move || {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                set_ghost(&ui, false);
            }
        });
    });
    show_toast(
        ui,
        i18n::tr("Ghost mode: clicks go to the app behind. Press {key} again (or click the tray icon) to leave.")
            .replace("{key}", &p.ghost_key),
    );
}

/// Performs a window shortcut (B-26).
fn run_shortcut(
    ui: &AppWindow,
    input: &Input,
    s: shortcuts::Shortcut,
    android_screen: bool,
    apple_live: bool,
) {
    use shortcuts::Shortcut;
    let need_apple = || {
        show_toast(
            ui,
            i18n::tr("This shortcut needs Apple Driver mode while mirroring."),
        );
    };
    match s {
        Shortcut::Mute => {
            ui.invoke_action("mute".into());
            let msg = if ui.get_muted() {
                i18n::tr("Sound on the PC: off (Alt+M)")
            } else {
                i18n::tr("Sound on the PC: on (Alt+M)")
            };
            show_toast(ui, msg);
        }
        Shortcut::Settings => {
            let open = !ui.get_settings_open();
            ui.set_settings_open(open);
            ui.invoke_settings_toggled(open);
        }
        Shortcut::Rotate => {
            ui.invoke_action("rotate".into());
            let r = ui.get_prefs().rotation;
            let label = if r.as_str() == "auto" {
                "Auto".to_string()
            } else {
                format!("{r}°")
            };
            show_toast(ui, trf!("Rotation: {label}", label = label));
        }
        Shortcut::Back => ui.invoke_action(
            if android_screen {
                "back"
            } else {
                "swipe-right"
            }
            .into(),
        ),
        Shortcut::Lock => ui.invoke_action("lock".into()),
        Shortcut::CopyText => copy_screen_text(ui),
        Shortcut::ClearText => {
            ocr::clear();
            show_toast(
                ui,
                i18n::tr("Copy history cleared. The next Alt+C starts fresh."),
            );
        }
        Shortcut::Android(a) => ui.invoke_action(a.into()),
        Shortcut::Apple(_) if !apple_live => need_apple(),
        Shortcut::Apple("paste-pc") => match pc_clipboard_text() {
            Some(t) => {
                let n = t.chars().count();
                input.send(Cmd::TypeText(t));
                show_toast(ui, trf!("Typing {n} characters on the device.", n = n));
            }
            None => show_toast(ui, i18n::tr("The PC clipboard has no text.")),
        },
        Shortcut::Apple(a) => input.send(Cmd::Action(a)),
    }
}

/// First frame of a session: remember the encoder list and the working
/// configuration, and say so when a fallback was needed.
fn android_session_ok(ui: &AppWindow, st: &android::Status, using: &str) {
    let Some((key, shared, saved)) = APP.with_borrow(|a| {
        Some((
            a.android_dev.clone()?,
            a.android.clone()?,
            a.android_good_saved,
        ))
    }) else {
        return;
    };
    if saved {
        return;
    }
    if let Some(list) = shared.encoders() {
        APP.with_borrow_mut(|a| a.android_encoders.insert(key.clone(), list));
    }
    let (w, h) = shared.frame_size();
    with_store(|s| {
        s.entry(&key).last_good = Some(zenlink_android::profiles::LastGood {
            codec: st.codec.clone(),
            encoder: st.encoder.clone(),
            width: w,
            height: h,
        });
    });
    APP.with_borrow_mut(|a| a.android_good_saved = true);
    refresh_encoder_ui(ui);
    if st.fell_back {
        show_toast(
            ui,
            trf!(
                "The chosen encoder failed; now using {using}. Your choice is kept.",
                using = using
            ),
        );
    }
}

/// Tray menu words in the UI language.
fn tray_labels(p: &Prefs) -> (String, String) {
    i18n::set(p.lang.as_str()); // prefs_changed asks before apply_prefs runs
    (i18n::tr("Open ZenLink"), i18n::tr("Quit"))
}

/// "Close to the tray": hide the window, keep the session (sound keeps
/// playing; Stop or Quit from the tray ends it).
fn hide_to_tray(ui: &AppWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    if let Some(h) = hwnd_of(ui.window()) {
        // A one-off "Minimize to tray" from the close dialog needs the icon too.
        let p = ui.get_prefs();
        let (open, quit) = tray_labels(&p);
        tray::set_icon(true, &open, &quit);
        let _ = unsafe { ShowWindow(h, SW_HIDE) };
        tray::HIDDEN.store(true, Relaxed);
    }
}

/// Brings the window back (from the tray, minimised or behind others).
fn show_window(ui: &AppWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    if let Some(h) = hwnd_of(ui.window()) {
        unsafe {
            let _ = ShowWindow(
                h,
                if IsIconic(h).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetForegroundWindow(h);
        }
        tray::HIDDEN.store(false, Relaxed);
        let p = ui.get_prefs();
        let (open, quit) = tray_labels(&p);
        tray::set_icon(p.close_action == "tray", &open, &quit);
    }
}

fn quit_app(ui: &AppWindow) {
    tray::set_icon(false, "", "");
    let _ = ui.window().hide();
    let _ = slint::quit_event_loop();
}

/// "Start with Windows" follows the registry (Task Manager's Startup tab can
/// change it). Reads only; Store builds use their StartupTask instead.
fn sync_start_with_windows(ui: &AppWindow) {
    if updater::is_store() {
        return;
    }
    let on = tray::start_with_windows_on();
    let mut p = ui.get_prefs();
    if p.start_with_windows != on {
        p.start_with_windows = on;
        prefs::save(&p);
        ui.set_prefs(p);
    }
}

/// The window's close button and Alt+F4 (the tray's Quit never asks).
fn close_requested(ui: &AppWindow) {
    match ui.get_prefs().close_action.as_str() {
        "tray" => hide_to_tray(ui),
        "quit" => quit_app(ui),
        _ => ui.set_close_ask(true),
    }
}

// ---- Updates (v2.5.1): see updater.rs and zenlink_core::update. ----

thread_local! {
    /// The last offer from "Check for updates" (`None` = nothing to install).
    static UPDATE_OFFER: RefCell<Option<(zenlink_core::update::Release, bool)>> = const { RefCell::new(None) };
}

fn update_channel(p: &Prefs) -> zenlink_core::update::Channel {
    use zenlink_core::update::Channel;
    Channel::parse(p.update_channel.as_str())
        .unwrap_or_else(|| Channel::of(&updater::current_version()))
}

fn update_clear(ui: &AppWindow) {
    UPDATE_OFFER.with_borrow_mut(|o| *o = None);
    ui.set_upd_version("".into());
    ui.set_upd_notes("".into());
    ui.set_upd_switch(false);
    ui.set_upd_progress(-1.0);
}

/// Asks GitHub for the newest release on the chosen channel. `manual`: the
/// user pressed the button (a startup check only shows a toast when there
/// is something new).
fn update_check(ui: &AppWindow, manual: bool) {
    use zenlink_core::update::Offer;
    if ui.get_upd_busy() {
        return;
    }
    let channel = update_channel(&ui.get_prefs());
    update_clear(ui);
    ui.set_upd_busy(true);
    ui.set_upd_status(i18n::tr("Checking for updates…").into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let r = updater::check(channel);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_upd_busy(false);
            match r {
                Ok(Offer::UpToDate) => {
                    log::info!("update: up to date ({channel:?})");
                    ui.set_upd_status(i18n::tr("ZenLink is up to date.").into());
                }
                Ok(offer) => {
                    let (rel, switch) = match offer {
                        Offer::Update(r) => (r, false),
                        Offer::Switch(r) => (r, true),
                        Offer::UpToDate => unreachable!(),
                    };
                    log::info!("update: offer {} (switch {switch})", rel.tag);
                    let v = rel.version.to_string();
                    let status = if switch {
                        trf!("Other edition: v{v}. Installs as a separate app; settings do not move over.", v = v)
                    } else {
                        trf!("New version available: v{v}", v = v)
                    };
                    ui.set_upd_status(status.into());
                    ui.set_upd_notes(zenlink_core::update::short_notes(&rel.notes, 360).into());
                    ui.set_upd_version(v.clone().into());
                    ui.set_upd_switch(switch);
                    if !manual && !switch {
                        let t = trf!("ZenLink v{v} is available. Open Settings › Updates.", v = v);
                        show_toast(&ui, t);
                    }
                    UPDATE_OFFER.with_borrow_mut(|o| *o = Some((rel, switch)));
                }
                Err(e) => {
                    log::warn!("update: check failed: {e}");
                    if manual {
                        ui.set_upd_status(trf!("Could not check: {e}", e = e).into());
                    } else {
                        ui.set_upd_status("".into());
                    }
                }
            }
        });
    });
}

/// Opens a page of this repository's releases in the default browser.
fn open_release_page(url: &str) {
    if url.starts_with("https://github.com/zenvyzenshiro/zenlink/") {
        let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
    }
}

/// "Update to vX": download, verify, start, quit (an update of this
/// edition) or keep running (the other edition installs next to it).
fn update_install(ui: &AppWindow) {
    let Some((rel, switch)) = UPDATE_OFFER.with_borrow(|o| o.clone()) else {
        return;
    };
    if ui.get_upd_busy() {
        return;
    }
    let kind = updater::kind();
    let Some(asset) = updater::asset_for(kind, &rel).cloned() else {
        // A source checkout (or a release without a verified file):
        // the page, where the user downloads it.
        open_release_page(&rel.page);
        return;
    };
    let dest = match updater::download_path(kind, &asset) {
        Ok(d) => d,
        Err(e) => {
            ui.set_upd_status(trf!("Failed: {e}", e = e).into());
            return;
        }
    };
    ui.set_upd_busy(true);
    ui.set_upd_progress(0.0);
    ui.set_upd_status(trf!("Downloading {_0}…", _0 = asset.name).into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let w = weak.clone();
        let progress = move |done: u64, total: u64| {
            let w = w.clone();
            let f = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    ui.set_upd_progress(f);
                }
            });
        };
        let r = updater::download(&asset, &dest, &progress).and_then(|_| {
            log::info!(
                "update: {} verified (sha256 {})",
                dest.display(),
                asset.sha256
            );
            updater::launch(kind, &dest, !switch)
        });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_upd_busy(false);
            ui.set_upd_progress(-1.0);
            match r {
                Ok(()) if switch => {
                    ui.set_upd_status(i18n::tr("The other edition's installer is open.").into());
                }
                Ok(()) => {
                    log::info!("update: started {}, quitting", rel.tag);
                    quit_app(&ui);
                }
                Err(e) => {
                    log::warn!("update: {e}");
                    ui.set_upd_status(trf!("Update failed: {e}", e = e).into());
                }
            }
        });
    });
}

fn main() -> Result<(), slint::PlatformError> {
    // v2.5.1: `ZenLink.exe --android-app …` is one Android app in its own
    // window (no backend, no tray, no single-instance check).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(appwin::ARG) {
        init_log_named(APP_LOG_FILE_NAME);
        let Some(a) = appwin::AppArgs::parse(&args) else {
            log::warn!("app window: bad arguments {args:?}");
            return Ok(());
        };
        return appwin::run(a);
    }
    // Tests run with their own prefs file (ZL_PREFS) and may run next to the
    // user's ZenLink; everyone else gets one ZenLink per user: a second start
    // shows the running one (also when it is hidden in the tray).
    let test_mode = std::env::var_os("ZL_PREFS").is_some();
    // A portable update: the old exe quits; wait for it before the
    // single-instance check, then delete it.
    updater::finish_portable_update();
    if !test_mode && tray::show_running_instance() {
        return Ok(());
    }
    let background = std::env::args().any(|a| a == tray::BACKGROUND_ARG);
    init_log();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .with_winit_window_attributes_hook(|attrs| {
            attrs
                .with_transparent(true)
                .with_decorations(false)
                .with_system_backdrop(BackdropType::None)
        })
        .select()?;

    let ui = AppWindow::new()?;
    ui.global::<Theme>().set_icons(icon_font().into());
    ui.global::<AppInfo>()
        .set_version(env!("CARGO_PKG_VERSION").into());
    ui.global::<AppInfo>().set_store(updater::is_store());
    ui.set_prefs(prefs::load());
    {
        let weak = ui.as_weak();
        tray::install(move |e| {
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = w.upgrade() else { return };
                match e {
                    // In Ghost mode the window cannot be clicked: the tray brings it back.
                    tray::Event::Toggle | tray::Event::Show if ghost::is_on() => {
                        set_ghost(&ui, false)
                    }
                    tray::Event::Toggle if !tray::HIDDEN.load(Relaxed) => hide_to_tray(&ui),
                    tray::Event::Toggle | tray::Event::Show => show_window(&ui),
                    tray::Event::Quit => quit_app(&ui),
                }
            });
        });
        let p = ui.get_prefs();
        let (open, quit) = tray_labels(&p);
        tray::set_icon(p.close_action == "tray", &open, &quit);
        if !test_mode && !updater::is_store() {
            // Keeps the Run entry pointing at this exe (a moved Portable.exe).
            // The Store edition starts with Windows through its StartupTask
            // (Windows Settings > Apps > Startup), not the Run key.
            // The toggle shows the real state (Task Manager can turn it off);
            // off never writes anything, so a first launch leaves no Run entry.
            sync_start_with_windows(&ui);
            if ui.get_prefs().start_with_windows {
                tray::set_start_with_windows(true, false);
            }
        }
        filedrop::install(&ui);
        filewin::install(&ui);
        {
            // Close dialog: act, and with "Remember" keep it as the close action.
            let weak = ui.as_weak();
            ui.on_close_choice(move |act, remember| {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_close_ask(false);
                if remember {
                    let mut p = ui.get_prefs();
                    p.close_action = act.clone();
                    ui.invoke_prefs_changed(p);
                }
                match act.as_str() {
                    "tray" => hide_to_tray(&ui),
                    "quit" => quit_app(&ui),
                    _ => {}
                }
            });
        }
        let weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                close_requested(&ui);
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    zenlink_core::api::serve(backend::handle);
    let api = Api::new();
    let input = Input::new();
    let touch_on = Arc::new(AtomicBool::new(ui.get_prefs().touch));
    let gestures_on = Arc::new(AtomicBool::new(ui.get_prefs().gestures));
    ui.set_message("Menyiapkan ZenLink…".into());
    // Test hook: every settings card starts with its explanation open.
    if std::env::var_os("ZL_INFO_OPEN").is_some() {
        ui.global::<UiTest>().set_info_open(true);
    }
    if std::env::var_os("ZL_OPEN_SETTINGS").is_some() {
        ui.set_settings_open(true);
    }

    // The in-process API (backend.rs), then a state poll loop.
    {
        let weak = ui.as_weak();
        let api = api.clone();
        std::thread::spawn(move || loop {
            if let Some(mut st) = api.state() {
                apple::overlay(&mut st);
                airplay::overlay(&mut st);
                usbdirect::overlay(&mut st);
                backend::observe(&st);
                let w = weak.clone();
                if slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        on_state(&ui, &st);
                    }
                })
                .is_err()
                {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(400));
        });
    }

    // Callbacks.
    {
        let api = api.clone();
        let weak = ui.as_weak();
        ui.on_start_cast(move || {
            if let Some(ui) = weak.upgrade() {
                // The card opens at step 0 (detect device) at once.
                let airplay = ui.get_mode_link().as_str() == "wireless"
                    && ui.get_flow_kind().as_str() == "usbdirect";
                let picked = ui
                    .get_apple_devices()
                    .iter()
                    .any(|d| d.selected && d.state != "offline");
                if !airplay && !picked {
                    show_toast(
                        &ui,
                        i18n::tr("Pick an iPhone or iPad first. Plug it in with the cable and unlock it; it shows up in the list."),
                    );
                    return;
                }
                if !airplay {
                    ui.set_flow_failed(false);
                    ui.set_flow_step(0);
                    APP.with_borrow_mut(|a| a.flow_started = Some(Instant::now()));
                }
            }
            let a = api.clone();
            std::thread::spawn(move || cast_start(&a));
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_flow_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_flow_step(-1);
                ui.set_flow_failed(false);
            }
        });
    }
    {
        // Apple device list: pick the device Start uses, or forget a saved one.
        let weak = ui.as_weak();
        let api2 = api.clone();
        ui.on_apple_pick(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let offline = ui
                .get_apple_devices()
                .iter()
                .find(|d| d.id == id)
                .map(|d| (d.state == "offline", d.title.to_string()));
            if let Some((true, name)) = offline {
                let t = trf!("{name} is not connected. Plug in the cable (or join the same Wi-Fi), unlock it, then press Start.", name = name);
                show_toast(&ui, t);
            }
            let a = api2.clone();
            let id = id.to_string();
            std::thread::spawn(move || a.post("/api/device/select", &json!({ "id": id })));
        });
        let api2 = api.clone();
        ui.on_apple_forget(move |id| {
            let a = api2.clone();
            let id = id.to_string();
            std::thread::spawn(move || {
                a.post("/api/device/select", &json!({ "id": id, "forget": true }))
            });
        });
    }
    {
        // AirPlay waiting view: Windows Firewall keeps the device out.
        let weak = ui.as_weak();
        ui.on_allow_network(move || {
            let weak = weak.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_firewall_msg(i18n::tr("Waiting for administrator permission…").into());
            }
            std::thread::spawn(move || {
                let r = drivers::allow_network();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let msg = match r {
                        Ok(()) => {
                            ui.set_firewall_blocked(false);
                            String::new()
                        }
                        Err(e) => tr_backend(&e),
                    };
                    ui.set_firewall_msg(msg.into());
                });
            });
        });
    }
    {
        // The ready view's button when the device needs something first.
        let weak = ui.as_weak();
        ui.on_need_fix(move || {
            let Some(ui) = weak.upgrade() else { return };
            match ui.get_device_need().as_str() {
                n @ ("restore" | "bind" | "removefilter") => confirm_driver(&ui, n),
                "mode" => {
                    ui.set_chooser_cancelable(true);
                    open_chooser(&ui);
                }
                _ => {
                    ui.set_settings_open(true);
                    ui.invoke_settings_toggled(true);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let api = api.clone();
        ui.on_sheet_answer(move |yes| {
            let Some(ui) = weak.upgrade() else { return };
            let kind = APP.with_borrow_mut(|a| a.sheet.take()).unwrap_or_default();
            if kind == "busy" {
                APP.with_borrow_mut(|a| a.sheet = Some(kind));
                return; // no buttons while the change runs
            }
            ui.set_sheet_open(false);
            if !yes {
                return;
            }
            if kind == "mfp" {
                // Windows' own Settings page (no download by ZenLink).
                let _ = std::process::Command::new("explorer.exe")
                    .arg("ms-settings:optionalfeatures")
                    .spawn();
            } else if kind == "hevc" {
                // HEVC Video Extensions from Device Manufacturer (free).
                let _ = std::process::Command::new("explorer.exe")
                    .arg("ms-windows-store://pdp/?productid=9N4WGH0Z6VHQ")
                    .spawn();
            } else if let Some(action) = kind.strip_prefix("drv:") {
                run_driver(&ui, &api, action.to_string());
            } else if kind == "restart" {
                show_toast(&ui, i18n::tr("Restarting the ZenLink service…"));
                let api = api.clone();
                std::thread::spawn(move || api.post_slow("/api/app/restart", &json!({}), 20));
            }
        });
    }
    {
        let api = api.clone();
        ui.on_stop_cast(move || {
            let a = api.clone();
            std::thread::spawn(move || cast_stop(&a));
        });
    }
    {
        let api = api.clone();
        let weak = ui.as_weak();
        ui.on_restart(move || {
            if let Some(ui) = weak.upgrade() {
                show_toast(&ui, i18n::tr("Restarting the connection…"));
            }
            let a = api.clone();
            std::thread::spawn(move || {
                if apple::in_use() {
                    apple::engine().stop();
                    if apple::start() {
                        return;
                    }
                }
                if usbdirect::stop() {
                    usbdirect::start();
                    return;
                }
                a.stop_cast();
                // The backend ignores Start while the old session is still
                // closing (that can take a few seconds): wait until it is
                // gone ("capture" is null), then start again.
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(15) {
                    match a.state() {
                        Some(st) if st.get("capture").is_none_or(Value::is_null) => break,
                        _ => std::thread::sleep(Duration::from_millis(250)),
                    }
                }
                std::thread::sleep(Duration::from_millis(300));
                a.start_cast();
            });
        });
    }
    {
        let weak = ui.as_weak();
        let (t, g) = (touch_on.clone(), gestures_on.clone());
        let last = std::cell::RefCell::new(ui.get_prefs());
        ui.on_prefs_changed(move |mut p| {
            // Video profile logic (phase A5), then store for the device.
            devprofiles::normalize(&last.borrow(), &mut p);
            if p.start_with_windows != last.borrow().start_with_windows
                && !test_mode
                && !updater::is_store()
            {
                tray::set_start_with_windows(p.start_with_windows, true);
            }
            let (open, quit) = tray_labels(&p);
            tray::set_icon(
                p.close_action == "tray" || tray::HIDDEN.load(Relaxed),
                &open,
                &quit,
            );
            let android_changed = devprofiles::picture_of(&p)
                != devprofiles::picture_of(&last.borrow())
                || p.android_profile != last.borrow().android_profile;
            *last.borrow_mut() = p.clone();
            if let Some(ui) = weak.upgrade() {
                if ui.get_prefs() != p {
                    ui.set_prefs(p.clone());
                }
                if android_changed {
                    android_store_prefs(&p);
                    refresh_encoder_ui(&ui);
                }
            }
            prefs::save(&p);
            t.store(p.touch, Relaxed);
            g.store(p.gestures, Relaxed);
            if let Some(ui) = weak.upgrade() {
                apply_look(&ui);
                push_audio(&p, ui.get_muted());
                // "Device only" sound and the Android picture options are
                // server start options: restart a running Android session.
                let key = android_key(&p);
                let restart = APP.with_borrow_mut(|a| {
                    let changed = a.android_key.as_deref() != Some(key.as_str());
                    if changed {
                        a.android_key = Some(key);
                    }
                    changed && a.android.is_some()
                });
                if restart {
                    let i = APP.with_borrow(|a| a.android_index);
                    show_toast(&ui, i18n::tr("Applying the Android settings…"));
                    android_start(&ui, i);
                }
                // Only picture settings refit the window. Any other setting
                // (volume, lip sync, theme...) used to shrink a window the
                // user had resized back to the default fit.
                let key = fit_prefs_key(&p);
                APP.with_borrow_mut(|a| {
                    if a.fit_prefs.as_deref() != Some(key.as_str()) {
                        a.fit_prefs = Some(key);
                        a.fitted = None;
                    }
                });
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_settings_toggled(move |open| {
            let Some(ui) = weak.upgrade() else { return };
            if open && !test_mode {
                sync_start_with_windows(&ui);
            }
            // The window grows/shrinks frame by frame with the panel slide
            // (see the frame timer), keeping the picture size steady.
            let sf = ui.window().scale_factor();
            let stage = ui.window().size().to_logical(sf).width - ui.get_chrome_w();
            // Same timing as the v2.2.31 Slint animation: 320 ms open, 240 ms close.
            let dur = Duration::from_millis(if open { 320 } else { 240 });
            let to = if open { 1.0 } else { 0.0 };
            let from = ui.get_sp();
            APP.with_borrow_mut(|a| {
                a.slide = Some((from, to, Instant::now(), dur));
                a.hold_stage = Some(stage);
                if let Some((w, h, _)) = a.fitted {
                    a.fitted = Some((w, h, open));
                }
            });
        });
    }
    ui.set_sp(if ui.get_settings_open() { 1.0 } else { 0.0 });
    ui.set_mp(if ui.get_menu_open() { 1.0 } else { 0.0 });
    if ui.get_settings_open() || ui.get_menu_open() {
        // Settings start open: the window is the usual idle stage plus the
        // panel, not the panel squeezed into the default window (the window
        // has no size yet here, so do not measure it).
        APP.with_borrow_mut(|a| a.hold_stage = Some(IDLE_STAGE_W));
    }
    // First screen: the platform selector, unless the user opted in to
    // remembering the platform. On iPhone / iPad the Apple chooser shows at
    // once unless a remembered mode exists (the backend can take a while).
    {
        let p = ui.get_prefs();
        android::set_audio(
            p.audio_out.as_str() != "device",
            p.volume / 100.0,
            ui.get_muted(),
        );
        apple::set_audio(
            p.audio_out.as_str() != "device",
            p.volume / 100.0,
            ui.get_muted(),
        );
        ui.set_hevc_ok(hevc_ok());
        check_media_foundation(&ui);
        let f = flow::Flow::launch(&p.platform, p.remember_platform, mode_remembered());
        apply_flow(&ui, &f);
        APP.with_borrow_mut(|a| a.flow = Some(f));
        // Widen once the window exists on screen.
        let w = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(60), move || {
            if let Some(ui) = w.upgrade() {
                if ui.get_chooser_open() || ui.get_screen().as_str() == "platform" {
                    widen_for_sheet(&ui);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_choose_platform(move |platform, remember| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(screen) = flow::parse_platform(platform.as_str()) else {
                return;
            };
            let mut p = ui.get_prefs();
            p.platform = platform.clone();
            p.remember_platform = remember;
            ui.set_prefs(p.clone());
            prefs::save(&p);
            // Before the backend answered, its settings file tells whether
            // the Apple chooser is needed.
            let needs = APP
                .with_borrow(|a| a.apple_needs)
                .unwrap_or_else(|| !mode_remembered());
            let f = APP.with_borrow_mut(|a| {
                let f = a
                    .flow
                    .get_or_insert_with(|| flow::Flow::launch("", false, true));
                f.choose(screen, needs);
                f.clone()
            });
            apply_flow(&ui, &f);
        });
    }
    {
        let weak = ui.as_weak();
        let api = api.clone();
        ui.on_change_platform(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Leave safely: end a running Apple session first.
            if APP.with_borrow(|a| a.streaming)
                || matches!(
                    ui.get_phase().as_str(),
                    "connecting" | "activating" | "streaming"
                )
            {
                let a = api.clone();
                std::thread::spawn(move || cast_stop(&a));
            }
            android_stop(&ui);
            let f = APP.with_borrow_mut(|a| {
                let f = a
                    .flow
                    .get_or_insert_with(|| flow::Flow::launch("", false, true));
                f.back();
                f.clone()
            });
            apply_flow(&ui, &f);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_android_wifi_device(move |i| {
            if let Some(ui) = weak.upgrade() {
                android_wifi_device(&ui, i);
            }
        });
        let weak = ui.as_weak();
        ui.on_android_wifi_connect(move |addr| {
            if let Some(ui) = weak.upgrade() {
                android_wifi_connect(&ui, addr.to_string());
            }
        });
        let weak = ui.as_weak();
        ui.on_android_wifi_pair(move |addr, code| {
            if let Some(ui) = weak.upgrade() {
                android_wifi_pair(&ui, addr.to_string(), code.to_string());
            }
        });
        let weak = ui.as_weak();
        ui.on_android_wifi_qr(move || {
            if let Some(ui) = weak.upgrade() {
                android_wifi_qr(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_install_adb(move || {
            if ADB_INSTALLING.swap(true, Relaxed) {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_adb_error("".into());
                ui.set_adb_progress(0.0);
            }
            let weak = weak.clone();
            std::thread::spawn(move || {
                let w = weak.clone();
                let progress = move |done: u64, total: u64| {
                    let f = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    let w = w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_adb_progress(f);
                        }
                    });
                };
                let result = android::install_adb(&progress);
                match &result {
                    Ok(p) => log::info!("android: adb installed at {}", p.display()),
                    Err(e) => log::warn!("android: adb install failed: {e}"),
                }
                ADB_INSTALLING.store(false, Relaxed);
                ADB_RETRY.store(true, Relaxed);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_adb_progress(-1.0);
                        if let Err(e) = result {
                            ui.set_adb_error(
                                format!(
                                    "{} ({e})",
                                    i18n::tr("Could not install adb. Check the internet connection and try again.")
                                )
                                .into(),
                            );
                        } else {
                            ui.set_adb_missing(false);
                        }
                    }
                });
            });
        });
        let weak = ui.as_weak();
        ui.on_android_wifi_qr_cancel(move || {
            QR_RUN.fetch_add(1, Relaxed);
            if let Some(ui) = weak.upgrade() {
                ui.set_android_qr_on(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_android_start(move |i| {
            if let Some(ui) = weak.upgrade() {
                APP.with_borrow_mut(|a| a.android_via = None);
                android_start(&ui, i);
            }
        });
        let weak = ui.as_weak();
        ui.on_android_pick(move |i| {
            if let Some(ui) = weak.upgrade() {
                let key = APP.with_borrow(|a| {
                    a.android_list
                        .get(usize::try_from(i).ok()?)
                        .map(|d| d.id.key.clone())
                });
                if let Some(key) = key {
                    APP.with_borrow_mut(|a| a.rail_sel = Some(format!("a:{key}")));
                    rail_refresh(&ui);
                    android_mark_picked(&ui);
                }
            }
        });
        let weak = ui.as_weak();
        ui.on_android_start_via(move |i, via| {
            if let Some(ui) = weak.upgrade() {
                APP.with_borrow_mut(|a| a.android_via = Some(via.as_str() == "wifi"));
                android_start(&ui, i);
            }
        });
        let weak = ui.as_weak();
        ui.on_rail_pick(move |i| {
            if let Some(ui) = weak.upgrade() {
                rail_pick(&ui, i);
            }
        });
        let weak = ui.as_weak();
        ui.on_rail_pop(move |i| {
            if let Some(ui) = weak.upgrade() {
                rail_pop(&ui, i);
            }
        });
        let weak = ui.as_weak();
        ui.on_adding_opened(move || {
            // The window may be as narrow as a phone picture: room for the page.
            if let Some(ui) = weak.upgrade() {
                widen_for_sheet(&ui);
            }
        });
        let weak = ui.as_weak();
        ui.on_files_toggled(move |open| {
            if let Some(ui) = weak.upgrade() {
                if open && !files_show_selected(&ui) {
                    ui.set_drawer_files(false);
                    ui.invoke_panel_toggled(false);
                }
            }
        });
        let weak = ui.as_weak();
        ui.on_panel_toggled(move |_| {
            let Some(ui) = weak.upgrade() else { return };
            // One panel at a time: switching Screen <-> Files keeps it open.
            let open = ui.get_drawer_screen() || ui.get_drawer_files();
            let to = if open { 1.0 } else { 0.0 };
            let from = ui.get_pp();
            if (from - to).abs() < 0.001 {
                return;
            }
            let sf = ui.window().scale_factor();
            let stage = ui.window().size().to_logical(sf).width - ui.get_chrome_w();
            let dur = Duration::from_millis(if open { 320 } else { 240 });
            APP.with_borrow_mut(|a| {
                a.panel_slide = Some((from, to, Instant::now(), dur));
                a.hold_stage = Some(stage);
            });
        });
        let weak = ui.as_weak();
        ui.on_menu_toggled(move |open| {
            let Some(ui) = weak.upgrade() else { return };
            let sf = ui.window().scale_factor();
            let stage = ui.window().size().to_logical(sf).width - ui.get_chrome_w();
            let dur = Duration::from_millis(if open { 320 } else { 240 });
            let from = ui.get_mp();
            APP.with_borrow_mut(|a| {
                a.menu_slide = Some((from, if open { 1.0 } else { 0.0 }, Instant::now(), dur));
                a.hold_stage = Some(stage);
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_android_stop(move || {
            if let Some(ui) = weak.upgrade() {
                android_stop(&ui);
            }
        });
    }
    {
        let weak = ui.as_weak();

        ui.on_android_apps_filter(move |s| {
            if let Some(ui) = weak.upgrade() {
                apps_show(&ui, s.as_str());
            }
        });
        let weak = ui.as_weak();
        ui.on_android_app_launch(move |i, place| {
            if let Some(ui) = weak.upgrade() {
                apps_launch(&ui, i, place.as_str());
            }
        });
        let weak = ui.as_weak();
        ui.on_android_apps_close(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_android_apps_open(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_android_restart(move || {
            if let Some(ui) = weak.upgrade() {
                let i = APP.with_borrow(|a| a.android_index);
                show_toast(&ui, i18n::tr("Restarting the connection…"));
                android_start(&ui, i);
            }
        });
    }
    start_android_poll(&ui);
    // Test hook: ZL_DEMO_SETTINGS=1 opens/closes settings every 1.5 s so the
    // slide can be filmed frame by frame.
    let demo = slint::Timer::default();
    if std::env::var_os("ZL_DEMO_SETTINGS").is_some() {
        let weak = ui.as_weak();
        demo.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(1500),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let open = !ui.get_settings_open();
                    ui.set_settings_open(open);
                    ui.invoke_settings_toggled(open);
                }
            },
        );
    }
    {
        let weak = ui.as_weak();
        let input = input.clone();
        ui.on_action(move |a| {
            let Some(ui) = weak.upgrade() else { return };
            if a.as_str() == "apps" {
                apps_open(&ui);
                return;
            }
            if ui.get_screen().as_str() == "android" {
                if let Some(sh) = android::active() {
                    if sh.action(a.as_str()) {
                        return;
                    }
                }
            }
            // Control Center / Notification Center: press again to close,
            // press the other one to switch (close first, then open).
            if let Some(name) = match a.as_str() {
                "control-center" => Some("control-center"),
                "notifications" => Some("notifications"),
                _ => None,
            } {
                let open = |input: &Input, n: &str| {
                    if n == "control-center" {
                        input.send(Cmd::Stroke(0.92, 0.0, 0.92, 0.62, 12, 16));
                    } else {
                        input.send(Cmd::Stroke(0.22, 0.0, 0.22, 0.62, 12, 16));
                    }
                };
                // Swipe up from the bottom closes either overlay.
                let close = |input: &Input| input.send(Cmd::Stroke(0.5, 0.97, 0.5, 0.25, 12, 16));
                let was = APP
                    .with_borrow(|x| x.panel)
                    .filter(|(_, at)| at.elapsed() < PANEL_TTL)
                    .map(|(n, _)| n);
                match was {
                    Some(p) if p == name => {
                        close(&input);
                        APP.with_borrow_mut(|x| x.panel = None);
                    }
                    Some(_) => {
                        close(&input);
                        APP.with_borrow_mut(|x| x.panel = Some((name, Instant::now())));
                        let input = input.clone();
                        slint::Timer::single_shot(Duration::from_millis(550), move || {
                            open(&input, name)
                        });
                    }
                    None => {
                        open(&input, name);
                        APP.with_borrow_mut(|x| x.panel = Some((name, Instant::now())));
                    }
                }
                return;
            }
            if matches!(
                a.as_str(),
                "swipe-left" | "swipe-right" | "home" | "switcher" | "lock"
            ) {
                APP.with_borrow_mut(|x| x.panel = None);
            }
            match a.as_str() {
                "swipe-left" => input.send(Cmd::Stroke(1.0, 0.5, 0.12, 0.5, 10, 20)),
                "swipe-right" => input.send(Cmd::Stroke(0.0, 0.5, 0.88, 0.5, 10, 20)),
                "home" => input.send(Cmd::Home(1)),
                "switcher" => input.send(Cmd::Home(2)),
                "control-center" => input.send(Cmd::Stroke(0.92, 0.0, 0.92, 0.62, 12, 16)),
                "notifications" => input.send(Cmd::Stroke(0.22, 0.0, 0.22, 0.62, 12, 16)),
                "lock" => input.send(Cmd::Action("lock")),
                "mute" => {
                    let m = !ui.get_muted();
                    ui.set_muted(m);
                    push_audio(&ui.get_prefs(), m);
                }
                "rotate" => {
                    let mut p = ui.get_prefs();
                    p.rotation = match p.rotation.as_str() {
                        "auto" => "90",
                        "90" => "180",
                        "180" => "270",
                        "270" => "0",
                        _ => "auto",
                    }
                    .into();
                    ui.set_prefs(p.clone());
                    prefs::save(&p);
                    APP.with_borrow_mut(|a| a.fitted = None);
                }
                _ => {}
            }
        });
    }
    {
        let api = api.clone();
        let weak = ui.as_weak();
        ui.on_choose_mode(move |mode, link, remember| {
            APP.with_borrow_mut(|a| a.mode_pending = true);
            let (api, w) = (api.clone(), weak.clone());
            let (mode, link) = (mode.to_string(), link.to_string());
            std::thread::spawn(move || {
                // The backend may still be starting (the chooser shows at
                // launch): wait for it, up to a minute.
                let t0 = Instant::now();
                let mut cur = api.get("/api/connection");
                while cur.is_none() && t0.elapsed() < Duration::from_secs(60) {
                    std::thread::sleep(Duration::from_millis(500));
                    cur = api.get("/api/connection");
                }
                let ok_link = cur.is_some()
                    && (cur
                        .as_ref()
                        .and_then(|c| c.get("link"))
                        .and_then(Value::as_str)
                        == Some(link.as_str())
                        || api
                            .post("/api/connection", &json!({ "link": link }))
                            .is_some());
                // A refused choice answers HTTP 400 (post() gives None). The
                // reply is the whole app state, whose "error" field is
                // usually null: not a failure.
                let failed = !ok_link
                    || api
                        .post(
                            "/api/connection",
                            &json!({ "mode": mode, "remember": remember }),
                        )
                        .is_none();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = w.upgrade() else { return };
                    APP.with_borrow_mut(|a| {
                        a.mode_pending = false;
                        a.mode_sent = (!failed).then(Instant::now);
                    });
                    if failed {
                        show_toast(&ui, i18n::tr("The mode was not saved, try again."));
                        open_chooser(&ui);
                    }
                });
            });
        });
    }
    {
        // Cable / Wi-Fi from Settings (both modes). The backend ends a
        // running session first.
        let api = api.clone();
        let weak = ui.as_weak();
        ui.on_set_link(move |link| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_mode_link() == link {
                return;
            }
            ui.set_mode_link(link.clone());
            show_toast(
                &ui,
                if link.as_str() == "wireless" {
                    i18n::tr("Switching to Wi-Fi…")
                } else {
                    i18n::tr("Switching to the cable…")
                },
            );
            let (api, w) = (api.clone(), weak.clone());
            let link = link.to_string();
            std::thread::spawn(move || {
                let ok = api
                    .post("/api/connection", &json!({ "link": link }))
                    .is_some();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        if !ok {
                            show_toast(
                                &ui,
                                i18n::tr("The connection could not be changed, try again."),
                            );
                        }
                    }
                });
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_save_passcode(move |code| {
            if let Some(ui) = weak.upgrade() {
                passcode_query(&ui, Some(code.to_string()));
            }
        });
    }
    {
        ui.set_upd_channel_own(
            match zenlink_core::update::Channel::of(&updater::current_version()) {
                zenlink_core::update::Channel::Beta => "beta",
                zenlink_core::update::Channel::Stable => "stable",
            }
            .into(),
        );
        let weak = ui.as_weak();
        ui.on_update_action(move |a: slint::SharedString| {
            let Some(ui) = weak.upgrade() else { return };
            match a.as_str() {
                "check" => update_check(&ui, true),
                "store" => updater::open_store_page(),
                "install" => update_install(&ui),
                "notes" => {
                    let page =
                        UPDATE_OFFER.with_borrow(|o| o.as_ref().map(|(r, _)| r.page.clone()));
                    open_release_page(
                        &page.unwrap_or_else(|| zenlink_core::update::RELEASES_PAGE.into()),
                    );
                }
                _ => {}
            }
        });
        // Startup check: a few seconds after start so it never slows the
        // first screen; never in tests (ZL_PREFS) unless a test feed is set.
        let feed_test = std::env::var_os("ZL_UPDATE_FEED").is_some();
        if ui.get_prefs().update_check && (!test_mode || feed_test) && !updater::is_store() {
            let weak = ui.as_weak();
            slint::Timer::single_shot(
                Duration::from_secs(if feed_test { 1 } else { 8 }),
                move || {
                    if let Some(ui) = weak.upgrade() {
                        update_check(&ui, false);
                    }
                },
            );
        }
    }
    {
        let api = api.clone();
        let weak = ui.as_weak();
        ui.on_driver_action(move |a: slint::SharedString| {
            if matches!(a.as_str(), "restore" | "bind" | "removefilter") {
                if let Some(ui) = weak.upgrade() {
                    confirm_driver(&ui, a.as_str());
                }
                return;
            }
            if a.as_str() == "mode" {
                if let Some(ui) = weak.upgrade() {
                    ui.set_chooser_cancelable(true);
                    open_chooser(&ui);
                }
                return;
            }
            if a.as_str() == "platform" {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_change_platform();
                }
                return;
            }
            if a.as_str() == "logs" {
                // The log folder in Explorer, for bug reports.
                let _ = std::fs::create_dir_all(zenlink_core::log_dir());
                let dir = updater::visible_log_dir();
                let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
                return;
            }
            if let Some(ui) = weak.upgrade() {
                show_toast(
                    &ui,
                    if a.as_str() == "wifi" {
                        i18n::tr("Setting up Wi-Fi…")
                    } else {
                        i18n::tr("Checking drivers…")
                    },
                );
            }
            let (api, w) = (api.clone(), weak.clone());
            let a = a.to_string();
            std::thread::spawn(move || {
                let r = if a == "wifi" {
                    Some(apple::prepare_wifi())
                } else {
                    api.get("/api/drivers")
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = w.upgrade() else { return };
                    let Some(r) = r else {
                        show_toast(&ui, i18n::tr("The backend did not answer."));
                        return;
                    };
                    if a == "wifi" {
                        let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        let msg = r
                            .get("message")
                            .and_then(Value::as_str)
                            .map(|m| tr_backend(m))
                            .unwrap_or_else(|| {
                                if ok {
                                    i18n::tr("Wi-Fi is ready. You can unplug the cable.")
                                } else {
                                    i18n::tr("Wi-Fi could not be set up.")
                                }
                            });
                        show_toast(&ui, msg);
                    } else {
                        let t = drivers_summary(&r);
                        APP.with_borrow_mut(|x| x.drivers_text = Some(t));
                        show_toast(&ui, i18n::tr("Drivers checked."));
                    }
                });
            });
        });
    }

    // Frameless window: move, resize, minimise, maximise, close.
    {
        let weak = ui.as_weak();
        let last_drag = std::cell::Cell::new(None::<Instant>);
        ui.on_window_action(move |a| {
            use slint::winit_030::winit::window::ResizeDirection as R;
            let Some(ui) = weak.upgrade() else { return };
            let w = ui.window();
            match a.as_str() {
                "close" => close_requested(&ui),
                "min" => w.set_minimized(true),
                "max" if is_full() => {}
                "max" => w.set_maximized(!w.is_maximized()),
                "fullscreen" => {
                    if !is_full() {
                        let was_max = w.is_maximized();
                        if was_max {
                            w.set_maximized(false);
                        }
                        if let Some((x, y, mw, mh)) = monitor_rect(w) {
                            let (pos, size) = (w.position(), w.size());
                            APP.with_borrow_mut(|a| a.full_restore = Some((pos, size, was_max)));
                            w.set_position(slint::PhysicalPosition::new(x, y));
                            // One pixel taller than the monitor: a GL window that
                            // exactly covers it is switched by the driver to an
                            // opaque full-screen mode that stayed after leaving
                            // (no glass, grey window, until a restart).
                            w.set_size(slint::PhysicalSize::new(mw, mh + 1));
                        }
                    } else if let Some((pos, size, was_max)) =
                        APP.with_borrow_mut(|a| a.full_restore.take())
                    {
                        w.set_position(pos);
                        w.set_size(size);
                        if was_max {
                            w.set_maximized(true);
                        }
                    }
                    ui.set_fullscreen(is_full());
                    // Leaving full screen: fit the window to the picture again.
                    // Corners and glass follow the new state: apply the theme
                    // again now and once the change has settled.
                    APP.with_borrow_mut(|a| {
                        a.fitted = None;
                        a.backdrop = None;
                    });
                    let w2 = ui.as_weak();
                    slint::Timer::single_shot(Duration::from_millis(350), move || {
                        if w2.upgrade().is_some() {
                            APP.with_borrow_mut(|a| a.backdrop = None);
                        }
                    });
                }
                "drag" if is_full() => {}
                "drag" => {
                    // Second press within 400 ms = double-click: maximise / restore.
                    let now = Instant::now();
                    if last_drag
                        .get()
                        .map(|t| now - t < Duration::from_millis(400))
                        .unwrap_or(false)
                    {
                        last_drag.set(None);
                        w.set_maximized(!w.is_maximized());
                    } else {
                        last_drag.set(Some(now));
                        w.with_winit_window(|ww| ww.drag_window());
                    }
                }
                r if r.starts_with("resize-") => {
                    let dir = match &r[7..] {
                        "n" => R::North,
                        "s" => R::South,
                        "w" => R::West,
                        "e" => R::East,
                        "nw" => R::NorthWest,
                        "ne" => R::NorthEast,
                        "sw" => R::SouthWest,
                        _ => R::SouthEast,
                    };
                    w.with_winit_window(|ww| ww.drag_resize_window(dir));
                }
                _ => {}
            }
        });
    }

    // PC keyboard to Android (Settings › PC keyboard).
    {
        // Android Ctrl+C / Ctrl+X: the phone's clipboard comes back here.
        let weak = ui.as_weak();
        zenlink_android::mirror::on_device_clipboard(move |text| {
            let msg = if text.is_empty() {
                i18n::tr("Nothing was copied on the phone.")
            } else if set_pc_clipboard_text(&text) {
                i18n::tr("Copied from the phone. Paste it on the PC with Ctrl+V.")
            } else {
                i18n::tr("Text copied on the phone, but Windows didn't allow it on the clipboard.")
            };
            let _ = weak.upgrade_in_event_loop(move |ui| show_toast(&ui, msg));
        });
    }
    {
        let weak = ui.as_weak();
        let input_keys = input.clone();
        ui.on_key_input(move |text, ctrl, shift, alt| {
            let Some(ui) = weak.upgrade() else {
                return false;
            };
            if !ctrl && !alt && ghost::is_key(&text, &ui.get_prefs().ghost_key) {
                set_ghost(&ui, true);
                return true;
            }
            let android_screen = ui.get_screen().as_str() == "android";
            let keyboard = ui.get_prefs().keyboard;
            let apple_live = !android_screen && APP.with_borrow(|a| a.streaming);
            let typing = keyboard
                && (if android_screen {
                    android::active().is_some()
                } else {
                    apple_live
                });
            if let Some(s) =
                shortcuts::route(android_screen, text.as_str(), ctrl, shift, alt, typing)
            {
                run_shortcut(&ui, &input_keys, s, android_screen, apple_live);
                return true;
            }
            if !keyboard {
                return false;
            }
            if !android_screen {
                // Apple Driver mode: the PC keyboard as a hardware keyboard.
                if !apple_live || ctrl || alt {
                    return false;
                }
                let mut chars = text.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return false;
                };
                let Some((usage, shifted)) = shortcuts::hid_usage(c) else {
                    return false;
                };
                let mut usages = Vec::with_capacity(2);
                if shifted {
                    usages.push(shortcuts::HID_SHIFT);
                }
                usages.push(usage);
                input_keys.send(Cmd::Keys(usages));
                return true;
            }
            let Some(sh) = android::active() else {
                return false;
            };
            match android::key_input(text.as_str(), ctrl, shift, alt) {
                android::KeyInput::Send(m) => sh.send(&m),
                android::KeyInput::Paste => {
                    if let Some(t) = pc_clipboard_text() {
                        sh.paste(t);
                    }
                }
                android::KeyInput::Ignore => return false,
            }
            true
        });
    }

    // Frame timer: native view placement, window fitting, stats.
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let input = input.clone();
        let (t, g) = (touch_on.clone(), gestures_on.clone());
        let clamped = std::cell::Cell::new(false);
        // Startup: the window is re-centred while its size still settles
        // (fit to the picture), until the user moves it or 3 s pass.
        let centring = std::cell::Cell::new(None::<(Instant, slint::PhysicalPosition)>);
        if std::env::var_os("ZL_PERF").is_some() {
            perf::install(&ui);
        }
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            let _tick = perf::Tick::new();
            let Some(ui) = weak.upgrade() else { return };
            // The first tick sizes the window for the screen; the settings
            // slide below waits a tick, else it would resize with the old
            // height (seen: the window stayed 900 px high).
            let first = !clamped.replace(true);
            if first {
                clamp_to_work_area(&ui);
                centring.set(Some((Instant::now(), ui.window().position())));
            } else if let Some((at, pos)) = centring.get() {
                let w = ui.window();
                let sliding = APP.with_borrow(|a| {
                    a.slide.is_some() || a.menu_slide.is_some() || a.panel_slide.is_some()
                });
                if at.elapsed() > Duration::from_secs(3) || w.position() != pos {
                    centring.set(None); // settled, or the user moved it
                } else if !sliding {
                    if let Some(wa) = work_area(&ui) {
                        let size = w.size().to_logical(w.scale_factor());
                        let (x, y) = centered(wa, (size.width, size.height));
                        let want = slint::LogicalPosition::new(x, y).to_physical(w.scale_factor());
                        if (want.x - pos.x).abs() > 1 || (want.y - pos.y).abs() > 1 {
                            w.set_position(want);
                            centring.set(Some((at, want)));
                        }
                    }
                }
            }
            // Auto unlock, a moment after the picture started.
            let unlock = APP.with_borrow_mut(|a| {
                let due = a.unlock_at.is_some_and(|at| Instant::now() >= at);
                if due {
                    a.unlock_at = None;
                }
                due
            });
            if unlock && ui.get_phase().as_str() == "streaming" {
                input.send(Cmd::Action("wake"));
                show_toast(&ui, i18n::tr("Unlocking the device…"));
            }
            APP.with_borrow_mut(|a| {
                if a.view.is_none() {
                    let Some(h) = hwnd_of(ui.window()) else { return };
                    a.backdrop = None;
                    match VideoView::new(h.0 as isize) {
                        Ok(v) => {
                            let input = input.clone();
                            let (t, g) = (t.clone(), g.clone());
                            let weak = ui.as_weak();
                            v.on_pointer(move |e| {
                                if matches!(e, PointerEvent::Down(..)) {
                                    // Keys go to the device (and the window's
                                    // shortcuts work) after a click on it.
                                    let weak = weak.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        // A click on the picture may close a
                                        // device overlay: forget it.
                                        APP.with_borrow_mut(|a| a.panel = None);
                                        if let Some(ui) = weak.upgrade() {
                                            ui.invoke_focus_keys();
                                        }
                                    });
                                }
                                // Android: the scrcpy control socket (touch,
                                // scroll, right click = BACK, middle = HOME).
                                if let Some(sh) = android::active() {
                                    if t.load(Relaxed) {
                                        sh.pointer(e, input.rotation.load(Relaxed));
                                    }
                                    return;
                                }
                                match e {
                                PointerEvent::Down(u, v) if t.load(Relaxed) => input.send(Cmd::Down(u, v)),
                                PointerEvent::Move(u, v) if t.load(Relaxed) => input.send(Cmd::Move(u, v)),
                                PointerEvent::Up(u, v) if t.load(Relaxed) => input.send(Cmd::Up(u, v)),
                                PointerEvent::Back if g.load(Relaxed) => input.send(Cmd::Stroke(0.0, 0.5, 0.65, 0.5, 14, 16)),
                                PointerEvent::Wheel(u, v, d) if g.load(Relaxed) => {
                                    let end = (v + 0.12 * d).clamp(0.02, 0.98);
                                    input.send(Cmd::Stroke(u, v, u, end, 5, 10));
                                }
                                PointerEvent::Pinch(u, v, d) if t.load(Relaxed) => {
                                    let input = input.clone();
                                    pinch::zoom(u, v, d, std::sync::Arc::new(move |p, f| input.send(Cmd::Fingers(p, f))));
                                }
                                _ => {}
                                }
                            });
                            a.view = Some(v);
                            if a.streaming {
                                start_apple_video(a.view.as_mut().unwrap());
                            } else if let Some(src) = a.android_pending.take() {
                                a.view.as_mut().unwrap().start_source(src);
                            }
                        }
                        Err(e) => {
                            ui.set_message(format!("Video: {e}").into());
                        }
                    }
                }
            });
            if APP.with_borrow(|a| a.backdrop.is_none()) {
                apply_look(&ui);
            }
            let p = ui.get_prefs();
            let sf = ui.window().scale_factor();
            let lw = ui.window().size().width as f32 / sf;
            if (lw < 640.0) != ui.get_compact() {
                ui.set_compact(lw < 640.0);
            }
            let (maxed, full) = (ui.window().is_maximized(), is_full());
            if maxed != ui.get_is_maximized() || full != ui.get_fullscreen() {
                // Window style changed: corners and glass go again.
                APP.with_borrow_mut(|a| a.backdrop = None);
            }
            ui.set_is_maximized(maxed);
            ui.set_fullscreen(full);
            // Settings slide: window width follows the animated chrome so
            // the picture keeps its size; ends when the slide settles.
            // ZL_SETTINGS_DEMO=1 (tests): open and close Settings every 1.5 s,
            // so the slide can be measured without touching the mouse.
            if std::env::var_os("ZL_SETTINGS_DEMO").is_some() {
                thread_local!(static DEMO: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) });
                let due = DEMO.with(|d| {
                    let at = d.get().unwrap_or_else(Instant::now);
                    let due = at.elapsed() >= Duration::from_millis(1500);
                    d.set(Some(if due { Instant::now() } else { at }));
                    due
                });
                if due {
                    let open = !ui.get_settings_open();
                    ui.set_settings_open(open);
                    ui.invoke_settings_toggled(open);
                }
            }
            if std::env::var_os("ZL_SETTINGS_DEMO").is_some() {
                log::info!(
                    "tick t={} window={lw:.1} stage_w={:.1} chrome={:.1} hold={:?}",
                    DEMO_T0.get_or_init(Instant::now).elapsed().as_millis(),
                    ui.get_stage_w(),
                    ui.get_chrome_w(),
                    APP.with_borrow(|a| a.hold_stage)
                );
            }
            // Settings slide: progress and window width change on the same
            // tick, so the layout never sees a panel wider than the window.
            if let Some((from, to, at, dur)) = APP.with_borrow(|a| a.slide) {
                let k = (at.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0);
                ui.set_sp(from + (to - from) * slide_ease(k));
                if k >= 1.0 {
                    APP.with_borrow_mut(|a| a.slide = None);
                }
            }
            // Device menu and side panel: like Settings, the window widens on
            // its right edge on the same tick, and the picture moves over.
            if let Some((from, to, at, dur)) = APP.with_borrow(|a| a.menu_slide) {
                let k = (at.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0);
                ui.set_mp(from + (to - from) * slide_ease(k));
                if k >= 1.0 {
                    APP.with_borrow_mut(|a| a.menu_slide = None);
                }
            }
            if let Some((from, to, at, dur)) = APP.with_borrow(|a| a.panel_slide) {
                let k = (at.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0);
                ui.set_pp(from + (to - from) * slide_ease(k));
                if k >= 1.0 {
                    APP.with_borrow_mut(|a| a.panel_slide = None);
                }
            }
            if let Some(stage) = APP.with_borrow(|a| a.hold_stage) {
                // Held before the window had a monitor: size it for this screen.
                let stage = if stage == IDLE_STAGE_W {
                    idle_stage_w(&ui)
                } else {
                    stage
                };
                if std::env::var_os("ZL_STATS_LOG").is_some() {
                    log::info!(
                        "slide window={lw:.1} stage_w={:.1} chrome={:.1} held={stage:.1}",
                        ui.get_stage_w(),
                        ui.get_chrome_w()
                    );
                }
                let chrome = ui.get_chrome_w();
                let target = (stage + chrome).max(MIN_W);
                let mut lh = ui.window().size().height as f32 / sf;
                if let Some((h, at)) = APP.with_borrow(|a| a.start_h) {
                    if at.elapsed() < Duration::from_secs(2) {
                        lh = lh.max(h);
                    }
                }
                if !first && (lw - target).abs() > 0.5 && !ui.window().is_maximized() && !is_full() {
                    let size = LogicalSize::new(target, lh);
                    ui.window().set_size(size);
                    if let Some(wa) = work_area(&ui) {
                        keep_on_screen(&ui, wa, size);
                    }
                }
                let end = 16.0
                    + if ui.get_settings_open() { 368.0 } else { 0.0 }
                    + if ui.get_menu_open() { 216.0 } else { 0.0 }
                    + if ui.get_drawer_screen() || ui.get_drawer_files() { 448.0 } else { 0.0 };
                if (chrome - end).abs() < 0.01 {
                    APP.with_borrow_mut(|a| {
                        a.hold_stage = None;
                        a.fit_set = Some((target, lh));
                    });
                }
            }
            // Labelled dock is ~690 px wide (measured): icons only below that.
            let bar = ui.get_stage_w() < 720.0;
            if bar != ui.get_compact_bar() {
                ui.set_compact_bar(bar);
            }
            let on_apple = ui.get_screen().as_str() == "apple";
            let on_android = ui.get_screen().as_str() == "android";
            if on_android {
                android_tick(&ui);
            }
            APP.with_borrow_mut(|a| {
                // The "Connect a device" page and the close dialog cover the stage: hide the picture meanwhile.
                let streaming = ((a.streaming && on_apple) || (a.android_live && on_android))
                    && !ui.get_adding()
                    && !ui.get_close_ask();
                // scrcpy sends the picture already turned; Apple reports the angle separately.
                let device_rotation = if on_android { 0 } else { a.device_rotation };
                let Some(v) = a.view.as_ref() else { return };
                v.set_crop(p.crop_l as u32, p.crop_t as u32, p.crop_r as u32, p.crop_b as u32);
                v.set_fit(match p.fit.as_str() {
                    "cover" => Fit::Cover,
                    "fill" => Fit::Fill,
                    _ => Fit::Contain,
                });
                // Rotation decision uses the unrotated frame size (read
                // directly: briefly setting rotation 0 let the render thread
                // draw a wrong frame).
                let raw = v.source_size();
                let rot = effective_rotation(&p, raw, device_rotation);
                v.set_rotation(rot);
                input.rotation.store(rot, Relaxed);
                v.set_background(0x000000);
                let (mut x, mut y, mut w, mut h) = (ui.get_stage_x() * sf, ui.get_stage_y() * sf, ui.get_stage_w() * sf, ui.get_stage_h() * sf);
                // Auto-Fit: shrink the native view to the picture itself, so the
                // glass stage shows around it instead of black letterbox bars.
                // Even inset on every side inside the glass stage.
                let inset = PIC_INSET * sf;
                if p.fit != "fill" {
                    x += inset;
                    y += inset;
                    w = (w - 2.0 * inset).max(1.0);
                    h = (h - 2.0 * inset).max(1.0);
                }
                let pic = v.shown_size();
                if p.fit != "cover" && p.fit != "fill" && pic.0 > 0 && pic.1 > 0 && w > 0.0 && h > 0.0 {
                    let aspect = pic.0 as f32 / pic.1 as f32;
                    let (pw, ph) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
                    x += (w - pw) / 2.0;
                    y += (h - ph) / 2.0;
                    (w, h) = (pw, ph);
                }
                // Toast and tooltip stay visible over the picture.
                let mut holes = Vec::new();
                let px = |v: f32| (v * sf).round() as i32;
                if ui.get_tip_on() {
                    holes.push((px(ui.get_tip_x()), px(ui.get_tip_y()), px(ui.get_tip_w()), px(ui.get_tip_h())));
                }
                if !ui.get_toast().is_empty() {
                    // 36 px high, rising 16 px into place above the dock.
                    let (ww, wh) = (ui.window().size().to_logical(sf).width, ui.window().size().to_logical(sf).height);
                    let tw = ui.get_toast_w();
                    holes.push((px((ww - tw) / 2.0), px(wh - 104.0), px(tw), px(52.0)));
                }
                if !ui.get_transfer().is_empty() {
                    let (ww, wh) = (ui.window().size().to_logical(sf).width, ui.window().size().to_logical(sf).height);
                    let tw = ui.get_transfer_w();
                    holes.push((px((ww - tw) / 2.0), px(wh - 150.0), px(tw), px(40.0)));
                }
                if (ui.get_android_apps_open() && on_android) || ui.get_drop_hover() || ui.get_files_open() {
                    holes.push((
                        px(ui.get_launcher_x()),
                        px(ui.get_launcher_y()),
                        px(ui.get_launcher_w()),
                        px(ui.get_launcher_h()),
                    ));
                }
                v.set_holes(&holes);
                v.set_bounds(x.round() as i32, y.round() as i32, w.round() as i32, h.round() as i32, streaming, ((22.0 - PIC_INSET) * sf).round() as i32);
                let shown = v.shown_size();
                if a.fit_prefs.is_none() {
                    a.fit_prefs = Some(fit_prefs_key(&p));
                }
                let free = a.full_restore.is_none() && !ui.window().is_maximized();
                if streaming && p.fit_window && shown.0 > 0 && free {
                    let key = (shown.0, shown.1, ui.get_settings_open());
                    if a.fitted != Some(key) {
                        a.fitted = Some(key);
                        a.fit_set = fit_window(&ui, shown, a.user_scale);
                    } else if a.hold_stage.is_none() {
                        // Not a size ZenLink set: the user resized the window.
                        // Remember how big they want the picture.
                        let cur = ui.window().size().to_logical(sf);
                        let moved = a.fit_set.map(|(w, h)| (cur.width - w).abs() > 2.0 || (cur.height - h).abs() > 2.0).unwrap_or(false);
                        if moved {
                            if let Some((dw, dh)) = default_fit(&ui, shown) {
                                let cw = ui.get_chrome_w() + 2.0 * PIC_INSET;
                                let ch = ui.get_chrome_h() + 2.0 * PIC_INSET;
                                let k = ((cur.width - cw) / dw).min((cur.height - ch) / dh);
                                if k.is_finite() && k > 0.0 {
                                    a.user_scale = k.clamp(0.2, 4.0);
                                }
                            }
                            a.fit_set = Some((cur.width, cur.height));
                        }
                    }
                }
                // Dot field: ~30 fps, only while there is no picture.
                let minimized = ui.window().is_minimized();
                if !streaming && ui.get_dots_on() && !minimized && !tray::HIDDEN.load(Relaxed) {
                    // 30 fps. Each change costs ~2 window draws (Slint's frame throttle redraws at
                    // the monitor's rate while a redraw is pending), so every tick would double it.
                    let fresh = a.dots_at.map(|t| t.elapsed() >= Duration::from_millis(30)).unwrap_or(true);
                    // Frozen while another window has focus (B-60): OBS / TikTok LIVE Studio Game
                    // Capture binds to whichever swap chain presents first, and a moving dot field
                    // made that the whole OpenGL window instead of the picture. Also saves CPU.
                    let focused = ui.window().with_winit_window(|w| w.has_focus()).unwrap_or(true);
                    if fresh && (focused || a.dots.is_none()) {
                        a.dots_at = Some(Instant::now());
                        let t = START.with(|s| s.elapsed().as_secs_f32());
                        let mouse = ui.get_stage_hover().then(|| (ui.get_stage_mouse_x(), ui.get_stage_mouse_y()));
                        let dark = a.dark;
                        let dots = a.dots.get_or_insert_with(dots::Dots::new);
                        if let Some(m) = dots.render(ui.get_stage_w(), ui.get_stage_h(), sf, t, mouse, dark) {
                            ui.set_dots_tiles(m);
                        }
                    }
                } else if a.dots.take().is_some() {
                    ui.set_dots_tiles(slint::ModelRc::default());
                }
                let due = a.meter_at.map(|t| t.elapsed() >= Duration::from_secs(1)).unwrap_or(true);
                if due {
                    a.meter_at = Some(Instant::now());
                    let s = v.stats();
                    // Apple: the backend says "streaming" but no frame came (B-56).
                    // Say what to do instead of an empty window, and log it.
                    if streaming && s.width > 0 {
                        a.live_since = None;
                    } else if let Some(t) = a.live_since.filter(|t| streaming && !on_android && t.elapsed() > Duration::from_secs(10)) {
                        a.live_since = None;
                        log::warn!("apple: no picture {:.0} s after streaming started (decoded {:.1} fps, hardware {})", t.elapsed().as_secs_f32(), s.decoded_fps, s.hardware);
                        let msg = i18n::tr("No picture from the iPhone/iPad yet. Unlock the device and press Restart. If it stays empty, send the logs (Settings › About › Open log folder).");
                        show_toast(&ui, msg.to_string());
                    }
                    if streaming && s.width > 0 && std::env::var_os("ZL_STATS_LOG").is_some() {
                        // Measuring frame pacing (tests): once a second, to the log.
                        log::info!("stats decoded={:.1} presented={:.1} dropped_total={}", s.decoded_fps, s.presented_fps, s.replaced);
                    }
                    if streaming && s.width > 0 {
                        // Android: sound latency (queue + output) next to the video numbers.
                        let sound = a
                            .android
                            .as_ref()
                            .filter(|_| on_android)
                            .and_then(|sh| sh.audio_ms())
                            .map(|ms| format!("  ·  {} {ms} ms", i18n::tr("sound")))
                            .unwrap_or_default();
                        ui.set_stats_line(format!("{}×{}  ·  {:.0} fps  ·  {:.1} Mbps  ·  decode {:.1} ms{sound}", s.width, s.height, s.presented_fps.min(s.decoded_fps), s.mbps, s.decode_ms).into());
                        ui.set_stats_full(
                            format!(
                                "Resolusi      {}×{}\nDecode        {:.1} fps ({})\nTampil        {:.1} fps\nTerlewat      {}\nDecode frame  {:.1} ms\nBitrate       {:.1} Mbps",
                                s.width,
                                s.height,
                                s.decoded_fps,
                                if s.hardware { "GPU" } else { "CPU" },
                                s.presented_fps,
                                s.replaced,
                                s.decode_ms,
                                s.mbps
                            )
                            .into(),
                        );
                    } else {
                        ui.set_stats_line("".into());
                        ui.set_stats_full("—".into());
                    }
                }
            });
        });
    }

    // Follow the Windows theme while "System" is selected.
    let theme_timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        theme_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(2),
            move || {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_prefs().theme.as_str() == "system"
                        && system_dark() != APP.with_borrow(|a| a.dark)
                    {
                        apply_look(&ui);
                    }
                }
            },
        );
    }

    apply_look(&ui);
    // "Start with Windows" (--background): into the tray when "Close to the
    // tray" is on, else minimised. The window exists once the loop runs.
    let start_timer = slint::Timer::default();
    if background {
        let weak = ui.as_weak();
        let tries = std::cell::Cell::new(0);
        start_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if tries.get() >= 1000 {
                    return; // done
                }
                tries.set(tries.get() + 1);
                if hwnd_of(ui.window()).is_none() && tries.get() < 200 {
                    return;
                }
                if ui.get_prefs().close_action == "tray" {
                    hide_to_tray(&ui);
                } else {
                    ui.window().set_minimized(true);
                }
                tries.set(1000);
            },
        );
    }
    // Until quit: the window may be hidden in the tray.
    let r = ui.show().and_then(|_| slint::run_event_loop_until_quit());
    drop(start_timer);
    tray::set_icon(false, "", "");
    let _ = ui.hide();
    APP.with_borrow_mut(|a| a.view = None);
    cast_stop_all();
    stop_own_adb_server();
    r
}

#[cfg(test)]
mod launcher_tests {
    use super::*;

    #[test]
    fn tile_letters() {
        assert_eq!(app_letters("Google Maps"), "GM");
        assert_eq!(app_letters("Chrome"), "C");
        assert_eq!(app_letters("  (Beta) app store x"), "BA");
        assert_eq!(app_letters("ñandu"), "Ñ");
        assert_eq!(app_letters(""), "");
    }

    #[test]
    fn tile_colour_is_fixed_per_package() {
        assert_eq!(
            app_tint("com.android.chrome"),
            app_tint("com.android.chrome")
        );
        let hues: std::collections::HashSet<_> = ["a.b", "c.d", "e.f", "g.h", "i.j", "k.l"]
            .iter()
            .map(|p| app_tint(p).as_argb_encoded())
            .collect();
        assert!(hues.len() > 1);
    }
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn windows_10_falls_back_to_mdl2() {
        assert_eq!(super::pick_icon_font(false), "Segoe MDL2 Assets");
        assert_eq!(super::pick_icon_font(true), "Segoe Fluent Icons");
    }
}

#[cfg(test)]
mod window_tests {
    use super::centered;

    #[test]
    fn first_window_opens_in_the_middle_of_the_work_area() {
        // 2560 x 1392 work area (taskbar at the bottom), 1024 x 1183 window.
        assert_eq!(
            centered((0.0, 0.0, 2560.0, 1392.0), (1024.0, 1183.0)),
            (768.0, 105.0)
        );
        // Second monitor left of the primary one.
        assert_eq!(
            centered((-1920.0, 148.0, 1920.0, 1032.0), (1000.0, 900.0)),
            (-1460.0, 214.0)
        );
        // Never above or left of the work area.
        assert_eq!(
            centered((0.0, 0.0, 800.0, 600.0), (900.0, 700.0)),
            (0.0, 0.0)
        );
    }
}

#[cfg(test)]
mod slide_tests {
    use super::slide_ease;

    #[test]
    fn slide_curve_matches_the_css_bezier() {
        assert!(slide_ease(0.0).abs() < 1e-4);
        assert!((slide_ease(1.0) - 1.0).abs() < 1e-4);
        // Fast start, slow end (ease-out like the Slint curve it replaces).
        assert!(slide_ease(0.25) > 0.6, "{}", slide_ease(0.25));
        let mut last = 0.0;
        for i in 1..=100 {
            let y = slide_ease(i as f32 / 100.0);
            assert!(y >= last - 1e-5, "monotonic at {i}");
            last = y;
        }
    }
}
