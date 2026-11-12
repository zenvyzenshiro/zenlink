//! Setup without an installer engine (R61). The release's Setup.exe is the portable ZenLink.exe
//! under another name: started as `*-Setup.exe` (or with `--install`) it copies itself into the
//! install folder, adds the Start menu shortcut and the Apps entry, and starts ZenLink. One file
//! instead of Inno Setup's Setup.tmp, which is new in every build and which Smart App Control
//! blocks for having no reputation yet (B-63). `ZenLink.exe --uninstall` removes it again.
//!
//! Same folder, Apps entry (Inno's AppId key) and shortcuts as the Inno Setup builds up to
//! v2.5.14, so it upgrades them in place. An older ZenLink's in-app update runs the new Setup
//! with Inno's switches (`/SILENT … /ZLRELAUNCH=1`); they mean the same here. The firewall rule
//! is not added here: ZenLink offers it in the window when Windows Firewall blocks devices.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows::core::{w, Interface, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    REG_DWORD, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_PublicDesktop,
    IShellLinkW, SHGetKnownFolderPath, ShellExecuteExW, ShellLink, KF_FLAG_DEFAULT,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDRETRY, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING,
    MB_OK, MB_RETRYCANCEL, MB_YESNO, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, SW_SHOWNORMAL,
};

use crate::i18n;

/// Inno Setup's AppId key: one "ZenLink" in Apps, old and new.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{6B0E8C1A-5D3F-4E0A-9C2B-2E7A1F5D9A10}_is1";
const EXE: &str = "ZenLink.exe";
const NO_WINDOW: u32 = 0x0800_0000;
/// The copy started with administrator rights (an install in Program Files).
const ELEVATED_ARG: &str = "--setup-elevated";

#[derive(Debug, PartialEq)]
pub enum Mode {
    Install {
        silent: bool,
        relaunch: bool,
        elevated: bool,
    },
    Uninstall {
        silent: bool,
        elevated: bool,
    },
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case(flag))
}

/// What `exe_name` started with `args` should do; `None`: run ZenLink. Only a build that
/// carries everything in one file (`portable`) installs itself.
fn parse(exe_name: &str, args: &[String], portable: bool) -> Option<Mode> {
    let silent = has(args, "/SILENT") || has(args, "/VERYSILENT");
    let elevated = has(args, ELEVATED_ARG);
    if has(args, "--uninstall") {
        return Some(Mode::Uninstall { silent, elevated });
    }
    // "-Setup-Direct.exe", also as a browser renames it ("... (1).exe").
    let lower = exe_name.to_ascii_lowercase();
    let setup_name = lower.contains("-setup") && lower.ends_with(".exe");
    (portable && (setup_name || has(args, "--install") || elevated)).then(|| Mode::Install {
        silent,
        relaunch: has(args, "/ZLRELAUNCH=1"),
        elevated,
    })
}

pub fn mode(args: &[String]) -> Option<Mode> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_string_lossy().into_owned();
    parse(&name, args, cfg!(feature = "portable"))
}

/// Runs Setup or the uninstaller; the process exit code.
pub fn run(m: Mode) -> i32 {
    crate::i18n::set(&crate::prefs::system_lang());
    let (r, silent) = match m {
        Mode::Install {
            silent,
            relaunch,
            elevated,
        } => (install(silent, relaunch, elevated), silent),
        Mode::Uninstall { silent, elevated } => (uninstall(silent, elevated), silent),
    };
    match r {
        Ok(code) => code,
        Err(e) => {
            log::warn!("setup: {e}");
            if !silent {
                say(&e, MB_OK | MB_ICONERROR);
            }
            1
        }
    }
}

fn say(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("ZenLink"), style) }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    n(a) == n(b)
}

fn reg_string(hive: HKEY, key: &str, name: &str) -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            hive,
            &HSTRING::from(key),
            &HSTRING::from(name),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// The installed ZenLink: the Apps entry's hive and folder.
fn existing() -> Option<(HKEY, PathBuf)> {
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .find_map(|hive| {
            let s = reg_string(hive, UNINSTALL_KEY, "InstallLocation")?;
            let s = s.trim().trim_end_matches('\\');
            (!s.is_empty()).then(|| (hive, PathBuf::from(s)))
        })
}

/// Inno's per-user default ({autopf} without admin rights).
fn default_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Programs")
        .join("ZenLink")
}

/// True when this exe is the installed copy (an update then runs the new Setup).
pub fn is_installed_copy() -> bool {
    let (Ok(me), Some((_, dir))) = (std::env::current_exe(), existing()) else {
        return false;
    };
    me.parent().is_some_and(|p| same_dir(p, &dir))
}

/// A running exe cannot be opened for writing (sharing violation).
fn busy(p: &Path) -> bool {
    p.exists()
        && matches!(
            std::fs::OpenOptions::new().write(true).open(p),
            Err(e) if e.raw_os_error() == Some(32)
        )
}

/// Waits until `check` is false: 15 s when silent, else asks to quit ZenLink and retry.
fn wait_until_closed(silent: bool, check: impl Fn() -> bool) -> Result<(), String> {
    let quiet_until = Instant::now() + Duration::from_secs(if silent { 15 } else { 3 });
    while check() {
        if Instant::now() < quiet_until {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        if silent
            || say(
                &i18n::tr(
                    "ZenLink is still running. Quit it (tray icon > Quit), then click Retry.",
                ),
                MB_RETRYCANCEL | MB_ICONWARNING,
            ) != IDRETRY
        {
            return Err(i18n::tr("ZenLink is still running, so it was not changed."));
        }
    }
    Ok(())
}

fn writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".zenlink-setup-test");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(probe)
}

/// Starts this exe again with administrator rights and waits; its exit code.
fn elevate(args: &str) -> Result<i32, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let file = HSTRING::from(me.as_os_str());
    let params = HSTRING::from(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| {
            i18n::tr("Administrator permission was not given. Nothing was changed.")
        })?;
        let mut code = 1u32;
        if !info.hProcess.is_invalid() {
            WaitForSingleObject(info.hProcess, INFINITE);
            let _ = GetExitCodeProcess(info.hProcess, &mut code);
            let _ = CloseHandle(info.hProcess);
        }
        Ok(code as i32)
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s.map(PathBuf::from)
    }
}

/// Start menu folder and desktop: the user's, or everyone's for an install in Program Files.
fn shortcut_dirs(hive: HKEY) -> (Option<PathBuf>, Option<PathBuf>) {
    let (programs, desktop) = if hive == HKEY_LOCAL_MACHINE {
        (FOLDERID_CommonPrograms, FOLDERID_PublicDesktop)
    } else {
        (FOLDERID_Programs, FOLDERID_Desktop)
    };
    (
        known_folder(&programs).map(|p| p.join("ZenLink")),
        known_folder(&desktop),
    )
}

fn shortcut(lnk: &Path, target: &Path) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.SetIconLocation(&HSTRING::from(target.as_os_str()), 0)?;
        link.cast::<IPersistFile>()?
            .Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

fn set_value(hive: HKEY, name: &str, kind: u32, data: &[u8]) {
    let r = unsafe {
        RegSetKeyValueW(
            hive,
            &HSTRING::from(UNINSTALL_KEY),
            &HSTRING::from(name),
            kind,
            Some(data.as_ptr().cast()),
            data.len() as u32,
        )
    };
    if r.is_err() {
        log::warn!("setup: Apps entry {name}: {r:?}");
    }
}

fn set_text(hive: HKEY, name: &str, text: &str) {
    let w: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    set_value(hive, name, REG_SZ.0, &w);
}

/// The Apps entry (Settings > Apps), in place of Inno's.
fn write_apps_entry(hive: HKEY, dir: &Path, exe: &Path) {
    let _ = unsafe { RegDeleteTreeW(hive, &HSTRING::from(UNINSTALL_KEY)) };
    let exe_s = exe.display().to_string();
    set_text(hive, "DisplayName", "ZenLink");
    set_text(hive, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    set_text(hive, "Publisher", "Zenvy Zenshiro");
    set_text(hive, "DisplayIcon", &exe_s);
    set_text(hive, "InstallLocation", &format!("{}\\", dir.display()));
    set_text(hive, "UninstallString", &format!("\"{exe_s}\" --uninstall"));
    set_text(
        hive,
        "QuietUninstallString",
        &format!("\"{exe_s}\" --uninstall /SILENT"),
    );
    set_text(
        hive,
        "URLInfoAbout",
        "https://github.com/zenvyzenshiro/zenlink",
    );
    set_text(
        hive,
        "HelpLink",
        "https://github.com/zenvyzenshiro/zenlink/issues",
    );
    let kb = std::fs::metadata(exe).map(|m| m.len() / 1024).unwrap_or(0) as u32;
    for (name, v) in [("NoModify", 1u32), ("NoRepair", 1), ("EstimatedSize", kb)] {
        set_value(hive, name, REG_DWORD.0, &v.to_le_bytes());
    }
}

/// An Inno Setup install in `dir`: remove what it put there, except ZenLink.exe (replaced next)
/// and languages a user added. Its shipped language files go too: a stale `lang\id.po` would
/// override the newer Indonesian built into ZenLink.exe.
fn remove_inno_files(dir: &Path, programs: Option<&Path>) {
    let list = std::fs::read_to_string(dir.join("install-manifest.txt")).unwrap_or_default();
    for name in list.lines().map(str::trim) {
        let lower = name.to_ascii_lowercase();
        if name.is_empty()
            || name.contains(['\\', '/', ':'])
            || name.contains("..")
            || lower == EXE.to_ascii_lowercase()
            || lower == "lang"
            || lower.starts_with("unins")
        {
            continue;
        }
        let p = dir.join(name);
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        if let Err(e) = r {
            if p.exists() {
                log::warn!("setup: could not remove {}: {e}", p.display());
            }
        }
    }
    for f in [
        "unins000.exe",
        "unins000.dat",
        "unins000.msg",
        "install-manifest.txt",
        r"lang\id.po",
        r"lang\zenlink.pot",
        r"lang\README.md",
    ] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let _ = std::fs::remove_dir(dir.join("lang")); // only when empty
                                                   // Inno's "Uninstall ZenLink" shortcut pointed at unins000.exe.
    if let Some(p) = programs {
        for n in ["Uninstall ZenLink.lnk", "Hapus ZenLink.lnk"] {
            let _ = std::fs::remove_file(p.join(n));
        }
    }
}

fn install(silent: bool, relaunch: bool, elevated: bool) -> Result<i32, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let found = existing();
    let first = found.is_none();
    let (hive, dir) = found.unwrap_or((HKEY_CURRENT_USER, default_dir()));
    let target = dir.join(EXE);
    log::info!(
        "setup: {} {} to {} (silent {silent}, elevated {elevated})",
        if first { "installing" } else { "updating" },
        env!("CARGO_PKG_VERSION"),
        dir.display()
    );
    if !silent && !elevated {
        let text = if first {
            trf!(
                "Install ZenLink {v} on this PC?\n\nFolder: {dir}\nZenLink is added to the Start menu and the desktop.",
                v = env!("CARGO_PKG_VERSION"),
                dir = dir.display()
            )
        } else {
            trf!(
                "Update ZenLink to {v}?\n\nFolder: {dir}\nYour settings are kept.",
                v = env!("CARGO_PKG_VERSION"),
                dir = dir.display()
            )
        };
        if say(&text, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return Ok(2);
        }
    }
    // An in-app update: the old ZenLink quits right after starting this.
    wait_until_closed(silent, || busy(&target))?;
    if let Err(e) = writable(&dir) {
        if e.kind() != std::io::ErrorKind::PermissionDenied || elevated {
            return Err(format!("{}: {e}", dir.display()));
        }
        let args = if silent {
            format!("{ELEVATED_ARG} /SILENT")
        } else {
            ELEVATED_ARG.to_string()
        };
        let code = elevate(&args)?;
        if code == 0 && (!silent || relaunch) {
            start(&target);
        }
        return Ok(code);
    }
    let (programs, desktop) = shortcut_dirs(hive);
    if dir.join("unins000.exe").exists() {
        remove_inno_files(&dir, programs.as_deref());
    }
    if !same_dir(&me, &target) {
        let new = dir.join("ZenLink.exe.new");
        let old = dir.join("ZenLink.exe.old");
        std::fs::copy(&me, &new).map_err(|e| format!("{}: {e}", new.display()))?;
        let _ = std::fs::remove_file(&old);
        if target.exists() {
            std::fs::rename(&target, &old).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        if let Err(e) = std::fs::rename(&new, &target) {
            let _ = std::fs::rename(&old, &target);
            return Err(format!("{}: {e}", target.display()));
        }
        let _ = std::fs::remove_file(&old);
    }
    write_apps_entry(hive, &dir, &target);
    if let Some(p) = &programs {
        let _ = std::fs::create_dir_all(p);
        if let Err(e) = shortcut(&p.join("ZenLink.lnk"), &target) {
            log::warn!("setup: Start menu shortcut: {e}");
        }
    }
    // An update keeps the shortcuts the user has (or removed).
    if first && !silent {
        if let Some(d) = &desktop {
            if let Err(e) = shortcut(&d.join("ZenLink.lnk"), &target) {
                log::warn!("setup: desktop shortcut: {e}");
            }
        }
    }
    log::info!("setup: done");
    // The elevated copy leaves the start to the one that asked for it (not as administrator).
    if !elevated && (!silent || relaunch) {
        start(&target);
    }
    Ok(0)
}

fn start(exe: &Path) {
    if let Err(e) = Command::new(exe).spawn() {
        log::warn!("setup: could not start {}: {e}", exe.display());
    }
}

fn uninstall(silent: bool, elevated: bool) -> Result<i32, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = me.parent().ok_or("no folder")?.to_path_buf();
    let hive = existing()
        .filter(|(_, d)| same_dir(d, &dir))
        .map_or(HKEY_CURRENT_USER, |(h, _)| h);
    if !silent
        && !elevated
        && say(
            &i18n::tr("Remove ZenLink from this PC?\n\nIts logs and downloaded tools are removed too; your settings stay."),
            MB_YESNO | MB_ICONWARNING,
        ) != IDYES
    {
        return Ok(2);
    }
    wait_until_closed(silent, crate::tray::is_running)?;
    if let Err(e) = writable(&dir) {
        if e.kind() == std::io::ErrorKind::PermissionDenied && !elevated {
            let args = if silent {
                format!("--uninstall {ELEVATED_ARG} /SILENT")
            } else {
                format!("--uninstall {ELEVATED_ARG}")
            };
            let code = elevate(&args)?;
            if code == 0 && !silent {
                say(
                    &i18n::tr("ZenLink was removed."),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            return Ok(code);
        }
    }
    crate::tray::set_start_with_windows(false, false);
    if !silent && crate::drivers::has_firewall_rule() {
        if let Err(e) = crate::drivers::remove_firewall_rules() {
            log::warn!("setup: firewall rule: {e}");
        }
    }
    let (programs, desktop) = shortcut_dirs(hive);
    if let Some(p) = programs {
        let _ = std::fs::remove_dir_all(p);
    }
    if let Some(d) = desktop {
        let _ = std::fs::remove_file(d.join("ZenLink.lnk"));
    }
    let _ = unsafe { RegDeleteTreeW(hive, &HSTRING::from(UNINSTALL_KEY)) };
    if let Some(data) = zenlink_core::local_data_dir() {
        let _ = std::fs::remove_dir_all(data);
    }
    // A running exe cannot delete itself: cmd does once this process has exited.
    let cmd = format!(
        "/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" \"{}\" & rmdir \"{}\"",
        me.display(),
        dir.join("ZenLink.exe.old").display(),
        dir.display()
    );
    let _ = Command::new("cmd.exe")
        .raw_arg(cmd)
        .creation_flags(NO_WINDOW)
        .spawn();
    if !silent && !elevated {
        say(
            &i18n::tr("ZenLink was removed."),
            MB_OK | MB_ICONINFORMATION,
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn setup_name_or_switch_installs_only_the_portable_build() {
        let none = args(&[]);
        assert_eq!(
            parse("ZenLink-v2.5.14-Setup.exe", &none, true),
            Some(Mode::Install {
                silent: false,
                relaunch: false,
                elevated: false
            })
        );
        assert!(parse("ZenLink-v2.5.14-Setup-Direct.exe", &none, true).is_some());
        assert!(parse("ZenLink-v2.5.14-Setup-Direct (1).exe", &none, true).is_some());
        assert_eq!(parse("ZenLink-v2.5.14-Setup.exe", &none, false), None);
        assert_eq!(parse("ZenLink-v2.5.14-Portable.exe", &none, true), None);
        assert_eq!(parse("ZenLink.exe", &none, true), None);
        assert!(matches!(
            parse("ZenLink.exe", &args(&["--install"]), true),
            Some(Mode::Install { .. })
        ));
    }

    #[test]
    fn inno_update_switches_are_understood() {
        // What ZenLink v2.5.1 to v2.5.14 pass to the Setup of an update.
        let a = args(&[
            "/SILENT",
            "/SP-",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/MERGETASKS=!firewall",
            "/ZLRELAUNCH=1",
        ]);
        assert_eq!(
            parse("ZenLink-v2.5.15-Setup.exe", &a, true),
            Some(Mode::Install {
                silent: true,
                relaunch: true,
                elevated: false
            })
        );
    }

    #[test]
    fn uninstall_switch() {
        assert_eq!(
            parse("ZenLink.exe", &args(&["--uninstall", "/SILENT"]), false),
            Some(Mode::Uninstall {
                silent: true,
                elevated: false
            })
        );
    }

    #[test]
    fn folders_compare_without_case_or_trailing_slash() {
        assert!(same_dir(
            Path::new(r"C:\Users\a\AppData\Local\Programs\ZenLink\"),
            Path::new(r"c:\users\a\appdata\local\programs\zenlink")
        ));
        assert!(!same_dir(Path::new(r"C:\a"), Path::new(r"C:\b")));
    }
}
