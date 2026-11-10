//! In-app update (v2.5.1): HTTPS through WinHTTP, SHA-256 through BCrypt
//! (both part of Windows, no extra crates), and the three ways to install.
//!
//! Nothing is downloaded until the user clicks "Update". A
//! file is only run after its SHA-256 matches the digest GitHub publishes for
//! the release asset; [`zenlink_core::update`] already refused assets from
//! anywhere but this repository's release downloads.

use std::path::{Path, PathBuf};
use zenlink_core::update::{Asset, Channel, Offer, Release, Version};

#[cfg(windows)]
use windows::core::{HSTRING, PCWSTR};
#[cfg(windows)]
use windows::Win32::Networking::WinHttp::*;
#[cfg(windows)]
use windows::Win32::Security::Cryptography::*;

/// `ZenLink.exe --replace-old=<path>`: a portable update started this exe;
/// delete the old one once it has exited.
pub const REPLACE_ARG: &str = "--replace-old=";

/// How this copy of ZenLink was installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Setup.exe: run the new Setup silently (same AppId, upgrades in place).
    Installed,
    /// Portable.exe: put the new exe next to this one.
    Portable,
    /// A source checkout or portable folder: only point to the release page.
    Source,
    /// The Microsoft Store package (MSIX): the Store installs updates.
    Store,
}

/// The Microsoft Store product (Partner Center Store ID).
pub const STORE_PRODUCT_ID: &str = "9N67KCFZSB1T";

/// True when this process runs from an MSIX package (the Microsoft Store
/// edition). An unpackaged exe gets APPMODEL_ERROR_NO_PACKAGE.
pub fn is_store() -> bool {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        static STORE: OnceLock<bool> = OnceLock::new();
        *STORE.get_or_init(|| {
            let mut len = 0u32;
            let r = unsafe {
                windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName(&mut len, None)
            };
            // ERROR_INSUFFICIENT_BUFFER (122): there is a package name to read.
            r.0 == 122
        })
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Opens ZenLink's page in the Microsoft Store app.
pub fn open_store_page() {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!(
            "ms-windows-store://pdp/?productid={STORE_PRODUCT_ID}"
        ))
        .spawn();
}

/// Where the logs really are.
pub fn visible_log_dir() -> PathBuf {
    zenlink_core::log_dir()
}

pub fn kind() -> Kind {
    if is_store() {
        return Kind::Store;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Kind::Source;
    };
    let dir = exe.parent().unwrap_or(Path::new("."));
    if cfg!(feature = "portable") {
        Kind::Portable
    } else if dir.join("unins000.exe").exists() {
        Kind::Installed
    } else {
        Kind::Source
    }
}

pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo version")
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::c_void;
    use std::io::Write;
    use std::time::{Duration, Instant};

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

fn last_error(what: &str) -> String {
    format!("{what}: {}", windows::core::Error::from_thread())
}

/// An open HTTPS GET with a 2xx answer; [`Response::read`] streams the body.
struct Response {
    _session: Handle,
    _connect: Handle,
    request: Handle,
    pub length: Option<u64>,
}

impl Response {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        let mut n = 0u32;
        unsafe {
            WinHttpReadData(
                self.request.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut n,
            )
        }
        .map_err(|e| format!("read: {e}"))?;
        Ok(n as usize)
    }
}

/// Splits `https://host/path` (only HTTPS is accepted).
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    (!host.is_empty() && !host.contains(['@', ':', ' ', '?', '#'])).then_some((host, path))
}

fn get(url: &str, accept: &str) -> Result<Response, String> {
    let (host, path) = split_url(url).ok_or("not an https URL")?;
    let agent = HSTRING::from(format!("ZenLink-Updater/{}", env!("CARGO_PKG_VERSION")));
    unsafe {
        let session = Handle(WinHttpOpen(
            &agent,
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        // TLS 1.2 and 1.3 only; resolve, connect, send 15 s, receive 30 s.
        let protocols = (WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3)
            .to_le_bytes();
        let _ = WinHttpSetOption(
            Some(session.0),
            WINHTTP_OPTION_SECURE_PROTOCOLS,
            Some(&protocols),
        );
        let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 15_000, 30_000);
        let connect = Handle(WinHttpConnect(
            session.0,
            &HSTRING::from(host),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ));
        if connect.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from("GET"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        // Redirects (GitHub sends downloads to its asset host) stay on
        // HTTPS: WinHTTP's default policy refuses HTTPS to HTTP.
        let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)
            .map_err(|e| format!("send: {e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| format!("receive: {e}"))?;
        let number = |info: u32| -> Option<u64> {
            let mut v = 0u32;
            let mut len = 4u32;
            WinHttpQueryHeaders(
                request.0,
                info | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut v as *mut u32).cast()),
                &mut len,
                std::ptr::null_mut(),
            )
            .ok()
            .map(|_| v as u64)
        };
        let status = number(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        let length = number(WINHTTP_QUERY_CONTENT_LENGTH);
        Ok(Response {
            _session: session,
            _connect: connect,
            request,
            length,
        })
    }
}

/// The releases feed: GitHub's API, or `ZL_UPDATE_FEED` (a local JSON file
/// or another https URL) for tests.
fn feed() -> Result<serde_json::Value, String> {
    let src = std::env::var("ZL_UPDATE_FEED").unwrap_or_else(|_| update::RELEASES_URL.to_string());
    let bytes = if src.starts_with("https://") {
        let mut r = get(&src, "application/vnd.github+json")?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > 8 << 20 {
                return Err("feed too large".into());
            }
        }
        out
    } else {
        std::fs::read(&src).map_err(|e| format!("{src}: {e}"))?
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("feed: {e}"))
}

/// Asks GitHub what to offer this build on `channel`.
pub fn check(channel: Channel) -> Result<Offer, String> {
    Ok(update::pick(&feed()?, &current_version(), channel))
}

/// The file a user of this install kind needs from `r`.
pub fn asset_for(kind: Kind, r: &Release) -> Option<&Asset> {
    match kind {
        Kind::Installed => r.setup.as_ref(),
        Kind::Portable => r.portable.as_ref(),
        Kind::Source | Kind::Store => None,
    }
}

struct Sha256(BCRYPT_HASH_HANDLE);
impl Sha256 {
    fn new() -> Result<Self, String> {
        let mut h = BCRYPT_HASH_HANDLE::default();
        let st = unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, None, None, 0) };
        st.ok().map_err(|e| format!("sha256: {e}"))?;
        Ok(Self(h))
    }
    fn update(&mut self, data: &[u8]) -> Result<(), String> {
        unsafe { BCryptHashData(self.0, data, 0) }
            .ok()
            .map_err(|e| format!("sha256: {e}"))
    }
    fn hex(self) -> Result<String, String> {
        let mut out = [0u8; 32];
        unsafe { BCryptFinishHash(self.0, &mut out, 0) }
            .ok()
            .map_err(|e| format!("sha256: {e}"))?;
        Ok(out.iter().map(|b| format!("{b:02x}")).collect())
    }
}
impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            let _ = BCryptDestroyHash(self.0);
        }
    }
}

/// SHA-256 of `data`, lower-case hex.
#[cfg(test)]
pub fn sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut h = Sha256::new()?;
    h.update(data)?;
    h.hex()
}

/// Downloads `a` to `dest` (through `dest.part`) and checks its size and
/// SHA-256. `progress(done, total)` runs about every 100 ms. On any problem
/// the partial file is removed and nothing is left at `dest`.
pub fn download(a: &Asset, dest: &Path, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    let part = dest.with_extension("part");
    let result = (|| {
        let mut r = get(&a.url, "application/octet-stream")?;
        if r.length.is_some_and(|l| l != a.size) {
            return Err(format!(
                "size {} instead of {}",
                r.length.unwrap_or(0),
                a.size
            ));
        }
        let mut f = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let mut hash = Sha256::new()?;
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        let mut last = Instant::now();
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > a.size {
                return Err("larger than announced".into());
            }
            hash.update(&buf[..n])?;
            f.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
            if last.elapsed() >= Duration::from_millis(100) {
                progress(done, a.size);
                last = Instant::now();
            }
        }
        f.sync_all().map_err(|e| format!("write: {e}"))?;
        drop(f);
        progress(done, a.size);
        if done != a.size {
            return Err(format!("got {done} of {} bytes", a.size));
        }
        let got = hash.hex()?;
        if got != a.sha256 {
            return Err(format!("SHA-256 mismatch ({got})"));
        }
        let _ = std::fs::remove_file(dest);
        std::fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Where an update for `kind` is downloaded.
pub fn download_path(kind: Kind, a: &Asset) -> Result<PathBuf, String> {
    match kind {
        Kind::Portable => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let dir = exe.parent().ok_or("no exe folder")?;
            Ok(dir.join(&a.name))
        }
        _ => {
            let dir = std::env::temp_dir().join(format!("{}-update", zenlink_core::DATA_DIR_NAME));
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(dir.join(&a.name))
        }
    }
}

/// Starts the downloaded file. `replace`: an update of this edition (the
/// caller quits right after); otherwise the other edition (beta/stable),
/// which installs next to this one, so its Setup runs normally and a
/// portable exe just starts.
pub fn launch(kind: Kind, file: &Path, replace: bool) -> Result<(), String> {
    use std::process::Command;
    let mut c = Command::new(file);
    match kind {
        _ if !replace => {}
        Kind::Installed => {
            // Same AppId: upgrades in place and keeps the settings. The
            // firewall task is left out so the update asks for no admin
            // rights (the rule from the first install stays).
            c.args([
                "/SILENT",
                "/SP-",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/MERGETASKS=!firewall",
                "/ZLRELAUNCH=1",
            ]);
        }
        Kind::Portable => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            if exe == file {
                return Err("already this file".into());
            }
            c.arg(format!("{REPLACE_ARG}{}", exe.display()));
        }
        Kind::Source => return Err("source checkout".into()),
        Kind::Store => return Err("the Microsoft Store installs updates".into()),
    }
    c.spawn()
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", file.display()))
}

/// The old exe a portable update may delete: an `.exe` in this exe's own
/// folder, not this exe.
fn replace_target(arg: &str) -> Option<PathBuf> {
    let old = PathBuf::from(arg.strip_prefix(REPLACE_ARG)?);
    let me = std::env::current_exe().ok()?;
    let ok = old
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        && old.parent() == me.parent()
        && old != me;
    ok.then_some(old)
}

/// Run first in `main`: after a portable update, wait (at most 15 s) until
/// the old ZenLink has exited and its exe can be deleted, so the
/// single-instance check does not find it.
pub fn finish_portable_update() {
    let Some(old) = std::env::args().find_map(|a| replace_target(&a)) else {
        return;
    };
    let until = Instant::now() + Duration::from_secs(15);
    while old.exists() && Instant::now() < until {
        if std::fs::remove_file(&old).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if old.exists() {
        log::warn!("update: could not remove {}", old.display());
    } else {
        log::info!("update: removed {}", old.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_values() {
        assert_eq!(
            sha256_hex(b"").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn only_https_urls_with_a_plain_host() {
        assert_eq!(
            split_url("https://api.github.com/repos/x?y=1"),
            Some(("api.github.com", "/repos/x?y=1"))
        );
        assert_eq!(split_url("https://github.com"), Some(("github.com", "/")));
        assert!(split_url("http://github.com/x").is_none());
        assert!(split_url("https://user@evil/x").is_none());
        assert!(split_url("https://host:8443/x").is_none());
    }

    #[test]
    fn replace_only_an_exe_next_to_this_one() {
        let me = std::env::current_exe().unwrap();
        let dir = me.parent().unwrap();
        let other = dir.join("ZenLink-old.exe");
        assert_eq!(
            replace_target(&format!("{REPLACE_ARG}{}", other.display())),
            Some(other)
        );
        assert!(replace_target(&format!("{REPLACE_ARG}{}", me.display())).is_none());
        assert!(replace_target(&format!(
            "{REPLACE_ARG}{}",
            dir.join("settings.json").display()
        ))
        .is_none());
        assert!(replace_target(&format!("{REPLACE_ARG}C:\\Windows\\notepad.exe")).is_none());
        assert!(replace_target("--other").is_none());
    }

    #[test]
    fn a_bad_download_leaves_nothing() {
        let dir = std::env::temp_dir().join("zl-update-test");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("x.exe");
        let a = Asset {
            name: "x.exe".into(),
            url: "http://example.invalid/x.exe".into(),
            size: 1,
            sha256: "00".repeat(32),
        };
        assert!(download(&a, &dest, &|_, _| {}).is_err());
        assert!(!dest.exists());
        assert!(!dest.with_extension("part").exists());
    }

    /// Real network: `cargo test --release -p zenlink-app -- --ignored
    /// live_download`. Fetches the GitHub feed and downloads the v2.5.0
    /// Portable.exe through WinHTTP (GitHub redirects it to its asset
    /// host), checking size and SHA-256. The file is deleted, never run.
    #[test]
    #[ignore]
    fn live_download() {
        let list = feed().unwrap();
        let rel = list
            .as_array()
            .unwrap()
            .iter()
            .filter_map(update::release)
            .find(|r| r.tag == "v2.5.0")
            .expect("v2.5.0 in the feed");
        let a = rel.portable.expect("v2.5.0 Portable.exe with a digest");
        let dir = std::env::temp_dir().join("zl-update-live");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join(&a.name);
        let t = Instant::now();
        download(&a, &dest, &|_, _| {}).unwrap();
        let len = std::fs::metadata(&dest).unwrap().len();
        eprintln!("{} bytes in {:?}, sha256 {}", len, t.elapsed(), a.sha256);
        assert_eq!(len, a.size);
        std::fs::remove_file(&dest).unwrap();
        // A wrong digest is refused and leaves nothing behind.
        let bad = Asset {
            sha256: "00".repeat(32),
            ..a
        };
        assert!(download(&bad, &dest, &|_, _| {})
            .unwrap_err()
            .contains("SHA-256"));
        assert!(!dest.exists());
    }
}
}

#[cfg(windows)]
pub use win::*;

#[cfg(not(windows))]
pub fn check(_channel: Channel) -> Result<Offer, String> {
    Err("Updates are managed via your package manager on Linux".into())
}

#[cfg(not(windows))]
pub fn asset_for(_kind: Kind, _r: &Release) -> Option<&Asset> {
    None
}

#[cfg(not(windows))]
pub fn sha256_hex(_data: &[u8]) -> Result<String, String> {
    Err("Not implemented on Linux".into())
}

#[cfg(not(windows))]
pub fn download(_a: &Asset, _dest: &Path, _progress: &dyn Fn(u64, u64)) -> Result<(), String> {
    Err("Not implemented on Linux".into())
}

#[cfg(not(windows))]
pub fn download_path(_kind: Kind, _a: &Asset) -> Result<PathBuf, String> {
    Err("Not implemented on Linux".into())
}

#[cfg(not(windows))]
pub fn launch(_kind: Kind, _file: &Path, _replace: bool) -> Result<(), String> {
    Err("Not implemented on Linux".into())
}

#[cfg(not(windows))]
pub fn finish_portable_update() {}
