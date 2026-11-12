//! Files dropped on the window (v2.5.1): an `.apk` is installed, anything
//! else is copied to the phone's Download folder.
//!
//! Both use the ADB sync protocol through the official `adb` server (no
//! extra tool): the file streams from disk in 64 KB chunks. An APK goes to
//! `/data/local/tmp` first and `pm install -r` installs it from there, like
//! `adb install` without streaming; nothing else is changed on the phone
//! (no `-t`, `-d` or `-g`: test, downgrade and auto-granted permissions stay
//! Android's decision).

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use crate::adb::{sync_send_from, sync_stat, AdbClient, AdbError, Target};

/// Where other files land: the shared Download folder (Files app,
/// gallery after the media scan).
pub const DOWNLOAD_DIR: &str = "/sdcard/Download";
/// Temporary place of an APK while `pm install` runs.
pub const APK_TMP: &str = "/data/local/tmp/zenlink-install.apk";
/// `pm install` of a large app can take a while.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);

/// What a dropped file becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Install,
    Copy,
}

pub fn kind(path: &Path) -> Kind {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("apk"))
    {
        Kind::Install
    } else {
        Kind::Copy
    }
}

/// `name` (already [`safe_name`]) usable on Windows: `CON`, `nul.txt`, `COM1` and the other
/// device names get a leading `_`, or Windows would open the device instead of a file.
pub fn windows_safe(name: String) -> String {
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    if DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        format!("_{name}")
    } else {
        name
    }
}

/// A file name that is safe on the phone's shared storage (FAT rules):
/// no folders, no control characters, no `"*/:<>?\|`, not `.` or `..`,
/// at most 200 bytes (the extension kept). `None` when nothing is left.
pub fn safe_name(name: &str) -> Option<String> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "\"*/:<>?\\|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() || clean.chars().all(|c| c == '.' || c == '_') {
        return None;
    }
    if clean.len() <= 200 {
        return Some(clean);
    }
    let (stem, ext) = split_ext(&clean);
    let mut stem = stem.to_string();
    while stem.len() + ext.len() > 200 {
        stem.pop();
    }
    Some(format!("{stem}{ext}"))
}

/// ("photo", ".jpg"); a leading dot is not an extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 10 => name.split_at(i),
        _ => (name, ""),
    }
}

/// `name`, or `name (1)`, `name (2)` … before the extension, the first that
/// `exists` says is free (at most 999 tries).
pub fn free_name(
    name: &str,
    exists: &mut dyn FnMut(&str) -> Result<bool, AdbError>,
) -> Result<String, AdbError> {
    if !exists(name)? {
        return Ok(name.to_string());
    }
    let (stem, ext) = split_ext(name);
    for n in 1..1000 {
        let candidate = format!("{stem} ({n}){ext}");
        if !exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(AdbError::Protocol("no free file name".into()))
}

/// A string as one shell word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Modification time in Unix seconds (0 when unknown), for sync `DONE`.
pub fn mtime(path: &Path) -> u32 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as u32)
}

/// Copies `local` to the Download folder under a free name and asks the
/// media scanner to index it. Returns the name used.
pub fn copy_to_download(
    client: &AdbClient,
    target: &Target,
    local: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<String, AdbError> {
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(safe_name)
        .ok_or_else(|| AdbError::Protocol("unusable file name".into()))?;
    let mut f = std::fs::File::open(local)?;
    let mut s = client.open_sync(target)?;
    let name = free_name(&name, &mut |n| {
        Ok(sync_stat(&mut s, &format!("{DOWNLOAD_DIR}/{n}"))?.is_some())
    })?;
    let remote = format!("{DOWNLOAD_DIR}/{name}");
    sync_send_from(&mut s, &remote, 0o100660, &mut f, mtime(local), progress)?;
    drop(s);
    // Gallery and Files see it at once (otherwise after the next scan).
    let _ = client.shell(
        target,
        &format!(
            "am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE -d {} >/dev/null 2>&1",
            shell_quote(&format!("file://{remote}"))
        ),
        4096,
    );
    Ok(name)
}

/// What `pm install` answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Ok,
    /// `Failure [INSTALL_FAILED_…: …]`: the reason, as Android wrote it.
    Failed(String),
}

/// Reads `pm install` output.
pub fn parse_install(out: &str) -> Installed {
    if out.lines().any(|l| l.trim() == "Success") {
        return Installed::Ok;
    }
    let reason = out
        .lines()
        .map(str::trim)
        .find_map(|l| {
            l.strip_prefix("Failure ")
                .or_else(|| l.strip_prefix("Error: "))
        })
        .map(|r| r.trim_matches(['[', ']']).to_string())
        .unwrap_or_else(|| out.trim().chars().take(200).collect());
    Installed::Failed(reason)
}

/// Installs (or updates) the APK at `local`. `progress` counts the copy.
pub fn install_apk(
    client: &AdbClient,
    target: &Target,
    local: &Path,
    progress: &mut dyn FnMut(u64),
) -> Result<Installed, AdbError> {
    let mut f = std::fs::File::open(local)?;
    let mut head = [0u8; 2];
    f.read_exact(&mut head)?;
    if &head != b"PK" {
        return Ok(Installed::Failed("not an APK (zip) file".into()));
    }
    let mut f = std::fs::File::open(local)?;
    {
        let mut s = client.open_sync(target)?;
        sync_send_from(&mut s, APK_TMP, 0o100644, &mut f, mtime(local), progress)?;
    }
    let slow = AdbClient {
        timeout: client.timeout.max(INSTALL_TIMEOUT),
        ..client.clone()
    };
    let out = slow.shell(target, &format!("pm install -r {APK_TMP} 2>&1"), 64 * 1024);
    let _ = client.shell(target, &format!("rm -f {APK_TMP}"), 4096);
    Ok(parse_install(&String::from_utf8_lossy(&out?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_or_copy() {
        assert_eq!(kind(Path::new(r"C:\x\WhatsApp.APK")), Kind::Install);
        assert_eq!(kind(Path::new(r"C:\x\photo.jpg")), Kind::Copy);
        assert_eq!(kind(Path::new(r"C:\x\apk")), Kind::Copy);
    }

    #[test]
    fn windows_device_names_get_a_prefix() {
        assert_eq!(windows_safe("CON".into()), "_CON");
        assert_eq!(windows_safe("nul.txt".into()), "_nul.txt");
        assert_eq!(windows_safe("Com1.tar.gz".into()), "_Com1.tar.gz");
        assert_eq!(windows_safe("console.log".into()), "console.log");
        assert_eq!(windows_safe("COM10".into()), "COM10");
    }

    #[test]
    fn names_are_safe_on_the_phone() {
        assert_eq!(
            safe_name("Foto liburan.jpg").as_deref(),
            Some("Foto liburan.jpg")
        );
        assert_eq!(safe_name("a:b*c?.txt").as_deref(), Some("a_b_c_.txt"));
        assert_eq!(
            safe_name("../../etc/passwd").as_deref(),
            Some(".._.._etc_passwd")
        );
        assert_eq!(safe_name("x\n.txt").as_deref(), Some("x_.txt"));
        assert_eq!(safe_name("name. ").as_deref(), Some("name"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("  "), None);
        let long = format!("{}.mp4", "é".repeat(150));
        let s = safe_name(&long).unwrap();
        assert!(s.len() <= 200 && s.ends_with(".mp4"), "{s}");
    }

    #[test]
    fn free_names_count_up_before_the_extension() {
        let taken = ["a.jpg", "a (1).jpg", "notes"];
        let mut exists = |n: &str| Ok(taken.contains(&n));
        assert_eq!(free_name("a.jpg", &mut exists).unwrap(), "a (2).jpg");
        assert_eq!(free_name("b.jpg", &mut exists).unwrap(), "b.jpg");
        assert_eq!(free_name("notes", &mut exists).unwrap(), "notes (1)");
        assert_eq!(
            free_name(".bashrc", &mut |n| Ok(n == ".bashrc")).unwrap(),
            ".bashrc (1)"
        );
    }

    #[test]
    fn quoting_keeps_one_word() {
        assert_eq!(
            shell_quote("file:///sdcard/Download/it's.jpg"),
            r"'file:///sdcard/Download/it'\''s.jpg'"
        );
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
    }

    #[test]
    fn install_results() {
        assert_eq!(
            parse_install("Performing Streamed Install\nSuccess\n"),
            Installed::Ok
        );
        assert_eq!(
            parse_install("Failure [INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected]\n"),
            Installed::Failed("INSTALL_FAILED_VERSION_DOWNGRADE: Downgrade detected".into())
        );
        assert_eq!(
            parse_install("Error: Unable to open file: /x\n"),
            Installed::Failed("Unable to open file: /x".into())
        );
    }

    #[test]
    fn streamed_send_matches_the_buffered_one() {
        struct Duplex {
            sent: Vec<u8>,
            reply: std::io::Cursor<Vec<u8>>,
        }
        impl Read for Duplex {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.reply.read(b)
            }
        }
        impl std::io::Write for Duplex {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.sent.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // A reader that returns 1000 bytes at a time: chunks are still 64 KB.
        struct Slow(std::io::Cursor<Vec<u8>>);
        impl Read for Slow {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let n = b.len().min(1000);
                self.0.read(&mut b[..n])
            }
        }
        let data: Vec<u8> = (0..150_000u32).map(|i| i as u8).collect();
        let mut a = Duplex {
            sent: Vec::new(),
            reply: std::io::Cursor::new(b"OKAY\0\0\0\0".to_vec()),
        };
        crate::adb::sync_send(&mut a, "/x", 0o100644, &data, 1).unwrap();
        let mut b = Duplex {
            sent: Vec::new(),
            reply: std::io::Cursor::new(b"OKAY\0\0\0\0".to_vec()),
        };
        let mut seen = Vec::new();
        sync_send_from(
            &mut b,
            "/x",
            0o100644,
            &mut Slow(std::io::Cursor::new(data)),
            1,
            &mut |n| seen.push(n),
        )
        .unwrap();
        assert_eq!(a.sent, b.sent);
        assert_eq!(seen, [65_536, 131_072, 150_000]);
        // STAT of a missing and an existing file.
        let mut c = Duplex {
            sent: Vec::new(),
            reply: std::io::Cursor::new(
                [
                    b"STAT".as_slice(),
                    &[0; 12],
                    b"STAT",
                    &0o100660u32.to_le_bytes(),
                    &[0; 8],
                ]
                .concat(),
            ),
        };
        assert_eq!(sync_stat(&mut c, "/sdcard/Download/a").unwrap(), None);
        assert_eq!(
            sync_stat(&mut c, "/sdcard/Download/b").unwrap(),
            Some(0o100660)
        );
        assert_eq!(&c.sent[..8], b"STAT\x12\0\0\0");
    }

    /// Real phone: `ZL_TEST_SERIAL=<serial> cargo test -p zenlink-android
    /// -- --ignored real_copy`. Copies a 200 KB file to Download twice (the
    /// second gets " (1)"), checks the sizes, then deletes both.
    #[test]
    #[ignore]
    fn real_copy() {
        let serial = std::env::var("ZL_TEST_SERIAL").expect("ZL_TEST_SERIAL");
        let target = Target::Serial(&serial);
        let client = AdbClient::default();
        let dir = std::env::temp_dir().join("zl-files-test");
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("zenlink drop test.bin");
        std::fs::write(&local, vec![42u8; 200_000]).unwrap();
        let t = std::time::Instant::now();
        let mut last = 0;
        let a = copy_to_download(&client, &target, &local, &mut |n| last = n).unwrap();
        let b = copy_to_download(&client, &target, &local, &mut |_| {}).unwrap();
        eprintln!("{a} / {b} in {:?}", t.elapsed());
        assert_eq!(last, 200_000);
        assert_eq!(a, "zenlink drop test.bin");
        assert_eq!(b, "zenlink drop test (1).bin");
        for n in [&a, &b] {
            let path = shell_quote(&format!("{DOWNLOAD_DIR}/{n}"));
            let size = client
                .shell(&target, &format!("stat -c %s {path}"), 64)
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&size).trim(), "200000");
            client.shell(&target, &format!("rm -f {path}"), 64).unwrap();
        }
    }
}
