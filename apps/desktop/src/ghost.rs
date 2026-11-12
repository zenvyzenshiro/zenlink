//! Ghost mode (R58): the window stays on top, fades to the chosen opacity and lets mouse
//! clicks through to the app behind it, to watch the phone while working in another app
//! (a chart in TradingView, for example). The shortcut turns it on while ZenLink has focus;
//! while Ghost mode is on the same key is a global hotkey that turns it off. The hotkey exists
//! only then, so the key stays free for other apps the rest of the time. A tray click also
//! leaves Ghost mode.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, GetWindowLongPtrW, PostThreadMessageW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA, MSG,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_HOTKEY, WM_QUIT, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

/// The keys Settings offers (F11 is full screen, F10 opens window menus).
pub const KEYS: [&str; 4] = ["F4", "F6", "F8", "F9"];

static ON: AtomicBool = AtomicBool::new(false);
/// Thread that owns the global hotkey (0: none).
static HOTKEY_THREAD: AtomicU32 = AtomicU32::new(0);

pub fn is_on() -> bool {
    ON.load(Relaxed)
}

/// `F<n>` as a number, for the keys in [`KEYS`].
fn f_number(key: &str) -> Option<u32> {
    let n: u32 = key.strip_prefix('F')?.parse().ok()?;
    KEYS.contains(&key).then_some(n)
}

/// Whether a Slint key event (`text`) is this shortcut. Slint's F1 is U+F704.
pub fn is_key(text: &str, key: &str) -> bool {
    f_number(key)
        .and_then(|n| char::from_u32(0xF703 + n))
        .is_some_and(|c| {
            let mut it = text.chars();
            it.next() == Some(c) && it.next().is_none()
        })
}

/// Turns Ghost mode on. `off` runs on the hotkey thread when the key is pressed again.
pub fn enter(hwnd: HWND, key: &str, opacity: f32, off: impl Fn() + Send + 'static) {
    let Some(n) = f_number(key) else { return };
    if ON.swap(true, Relaxed) {
        return;
    }
    let alpha = (opacity.clamp(20.0, 100.0) * 2.55).round() as u8;
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex | (WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) as isize,
        );
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    let vk = 0x6F + n; // VK_F1 = 0x70
    std::thread::Builder::new()
        .name("ghost-hotkey".into())
        .spawn(move || unsafe {
            HOTKEY_THREAD.store(GetCurrentThreadId(), Relaxed);
            if let Err(e) = RegisterHotKey(None, 1, MOD_NOREPEAT, vk) {
                // Another app owns the key: the tray still leaves Ghost mode.
                log::warn!("ghost: F{n} is taken by another app ({e})");
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_HOTKEY {
                    off();
                }
            }
            let _ = UnregisterHotKey(None, 1);
        })
        .ok();
    log::info!("ghost: on (F{n}, {alpha}/255)");
}

/// Turns Ghost mode off; `on_top`: the window's normal "Always on top" setting.
pub fn leave(hwnd: HWND, on_top: bool) {
    if !ON.swap(false, Relaxed) {
        return;
    }
    let t = HOTKEY_THREAD.swap(0, Relaxed);
    if t != 0 {
        let _ = unsafe { PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex & !((WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0) as isize),
        );
        let _ = SetWindowPos(
            hwnd,
            Some(if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    log::info!("ghost: off");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert!(is_key("\u{F707}", "F4"));
        assert!(is_key("\u{F70C}", "F9"));
        assert!(!is_key("\u{F707}", "F6"));
        assert!(!is_key("\u{F70E}", "F11"), "only the offered keys");
        assert!(!is_key("4", "F4"));
    }
}
