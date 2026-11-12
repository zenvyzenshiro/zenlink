//! Settings, saved as JSON in %APPDATA%\ZenLink-Next\next-prefs.json.
//! The stable v2.2.31 keeps %APPDATA%\ZenLink; the beta never touches it.

use serde_json::{json, Value};

use crate::Prefs;

fn path() -> Option<std::path::PathBuf> {
    // Tests use their own file so they never change the user's settings.
    if let Some(p) = std::env::var_os("ZL_PREFS") {
        return Some(p.into());
    }
    let base = std::env::var_os("APPDATA")?;
    let dir = zenlink_core::data_dir_in(std::path::Path::new(&base));
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("next-prefs.json"))
}

/// A language ZenLink has a file for that Windows lists among the user's
/// languages (or uses as its locale), English otherwise. Only the first-run
/// default: a language picked in the app is saved and wins.
pub fn system_lang() -> String {
    let have: Vec<String> = crate::i18n::languages()
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    lang_from(&windows_languages(), &have)
}

fn lang_from(tags: &[String], have: &[String]) -> String {
    tags.iter()
        .map(|t| t.to_ascii_lowercase())
        .map(|t| match t.split('-').next().unwrap_or("") {
            "in" => "id".to_string(), // the old code for Indonesian
            base => base.to_string(),
        })
        .find(|base| base != "en" && have.contains(base))
        .unwrap_or_else(|| "en".into())
}

/// Language tags from HKCU\Control Panel\International: the user's
/// language list ("User Profile\Languages") and the regional format.
fn windows_languages() -> Vec<String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ,
    };

    let mut out = Vec::new();
    for (key, value, kind) in [
        (
            w!(r"Control Panel\International\User Profile"),
            w!("Languages"),
            RRF_RT_REG_MULTI_SZ,
        ),
        (
            w!(r"Control Panel\International"),
            w!("LocaleName"),
            RRF_RT_REG_SZ,
        ),
    ] {
        let mut buf = vec![0u16; 2048];
        let mut size = (buf.len() * 2) as u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key,
                value,
                kind,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if ok.is_ok() {
            buf.truncate(size as usize / 2);
            out.extend(
                buf.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(String::from_utf16_lossy),
            );
        }
    }
    out
}

/// Glass thickness slider (0..100) to (glass on, thickness 0..1 for the
/// backdrop and theme). The old linear scale put every visible change in
/// 0..5%: now 0 = clear (no blur), 1..75 = blur up to the default frosted
/// look (old 5%), 75..100 = thicker, 100 = solid, no glass at all.
pub fn glass_level(slider: f32) -> (bool, f32) {
    let s = slider.clamp(0.0, 100.0);
    if s >= 99.5 {
        (false, 1.0)
    } else if s <= 0.0 {
        (true, 0.0)
    } else if s <= 75.0 {
        // 0.03 is where the backdrop turns its blur on.
        (true, 0.03 + 0.02 * s / 75.0)
    } else {
        (true, 0.05 + 0.95 * (s - 75.0) / 25.0)
    }
}

/// The slider position for an old linear thickness (0..1).
fn slider_for(thick: f32) -> f32 {
    let t = thick.clamp(0.0, 1.0);
    let s = if t <= 0.0 {
        0.0
    } else if t <= 0.05 {
        (75.0 * (t - 0.03) / 0.02).max(1.0)
    } else {
        75.0 + 25.0 * (t - 0.05) / 0.95
    };
    // shortcut: old 100% (thickest glass) lands on 99, not on the new "no glass" 100.
    s.round().min(99.0)
}

pub fn defaults() -> Prefs {
    Prefs {
        theme: "dark".into(),
        glass_on: true,
        glass: 75.0,
        lens: true,
        lang: system_lang().into(),
        fit: "contain".into(),
        fit_window: true,
        rotation: "auto".into(),
        crop_l: 0.0,
        crop_t: 0.0,
        crop_r: 0.0,
        crop_b: 0.0,
        audio_out: "pc".into(),
        volume: 100.0,
        lip_sync: true,
        audio_buffer: "low".into(),
        touch: true,
        keyboard: true,
        gestures: true,
        start_with_windows: false,
        close_action: "ask".into(),
        auto_unlock: false,
        unlock_on_lock: false,
        unlock_limit: true,
        on_top: false,
        ghost_key: "F4".into(),
        ghost_opacity: 100.0,
        stats: false,
        dots: true,
        platform: "".into(),
        remember_platform: false,
        android_max_size: "1920".into(),
        android_fps: "60".into(),
        android_bitrate: "8".into(),
        android_codec: "auto".into(),
        android_profile: "balanced".into(),
        android_encoder: "".into(),
        update_check: true,
        update_channel: "".into(),
    }
}

pub fn load() -> Prefs {
    match path()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| parse(&b))
    {
        Some(v) => from_value(&v),
        None => defaults(),
    }
}

/// Parses the file, also when an editor saved it with a UTF-8 byte order
/// mark (serde_json rejects one, which would silently reset every setting).
fn parse(bytes: &[u8]) -> Option<Value> {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(b).ok()
}

/// Reads a saved file; unknown or invalid values keep their defaults, so
/// files from older versions load without losing language, theme or others.
pub fn from_value(v: &Value) -> Prefs {
    let mut p = defaults();
    let s = |k: &str, d: &mut slint::SharedString, allowed: &[&str]| {
        if let Some(x) = v.get(k).and_then(Value::as_str) {
            if allowed.contains(&x) {
                *d = x.into();
            }
        }
    };
    let f = |k: &str, d: &mut f32, lo: f32, hi: f32| {
        if let Some(x) = v.get(k).and_then(Value::as_f64) {
            *d = (x as f32).clamp(lo, hi);
        }
    };
    let b = |k: &str, d: &mut bool| {
        if let Some(x) = v.get(k).and_then(Value::as_bool) {
            *d = x;
        }
    };
    s("theme", &mut p.theme, &["light", "dark", "system"]);
    b("glassOn", &mut p.glass_on);
    // Version 2 made 30% the default thickness; older files saved 0%.
    // Version 3 spread the slider (see glass_level): a version 2 value keeps its look.
    match v.get("version").and_then(Value::as_u64).unwrap_or(1) {
        0 | 1 => {}
        2 => {
            let mut old = 30.0;
            f("glassThick", &mut old, 0.0, 100.0);
            p.glass = slider_for(old / 100.0);
        }
        _ => f("glassThick", &mut p.glass, 0.0, 100.0),
    }
    b("lens", &mut p.lens);
    s("lang", &mut p.lang, &["id", "en"]);
    s("fit", &mut p.fit, &["contain", "cover", "fill"]);
    b("fitWindow", &mut p.fit_window);
    s(
        "rotation",
        &mut p.rotation,
        &["auto", "0", "90", "180", "270"],
    );
    f("cropL", &mut p.crop_l, 0.0, 400.0);
    f("cropT", &mut p.crop_t, 0.0, 400.0);
    f("cropR", &mut p.crop_r, 0.0, 400.0);
    f("cropB", &mut p.crop_b, 0.0, 400.0);
    s("audioOut", &mut p.audio_out, &["pc", "device", "both"]);
    f("volume", &mut p.volume, 0.0, 100.0);
    b("lipSync", &mut p.lip_sync);
    s(
        "audioBuffer",
        &mut p.audio_buffer,
        &["low", "normal", "high"],
    );
    b("touch", &mut p.touch);
    b("keyboard", &mut p.keyboard);
    b("gestures", &mut p.gestures);
    // "autoStart" (start when plugged in) was never implemented; not read.
    b("startWithWindows", &mut p.start_with_windows);
    // "ask" | "tray" | "quit". Files from before it only had closeToTray:
    // true keeps going to the tray, false or missing asks.
    if v.get("closeToTray").and_then(Value::as_bool) == Some(true) {
        p.close_action = "tray".into();
    }
    s("closeAction", &mut p.close_action, &["ask", "tray", "quit"]);
    b("autoUnlock", &mut p.auto_unlock);
    b("unlockOnLock", &mut p.unlock_on_lock);
    b("unlockLimit", &mut p.unlock_limit);
    b("onTop", &mut p.on_top);
    s("ghostKey", &mut p.ghost_key, &crate::ghost::KEYS);
    f("ghostOpacity", &mut p.ghost_opacity, 20.0, 100.0);
    b("stats", &mut p.stats);
    b("dots", &mut p.dots);
    // Device platform (phase A2). Separate from the backend's Apple
    // "Remember my choice" (rememberConnectionMode in settings.json).
    s("platform", &mut p.platform, &["apple", "android"]);
    b("rememberPlatform", &mut p.remember_platform);
    b("updateCheck", &mut p.update_check);
    s("updateChannel", &mut p.update_channel, &["stable", "beta"]);
    // Android picture (phase A4): scrcpy server options.
    s(
        "androidMaxSize",
        &mut p.android_max_size,
        &["1280", "1920", "2560", "0"],
    );
    s(
        "androidFps",
        &mut p.android_fps,
        &["30", "60", "90", "120", "144"],
    );
    s(
        "androidBitrate",
        &mut p.android_bitrate,
        &["4", "8", "16", "24", "40"],
    );
    s(
        "androidCodec",
        &mut p.android_codec,
        &["auto", "h264", "h265"],
    );
    // Phase A5: video profile and a specific encoder (from the phone's list).
    s(
        "androidProfile",
        &mut p.android_profile,
        &[
            "low-latency",
            "balanced",
            "high-quality",
            "streaming",
            "custom",
        ],
    );
    if v.get("androidProfile").is_none() {
        // Settings from before A5: the profile their values match.
        p.android_profile =
            zenlink_android::profiles::matching(&crate::devprofiles::picture_of(&p))
                .as_str()
                .into();
    }
    if let Some(e) = v.get("androidEncoder").and_then(Value::as_str) {
        if e.is_empty() || zenlink_android::mirror::is_encoder_name(e) {
            p.android_encoder = e.into();
        }
    }
    p
}

pub fn save(p: &Prefs) {
    if let Some(f) = path() {
        let _ = std::fs::write(
            f,
            serde_json::to_vec_pretty(&to_value(p)).unwrap_or_default(),
        );
    }
}

pub fn to_value(p: &Prefs) -> Value {
    json!({
        "version": 3, "theme": p.theme.as_str(), "glassOn": p.glass_on, "glassThick": p.glass, "lens": p.lens, "lang": p.lang.as_str(), "fit": p.fit.as_str(),
        "fitWindow": p.fit_window, "rotation": p.rotation.as_str(),
        "cropL": p.crop_l, "cropT": p.crop_t, "cropR": p.crop_r, "cropB": p.crop_b,
        "audioOut": p.audio_out.as_str(), "volume": p.volume, "lipSync": p.lip_sync,
        "audioBuffer": p.audio_buffer.as_str(), "touch": p.touch, "keyboard": p.keyboard,
        "gestures": p.gestures, "startWithWindows": p.start_with_windows, "closeAction": p.close_action.as_str(), "closeToTray": p.close_action == "tray", "autoUnlock": p.auto_unlock, "unlockOnLock": p.unlock_on_lock, "unlockLimit": p.unlock_limit,
        "onTop": p.on_top, "ghostKey": p.ghost_key.as_str(), "ghostOpacity": p.ghost_opacity,"stats": p.stats, "dots": p.dots,
        "platform": p.platform.as_str(), "rememberPlatform": p.remember_platform,
        "androidMaxSize": p.android_max_size.as_str(), "androidFps": p.android_fps.as_str(),
        "androidBitrate": p.android_bitrate.as_str(), "androidCodec": p.android_codec.as_str(),
        "androidProfile": p.android_profile.as_str(), "androidEncoder": p.android_encoder.as_str(),
        "updateCheck": p.update_check, "updateChannel": p.update_channel.as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_with_a_byte_order_mark_still_loads() {
        let v = parse(b"\xEF\xBB\xBF{\"startWithWindows\": true}").unwrap();
        assert!(from_value(&v).start_with_windows);
        assert!(parse(b"{}").is_some());
        assert!(parse(b"not json").is_none());
    }

    #[test]
    fn language_follows_windows() {
        let tags = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let have = tags(&["en", "id"]);
        assert_eq!(lang_from(&tags(&["en-US", "id-ID"]), &have), "id");
        assert_eq!(lang_from(&tags(&["id"]), &have), "id");
        assert_eq!(lang_from(&tags(&["in-ID"]), &have), "id");
        assert_eq!(lang_from(&tags(&["en-US", "ja-JP"]), &have), "en");
        assert_eq!(lang_from(&tags(&["it-IT"]), &have), "en", "no Italian file");
        assert_eq!(lang_from(&tags(&["it-IT"]), &tags(&["en", "it"])), "it");
        assert_eq!(lang_from(&[], &have), "en");
    }

    #[test]
    fn fresh_install_defaults() {
        let p = defaults();
        assert_eq!(
            p.lang.as_str(),
            system_lang().as_str(),
            "the Windows language on a fresh install"
        );
        assert_eq!(p.theme.as_str(), "dark", "Liquid Glass Dark by default");
        assert_eq!(p.platform.as_str(), "");
        assert!(!p.remember_platform);
    }

    #[test]
    fn old_file_keeps_language_and_theme_and_has_no_platform() {
        let old = json!({"version": 3, "theme": "light", "lang": "en", "glassThick": 55.0});
        let p = from_value(&old);
        assert_eq!(
            (p.theme.as_str(), p.lang.as_str(), p.glass),
            ("light", "en", 55.0)
        );
        assert_eq!(p.platform.as_str(), "");
        assert!(!p.remember_platform);
    }

    #[test]
    fn platform_round_trip_and_validation() {
        let mut p = defaults();
        p.platform = "android".into();
        p.remember_platform = true;
        let back = from_value(&to_value(&p));
        assert_eq!(back.platform.as_str(), "android");
        assert!(back.remember_platform);
        let bad = from_value(&json!({"platform": "windows-phone"}));
        assert_eq!(bad.platform.as_str(), "");
    }

    #[test]
    fn glass_slider_spreads_the_look() {
        assert_eq!(glass_level(0.0), (true, 0.0), "0 = clear");
        assert!(glass_level(1.0).1 >= 0.03, "1 already blurs");
        assert!(
            (glass_level(75.0).1 - 0.05).abs() < 1e-6,
            "75 = old 5% look"
        );
        assert!(!glass_level(100.0).0, "100 = no glass");
        let mut last = -1.0;
        for s in 0..100 {
            let t = glass_level(s as f32).1;
            assert!(t > last, "thicker at every step ({s})");
            last = t;
        }
        // Old files keep their look.
        let at = |o: f64| from_value(&json!({"version": 2, "glassThick": o})).glass;
        assert_eq!(at(5.0), 75.0);
        assert_eq!(at(0.0), 0.0);
        assert_eq!(at(100.0), 99.0);
        assert_eq!(
            from_value(&json!({"version": 3, "glassThick": 40.0})).glass,
            40.0
        );
        assert_eq!(defaults().glass, 75.0);
    }

    #[test]
    fn close_action_migrates_and_round_trips() {
        let act = |v: Value| from_value(&v).close_action.to_string();
        assert_eq!(act(json!({})), "ask", "fresh install asks");
        assert_eq!(act(json!({"closeToTray": true})), "tray");
        assert_eq!(act(json!({"closeToTray": false})), "ask");
        assert_eq!(
            act(json!({"closeToTray": true, "closeAction": "quit"})),
            "quit"
        );
        assert_eq!(act(json!({"closeAction": "explode"})), "ask");
        for a in ["ask", "tray", "quit"] {
            let mut p = defaults();
            p.close_action = a.into();
            let v = to_value(&p);
            assert_eq!(
                v["closeToTray"],
                json!(a == "tray"),
                "old readers still work"
            );
            assert_eq!(from_value(&v).close_action.as_str(), a);
        }
    }
}
