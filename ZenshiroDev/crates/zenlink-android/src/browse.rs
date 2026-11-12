//! Device file manager (v2.6): browse the phone's shared storage and copy
//! files both ways with progress and cancel.
//!
//! Everything goes through the ADB **sync** protocol of the official `adb`
//! server (AOSP `packages/modules/adb/SYNC.TXT`), the same one `adb ls`,
//! `adb pull` and `adb push` use; nothing is installed on the phone:
//!
//! - `LIST <path>` -> `DENT` (mode, size, mtime, name) ... `DONE`;
//! - `RECV <path>` -> `DATA` chunks ... `DONE`, or `FAIL <message>`;
//! - `SEND <path>,<mode>` + `DATA` ... `DONE <mtime>` -> `OKAY` / `FAIL`.
//!
//! Folder changes (new folder, delete) use `shell:` with every path
//! quoted as one word. Version-1 sync messages carry sizes as 32 bits, so a
//! listed file over 4 GiB shows a wrong size; the copy itself is not
//! limited (it streams until `DONE`).
//!
//! A copy runs on its own sync socket and does not disturb mirroring. Cancel
//! is cooperative: the loop checks the flag between chunks, then the socket
//! is dropped (the protocol has no abort) and the half-written file is
//! removed.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::adb::{sync_stat, AdbClient, AdbError, Result, Target};
use crate::files::{free_name, mtime, safe_name, shell_quote};

/// Largest DATA chunk the device sends or accepts (`SYNC_DATA_MAX`).
const SYNC_DATA_MAX: usize = 64 * 1024;

/// Folders offered as shortcuts; each exists on a normal phone (`/sdcard`
/// is the user's shared storage, the one the Files app shows).
pub const QUICK_DIRS: &[(&str, &str)] = &[
    ("Internal storage", "/sdcard"),
    ("Camera", "/sdcard/DCIM/Camera"),
    ("Download", "/sdcard/Download"),
    ("Pictures", "/sdcard/Pictures"),
    ("Movies", "/sdcard/Movies"),
    ("Documents", "/sdcard/Documents"),
    ("Music", "/sdcard/Music"),
];

/// One entry of a folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Unix mode bits (`0o040000` folder, `0o100000` file, `0o120000` link).
    pub mode: u32,
    pub size: u64,
    /// Seconds since 1970.
    pub mtime: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
    pub fn is_link(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

/// Why a copy ended early.
#[derive(Debug)]
pub enum CopyError {
    Cancelled,
    Adb(AdbError),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Adb(e) => e.fmt(f),
        }
    }
}

impl From<AdbError> for CopyError {
    fn from(e: AdbError) -> Self {
        Self::Adb(e)
    }
}

impl From<io::Error> for CopyError {
    fn from(e: io::Error) -> Self {
        Self::Adb(AdbError::Io(e))
    }
}

fn request(cmd: &[u8; 4], arg: &str) -> Result<Vec<u8>> {
    if arg.len() > 1024 {
        return Err(AdbError::Protocol("remote path too long".into()));
    }
    let mut r = Vec::with_capacity(8 + arg.len());
    r.extend_from_slice(cmd);
    r.extend_from_slice(&(arg.len() as u32).to_le_bytes());
    r.extend_from_slice(arg.as_bytes());
    Ok(r)
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn read_fail<S: Read>(s: &mut S, len: usize) -> AdbError {
    let mut msg = vec![0u8; len.min(0xFFFF)];
    match s.read_exact(&mut msg) {
        Ok(()) => AdbError::Fail(String::from_utf8_lossy(&msg).into_owned()),
        Err(e) => AdbError::Io(e),
    }
}

/// `LIST` on an open sync socket: the folder's entries without `.` and `..`.
pub fn sync_list<S: Read + Write>(s: &mut S, path: &str) -> Result<Vec<Entry>> {
    s.write_all(&request(b"LIST", path)?)?;
    let mut out = Vec::new();
    loop {
        let mut h = [0u8; 20];
        s.read_exact(&mut h)?;
        match &h[..4] {
            b"DENT" => {
                let len = u32_at(&h, 16) as usize;
                if len > 4096 {
                    return Err(AdbError::Protocol("sync: name too long".into()));
                }
                let mut name = vec![0u8; len];
                s.read_exact(&mut name)?;
                let name = String::from_utf8_lossy(&name).into_owned();
                if name != "." && name != ".." {
                    out.push(Entry {
                        name,
                        mode: u32_at(&h, 4),
                        size: u64::from(u32_at(&h, 8)),
                        mtime: u32_at(&h, 12),
                    });
                }
            }
            b"DONE" => return Ok(out),
            b"FAIL" => {
                // FAIL carries its length in the second word; 12 bytes of the
                // 20 already read belong to the message.
                let len = u32_at(&h, 4) as usize;
                let mut msg = h[8..20.min(8 + len)].to_vec();
                if len > 12 {
                    let mut rest = vec![0u8; (len - 12).min(0xFFFF)];
                    s.read_exact(&mut rest)?;
                    msg.extend_from_slice(&rest);
                }
                return Err(AdbError::Fail(String::from_utf8_lossy(&msg).into_owned()));
            }
            other => {
                return Err(AdbError::Protocol(format!(
                    "sync: expected DENT/DONE, got {:?}",
                    String::from_utf8_lossy(other)
                )))
            }
        }
    }
}

/// `RECV` on an open sync socket into `dst`, calling `progress(bytes)` after
/// every chunk. Returns the bytes received.
pub fn sync_recv_to<S: Read + Write>(
    s: &mut S,
    remote: &str,
    dst: &mut dyn Write,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> std::result::Result<u64, CopyError> {
    s.write_all(&request(b"RECV", remote)?)?;
    let mut buf = vec![0u8; SYNC_DATA_MAX];
    let mut got = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let mut h = [0u8; 8];
        s.read_exact(&mut h)?;
        let len = u32_at(&h, 4) as usize;
        match &h[..4] {
            b"DATA" => {
                if len > SYNC_DATA_MAX {
                    return Err(AdbError::Protocol("sync: chunk too large".into()).into());
                }
                s.read_exact(&mut buf[..len])?;
                dst.write_all(&buf[..len])?;
                got += len as u64;
                progress(got);
            }
            b"DONE" => return Ok(got),
            b"FAIL" => return Err(read_fail(s, len).into()),
            other => {
                return Err(AdbError::Protocol(format!(
                    "sync: expected DATA/DONE, got {:?}",
                    String::from_utf8_lossy(other)
                ))
                .into())
            }
        }
    }
}

/// `SEND` on an open sync socket from `src` (streamed in 64 KB chunks),
/// checking `cancel` between chunks.
pub fn sync_send_cancellable<S: Read + Write>(
    s: &mut S,
    remote: &str,
    mode: u32,
    src: &mut dyn Read,
    mtime: u32,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> std::result::Result<(), CopyError> {
    let spec = format!("{remote},{mode}");
    s.write_all(&request(b"SEND", &spec)?)?;
    let mut d = vec![0u8; 8 + SYNC_DATA_MAX];
    let mut sent = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let mut n = 0;
        while n < SYNC_DATA_MAX {
            match src.read(&mut d[8 + n..])? {
                0 => break,
                k => n += k,
            }
        }
        if n == 0 {
            break;
        }
        d[..4].copy_from_slice(b"DATA");
        d[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        s.write_all(&d[..8 + n])?;
        sent += n as u64;
        progress(sent);
        if n < SYNC_DATA_MAX {
            break;
        }
    }
    let mut done = Vec::with_capacity(8);
    done.extend_from_slice(b"DONE");
    done.extend_from_slice(&mtime.to_le_bytes());
    s.write_all(&done)?;
    s.flush()?;
    let mut reply = [0u8; 8];
    s.read_exact(&mut reply)?;
    match &reply[..4] {
        b"OKAY" => Ok(()),
        b"FAIL" => Err(read_fail(s, u32_at(&reply, 4) as usize).into()),
        other => Err(AdbError::Protocol(format!(
            "sync: expected OKAY/FAIL, got {:?}",
            String::from_utf8_lossy(other)
        ))
        .into()),
    }
}

/// `parent/name` on the phone.
pub fn join(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The parent folder, `None` at `/`.
pub fn parent(path: &str) -> Option<&str> {
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return None;
    }
    match p.rfind('/') {
        Some(0) => Some("/"),
        Some(i) => Some(&p[..i]),
        None => None,
    }
}

/// Folders first, then names, case-insensitive (like the phone's Files app).
pub fn sort_entries(v: &mut [Entry]) {
    v.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Lists a folder, sorted. Hidden entries (leading dot) stay unless `hidden`.
pub fn list(client: &AdbClient, target: &Target, path: &str, hidden: bool) -> Result<Vec<Entry>> {
    let mut s = client.open_sync(target)?;
    let mut v = sync_list(&mut s, path)?;
    let _ = s.write_all(b"QUIT\0\0\0\0");
    if !hidden {
        v.retain(|e| !e.name.starts_with('.'));
    }
    sort_entries(&mut v);
    Ok(v)
}

/// A local path that does not exist yet: `name`, `name (1)`, ...
pub fn free_local(dir: &Path, name: &str) -> PathBuf {
    let name = crate::files::windows_safe(safe_name(name).unwrap_or_else(|| "file".into()));
    let mut taken = |n: &str| -> std::result::Result<bool, AdbError> { Ok(dir.join(n).exists()) };
    let n = free_name(&name, &mut taken).unwrap_or(name);
    dir.join(n)
}

/// Copies one file from the phone into `local_dir` under a free name.
/// Writes to `<name>.part` first, so a cancelled or failed copy never
/// leaves a broken file with the real name.
pub fn pull(
    client: &AdbClient,
    target: &Target,
    remote: &str,
    local_dir: &Path,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> std::result::Result<PathBuf, CopyError> {
    let name = remote.rsplit('/').next().unwrap_or("file");
    let dest = free_local(local_dir, name);
    // A free name too: never truncate a file the user already has.
    let part = free_local(
        local_dir,
        &format!(
            "{}.part",
            dest.file_name().and_then(|n| n.to_str()).unwrap_or("file")
        ),
    );
    let result = (|| {
        let mut s = client.open_sync(target)?;
        let mut f = io::BufWriter::new(std::fs::File::create(&part)?);
        sync_recv_to(&mut s, remote, &mut f, progress, cancel)?;
        f.flush()?;
        drop(f);
        let _ = s.write_all(b"QUIT\0\0\0\0");
        std::fs::rename(&part, &dest)?;
        Ok(dest.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// Copies one local file into `remote_dir` on the phone under a free name,
/// then asks the media scanner to index it. A cancelled copy is removed.
pub fn push(
    client: &AdbClient,
    target: &Target,
    local: &Path,
    remote_dir: &str,
    progress: &mut dyn FnMut(u64),
    cancel: &AtomicBool,
) -> std::result::Result<String, CopyError> {
    let name = local
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(safe_name)
        .ok_or_else(|| AdbError::Protocol("unusable file name".into()))?;
    let mut f = std::fs::File::open(local)?;
    let mut s = client.open_sync(target)?;
    let name = free_name(&name, &mut |n| {
        Ok(sync_stat(&mut s, &join(remote_dir, n))?.is_some())
    })?;
    let remote = join(remote_dir, &name);
    let r = sync_send_cancellable(
        &mut s,
        &remote,
        0o100660,
        &mut f,
        mtime(local),
        progress,
        cancel,
    );
    if r.is_ok() {
        let _ = s.write_all(b"QUIT\0\0\0\0");
    }
    drop(s);
    match r {
        Ok(()) => {
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
        Err(e) => {
            let _ = client.shell(target, &format!("rm -f {}", shell_quote(&remote)), 1024);
            Err(e)
        }
    }
}

/// Runs a shell command that prints nothing on success; its output is the error.
fn quiet(client: &AdbClient, target: &Target, cmd: &str) -> Result<()> {
    let out = client.shell(target, &format!("{cmd} 2>&1"), 4096)?;
    let msg = String::from_utf8_lossy(&out).trim().to_string();
    if msg.is_empty() {
        Ok(())
    } else {
        Err(AdbError::Fail(msg))
    }
}

pub fn make_dir(client: &AdbClient, target: &Target, path: &str) -> Result<()> {
    quiet(client, target, &format!("mkdir -p {}", shell_quote(path)))
}

/// Deletes a file or a whole folder. Refuses anything outside `/sdcard`
/// and the shared-storage root itself.
pub fn delete(client: &AdbClient, target: &Target, path: &str) -> Result<()> {
    let p = path.trim_end_matches('/');
    let inside = ["/sdcard/", "/storage/emulated/0/"]
        .iter()
        .any(|root| p.starts_with(root) && p.len() > root.len());
    // "." and ".." segments or "//" could point back at the storage root itself.
    let odd = p
        .split('/')
        .skip(1)
        .any(|seg| seg.is_empty() || seg == "." || seg == "..");
    if !inside || odd {
        return Err(AdbError::Protocol(
            "only files in the phone's storage can be deleted".into(),
        ));
    }
    quiet(client, target, &format!("rm -rf {}", shell_quote(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sync socket stand-in: reads scripted bytes, records what is written.
    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Duplex {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: io::Cursor::new(input),
                output: Vec::new(),
            }
        }
    }
    impl Read for Duplex {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.input.read(b)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn dent(name: &str, mode: u32, size: u32, mtime: u32) -> Vec<u8> {
        let mut v = b"DENT".to_vec();
        for x in [mode, size, mtime, name.len() as u32] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn chunk(tag: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn list_parses_entries_and_skips_dots() {
        let mut input = Vec::new();
        input.extend(dent(".", 0o040771, 3452, 1));
        input.extend(dent("..", 0o040771, 3452, 1));
        input.extend(dent("DCIM", 0o040771, 3452, 1_700_000_000));
        input.extend(dent("song.mp3", 0o100660, 4_000_000, 1_700_000_100));
        input.extend(b"DONE".iter().copied().chain([0u8; 16]));
        let mut s = Duplex::new(input);
        let v = sync_list(&mut s, "/sdcard").unwrap();
        assert_eq!(&s.output[..8], b"LIST\x07\0\0\0");
        assert_eq!(&s.output[8..], b"/sdcard");
        assert_eq!(v.len(), 2);
        assert!(v[0].is_dir());
        assert_eq!(v[1].name, "song.mp3");
        assert_eq!(v[1].size, 4_000_000);
        assert!(!v[1].is_dir());
    }

    #[test]
    fn list_fail_carries_the_message() {
        let mut input = b"FAIL".to_vec();
        input.extend_from_slice(&17u32.to_le_bytes());
        input.extend_from_slice(b"permission denied");
        let mut s = Duplex::new(input);
        match sync_list(&mut s, "/data") {
            Err(AdbError::Fail(m)) => assert_eq!(m, "permission denied"),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn recv_streams_chunks_with_progress() {
        let a = vec![1u8; SYNC_DATA_MAX];
        let b = vec![2u8; 10];
        let mut input = chunk(b"DATA", &a);
        input.extend(chunk(b"DATA", &b));
        input.extend(chunk(b"DONE", &[]));
        let mut s = Duplex::new(input);
        let mut out = Vec::new();
        let mut seen = Vec::new();
        let n = sync_recv_to(
            &mut s,
            "/sdcard/x",
            &mut out,
            &mut |p| seen.push(p),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(n, SYNC_DATA_MAX as u64 + 10);
        assert_eq!(out.len(), n as usize);
        assert_eq!(seen, vec![SYNC_DATA_MAX as u64, n]);
        assert_eq!(&s.output[..4], b"RECV");
    }

    #[test]
    fn recv_fail_and_cancel() {
        let mut s = Duplex::new(chunk(b"FAIL", b"No such file or directory"));
        match sync_recv_to(
            &mut s,
            "/x",
            &mut Vec::new(),
            &mut |_| {},
            &AtomicBool::new(false),
        ) {
            Err(CopyError::Adb(AdbError::Fail(m))) => assert!(m.contains("No such file")),
            r => panic!("{r:?}"),
        }
        let mut s = Duplex::new(chunk(b"DATA", b"abc"));
        let r = sync_recv_to(
            &mut s,
            "/x",
            &mut Vec::new(),
            &mut |_| {},
            &AtomicBool::new(true),
        );
        assert!(matches!(r, Err(CopyError::Cancelled)));
    }

    #[test]
    fn send_cancel_stops_before_data() {
        let mut s = Duplex::new(Vec::new());
        let r = sync_send_cancellable(
            &mut s,
            "/sdcard/Download/a.bin",
            0o100660,
            &mut io::Cursor::new(vec![0u8; 200_000]),
            0,
            &mut |_| {},
            &AtomicBool::new(true),
        );
        assert!(matches!(r, Err(CopyError::Cancelled)));
        assert!(!s.output.windows(4).any(|w| w == b"DATA"));
    }

    #[test]
    fn send_streams_and_reads_okay() {
        let mut s = Duplex::new(b"OKAY\0\0\0\0".to_vec());
        let mut last = 0;
        sync_send_cancellable(
            &mut s,
            "/sdcard/a",
            0o100660,
            &mut io::Cursor::new(vec![7u8; 70_000]),
            5,
            &mut |p| last = p,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(last, 70_000);
        assert!(s.output.ends_with(b"DONE\x05\0\0\0"));
    }

    #[test]
    fn paths_and_sorting() {
        assert_eq!(join("/sdcard", "a"), "/sdcard/a");
        assert_eq!(join("/", "sdcard"), "/sdcard");
        assert_eq!(parent("/sdcard/DCIM/"), Some("/sdcard"));
        assert_eq!(parent("/sdcard"), Some("/"));
        assert_eq!(parent("/"), None);
        let f = |n: &str, d: bool| Entry {
            name: n.into(),
            mode: if d { 0o040000 } else { 0o100000 },
            size: 0,
            mtime: 0,
        };
        let mut v = vec![
            f("b.txt", false),
            f("Zed", true),
            f("a.txt", false),
            f("alpha", true),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zed", "a.txt", "b.txt"]);
    }

    #[test]
    fn delete_refuses_outside_storage() {
        let c = AdbClient::new(1);
        let t = Target::Serial("x");
        for p in [
            "/",
            "/sdcard",
            "/sdcard/",
            "/system/app",
            "/sdcard/../data",
            "/sdcard/a/..",
        ] {
            assert!(
                matches!(delete(&c, &t, p), Err(AdbError::Protocol(_))),
                "{p}"
            );
        }
    }

    #[test]
    fn free_local_avoids_existing_files() {
        let dir = std::env::temp_dir().join(format!("zl-browse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        assert_eq!(free_local(&dir, "a.jpg"), dir.join("a (1).jpg"));
        assert_eq!(free_local(&dir, "b.jpg"), dir.join("b.jpg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
