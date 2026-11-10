//! B-15: a phone is plugged in, but ADB does not see it. Windows lists such a
//! phone as a portable device (MTP / PTP, class WPD) with no ADB interface.

#[cfg(windows)]
mod imp {
    use windows::core::GUID;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
        SetupDiGetDeviceInstanceIdW, DIGCF_PRESENT, SP_DEVINFO_DATA,
    };

    const WPD_CLASS: GUID = GUID::from_u128(0xeec5ad98_8080_425f_922a_dabf3de3f69a);

    pub fn phone_without_adb() -> bool {
        instance_ids().iter().any(|id| is_android_candidate(id))
    }

    fn instance_ids() -> Vec<String> {
        let mut out = Vec::new();
        unsafe {
            let Ok(set) = SetupDiGetClassDevsW(Some(&WPD_CLASS), None, None, DIGCF_PRESENT) else {
                return out;
            };
            let mut i = 0;
            loop {
                let mut info = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(set, i, &mut info).is_err() {
                    break;
                }
                let mut buf = [0u16; 512];
                if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut buf), None).is_ok() {
                    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    out.push(String::from_utf16_lossy(&buf[..n]));
                }
                i += 1;
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
        out
    }
}

pub fn is_android_candidate(instance_id: &str) -> bool {
    let id = instance_id.to_ascii_uppercase();
    id.starts_with("USB\\") && !id.contains("VID_05AC")
}

#[cfg(windows)]
pub use imp::phone_without_adb;

#[cfg(not(windows))]
pub fn phone_without_adb() -> bool {
    false
}
