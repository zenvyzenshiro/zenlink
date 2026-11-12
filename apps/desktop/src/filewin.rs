//! v2.6: the selected Android phone's files, in the drawer under the picture.
//!
//! The drawer browses the phone's shared storage over the ADB sync protocol
//! (`zenlink_android::browse`), copies files to the PC
//! (`Downloads\ZenLink\<phone>`) and sends files dropped on the window to
//! the folder that is open. Copies run one at a time on their own thread
//! with progress and cancel; browsing stays responsive meanwhile. State lives
//! in the Slint `Files` global.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel};
use zenlink_android::adb::{AdbClient, Target};
use zenlink_android::browse::{self, CopyError, Entry};

use crate::{i18n, AppWindow, FileRow, Files, PlaceRow};

/// The phone the drawer shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileArgs {
    pub serial: String,
    pub transport_id: Option<u64>,
    pub device: String,
}

impl FileArgs {
    fn target(&self) -> Target<'_> {
        match self.transport_id {
            Some(id) => Target::TransportId(id),
            None => Target::Serial(&self.serial),
        }
    }
}

/// "12.3 MB"; folders show a word instead.
pub fn size_text(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", U[i])
    } else {
        format!("{v:.0} {}", U[i])
    }
}

/// `YYYY-MM-DD` (UTC) from Unix seconds; empty for 0.
pub fn date_text(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    // Civil-from-days (Howard Hinnant), no time zone library needed.
    let z = i64::from(secs) / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Segoe Fluent Icons code point for an entry.
pub fn icon_for(e: &Entry) -> &'static str {
    if e.is_dir() {
        return "\u{E8B7}";
    }
    let ext = e
        .name
        .rsplit_once('.')
        .map(|(_, x)| x.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "bmp" | "dng" => "\u{EB9F}",
        "mp4" | "mkv" | "mov" | "webm" | "3gp" | "avi" => "\u{E714}",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "amr" => "\u{E8D6}",
        "apk" | "apks" | "xapk" => "\u{E7B8}",
        "pdf" | "doc" | "docx" | "txt" | "xls" | "xlsx" | "ppt" | "pptx" | "md" => "\u{E8A5}",
        "zip" | "rar" | "7z" | "tar" | "gz" => "\u{F012}",
        _ => "\u{E7C3}",
    }
}

/// Where copies from the phone land: `Downloads\ZenLink\<phone>`.
pub fn download_dir(device: &str) -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Downloads")
        .join("ZenLink");
    let name = zenlink_android::files::safe_name(device).unwrap_or_else(|| "Android".into());
    base.join(zenlink_android::files::windows_safe(name))
}

#[derive(Clone)]
enum CopyJob {
    Pull {
        phone: FileArgs,
        remote: String,
        size: u64,
        open: bool,
    },
    Push {
        phone: FileArgs,
        local: PathBuf,
        dir: String,
    },
}

impl CopyJob {
    fn label(&self) -> String {
        match self {
            CopyJob::Pull { remote, .. } => remote.rsplit('/').next().unwrap_or("").to_string(),
            CopyJob::Push { local, .. } => local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

struct State {
    /// The phone shown; `None` until the drawer first opens.
    phone: Option<FileArgs>,
    path: String,
    entries: Vec<Entry>,
    hidden: bool,
    /// Bumped on every listing request; a late answer for an older folder is dropped.
    gen: u64,
    copier: Option<Sender<CopyJob>>,
}

thread_local! {
    static ST: RefCell<State> = const {
        RefCell::new(State {
            phone: None,
            path: String::new(),
            entries: Vec::new(),
            hidden: false,
            gen: 0,
            copier: None,
        })
    };
}

static CANCEL: AtomicBool = AtomicBool::new(false);
static QUEUED: AtomicUsize = AtomicUsize::new(0);

fn show_entries(ui: &AppWindow) {
    let rows: Vec<FileRow> = ST.with_borrow(|s| {
        s.entries
            .iter()
            .map(|e| FileRow {
                name: e.name.clone().into(),
                size: if e.is_dir() {
                    "".into()
                } else {
                    size_text(e.size).into()
                },
                date: date_text(e.mtime).into(),
                dir: e.is_dir(),
                icon: icon_for(e).into(),
            })
            .collect()
    });
    let f = ui.global::<Files>();
    f.set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    f.set_selected(-1);
}

/// Lists `path` on a short-lived thread and shows it when it arrives.
fn go(ui: &AppWindow, path: String) {
    let Some((phone, gen, hidden)) = ST.with_borrow_mut(|s| {
        let phone = s.phone.clone()?;
        s.gen += 1;
        s.path = path.clone();
        Some((phone, s.gen, s.hidden))
    }) else {
        return;
    };
    let f = ui.global::<Files>();
    f.set_path(path.clone().into());
    f.set_phase("loading".into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let r = browse::list(&AdbClient::default(), &phone.target(), &path, hidden);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ST.with_borrow(|s| s.gen) != gen {
                return;
            }
            let f = ui.global::<Files>();
            match r {
                Ok(v) => {
                    ST.with_borrow_mut(|s| s.entries = v);
                    show_entries(&ui);
                    f.set_phase("ready".into());
                }
                Err(e) => {
                    log::warn!("files: list {path}: {e}");
                    f.set_message(error_text(&e.to_string()).into());
                    f.set_phase("error".into());
                }
            }
        });
    });
}

fn current_path() -> String {
    ST.with_borrow(|s| s.path.clone())
}

fn notice(ui: &AppWindow, text: String) {
    ui.global::<Files>().set_notice(text.into());
}

fn error_text(e: &str) -> String {
    let low = e.to_lowercase();
    if low.contains("permission denied") {
        i18n::tr("Android does not allow opening this folder.")
    } else if low.contains("not found") || low.contains("offline") || low.contains("no devices") {
        i18n::tr("The phone is not connected. Connect it again, then Refresh.")
    } else {
        format!("{}: {e}", i18n::tr("Failed"))
    }
}

fn entry_at(i: i32) -> Option<(FileArgs, String, Entry)> {
    ST.with_borrow(|s| {
        let e = s.entries.get(usize::try_from(i).ok()?)?.clone();
        Some((s.phone.clone()?, browse::join(&s.path, &e.name), e))
    })
}

fn enqueue(job: CopyJob) {
    QUEUED.fetch_add(1, Ordering::Relaxed);
    ST.with_borrow(|s| {
        if let Some(tx) = &s.copier {
            let _ = tx.send(job);
        }
    });
}

/// The copy thread: one job at a time, progress shown in the strip.
fn start_copier(ui: &AppWindow) -> Sender<CopyJob> {
    let (tx, rx) = channel::<CopyJob>();
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let client = AdbClient::default();
        for job in rx {
            QUEUED.fetch_sub(1, Ordering::Relaxed);
            CANCEL.store(false, Ordering::Relaxed);
            let label = job.label();
            let total = match &job {
                CopyJob::Pull { size, .. } => *size,
                CopyJob::Push { local, .. } => std::fs::metadata(local).map_or(0, |m| m.len()),
            };
            {
                let weak = weak.clone();
                let label = label.clone();
                let q = QUEUED.load(Ordering::Relaxed) as i32;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let f = ui.global::<Files>();
                        f.set_busy(true);
                        f.set_job(label.into());
                        f.set_job_progress(if total > 0 { 0.0 } else { -1.0 });
                        f.set_queued(q);
                    }
                });
            }
            let mut last = Instant::now();
            let mut progress = |done: u64| {
                if total == 0 || last.elapsed() < Duration::from_millis(80) {
                    return;
                }
                last = Instant::now();
                let p = (done as f64 / total as f64).min(1.0) as f32;
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<Files>().set_job_progress(p);
                    }
                });
            };
            let result: Result<(String, Option<PathBuf>, bool), CopyError> = match &job {
                CopyJob::Pull {
                    phone,
                    remote,
                    open,
                    ..
                } => {
                    let dir = download_dir(&phone.device);
                    std::fs::create_dir_all(&dir)
                        .map_err(CopyError::from)
                        .and_then(|_| {
                            browse::pull(
                                &client,
                                &phone.target(),
                                remote,
                                &dir,
                                &mut progress,
                                &CANCEL,
                            )
                        })
                        .map(|p| (label.clone(), Some(p), *open))
                }
                CopyJob::Push { phone, local, dir } => {
                    browse::push(&client, &phone.target(), local, dir, &mut progress, &CANCEL)
                        .map(|name| (name, None, false))
                }
            };
            let refresh = matches!(job, CopyJob::Push { .. });
            let weak = weak.clone();
            let left = QUEUED.load(Ordering::Relaxed);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                if left == 0 {
                    ui.global::<Files>().set_busy(false);
                }
                match result {
                    Ok((name, Some(path), open)) => {
                        log::info!("files: copied {} to {}", name, path.display());
                        notice(&ui, format!("{} {}", i18n::tr("Copied to"), path.display()));
                        if open {
                            open_on_pc(&path);
                        }
                    }
                    Ok((name, None, _)) => {
                        log::info!("files: sent {name}");
                        notice(&ui, format!("{} {name}", i18n::tr("Sent:")));
                    }
                    Err(CopyError::Cancelled) => notice(&ui, i18n::tr("Cancelled.")),
                    Err(CopyError::Adb(e)) => {
                        log::warn!("files: copy failed: {e}");
                        notice(&ui, error_text(&e.to_string()));
                    }
                }
                if refresh {
                    go(&ui, current_path());
                }
            });
        }
    });
    tx
}

/// Files that would run code when opened; from the phone they are only shown
/// in their folder, never started.
fn runs_code(path: &Path) -> bool {
    const RISKY: &[&str] = &[
        "exe",
        "com",
        "scr",
        "pif",
        "msi",
        "msix",
        "appx",
        "bat",
        "cmd",
        "ps1",
        "psm1",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "hta",
        "lnk",
        "url",
        "reg",
        "cpl",
        "dll",
        "jar",
        "chm",
        "inf",
        "scf",
        "msc",
        "application",
        "appref-ms",
        "settingcontent-ms",
        // Office macros, add-ins and query files, scripts, shell and theme files.
        "docm",
        "dotm",
        "xlsm",
        "xltm",
        "xlam",
        "pptm",
        "potm",
        "ppam",
        "xll",
        "wll",
        "iqy",
        "slk",
        "rdp",
        "py",
        "pyw",
        "sct",
        "ws",
        "msp",
        "theme",
        "themepack",
        "library-ms",
        "search-ms",
        "searchconnector-ms",
        "diagcab",
        "gadget",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| RISKY.contains(&e.to_ascii_lowercase().as_str()))
}

/// Opens a file with its app, or a folder in Explorer. A file that would run
/// code is selected in its folder instead.
fn open_on_pc(path: &Path) {
    let mut c = std::process::Command::new("explorer.exe");
    if path.is_file() && runs_code(path) {
        c.arg(format!("/select,{}", path.display()));
    } else {
        c.arg(path);
    }
    if let Err(e) = c.spawn() {
        log::warn!("files: explorer: {e}");
    }
}

/// Runs a folder change on a thread, then shows the result and re-lists.
fn change(
    ui: &AppWindow,
    ok: String,
    work: impl FnOnce(&AdbClient, &Target) -> zenlink_android::adb::Result<()> + Send + 'static,
) {
    let Some(phone) = ST.with_borrow(|s| s.phone.clone()) else {
        return;
    };
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let r = work(&AdbClient::default(), &phone.target());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match r {
                Ok(()) => {
                    if !ok.is_empty() {
                        notice(&ui, ok);
                    }
                }
                Err(e) => notice(&ui, error_text(&e.to_string())),
            }
            go(&ui, current_path());
        });
    });
}

/// Registers the drawer's callbacks; call once at start.
pub fn install(ui: &AppWindow) {
    let f = ui.global::<Files>();
    let places: Vec<PlaceRow> = browse::QUICK_DIRS
        .iter()
        .map(|(label, path)| {
            // The label is an English key; the drawer shows it translated.
            let icon = match *label {
                "Internal storage" => "\u{EDA2}",
                "Camera" => "\u{E722}",
                "Download" => "\u{E896}",
                "Pictures" => "\u{EB9F}",
                "Movies" => "\u{E714}",
                "Documents" => "\u{E8A5}",
                "Music" => "\u{E8D6}",
                _ => "\u{E8B7}",
            };
            PlaceRow {
                label: (*label).into(),
                path: (*path).into(),
                icon: icon.into(),
            }
        })
        .collect();
    f.set_places(ModelRc::from(Rc::new(VecModel::from(places))));
    let tx = start_copier(ui);
    ST.with_borrow_mut(|s| s.copier = Some(tx));

    let weak = ui.as_weak();
    f.on_go(move |path| {
        if let Some(ui) = weak.upgrade() {
            go(&ui, path.to_string());
        }
    });
    let weak = ui.as_weak();
    f.on_up(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(p) = browse::parent(&current_path()) {
            go(&ui, p.to_string());
        }
    });
    let weak = ui.as_weak();
    f.on_refresh(move || {
        if let Some(ui) = weak.upgrade() {
            go(&ui, current_path());
        }
    });
    let weak = ui.as_weak();
    f.on_show_hidden(move |on| {
        let Some(ui) = weak.upgrade() else { return };
        ST.with_borrow_mut(|s| s.hidden = on);
        ui.global::<Files>().set_hidden_files(on);
        go(&ui, current_path());
    });
    let weak = ui.as_weak();
    f.on_open_row(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some((phone, path, e)) = entry_at(i) else {
            return;
        };
        if e.is_dir() || e.is_link() {
            go(&ui, path);
        } else {
            enqueue(CopyJob::Pull {
                phone,
                remote: path,
                size: e.size,
                open: true,
            });
        }
    });
    f.on_copy_to_pc(move |i| {
        if let Some((phone, path, e)) = entry_at(i) {
            if !e.is_dir() {
                enqueue(CopyJob::Pull {
                    phone,
                    remote: path,
                    size: e.size,
                    open: false,
                });
            }
        }
    });
    let weak = ui.as_weak();
    f.on_delete_row(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let Some((_, path, _)) = entry_at(i) else {
            return;
        };
        log::info!("files: delete {path}");
        let ok = i18n::tr("Deleted.");
        change(&ui, ok, move |c, t| browse::delete(c, t, &path));
    });
    let weak = ui.as_weak();
    f.on_new_folder(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(name) = zenlink_android::files::safe_name(name.as_str()) else {
            notice(&ui, i18n::tr("That folder name cannot be used."));
            return;
        };
        let dir = browse::join(&current_path(), &name);
        change(&ui, String::new(), move |c, t| browse::make_dir(c, t, &dir));
    });
    f.on_cancel_job(|| CANCEL.store(true, Ordering::Relaxed));
    f.on_open_downloads(|| {
        if let Some(phone) = ST.with_borrow(|s| s.phone.clone()) {
            let dir = download_dir(&phone.device);
            let _ = std::fs::create_dir_all(&dir);
            open_on_pc(&dir);
        }
    });
}

/// Shows `phone` in the drawer: a new phone starts at `/sdcard`, the same
/// phone keeps its folder and refreshes.
pub fn show(ui: &AppWindow, phone: FileArgs) {
    ui.global::<Files>().set_device(phone.device.clone().into());
    // Always keep the newest target: a reconnected phone has a new transport id.
    let same = ST.with_borrow_mut(|s| {
        let same = s.phone.as_ref().map(|p| &p.serial) == Some(&phone.serial);
        s.phone = Some(phone);
        same
    });
    if same {
        go(ui, current_path());
        return;
    }
    ST.with_borrow_mut(|s| s.entries.clear());
    notice(ui, String::new());
    go(ui, "/sdcard".into());
}

/// A file dropped on the window while the drawer shows a phone: it goes to
/// the open folder. False when the drawer is closed (filedrop handles it).
pub fn dropped(ui: &AppWindow, path: &Path) -> bool {
    if !ui.get_drawer_files() {
        return false;
    }
    let Some(phone) = ST.with_borrow(|s| s.phone.clone()) else {
        return false;
    };
    if path.is_file() {
        enqueue(CopyJob::Push {
            phone,
            local: path.to_path_buf(),
            dir: current_path(),
        });
    } else {
        notice(ui, i18n::tr("Folders cannot be sent yet: drop the files."));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_dates() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(1536), "1.5 KB");
        assert_eq!(size_text(250 * 1024 * 1024), "250 MB");
        assert_eq!(date_text(0), "");
        assert_eq!(date_text(1_700_000_000), "2023-11-14");
        assert_eq!(date_text(951_782_400), "2000-02-29");
    }

    #[test]
    fn icons_by_kind() {
        let e = |n: &str, mode| Entry {
            name: n.into(),
            mode,
            size: 0,
            mtime: 0,
        };
        assert_eq!(icon_for(&e("DCIM", 0o040771)), "\u{E8B7}");
        assert_eq!(icon_for(&e("IMG_1.JPG", 0o100660)), "\u{EB9F}");
        assert_eq!(icon_for(&e("a.apk", 0o100660)), "\u{E7B8}");
        assert_eq!(icon_for(&e("noext", 0o100660)), "\u{E7C3}");
    }

    #[test]
    fn phone_files_that_run_code_are_never_started() {
        for n in ["setup.exe", "a.BAT", "x.lnk", "y.hta", "z.ps1", "noext"] {
            assert!(runs_code(Path::new(n)), "{n}");
        }
        for n in ["IMG_1.jpg", "song.mp3", "doc.pdf", "clip.MP4"] {
            assert!(!runs_code(Path::new(n)), "{n}");
        }
    }

    #[test]
    fn downloads_go_to_a_safe_folder() {
        let d = download_dir("Pixel: 8/Pro");
        assert!(d.ends_with("Pixel_ 8_Pro"));
        assert!(d.to_string_lossy().contains("ZenLink"));
    }
}
