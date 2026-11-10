//! AAC / AAC-ELD decoding through zlaac.dll.

use std::path::{Path, PathBuf};

pub const PLAYFAIR: &[u8] = include_bytes!("../../../ZenshiroDev/resources/windows/native/playfair/playfair.dll");

pub fn find_native(sub: &str, name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let source = std::env::var_os("ZENLINK_SOURCE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = exe.parent().map(Path::to_path_buf).into_iter().collect();
    dirs.extend(
        source
            .as_ref()
            .map(|s| s.join(format!("ZenshiroDev/resources/windows/native/{sub}"))),
    );
    dirs.extend(
        source
            .as_ref()
            .map(|s| s.join(format!("resources/windows/native/{sub}"))),
    );
    dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
}

pub fn unpack_embedded(stem: &str, dll: &[u8]) -> Option<PathBuf> {
    let dir = zenlink_core::data_dir().join("native");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{stem}-{}.dll", env!("CARGO_PKG_VERSION")));
    if path.is_file() {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() == dll.len() as u64 {
                return Some(path);
            }
        }
    }
    let tmp = dir.join(format!("{stem}-{}.tmp", std::process::id()));
    std::fs::write(&tmp, dll).ok()?;
    let _ = std::fs::rename(&tmp, &path);
    Some(path)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::{s, HSTRING, PCWSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    type Open = unsafe extern "C" fn(*const u8, i32, i32, i32, i32) -> *mut std::ffi::c_void;
    type Decode = unsafe extern "C" fn(*mut std::ffi::c_void, *const u8, i32, *mut i16, i32) -> i32;
    type Close = unsafe extern "C" fn(*mut std::ffi::c_void);

    const CAP: usize = 8192;
    const ZLAAC: &[u8] = include_bytes!("../../../ZenshiroDev/resources/windows/native/zlaac/zlaac.dll");

    pub struct Aac {
        h: *mut std::ffi::c_void,
        decode: Decode,
        close: Close,
        buf: Vec<i16>,
    }

    unsafe impl Send for Aac {}

    fn find_dll() -> Option<PathBuf> {
        find_native("zlaac", "zlaac.dll").or_else(|| unpack_embedded("zlaac", ZLAAC))
    }

    impl Aac {
        pub fn new(asc: &[u8], rate: i32) -> Result<Self, String> {
            Self::with_dll(&find_dll().ok_or("zlaac.dll not found")?, asc, rate)
        }

        pub fn with_dll(dll: &Path, asc: &[u8], rate: i32) -> Result<Self, String> {
            unsafe {
                let lib = LoadLibraryW(PCWSTR(HSTRING::from(dll.as_os_str()).as_ptr()))
                    .map_err(|e| format!("load {}: {e}", dll.display()))?;
                let open: Open =
                    std::mem::transmute(GetProcAddress(lib, s!("zl_aac_open")).ok_or("zl_aac_open")?);
                let decode: Decode = std::mem::transmute(
                    GetProcAddress(lib, s!("zl_aac_decode")).ok_or("zl_aac_decode")?,
                );
                let close: Close =
                    std::mem::transmute(GetProcAddress(lib, s!("zl_aac_close")).ok_or("zl_aac_close")?);
                let h = open(asc.as_ptr(), asc.len() as i32, rate, 2, 48000);
                if h.is_null() {
                    return Err("AAC decoder did not open (bad codec config)".into());
                }
                Ok(Self {
                    h,
                    decode,
                    close,
                    buf: vec![0; CAP * 2],
                })
            }
        }

        pub fn decode(&mut self, au: &[u8]) -> &[u8] {
            if au.is_empty() {
                return &[];
            }
            let n = unsafe {
                (self.decode)(
                    self.h,
                    au.as_ptr(),
                    au.len() as i32,
                    self.buf.as_mut_ptr(),
                    CAP as i32,
                )
            };
            let n = (n.max(0) as usize).min(CAP);
            unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast::<u8>(), n * 4) }
        }
    }

    impl Drop for Aac {
        fn drop(&mut self) {
            unsafe { (self.close)(self.h) }
        }
    }
}

#[cfg(windows)]
pub use imp::Aac;

#[cfg(not(windows))]
pub struct Aac;

#[cfg(not(windows))]
impl Aac {
    pub fn new(_asc: &[u8], _rate: i32) -> Result<Self, String> {
        Err("AAC DLL is Windows only".into())
    }
    pub fn with_dll(_dll: &Path, _asc: &[u8], _rate: i32) -> Result<Self, String> {
        Err("AAC DLL is Windows only".into())
    }
    pub fn decode(&mut self, _au: &[u8]) -> &[u8] {
        &[]
    }
}
