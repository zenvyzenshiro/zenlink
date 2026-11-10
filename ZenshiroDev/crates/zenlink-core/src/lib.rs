//! Shared pieces of the ZenLink window: the backend API (served in this process since the
//! Python sidecar was removed), decoder records, data folders and updates.

use std::path::{Path, PathBuf};

pub mod api;
pub mod nal;
pub mod source;
pub mod stream;
pub mod update;

/// Product name for window titles and messages.
pub const APP_NAME: &str = "ZenLink";

/// Folder name under %APPDATA% and %LOCALAPPDATA% for prefs, settings and the
/// native DLLs. The stable build uses "ZenLink"; the beta never touches it.
pub const DATA_DIR_NAME: &str = "ZenLink-Next";

/// Window log file, in `log_dir()`. The stable build writes zenlink-next.log.
pub const LOG_FILE_NAME: &str = "zenlink-next-v25.log";

/// All logs (window, app windows, backend, crashes):
/// `%LOCALAPPDATA%\ZenLink-Next\logs`, %TEMP% when that is unavailable.
/// The backend (zenlink/paths.py LOG_DIR) uses the same folder.
pub fn log_dir() -> PathBuf {
    local_data_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(std::env::temp_dir)
}

/// The beta's data folder inside a base folder such as %APPDATA% or %LOCALAPPDATA%.
pub fn data_dir_in(base: &Path) -> PathBuf {
    base.join(DATA_DIR_NAME)
}

/// The app's data folder.
pub fn data_dir() -> PathBuf {
    local_data_dir().unwrap_or_else(std::env::temp_dir)
}

/// `%LOCALAPPDATA%\ZenLink-Next`, when LOCALAPPDATA is set, or XDG on Linux.
pub fn local_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|b| data_dir_in(Path::new(&b)))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|b| data_dir_in(&b))
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;

    #[test]
    fn beta_names_carry_beta() {
        assert_eq!(APP_NAME, "ZenLink");
        assert!(DATA_DIR_NAME.contains("Next"));
        assert!(LOG_FILE_NAME.contains("next-v25"));
        assert_ne!(LOG_FILE_NAME, "zenlink-next.log");
    }

    #[test]
    fn data_dir_is_beta_folder() {
        let base = Path::new("base");
        assert_eq!(data_dir_in(base), base.join("ZenLink-Next"));
        assert_ne!(data_dir_in(base), base.join("ZenLink"));
    }
}

/// Asks Windows to ACK every TCP segment at once on a socket that receives video.
/// Windows holds ACKs up to 200 ms; a sender with Nagle on (iOS) then holds its next small
/// frame until the ACK arrives, so a still or slow-moving screen comes in bursts with gaps of
/// several hundred ms and looks choppy although the frame rate is fine.
#[cfg(windows)]
pub fn quick_ack(s: &impl std::os::windows::io::AsRawSocket) {
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_TCP_SET_ACK_FREQUENCY, SOCKET};
    let freq: u32 = 1;
    let mut n = 0u32;
    // SAFETY: a valid socket handle and a 4-byte input buffer that outlives the call.
    let r = unsafe {
        WSAIoctl(
            SOCKET(s.as_raw_socket() as usize),
            SIO_TCP_SET_ACK_FREQUENCY,
            Some(&freq as *const u32 as _),
            4,
            None,
            0,
            &mut n,
            None,
            None,
        )
    };
    if r != 0 {
        log::info!("quick ACK not available on this socket");
    }
}

#[cfg(not(windows))]
pub fn quick_ack<T>(_: &T) {}
