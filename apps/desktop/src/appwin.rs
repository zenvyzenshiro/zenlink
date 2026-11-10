//! v2.5.1: one Android app in its own window.
//!
//! The main window starts `ZenLink.exe --android-app serial=â€¦ package=â€¦`.
//! That process opens a small window with its own taskbar entry and starts
//! one scrcpy session on a new virtual display of the phone
//! (`new_display`, Android 10+), then starts the app on it (`START_APP`).
//! The phone's own screen stays free, and several apps can be open at once,
//! each in its own window and session.
//!
//! The window is the same picture path as the main window (`VideoView`,
//! D3D11 child window, one frame per vblank). The virtual display follows
//! the window size (`flex_display` + `RESIZE_DISPLAY`), so the app lays
//! itself out for the window like on a tablet or desktop. Sound is off: it
//! is the phone's sound, and the main window may already play it.
//!
//! This process has no backend, no tray and no single-instance check; it
//! reads the prefs (language, theme, video settings) and never writes them.
//!
//! v2.6 multi-device: with `screen=1` (no package) the window mirrors the
//! phone's own screen instead, so a second, third ... phone gets its own
//! window, decoder and control next to the main window. Each window title is
//! `<device> · ZenLink`, so OBS can capture each phone as its own source.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use zenlink_android::apps::is_package_name;
use zenlink_android::control::Control;
use zenlink_android::mirror::NewDisplay;
use zenlink_video::{Fit, PointerEvent, VideoView};

use crate::{android, prefs, AppWin, I18n, Theme};

/// First argument that turns ZenLink.exe into an app window.
pub const ARG: &str = "--android-app";

/// What the main window passes to an app window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppArgs {
    pub serial: String,
    pub transport_id: Option<u64>,
    pub package: String,
    /// App label and device name, for the title only.
    pub name: String,
    pub device: String,
    /// Mirror the phone's own screen (multi-device) instead of an app.
    pub screen: bool,
}

impl AppArgs {
    /// The command line after the exe: `--android-app key=valueâ€¦`.
    pub fn to_args(&self) -> Vec<String> {
        let mut v = vec![
            ARG.to_string(),
            format!("serial={}", self.serial),
            format!("package={}", self.package),
            format!("name={}", self.name),
            format!("device={}", self.device),
        ];
        if let Some(t) = self.transport_id {
            v.push(format!("transport={t}"));
        }
        if self.screen {
            v.push("screen=1".into());
        }
        v
    }

    /// Parses the arguments after the exe; `None` when the first one is not
    /// [`ARG`] or the serial or package is missing or unsafe.
    pub fn parse(args: &[String]) -> Option<Self> {
        let (first, rest) = args.split_first()?;
        if first != ARG {
            return None;
        }
        let mut a = AppArgs::default();
        for kv in rest {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "serial" => a.serial = v.to_string(),
                "package" => a.package = v.to_string(),
                "name" => a.name = v.to_string(),
                "device" => a.device = v.to_string(),
                "transport" => a.transport_id = v.parse().ok(),
                "screen" => a.screen = v == "1",
                _ => {}
            }
        }
        // The serial goes into ADB requests ("host:transport:<serial>"):
        // no spaces or control characters.
        let serial_ok = !a.serial.is_empty()
            && a.serial.len() <= 128
            && a.serial.bytes().all(|b| b.is_ascii_graphic());
        if a.screen {
            a.package.clear();
        }
        if !serial_ok || (!a.screen && !is_package_name(&a.package)) {
            return None;
        }
        if a.name.trim().is_empty() {
            a.name = if a.screen {
                a.serial.clone()
            } else {
                a.package.clone()
            };
        }
        Some(a)
    }
}

/// Opens an app window: a new ZenLink.exe process. A test prefs file
/// (`ZL_PREFS`) is inherited with the environment.
pub fn spawn(a: &AppArgs) -> std::io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).args(a.to_args()).spawn()
}

/// Exit code of a device window whose "Merge" button was pressed: the main
/// window takes the phone back.
pub const MERGE_EXIT: i32 = 7;
static MERGE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Virtual display size for a picture area of `w`Ã—`h` pixels: multiples of
/// 8 (video encoders work in blocks), within what phone encoders take.
pub fn display_size(w: f32, h: f32) -> (u16, u16) {
    let fit = |v: f32| ((v.max(0.0) as u32 / 8 * 8).clamp(160, 3840)) as u16;
    (fit(w), fit(h))
}

/// Density for the virtual display: Android's 160 dpi is 1 dp per pixel,
/// so the app's layout follows the PC's display scale.
pub fn display_dpi(scale: f32) -> u32 {
    ((160.0 * scale).round() as u32).clamp(120, 640)
}

struct State {
    view: Option<VideoView>,
    shared: Option<Arc<android::Shared>>,
    /// Display size asked for (start or last RESIZE_DISPLAY).
    asked: (u16, u16),
    /// A new size waiting to settle before it is sent (window being dragged).
    pending: Option<((u16, u16), Instant)>,
    started: bool,
}

thread_local! {
    static ST: RefCell<State> = const {
        RefCell::new(State { view: None, shared: None, asked: (0, 0), pending: None, started: false })
    };
}

/// Picture area in physical pixels: the window below the button strip.
fn picture_rect(ui: &AppWin) -> (f32, f32, f32, f32) {
    let sf = ui.window().scale_factor();
    let size = ui.window().size();
    // Inside the glass stage: between header and dock, inset on every side.
    let side = (ui.get_pad() + ui.get_inset()) * sf;
    let top = (ui.get_pad() + ui.get_bar_h() + ui.get_gap() + ui.get_inset()) * sf;
    let bottom = (ui.get_inset() + ui.get_gap() + ui.get_dock_h() + ui.get_pad()) * sf;
    (
        side,
        top,
        (size.width as f32 - 2.0 * side).max(1.0),
        (size.height as f32 - top - bottom).max(1.0),
    )
}

/// The picture's own rectangle inside `r` (contain fit), so the glass stage
/// shows around it instead of black bars.
fn fit_rect(r: (f32, f32, f32, f32), pic: (u32, u32)) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = r;
    if pic.0 == 0 || pic.1 == 0 {
        return r;
    }
    let aspect = pic.0 as f32 / pic.1 as f32;
    let (pw, ph) = if w / h > aspect {
        (h * aspect, h)
    } else {
        (w, w / aspect)
    };
    (x + (w - pw) / 2.0, y + (h - ph) / 2.0, pw, ph)
}

fn start_session(ui: &AppWin, a: &AppArgs) {
    stop_session();
    let server = match crate::scrcpy_server() {
        Ok(s) => s,
        Err(e) => {
            ui.set_phase("error".into());
            ui.set_message(e.into());
            return;
        }
    };
    let (_, _, w, h) = picture_rect(ui);
    let size = display_size(w, h);
    let p = prefs::load();
    let mut o = crate::android_options(&p, false, None);
    // Sound stays with the main window (one sound at a time, no echo).
    o.audio = false;
    if !a.screen {
        o.max_size = 0; // the display already has the window's size
        o.new_display = Some(NewDisplay {
            width: size.0.into(),
            height: size.1.into(),
            dpi: display_dpi(ui.window().scale_factor()),
            flex: true,
        });
    }
    let shared = Arc::new(android::Shared::default());
    let mut src = android::DeviceSource::new(
        a.serial.clone(),
        a.transport_id,
        a.device.clone(),
        server,
        shared.clone(),
        o,
        android::Smart {
            pref: crate::android_pref(&p),
            dec: crate::android_decoders(),
            known: None,
        },
    );
    if a.screen {
        log::info!("device window: {} ({})", a.device, a.serial);
    } else {
        src.start_app = Some(a.package.clone());
        log::info!(
            "app window: {} on {} ({}x{})",
            a.package,
            a.serial,
            size.0,
            size.1
        );
    }
    android::set_active(Some(shared.clone()));
    ST.with_borrow_mut(|s| {
        s.asked = size;
        s.pending = None;
        s.shared = Some(shared);
        s.started = true;
        if let Some(v) = s.view.as_mut() {
            v.start_source(Box::new(src));
        }
    });
    ui.set_phase("connecting".into());
    ui.set_message("".into());
}

fn stop_session() {
    ST.with_borrow_mut(|s| {
        android::set_active(None);
        if let Some(sh) = s.shared.take() {
            sh.stop();
        }
        if let Some(v) = s.view.as_mut() {
            v.stop();
        }
    });
}

/// Every frame tick: the video view, its place, the status and resizes.
fn tick(ui: &AppWin, a: &AppArgs) {
    let need_view = ST.with_borrow(|s| s.view.is_none());
    if need_view {
        let Some(h) = crate::hwnd_of(ui.window()) else {
            return;
        };
        match VideoView::new(h.0 as isize) {
            Ok(v) => {
                let weak = ui.as_weak();
                v.on_pointer(move |e| {
                    if matches!(e, PointerEvent::Down(..)) {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.invoke_focus_keys();
                            }
                        });
                    }
                    if let Some(sh) = android::active() {
                        sh.pointer(e, 0);
                    }
                });
                v.set_fit(Fit::Contain);
                v.set_background(0x000000);
                ST.with_borrow_mut(|s| s.view = Some(v));
            }
            Err(e) => {
                ui.set_phase("error".into());
                ui.set_message(format!("Video: {e}").into());
                return;
            }
        }
    }
    let size = ui.window().size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    if !ST.with_borrow(|s| s.started) {
        start_session(ui, a);
    }
    let (x, y, w, h) = picture_rect(ui);
    let phase = ST.with_borrow(|s| s.shared.as_ref().map(|sh| sh.status()));
    let streaming = phase
        .as_ref()
        .is_some_and(|st| st.phase == android::Phase::Streaming);
    if let Some(st) = &phase {
        let ui_phase = match st.phase {
            android::Phase::Streaming => "streaming",
            android::Phase::Error => "error",
            _ => "connecting",
        };
        if ui.get_phase().as_str() != ui_phase {
            ui.set_phase(ui_phase.into());
            if st.phase == android::Phase::Error {
                log::warn!("app window: {}", st.error);
                ui.set_message(error_text(&st.error, a.screen).into());
            }
        }
    }
    ST.with_borrow_mut(|s| {
        let Some(v) = s.view.as_ref() else { return };
        let (px, py, pw, ph) = fit_rect((x, y, w, h), v.shown_size());
        let radius = (12.0 * ui.window().scale_factor()).round() as i32;
        v.set_bounds(
            px.round() as i32,
            py.round() as i32,
            pw.round() as i32,
            ph.round() as i32,
            streaming,
            radius,
        );
        if a.screen {
            return; // the phone's own screen keeps its size
        }
        // Flex display: follow the window once its size settles.
        let want = display_size(w, h);
        let now = Instant::now();
        if want == s.asked {
            s.pending = None;
        } else if s.pending.map(|(sz, _)| sz) != Some(want) {
            s.pending = Some((want, now));
        } else if let Some((sz, at)) = s.pending {
            if streaming && now.duration_since(at) >= Duration::from_millis(250) {
                if let Some(sh) = &s.shared {
                    sh.send(&[Control::ResizeDisplay {
                        width: sz.0,
                        height: sz.1,
                    }]);
                }
                s.asked = sz;
                s.pending = None;
            }
        }
    });
}

/// A short reason for the window, from the session's English error.
fn error_text(e: &str, screen: bool) -> String {
    let low = e.to_lowercase();
    if low.contains("device") && (low.contains("not found") || low.contains("offline")) {
        return crate::i18n::tr(
            "The phone is no longer connected. Connect it again, then Try again.",
        );
    }
    if screen {
        return trf!("The phone's screen could not be shown: {e}", e = e);
    }
    trf!(
        "The app could not open on a virtual display (Android 10+): {e}",
        e = e
    )
}

/// Runs the app window until it is closed.
pub fn run(a: AppArgs) -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        // Frameless glass like the main window.
        .with_winit_window_attributes_hook(|attrs| {
            #[cfg(windows)]
            {
                use slint::winit_030::winit::platform::windows::{
                    BackdropType, WindowAttributesExtWindows,
                };
                attrs
                    .with_transparent(true)
                    .with_decorations(false)
                    .with_system_backdrop(BackdropType::None)
            }
            #[cfg(not(windows))]
            {
                attrs
                    .with_transparent(false)
                    .with_decorations(false)
            }
        })
        .select()?;
    let ui = AppWin::new()?;
    let p = prefs::load();
    let dark = match p.theme.as_str() {
        "light" => false,
        "system" => crate::system_dark(),
        _ => true,
    };
    ui.global::<Theme>().set_dark(dark);
    // Same glass as the main window (Settings › Liquid Glass and thickness).
    let (level_on, thick) = crate::prefs::glass_level(p.glass);
    let glass_on = p.glass_on && level_on;
    ui.global::<Theme>()
        .set_glass(if glass_on { 1.0 - 0.75 * thick } else { 0.0 });
    ui.global::<Theme>().set_icons(crate::icon_font().into());
    crate::set_lang(&ui.global::<I18n>(), p.lang.as_str());
    ui.set_app_name(a.name.clone().into());
    ui.set_device(a.device.clone().into());
    ui.set_screen(a.screen);

    ui.on_key_input(|text, ctrl, shift, alt| {
        let Some(sh) = android::active() else {
            return false;
        };
        match android::key_input(text.as_str(), ctrl, shift, alt) {
            android::KeyInput::Send(m) => sh.send(&m),
            android::KeyInput::Paste => {
                if let Some(t) = crate::pc_clipboard_text() {
                    sh.paste(t);
                }
            }
            android::KeyInput::Ignore => return false,
        }
        true
    });
    {
        let package = a.package.clone();
        ui.on_action(move |name| {
            if name.as_str() == "merge" {
                MERGE.store(true, std::sync::atomic::Ordering::Relaxed);
                stop_session();
                let _ = slint::quit_event_loop();
                return;
            }
            let Some(sh) = android::active() else { return };
            match name.as_str() {
                // "+" force-stops it first: a clean start of the app.
                "restart-app" => sh.send(&[Control::StartApp(format!("+{package}"))]),
                n => {
                    sh.action(n);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let a = a.clone();
        ui.on_retry(move || {
            if let Some(ui) = weak.upgrade() {
                start_session(&ui, &a);
            }
        });
    }
    {
        // Header buttons, drag and edge resize (no system frame).
        let weak = ui.as_weak();
        let last_drag = std::cell::Cell::new(None::<Instant>);
        let full_restore =
            std::cell::Cell::new(None::<(slint::PhysicalPosition, slint::PhysicalSize, bool)>);
        ui.on_window_action(move |act| {
            use slint::winit_030::winit::window::ResizeDirection as R;
            use slint::winit_030::WinitWindowAccessor;
            let Some(ui) = weak.upgrade() else { return };
            let w = ui.window();
            match act.as_str() {
                "close" => {
                    stop_session();
                    let _ = slint::quit_event_loop();
                }
                "min" => w.set_minimized(true),
                "max" if !ui.get_is_fullscreen() => w.set_maximized(!w.is_maximized()),
                // Own full screen, as the main window: winit's exact-monitor
                // size makes the GL driver go opaque and stay so (no glass after).
                "fullscreen" => {
                    if let Some((pos, size, was_max)) = full_restore.take() {
                        w.set_position(pos);
                        w.set_size(size);
                        if was_max {
                            w.set_maximized(true);
                        }
                    } else if let Some((x, y, mw, mh)) = crate::monitor_rect(w) {
                        let was_max = w.is_maximized();
                        if was_max {
                            w.set_maximized(false);
                        }
                        full_restore.set(Some((w.position(), w.size(), was_max)));
                        w.set_position(slint::PhysicalPosition::new(x, y));
                        w.set_size(slint::PhysicalSize::new(mw, mh + 1));
                    }
                    ui.set_is_fullscreen(full_restore.get().is_some());
                }
                "drag" | "max" if ui.get_is_fullscreen() => {}
                "drag" => {
                    // Second press within 400 ms = double-click: maximise / restore.
                    let now = Instant::now();
                    if last_drag
                        .get()
                        .is_some_and(|t| now - t < Duration::from_millis(400))
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
    ui.window().on_close_requested(|| {
        stop_session();
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        // Glass blur and rounded corners, applied again when maximised or full
        // screen changes. Full screen turns the blur off like the main window:
        // a blur kept through it came back black afterwards.
        let mut applied: Option<(bool, bool)> = None;
        let mut changed: Option<Instant> = None;
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let full = ui.get_is_fullscreen();
                // Square corners in full screen too.
                let max = ui.window().is_maximized() || full;
                if ui.get_is_maximized() != ui.window().is_maximized() {
                    ui.set_is_maximized(ui.window().is_maximized());
                }
                // Once more after the change settles (as the main window does).
                if changed.is_some_and(|t: Instant| t.elapsed() >= Duration::from_millis(350)) {
                    changed = None;
                    applied = None;
                }
                if applied != Some((max, full)) {
                    if applied.is_some() {
                        changed = Some(Instant::now());
                    }
                    if let Some(h) = crate::hwnd_of(ui.window()) {
                        let mode = if glass_on && !full {
                            crate::backdrop::Backdrop::Accent
                        } else {
                            crate::backdrop::Backdrop::None
                        };
                        crate::backdrop::apply(h, mode, dark, thick, !max);
                        applied = Some((max, full));
                    }
                }
                tick(&ui, &a);
            },
        );
    }
    ui.show()?;
    // Centred on its monitor (winit would put it at the top-left corner).
    if let Some((x, y, mw, mh)) = crate::monitor_rect(ui.window()) {
        let s = ui.window().size();
        ui.window().set_position(slint::PhysicalPosition::new(
            x + (mw as i32 - s.width as i32) / 2,
            y + (mh as i32 - s.height as i32) / 2,
        ));
    }
    slint::run_event_loop()?;
    let _ = ui.hide();
    stop_session();
    ST.with_borrow_mut(|s| s.view = None);
    if MERGE.load(std::sync::atomic::Ordering::Relaxed) {
        std::process::exit(MERGE_EXIT);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppArgs {
        AppArgs {
            serial: "192.168.1.20:37099".into(),
            transport_id: Some(7),
            package: "org.videolan.vlc".into(),
            name: "VLC for Android".into(),
            device: "TECNO POVA 6".into(),
            screen: false,
        }
    }

    #[test]
    fn picture_is_centred_inside_the_stage() {
        // Portrait phone in a wide stage: full height, centred horizontally.
        assert_eq!(
            fit_rect((0.0, 0.0, 400.0, 200.0), (100, 200)),
            (150.0, 0.0, 100.0, 200.0)
        );
        // Landscape picture in a tall stage: full width, centred vertically.
        assert_eq!(
            fit_rect((10.0, 20.0, 200.0, 400.0), (200, 100)),
            (10.0, 170.0, 200.0, 100.0)
        );
        // No frame yet: the whole stage.
        assert_eq!(fit_rect((1.0, 2.0, 3.0, 4.0), (0, 0)), (1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn arguments_round_trip() {
        let a = sample();
        assert_eq!(AppArgs::parse(&a.to_args()), Some(a));
    }

    #[test]
    fn only_the_app_window_argument_starts_it() {
        assert_eq!(AppArgs::parse(&[]), None);
        assert_eq!(AppArgs::parse(&["--background".into()]), None);
    }

    #[test]
    fn unsafe_values_are_refused() {
        let mut a = sample();
        a.package = "org.x;reboot".into();
        assert_eq!(AppArgs::parse(&a.to_args()), None);
        let mut a = sample();
        a.serial = "abc def".into();
        assert_eq!(AppArgs::parse(&a.to_args()), None);
        let mut a = sample();
        a.serial.clear();
        assert_eq!(AppArgs::parse(&a.to_args()), None);
    }

    #[test]
    fn device_window_needs_no_package() {
        let a = AppArgs {
            serial: "R58N".into(),
            name: "Galaxy S24".into(),
            device: "Galaxy S24".into(),
            screen: true,
            ..AppArgs::default()
        };
        assert_eq!(AppArgs::parse(&a.to_args()), Some(a));
        // A package passed by mistake is ignored, never started.
        let b = AppArgs::parse(&[
            ARG.into(),
            "serial=R58N".into(),
            "screen=1".into(),
            "package=x;reboot".into(),
        ])
        .unwrap();
        assert!(b.package.is_empty());
        assert_eq!(b.name, "R58N");
    }

    #[test]
    fn missing_name_falls_back_to_the_package() {
        let a =
            AppArgs::parse(&[ARG.into(), "serial=X1".into(), "package=com.a.b".into()]).unwrap();
        assert_eq!(a.name, "com.a.b");
        assert_eq!(a.transport_id, None);
    }

    #[test]
    fn display_follows_the_window_in_blocks_of_8() {
        assert_eq!(display_size(430.0, 776.0), (424, 776));
        assert_eq!(display_size(100.0, 9000.0), (160, 3840));
        assert_eq!(display_dpi(1.0), 160);
        assert_eq!(display_dpi(1.5), 240);
        assert_eq!(display_dpi(10.0), 640);
    }
}
