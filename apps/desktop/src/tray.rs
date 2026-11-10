//! Notification-area icon ("Close to the tray"), "Start with Windows" and
//! one window per user.

use std::sync::atomic::AtomicBool;

pub const BACKGROUND_ARG: &str = "--background";

/// What the tray asks the window to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Left click: show the window, or hide it when it is shown.
    Toggle,
    /// Menu "Open", or a second ZenLink.exe was started.
    Show,
    Quit,
}

/// True while the window is hidden in the tray (no work for it then).
pub static HIDDEN: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
        RRF_RT_REG_BINARY, RRF_RT_REG_SZ,
    };
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, FindWindowW,
        GetCursorPos, GetSystemMetrics, LoadIconW, LoadImageW, PostMessageW, RegisterClassW,
        RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, HICON, IDI_APPLICATION,
        IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, SM_CXSMICON, SM_CYSMICON,
        TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU,
        WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    const CLASS: PCWSTR = w!("ZenLinkTrayV25");
    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    const RUN_VALUE: PCWSTR = w!("ZenLink");
    const APPROVED_KEY: PCWSTR =
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run");

    const WM_TRAY: u32 = WM_APP + 0x51;
    const WM_SHOW_OTHER: u32 = WM_APP + 0x52;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;

    type Handler = Rc<dyn Fn(Event)>;

    thread_local! {
        static HWND_TRAY: Cell<Option<isize>> = const { Cell::new(None) };
        static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
        static LABELS: RefCell<(String, String)> = RefCell::new(("Open ZenLink".into(), "Quit".into()));
        static ICON_ON: Cell<bool> = const { Cell::new(false) };
        static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn show_running_instance() -> bool {
        match unsafe { FindWindowW(CLASS, PCWSTR::null()) } {
            Ok(h) if h.0 != std::ptr::null_mut() => {
                unsafe {
                    let _ = PostMessageW(Some(h), WM_SHOW_OTHER, WPARAM(0), LPARAM(0));
                }
                true
            }
            _ => false,
        }
    }

    pub fn install(handler: impl Fn(Event) + 'static) {
        HANDLER.with(|h| *h.borrow_mut() = Some(Rc::new(handler)));
        TASKBAR_CREATED.with(|c| {
            let id = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
            c.set(id);
        });
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()).unwrap() };
        let wnd_class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&wnd_class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS,
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance),
                None,
            );
            if let Ok(h) = hwnd {
                HWND_TRAY.with(|cell| cell.set(Some(h.0 as isize)));
            }
        }
    }

    pub fn set_icon(on: bool, open: &str, quit: &str) {
        LABELS.with(|l| *l.borrow_mut() = (open.to_string(), quit.to_string()));
        let was_on = ICON_ON.with(|c| c.replace(on));
        if was_on == on {
            return;
        }
        if on {
            notify(NIM_ADD);
        } else {
            notify(NIM_DELETE);
        }
    }

    fn notify(op: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE) -> bool {
        let Some(hwnd) = HWND_TRAY.with(|c| c.get()) else {
            return false;
        };
        let mut tip = [0u16; 128];
        let t = wide("ZenLink");
        let n = t.len().min(tip.len());
        tip[..n].copy_from_slice(&t[..n]);
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(hwnd as _),
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon_handle(),
            szTip: tip,
            ..Default::default()
        };
        unsafe { Shell_NotifyIconW(op, &data).as_bool() }
    }

    fn icon_handle() -> HICON {
        let inst = unsafe { GetModuleHandleW(PCWSTR::null()).unwrap() };
        let cx = unsafe { GetSystemMetrics(SM_CXSMICON) };
        let cy = unsafe { GetSystemMetrics(SM_CYSMICON) };
        if let Ok(h) = unsafe {
            LoadImageW(
                Some(inst.into()),
                windows::core::PCWSTR(1 as _),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
        } {
            if !h.is_invalid() {
                return HICON(h.0);
            }
        }
        unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        let taskbar = TASKBAR_CREATED.with(|c| c.get());
        if taskbar != 0 && msg == taskbar {
            if ICON_ON.with(|c| c.get()) {
                notify(NIM_ADD);
            }
            return LRESULT(0);
        }
        match msg {
            WM_SHOW_OTHER => {
                dispatch(Event::Show);
                LRESULT(0)
            }
            WM_TRAY => {
                let event = l.0 as u32;
                match event {
                    WM_LBUTTONUP => dispatch(Event::Toggle),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    fn dispatch(e: Event) {
        let h = HANDLER.with(|cell| cell.borrow().clone());
        if let Some(f) = h {
            f(e);
        }
    }

    fn show_menu(hwnd: HWND) {
        let (open_label, quit_label) = LABELS.with(|l| l.borrow().clone());
        unsafe {
            let menu = match CreatePopupMenu() {
                Ok(m) => m,
                Err(_) => return,
            };
            let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, PCWSTR(wide(&open_label).as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, PCWSTR(wide(&quit_label).as_ptr()));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD,
                pt.x,
                pt.y,
                0,
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                ID_OPEN => dispatch(Event::Show),
                ID_QUIT => dispatch(Event::Quit),
                _ => {}
            }
        }
    }

    pub fn run_command(exe: &std::path::Path) -> String {
        format!("\"{}\" {BACKGROUND_ARG}", exe.display())
    }

    pub fn set_start_with_windows(on: bool, user_action: bool) {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return,
        };
        let cmd = wide(&run_command(&exe));
        unsafe {
            if on {
                let _ = RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    RUN_KEY,
                    RUN_VALUE,
                    REG_SZ.0,
                    Some(cmd.as_ptr() as _),
                    (cmd.len() * 2) as u32,
                );
            } else {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            }
            if user_action {
                let flag: [u8; 12] = if on {
                    [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                } else {
                    [3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                };
                let _ = RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    APPROVED_KEY,
                    RUN_VALUE,
                    windows::Win32::System::Registry::REG_BINARY.0,
                    Some(flag.as_ptr() as _),
                    flag.len() as u32,
                );
            }
        }
    }

    pub fn start_with_windows_on() -> bool {
        let mut buf = [0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr() as _),
                Some(&mut len),
            )
            .is_ok()
        };
        if !ok {
            return false;
        }
        let mut flags = [0u8; 12];
        let mut flen = flags.len() as u32;
        let app_ok = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                APPROVED_KEY,
                RUN_VALUE,
                RRF_RT_REG_BINARY,
                None,
                Some(flags.as_mut_ptr() as _),
                Some(&mut flen),
            )
            .is_ok()
        };
        if app_ok && flen >= 1 {
            return (flags[0] & 1) == 0;
        }
        true
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
pub fn show_running_instance() -> bool {
    false
}

#[cfg(not(windows))]
pub fn install(_handler: impl Fn(Event) + 'static) {}

#[cfg(not(windows))]
pub fn set_icon(_on: bool, _open: &str, _quit: &str) {}

#[cfg(not(windows))]
pub fn run_command(exe: &std::path::Path) -> String {
    format!("\"{}\" {BACKGROUND_ARG}", exe.display())
}

#[cfg(not(windows))]
pub fn set_start_with_windows(_on: bool, _user_action: bool) {}

#[cfg(not(windows))]
pub fn start_with_windows_on() -> bool {
    false
}
