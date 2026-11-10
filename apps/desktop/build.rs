use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    slint_build::compile("ui/app.slint").expect("compile Slint UI");
    windows_resources();
    if std::env::var_os("CARGO_FEATURE_EMBEDDED_BACKEND").is_some() {
        pack_adb();
    }
    // B-27: Windows N / KN have no mfplat.dll until the Media Feature Pack
    // is installed. A normal import would stop the loader before ZenLink
    // runs; delay-loaded, ZenLink starts and explains instead
    // (zenlink_video::media_foundation_present guards every call).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:mfplat.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}

/// Portable.exe (B-28): deflate Google's adb files from `ZENLINK_ADB_DIR`
/// into OUT_DIR and write `packed_adb.rs` (name, original size, bytes) for
/// `android::PACKED_ADB`. About 9.6 MB of files take about 4.3 MB.
fn pack_adb() {
    println!("cargo:rerun-if-env-changed=ZENLINK_ADB_DIR");
    let src = PathBuf::from(
        std::env::var_os("ZENLINK_ADB_DIR")
            .expect("ZENLINK_ADB_DIR must name Google's platform-tools folder"),
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut code = String::from("[\n");
    for name in ["adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll", "NOTICE.txt"] {
        let path = src.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let packed = miniz_oxide::deflate::compress_to_vec(&data, 9);
        let dest = out.join(format!("{name}.deflate"));
        std::fs::write(&dest, packed).expect("write packed adb");
        code += &format!(
            "    ({name:?}, {}, include_bytes!({:?})),\n",
            data.len(),
            dest.to_string_lossy()
        );
    }
    code += "]\n";
    std::fs::write(out.join("packed_adb.rs"), code).expect("write packed_adb.rs");
}

/// The exe's icon and version (Explorer, taskbar, OBS window list). Needs
/// rc.exe from the Windows SDK; without it the exe has no icon resource.
fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest.join("../../../assets/zenlink.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found: ZenLink.exe is built without an icon");
        return;
    };
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut nums: Vec<String> = ver
        .split(['.', '-'])
        .take(3)
        .map(|s| s.parse::<u16>().unwrap_or(0).to_string())
        .collect();
    nums.resize(4, "0".into());
    let nums = nums.join(",");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out.join("zenlink.rc");
    let icon_path = std::fs::canonicalize(&icon)
        .unwrap_or(icon)
        .display()
        .to_string()
        .replace("\\\\?\\", "")
        .replace('\\', "\\\\");
    std::fs::write(
        &rc_file,
        format!(
            r#"1 ICON "{icon_path}"
1 VERSIONINFO
FILEVERSION {nums}
PRODUCTVERSION {nums}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Zenvy Zenshiro"
      VALUE "FileDescription", "ZenLink - iPhone, iPad and Android mirroring for Windows"
      VALUE "FileVersion", "{ver}"
      VALUE "InternalName", "ZenLink"
      VALUE "LegalCopyright", "Copyright (C) 2026 Zenvy Zenshiro. GPL-3.0-or-later."
      VALUE "OriginalFilename", "ZenLink.exe"
      VALUE "ProductName", "ZenLink"
      VALUE "ProductVersion", "{ver}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        ),
    )
    .unwrap();
    let res = out.join("zenlink.res");
    let ok = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc_file)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!("cargo:warning=rc.exe failed: ZenLink.exe is built without an icon");
    }
}

fn find_rc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RC")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .map(|e| e.path().join(r"x64\rc.exe"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.pop()
}
