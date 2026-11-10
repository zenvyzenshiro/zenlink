//! USB driver stack of connected iPhones/iPads (port of backend drivers/status.py and
//! drivers/manager.py).
//!
//! Reading changes nothing: device registry keys, the Configuration Manager API for presence,
//! the published INF files and the service registry. pnputil's text output is never parsed
//! (Windows translates it). A change runs only after the user confirmed it in Settings: then
//! `tools/usb_filter.ps1` runs elevated (UAC), backs up the driver state, applies the change,
//! verifies the device restarted and rolls back on failure.
//!
//! USB Direct state of a device: `replaced` (parent bound to libusb0: USB Direct works),
//! `apple` (Apple drivers only), `coexist` (libusb0 upper filter on usbccgp: UNSAFE, it crashed
//! Windows on an iPad; never used, removal offered), `unknown`.

pub const ACTIONS: [&str; 3] = ["bind", "restore", "removefilter"];

#[cfg(windows)]
mod imp {
use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS, CM_LOCATE_DEVNODE_NORMAL,
    CM_PROB, CR_SUCCESS, DN_STARTED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_READ, REG_MULTI_SZ, REG_VALUE_TYPE,
};

const NO_WINDOW: u32 = 0x0800_0000;

struct Key(HKEY);

impl Key {
    fn open(parent: HKEY, path: &str) -> Option<Self> {
        let mut h = HKEY::default();
        unsafe { RegOpenKeyExW(parent, &HSTRING::from(path), None, KEY_READ, &mut h) }
            .ok()
            .ok()?;
        Some(Self(h))
    }

    fn sub(&self, path: &str) -> Option<Self> {
        Self::open(self.0, path)
    }

    /// REG_SZ / REG_EXPAND_SZ as one string, REG_MULTI_SZ as several.
    fn strings(&self, name: &str) -> Vec<String> {
        let name = HSTRING::from(name);
        let (mut kind, mut size) = (REG_VALUE_TYPE::default(), 0u32);
        if unsafe { RegQueryValueExW(self.0, &name, None, Some(&mut kind), None, Some(&mut size)) }
            .is_err()
            || size == 0
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        if unsafe {
            RegQueryValueExW(
                self.0,
                &name,
                None,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .is_err()
        {
            return Vec::new();
        }
        buf.truncate(size as usize / 2);
        let all: Vec<String> = buf
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        if kind == REG_MULTI_SZ {
            all
        } else {
            all.into_iter().take(1).collect()
        }
    }

    fn value_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let r = unsafe {
                RegEnumValueW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if r.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }

    fn string(&self, name: &str) -> String {
        self.strings(name).into_iter().next().unwrap_or_default()
    }

    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            let r = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    Some(PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if r.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn windir() -> PathBuf {
    PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()))
}

/// "Started", "Problem (code N)" or "Stopped" for a present device node; None when absent.
fn devnode_status(instance: &str) -> Option<String> {
    let mut dev = 0u32;
    if unsafe { CM_Locate_DevNodeW(&mut dev, &HSTRING::from(instance), CM_LOCATE_DEVNODE_NORMAL) }
        != CR_SUCCESS
    {
        return None;
    }
    let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS::default(), CM_PROB::default());
    if unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, dev, 0) } != CR_SUCCESS {
        return Some("Unknown".into());
    }
    Some(if problem.0 != 0 {
        format!("Problem (code {})", problem.0)
    } else if status.0 & DN_STARTED.0 != 0 {
        "Started".into()
    } else {
        "Stopped".into()
    })
}

fn hex_pid(s: &str) -> bool {
    s.len() == 4 && s.starts_with("12") && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `VID_05AC&PID_12xx[&MI_nn]` -> (pid, interface).
fn parse_hwid(hwid: &str) -> Option<(String, Option<u8>)> {
    let up = hwid.to_uppercase();
    let rest = up.strip_prefix("VID_05AC&PID_")?;
    let (pid, mi) = rest.split_at(rest.len().min(4));
    if !hex_pid(pid) {
        return None;
    }
    match mi {
        "" => Some((pid.into(), None)),
        m => {
            let n = m.strip_prefix("&MI_").filter(|n| n.len() == 2)?;
            Some((pid.into(), Some(u8::from_str_radix(n, 16).ok()?)))
        }
    }
}

struct Node {
    hwid: String,
    serial: String,
    status: String,
}

fn connected_nodes() -> Vec<Node> {
    let Some(usb) = Key::open(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Enum\USB") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for hwid in usb
        .subkeys()
        .into_iter()
        .filter(|h| parse_hwid(h).is_some())
    {
        let Some(dev) = usb.sub(&hwid) else { continue };
        for serial in dev.subkeys() {
            if let Some(status) = devnode_status(&format!(r"USB\{hwid}\{serial}")) {
                out.push(Node {
                    hwid: hwid.clone(),
                    serial,
                    status,
                });
            }
        }
    }
    out
}

struct EnumInfo {
    service: String,
    upper: Vec<String>,
    lower: Vec<String>,
    desc: String,
}

fn enum_key(instance: &str) -> EnumInfo {
    let Some(k) = Key::open(
        HKEY_LOCAL_MACHINE,
        &format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"),
    ) else {
        return EnumInfo {
            service: String::new(),
            upper: vec![],
            lower: vec![],
            desc: String::new(),
        };
    };
    let mut desc = k.string("FriendlyName");
    if desc.is_empty() {
        desc = k.string("DeviceDesc");
    }
    EnumInfo {
        service: k.string("Service"),
        upper: k.strings("UpperFilters"),
        lower: k.strings("LowerFilters"),
        desc: desc.rsplit(';').next().unwrap_or("").to_string(),
    }
}

fn service_state(name: &str) -> (bool, bool) {
    if Key::open(
        HKEY_LOCAL_MACHINE,
        &format!(r"SYSTEM\CurrentControlSet\Services\{name}"),
    )
    .is_none()
    {
        return (false, false);
    }
    let running = Command::new("sc")
        .args(["query", name])
        .creation_flags(NO_WINDOW)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("RUNNING"));
    (true, running)
}

/// Portable.exe carries the driver tool and libusb-win32 (installed builds have them beside
/// ZenLink.exe), unpacked on first use. Paths keep the installed layout: tools\ next to drivers\.
#[cfg(feature = "portable")]
const KIT: &[(&str, &[u8])] = &[
    (
        "tools/usb_filter.ps1",
        include_bytes!("../../../ZenshiroDev/tooling/windows/tools/usb_filter.ps1"),
    ),
    (
        "drivers/libusb-win32/amd64/libusb0.sys",
        include_bytes!(
            "../../../ZenshiroDev/resources/windows/drivers/libusb-win32/amd64/libusb0.sys"
        ),
    ),
    (
        "drivers/libusb-win32/amd64/libusb0.dll",
        include_bytes!(
            "../../../ZenshiroDev/resources/windows/drivers/libusb-win32/amd64/libusb0.dll"
        ),
    ),
    (
        "drivers/libusb-win32/x86/libusb0_x86.dll",
        include_bytes!(
            "../../../ZenshiroDev/resources/windows/drivers/libusb-win32/x86/libusb0_x86.dll"
        ),
    ),
];
#[cfg(not(feature = "portable"))]
const KIT: &[(&str, &[u8])] = &[];

fn unpack_kit() -> Option<PathBuf> {
    let dir = zenlink_core::local_data_dir()?.join("driver-kit");
    for (name, data) in KIT {
        let p = dir.join(name);
        // Rewritten each time: what runs elevated is always this exe's copy.
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(&p, data).ok()?;
    }
    (!KIT.is_empty()).then_some(dir)
}

/// The app's read-only folders: beside ZenLink.exe (installed), the repository's (source
/// checkout), else the Portable.exe kit.
fn app_dir(repo: &[&str], frozen: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let beside = exe.parent().map(|d| d.join(frozen)).unwrap_or_default();
    if beside.exists() {
        return beside;
    }
    let source = std::env::var_os("ZENLINK_SOURCE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let in_repo = source
        .into_iter()
        .chain(exe.ancestors().skip(1).map(Path::to_path_buf))
        .map(|d| repo.iter().fold(d, |p, s| p.join(s)))
        .find(|p| p.exists());
    in_repo
        .or_else(|| unpack_kit().map(|k| k.join(frozen)))
        .unwrap_or(beside)
}

pub fn tools_dir() -> PathBuf {
    app_dir(&["ZenshiroDev", "tooling", "windows", "tools"], "tools")
}

fn bundled_libusb() -> PathBuf {
    app_dir(
        &["ZenshiroDev", "resources", "windows", "drivers"],
        "drivers",
    )
    .join("libusb-win32")
}

fn libusb0_status(packages: &BTreeMap<String, String>) -> Value {
    let (service, _) = service_state("libusb0");
    let sys32 = windir().join("System32");
    let sys = sys32.join("drivers").join("libusb0.sys").exists();
    let dll_path = sys32.join("libusb0.dll");
    let dll = dll_path.exists();
    let b = bundled_libusb();
    let bundled = [
        r"amd64\libusb0.sys",
        r"amd64\libusb0.dll",
        r"x86\libusb0_x86.dll",
    ]
    .iter()
    .all(|f| b.join(f).exists());
    let installed = service && sys && dll;
    json!({
        "service": service, "sys": sys, "dll": dll, "dllPath": dll_path,
        "bundled": bundled, "installed": installed, "available": installed || bundled,
        "zadig": true, "packages": packages,
    })
}

type InfCache = (Vec<(String, u64, u64)>, BTreeMap<String, String>);
static INF_CACHE: Mutex<Option<InfCache>> = Mutex::new(None);

/// Text without INF comments; INF files are ANSI/UTF-8 or UTF-16 (libwdi writes UTF-16).
fn inf_text(raw: &[u8]) -> String {
    let utf16 =
        raw.starts_with(&[0xFF, 0xFE]) || raw.starts_with(&[0xFE, 0xFF]) || raw.get(1) == Some(&0);
    let text = if utf16 {
        let be = raw.starts_with(&[0xFE, 0xFF]);
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| {
                if be {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    u16::from_le_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    text.lines()
        .map(|l| l.split(';').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// USB PIDs a libusb0 driver package covers (`AddService = libusb0` and `VID_05AC&PID_xxxx`).
fn inf_pids(text: &str) -> Vec<String> {
    let low = text.to_lowercase();
    let adds = low.lines().any(|l| {
        let l = l.replace([' ', '\t', '"'], "");
        l.strip_prefix("addservice=").is_some_and(|r| {
            r.strip_prefix("libusb0")
                .is_some_and(|t| !t.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        })
    });
    if !adds {
        return Vec::new();
    }
    let up = text.to_uppercase();
    let mut out = Vec::new();
    for (i, _) in up.match_indices("VID_05AC&PID_") {
        let pid = &up[i + 13..(i + 17).min(up.len())];
        let next = up[(i + 17).min(up.len())..].chars().next();
        if pid.len() == 4
            && pid.chars().all(|c| c.is_ascii_hexdigit())
            && !next.is_some_and(|c| c.is_ascii_hexdigit() || c == '&')
        {
            out.push(pid.to_string());
        }
    }
    out
}

/// libusb-win32 device-driver packages in the driver store, by PID. libwdi (TikTok LIVE
/// Studio, Zadig) makes one per model, so an iPad needs its own.
pub fn libusb0_packages() -> BTreeMap<String, String> {
    let inf = windir().join("INF");
    let mut entries: Vec<(String, u64, u64)> = std::fs::read_dir(&inf)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let low = name.to_lowercase();
                    let num = low.strip_prefix("oem")?.strip_suffix(".inf")?;
                    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    let m = e.metadata().ok()?;
                    let t = m
                        .modified()
                        .ok()?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_nanos() as u64;
                    Some((name, t, m.len()))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    // ~200 INF files: only read again when one was added, removed or rewritten.
    if let Some((k, found)) = INF_CACHE.lock().unwrap().as_ref() {
        if *k == entries {
            return found.clone();
        }
    }
    let mut found = BTreeMap::new();
    for (name, _, _) in &entries {
        if let Ok(raw) = std::fs::read(inf.join(name)) {
            for pid in inf_pids(&inf_text(&raw)) {
                found.entry(pid).or_insert_with(|| name.clone());
            }
        }
    }
    *INF_CACHE.lock().unwrap() = Some((entries, found.clone()));
    found
}

/// One entry per connected Apple composite device (the parent node).
pub fn device_states(packages: &BTreeMap<String, String>) -> Vec<Value> {
    let nodes = connected_nodes();
    let mut children: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for n in &nodes {
        if let Some((pid, Some(mi))) = parse_hwid(&n.hwid) {
            let info = enum_key(&format!(r"USB\{}\{}", n.hwid, n.serial));
            children.entry(pid).or_default().push(json!({
                "interface": mi, "service": info.service, "name": info.desc, "upper": info.upper, "status": n.status,
            }));
        }
    }
    let mut out = Vec::new();
    for n in &nodes {
        let Some((pid, None)) = parse_hwid(&n.hwid) else {
            continue;
        };
        let instance = format!(r"USB\{}\{}", n.hwid, n.serial);
        let info = enum_key(&instance);
        let service = info.service.to_lowercase();
        let has_filter = |u: &[String]| u.iter().any(|f| f.eq_ignore_ascii_case("libusb0"));
        let mode = match service.as_str() {
            "libusb0" => "replaced",
            "usbccgp" if has_filter(&info.upper) => "coexist",
            "usbccgp" => "apple",
            _ => "unknown",
        };
        let mut kids = children.get(&pid).cloned().unwrap_or_default();
        kids.sort_by_key(|c| c["interface"].as_u64());
        let stale: Vec<Value> = kids
            .iter()
            .filter(|c| {
                c["upper"].as_array().is_some_and(|u| {
                    u.iter().any(|f| {
                        f.as_str()
                            .is_some_and(|f| f.eq_ignore_ascii_case("libusb0"))
                    })
                })
            })
            .map(|c| c["interface"].clone())
            .collect();
        let family = match pid.as_str() {
            "12A8" => "iPhone",
            "12AB" => "iPad",
            _ if info.desc.to_lowercase().contains("ipad") => "iPad",
            _ => "iPhone",
        };
        out.push(json!({
            "instance": instance, "serial": n.serial, "pid": pid, "family": family, "status": n.status,
            "service": info.service, "upperFilters": info.upper, "lowerFilters": info.lower, "mode": mode,
            "interfaces": kids, "unsafeFilter": has_filter(&info.upper) || !stale.is_empty(), "staleChildFilters": stale,
            "usbDirectReady": mode == "replaced", "appleReady": mode == "apple", "libusbPackage": packages.get(&pid),
        }));
    }
    out
}

pub fn snapshot() -> Value {
    let packages = libusb0_packages();
    let (service, running) = service_state("Apple Mobile Device Service");
    json!({
        "apple": { "service": service, "running": running },
        "libusb0": libusb0_status(&packages),
        "devices": device_states(&packages),
    })
}

/// One elevated action on one device; blocks until it finishes. The result is the tool's
/// own report ({ok, message, rolledBack, backup}).
const FIREWALL_RULES: &str =
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";

/// True when Windows Firewall keeps devices out of this ZenLink.exe: an active inbound block rule
/// names it (a dismissed "Allow access" prompt leaves one), or no inbound allow rule does.
pub fn firewall_blocked() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Some(k) = Key::open(HKEY_LOCAL_MACHINE, FIREWALL_RULES) else {
        return false;
    };
    let rules: Vec<String> = k.value_names().iter().map(|n| k.string(n)).collect();
    blocked_by(&rules, &exe.to_string_lossy())
}

fn blocked_by(rules: &[String], exe: &str) -> bool {
    let app = format!("|app={}|", exe.to_lowercase());
    let mut allowed = false;
    for r in rules.iter().map(|r| r.to_lowercase()) {
        if !(r.contains("|dir=in|") && r.contains("|active=true|") && r.contains(&app)) {
            continue;
        }
        if r.contains("|action=block|") {
            return true;
        }
        allowed = true;
    }
    !allowed
}

/// Replaces this exe's inbound rules with one allow rule for the local network (the installer's
/// "ZenLink (AirPlay)" rule). Asks for administrator permission.
pub fn allow_network() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.display().to_string().replace('"', "");
    let cmd = format!(
        "/c netsh advfirewall firewall delete rule name=all dir=in program=\"{exe}\" & \
         netsh advfirewall firewall add rule name=\"ZenLink (AirPlay)\" dir=in action=allow \
         program=\"{exe}\" enable=yes profile=any remoteip=localsubnet"
    );
    let launcher = format!(
        "try {{ $p = Start-Process -FilePath cmd.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru \
         -ArgumentList '{}'; exit $p.ExitCode }} catch {{ exit 1223 }}",
        cmd.replace('\'', "''")
    );
    log::info!("firewall: allowing {exe} on the local network (elevated)");
    let code = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &launcher,
        ])
        .creation_flags(NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())?
        .code();
    match code {
        Some(0) if !firewall_blocked() => Ok(()),
        Some(1223) => Err("Administrator permission was not given. Nothing was changed.".into()),
        c => Err(format!(
            "Windows Firewall did not take the rule (exit {}).",
            c.unwrap_or(-1)
        )),
    }
}

pub fn run_action(action: &str, instance: &str) -> Result<Value, String> {
    if !ACTIONS.contains(&action) {
        return Err(format!("Unknown action: {action}"));
    }
    if !device_states(&BTreeMap::new())
        .iter()
        .any(|d| d["instance"] == instance)
    {
        return Err("That device is not connected anymore. Plug it in and try again.".into());
    }
    let script = tools_dir().join("usb_filter.ps1");
    let result_path = std::env::temp_dir().join(format!("zenlink-driver-{}.json", uuid_hex()));
    let args = format!(
        "-NoProfile -ExecutionPolicy Bypass -File \"{}\" -Action {action} -InstanceId \"{instance}\" -ResultPath \"{}\"",
        script.display(),
        result_path.display()
    );
    // Start-Process -Verb RunAs shows the UAC prompt; declining throws.
    let launcher = format!(
        "try {{ $p = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Normal -Wait -PassThru \
         -ArgumentList '{}'; exit $p.ExitCode }} catch {{ exit 1223 }}",
        args.replace('\'', "''")
    );
    log::info!("driver action {action} on {instance} (elevated)");
    let started = Instant::now();
    let code = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &launcher,
        ])
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?
        .status
        .code();
    if code == Some(1223) && !result_path.exists() {
        return Err("Administrator permission was not given. Nothing was changed.".into());
    }
    let read = std::fs::read(&result_path);
    let _ = std::fs::remove_file(&result_path);
    let mut result: Value = read
        .ok()
        .and_then(|b| serde_json::from_slice(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b)).ok())
        .ok_or_else(|| {
            format!(
                "The driver tool did not report a result (exit {}). Nothing is known to have changed.",
                code.unwrap_or(-1)
            )
        })?;
    result["seconds"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
    log::info!(
        "driver action result: ok={} message={}",
        result["ok"],
        result["message"]
    );
    Ok(result)
}

fn uuid_hex() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}{:x}", t.as_nanos(), std::process::id())
}
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
pub fn tools_dir() -> std::path::PathBuf {
    zenlink_core::data_dir().join("tools")
}

#[cfg(not(windows))]
pub fn libusb0_packages() -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::new()
}

#[cfg(not(windows))]
pub fn device_states(_packages: &std::collections::BTreeMap<String, String>) -> Vec<serde_json::Value> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn snapshot() -> serde_json::Value {
    serde_json::json!({
        "devices": [],
        "firewall_blocked": false,
        "packages": {}
    })
}

#[cfg(not(windows))]
pub fn firewall_blocked() -> bool {
    false
}

#[cfg(not(windows))]
pub fn allow_network() -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
pub fn run_action(_action: &str, _instance: &str) -> Result<serde_json::Value, String> {
    Err("Driver actions not applicable on Linux".into())
}

#[cfg(test)]
mod tests {

    #[test]
    fn firewall_rules_for_this_exe() {
        let exe = r"E:\ZenLink\ZenLink.exe";
        let allow =
            r"v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=e:\zenlink\zenlink.exe|Name=x|"
                .to_string();
        let block = r"v2.10|Action=Block|Active=TRUE|Dir=In|Protocol=6|Profile=Public|App=E:\zenlink\zenlink.exe|Name=x|".to_string();
        let other = r"v2.10|Action=Allow|Active=TRUE|Dir=In|App=C:\other.exe|Name=y|".to_string();
        let off = r"v2.10|Action=Block|Active=FALSE|Dir=In|App=E:\zenlink\zenlink.exe|Name=x|"
            .to_string();
        assert!(!blocked_by(&[allow.clone(), other.clone(), off], exe));
        assert!(blocked_by(&[allow, block], exe));
        assert!(blocked_by(&[other], exe)); // no rule: Windows keeps it out
    }

    use super::*;

    #[test]
    fn hardware_ids() {
        assert_eq!(parse_hwid("VID_05AC&PID_12A8"), Some(("12A8".into(), None)));
        assert_eq!(
            parse_hwid("vid_05ac&pid_12ab&mi_01"),
            Some(("12AB".into(), Some(1)))
        );
        assert_eq!(parse_hwid("VID_05AC&PID_8600"), None);
        assert_eq!(parse_hwid("VID_1234&PID_12A8"), None);
    }

    #[test]
    fn libusb_inf_packages() {
        let inf = "[Devices]\n\"iPhone\" = LIBUSB_DEV, USB\\VID_05AC&PID_12A8\n; USB\\VID_05AC&PID_12AB\n\
                   [LIBUSB_DEV.Services]\nAddService = libusb0, 0x00000002, LIBUSB.AddService\n\
                   x = USB\\VID_05AC&PID_12AA&MI_00\n";
        assert_eq!(
            inf_pids(&inf_text(inf.as_bytes())),
            vec!["12A8".to_string()]
        );
        let utf16: Vec<u8> = [0xFFu8, 0xFE]
            .into_iter()
            .chain(inf.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(inf_pids(&inf_text(&utf16)), vec!["12A8".to_string()]);
        assert!(inf_pids("AddService = libusb0x, 2\nUSB\\VID_05AC&PID_12A8").is_empty());
        assert!(inf_pids("AddService = WinUSB\nUSB\\VID_05AC&PID_12A8").is_empty());
    }
}
