//! Per-device Android settings (phase A5, master prompt section 7), saved as
//! JSON next to the window prefs: `android-devices.json` in the beta data
//! folder, or `<ZL_PREFS>.devices.json` in tests.
//!
//! Keyed by the stable device identity, so a phone keeps its settings
//! between USB and Wi-Fi. No ADB keys, no IP addresses.

use serde_json::{json, Value};
use zenlink_android::profiles::{matching, DeviceProfile, LastGood, Picture, Profile, Store};

use crate::Prefs;

fn path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("ZL_PREFS") {
        let mut s = p;
        s.push(".devices.json");
        return Some(s.into());
    }
    let base = std::env::var_os("APPDATA")?;
    let dir = zenlink_core::data_dir_in(std::path::Path::new(&base));
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("android-devices.json"))
}

pub fn load() -> Store {
    path()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| from_value(&v))
        .unwrap_or_default()
}

pub fn save(s: &Store) {
    if let Some(f) = path() {
        crate::prefs::write_atomic(
            &f,
            &serde_json::to_vec_pretty(&to_value(s)).unwrap_or_default(),
        );
    }
}

/// Allowed values, same lists as the prefs (anything else is dropped).
const MAX_SIZES: &[&str] = &["1280", "1920", "2560", "0"];
const FPS: &[&str] = &["30", "60", "90", "120", "144"];
const BITRATES: &[&str] = &["4", "8", "16", "24", "40"];
const CODECS: &[&str] = &["auto", "h264", "h265"];

fn pick(v: &Value, key: &str, allowed: &[&str], default: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| allowed.contains(s))
        .unwrap_or(default)
        .to_string()
}

fn text(v: &Value, key: &str) -> String {
    let s = v.get(key).and_then(Value::as_str).unwrap_or_default();
    s.chars().take(120).collect()
}

fn encoder(v: &Value, key: &str) -> String {
    let s = text(v, key);
    if s.is_empty() || zenlink_android::mirror::is_encoder_name(&s) {
        s
    } else {
        String::new()
    }
}

pub fn from_value(v: &Value) -> Store {
    let mut store = Store::default();
    let Some(list) = v.get("devices").and_then(Value::as_array) else {
        return store;
    };
    for d in list {
        let key = text(d, "key");
        if key.is_empty() || store.get(&key).is_some() {
            continue;
        }
        let base = DeviceProfile::new(&key);
        let p = d.get("picture").unwrap_or(&Value::Null);
        let picture = Picture {
            max_size: pick(p, "maxSize", MAX_SIZES, &base.picture.max_size),
            fps: pick(p, "fps", FPS, &base.picture.fps),
            bitrate: pick(p, "bitrate", BITRATES, &base.picture.bitrate),
            codec: pick(p, "codec", CODECS, &base.picture.codec),
            encoder: encoder(p, "encoder"),
        };
        let profile = d
            .get("profile")
            .and_then(Value::as_str)
            .and_then(Profile::parse)
            .unwrap_or_else(|| matching(&picture));
        let last_good = d
            .get("lastGood")
            .filter(|l| l.is_object())
            .map(|l| LastGood {
                codec: pick(l, "codec", CODECS, "h264"),
                encoder: encoder(l, "encoder"),
                width: l.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
                height: l.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
            });
        let e = store.entry(&key);
        e.nickname = text(d, "nickname");
        e.name = text(d, "name");
        e.manufacturer = text(d, "manufacturer");
        e.model = text(d, "model");
        e.android = text(d, "android");
        e.connection = pick(d, "connection", &["usb", "wifi", ""], "");
        // A fixed profile is kept only while the values still are its own.
        e.profile = if zenlink_android::profiles::picture(profile).as_ref() == Some(&picture) {
            profile
        } else {
            matching(&picture)
        };
        e.picture = picture;
        e.last_good = last_good;
    }
    store
}

pub fn to_value(s: &Store) -> Value {
    let devices: Vec<Value> = s
        .devices
        .iter()
        .map(|d| {
            json!({
                "key": d.key, "nickname": d.nickname, "name": d.name,
                "manufacturer": d.manufacturer, "model": d.model, "android": d.android,
                "connection": d.connection, "profile": d.profile.as_str(),
                "picture": {
                    "maxSize": d.picture.max_size, "fps": d.picture.fps,
                    "bitrate": d.picture.bitrate, "codec": d.picture.codec,
                    "encoder": d.picture.encoder,
                },
                "lastGood": d.last_good.as_ref().map(|l| json!({
                    "codec": l.codec, "encoder": l.encoder,
                    "width": l.width, "height": l.height,
                })),
            })
        })
        .collect();
    json!({ "version": 1, "devices": devices })
}

/// The picture part of the window prefs.
pub fn picture_of(p: &Prefs) -> Picture {
    Picture {
        max_size: p.android_max_size.to_string(),
        fps: p.android_fps.to_string(),
        bitrate: p.android_bitrate.to_string(),
        codec: p.android_codec.to_string(),
        encoder: p.android_encoder.to_string(),
    }
}

/// Puts a device's settings into the window prefs.
pub fn apply(p: &mut Prefs, d: &DeviceProfile) {
    p.android_profile = d.profile.as_str().into();
    p.android_max_size = d.picture.max_size.as_str().into();
    p.android_fps = d.picture.fps.as_str().into();
    p.android_bitrate = d.picture.bitrate.as_str().into();
    p.android_codec = d.picture.codec.as_str().into();
    p.android_encoder = d.picture.encoder.as_str().into();
}

/// Profile logic after a Settings change: picking a profile fills in its
/// values; changing a value selects the profile it matches (or Custom).
/// Changing the codec forgets an encoder that belongs to another codec.
pub fn normalize(old: &Prefs, new: &mut Prefs) {
    if new.android_codec != old.android_codec {
        new.android_encoder = "".into();
    }
    let picked = Profile::parse(&new.android_profile).unwrap_or(Profile::Balanced);
    if new.android_profile != old.android_profile && picked != Profile::Custom {
        let mut d = DeviceProfile::new("");
        d.set_profile(picked);
        apply(new, &d);
        return;
    }
    if new.android_profile != old.android_profile {
        // Custom picked: keep the current values.
        return;
    }
    if picture_of(new) != picture_of(old) {
        new.android_profile = matching(&picture_of(new)).as_str().into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_through_json() {
        let mut s = Store::default();
        let e = s.entry("android/R5CX10ABCDE");
        e.name = "Galaxy".into();
        e.connection = "usb".into();
        e.set_profile(Profile::HighQuality);
        e.last_good = Some(LastGood {
            codec: "h265".into(),
            encoder: "c2.exynos.hevc.encoder".into(),
            width: 1080,
            height: 2340,
        });
        let e = s.entry("android/OTHER1234");
        let mut pic = e.picture.clone();
        pic.fps = "120".into();
        pic.encoder = "c2.qti.avc.encoder".into();
        pic.codec = "h264".into();
        e.set_picture(pic);
        let back = from_value(&to_value(&s));
        assert_eq!(back, s);
    }

    #[test]
    fn picking_a_profile_fills_its_values_and_editing_one_makes_custom() {
        let old = crate::prefs::defaults();
        assert_eq!(old.android_profile.as_str(), "balanced");
        let mut new = old.clone();
        new.android_profile = "high-quality".into();
        normalize(&old, &mut new);
        assert_eq!(new.android_max_size.as_str(), "0");
        assert_eq!(new.android_bitrate.as_str(), "24");
        assert_eq!(new.android_codec.as_str(), "h265");

        let old = new.clone();
        new.android_fps = "120".into();
        normalize(&old, &mut new);
        assert_eq!(new.android_profile.as_str(), "custom");

        // Custom keeps the values the user had.
        let old = new.clone();
        new.android_profile = "custom".into();
        normalize(&old, &mut new);
        assert_eq!(new.android_fps.as_str(), "120");

        // Values that equal a profile select it again.
        let old = new.clone();
        new.android_fps = "60".into();
        normalize(&old, &mut new);
        assert_eq!(new.android_profile.as_str(), "high-quality");
    }

    #[test]
    fn changing_the_codec_forgets_the_encoder() {
        let mut old = crate::prefs::defaults();
        old.android_codec = "h264".into();
        old.android_encoder = "c2.qti.avc.encoder".into();
        let mut new = old.clone();
        new.android_codec = "h265".into();
        normalize(&old, &mut new);
        assert_eq!(new.android_encoder.as_str(), "");
    }

    #[test]
    fn bad_values_are_dropped() {
        let v = json!({ "devices": [
            { "key": "android/A1B2C3D4", "profile": "nope",
              "picture": { "maxSize": "99999", "fps": "1000", "codec": "vp9",
                           "encoder": "x; rm -rf /" } },
            { "key": "" },
            { "key": "android/A1B2C3D4" },
        ]});
        let s = from_value(&v);
        assert_eq!(s.devices.len(), 1);
        let d = &s.devices[0];
        assert_eq!(d.picture.max_size, "1920");
        assert_eq!(d.picture.fps, "60");
        assert_eq!(d.picture.codec, "auto");
        assert_eq!(d.picture.encoder, "");
        assert_eq!(d.profile, Profile::Balanced);
        assert!(from_value(&json!(null)).devices.is_empty());
    }
}
