//! The backend's state machine in the window's process (port of backend server.py,
//! castmanager.py, connection.py): connection mode, the device list from Windows, the picked
//! and known devices, drivers. Served through `zenlink_core::api` without HTTP. The casting
//! sessions themselves live in apple.rs, airplay.rs and usbdirect.rs and lay their state over
//! this one.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

const MODES: [&str; 2] = ["apple", "usbdirect"];
const LINKS: [&str; 2] = ["wired", "wireless"];
/// The AirPlay name devices pick in Screen Mirroring (airplay.rs).
const RECEIVER: &str = "ZenLinkStudio";
const CHOOSE_MODE: &str = "Choose a connection mode first.";

struct Conn {
    active: Option<String>,
    pending: Option<String>,
    saved: Option<String>,
    remember: bool,
    link: String,
}

struct State {
    conn: Conn,
    error: Option<String>,
    drivers: Value,
    drivers_at: Option<Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// A consented driver change is running.
static DRIVER_BUSY: AtomicBool = AtomicBool::new(false);
/// A session runs (last overlaid state): drivers are not rescanned then.
static CASTING: AtomicBool = AtomicBool::new(false);

fn settings_path() -> Option<std::path::PathBuf> {
    crate::backend_data_dir().map(|d| d.join("settings.json"))
}

fn load() -> Map<String, Value> {
    settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Read-modify-write of settings.json, atomically replaced.
fn update(f: impl FnOnce(&mut Map<String, Value>)) {
    let Some(p) = settings_path() else { return };
    let mut data = load();
    f(&mut data);
    let tmp = p.with_extension("tmp");
    let body = serde_json::to_vec_pretty(&Value::Object(data)).unwrap_or_default();
    if std::fs::write(&tmp, body)
        .and_then(|_| std::fs::rename(&tmp, &p))
        .is_err()
    {
        log::warn!("could not save {}", p.display());
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap();
    let s = g.get_or_insert_with(|| {
        let d = load();
        let saved = d
            .get("connectionMode")
            .and_then(Value::as_str)
            .filter(|m| MODES.contains(m))
            .map(String::from);
        let remember = d
            .get("rememberConnectionMode")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && saved.is_some();
        let saved = saved.filter(|_| remember);
        let link = d
            .get("appleLink")
            .and_then(Value::as_str)
            .filter(|l| LINKS.contains(l))
            .unwrap_or("wired");
        State {
            // A remembered mode applies straight away; otherwise wait for a choice.
            conn: Conn {
                active: saved.clone(),
                pending: None,
                saved,
                remember,
                link: link.into(),
            },
            error: None,
            drivers: json!({ "apple": {}, "libusb0": {}, "devices": [] }),
            drivers_at: None,
        }
    });
    f(s)
}

fn conn_snapshot(c: &Conn) -> Value {
    json!({
        "active": c.active, "pending": c.pending, "saved": c.saved, "remember": c.remember, "link": c.link,
        "needsChoice": c.active.is_none(), "restartRequired": c.pending.is_some(),
    })
}

/// A name the user gave the device, the model learned before, else the series by chip id
/// (apple/models.py model_name).
fn model_name(serial: &str, family: &str) -> String {
    let names: Value = crate::backend_data_dir()
        .and_then(|d| std::fs::read(d.join("devices.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    names
        .get(serial)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            names
                .pointer(&format!("/_models/{serial}"))
                .and_then(Value::as_str)
        })
        .map(String::from)
        .unwrap_or_else(|| zenlink_apple::models::series_name(serial, family))
}

/// Stores the exact model learned over the cable in devices.json `_models` (models.py
/// remember_product); unknown product types are skipped.
pub fn remember_product(serial: &str, product_type: &str) {
    let (Some(dir), Some(model)) = (
        crate::backend_data_dir(),
        zenlink_apple::models::product_name(product_type),
    ) else {
        return;
    };
    if serial.is_empty() {
        return;
    }
    let p = dir.join("devices.json");
    let mut names: Value = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if names
        .pointer(&format!("/_models/{serial}"))
        .and_then(Value::as_str)
        == Some(model)
    {
        return;
    }
    if !names.is_object() {
        names = json!({});
    }
    names["_models"][serial] = json!(model);
    let tmp = p.with_extension("tmp");
    let body = serde_json::to_vec_pretty(&names).unwrap_or_default();
    if std::fs::create_dir_all(&dir)
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, &p))
        .is_err()
    {
        log::warn!("devices.json: could not save the model of {serial}");
    }
}

/// What stands between a Windows-listed device and the chosen mode (None = ready).
fn need(s: &State, d: &Value) -> Option<&'static str> {
    let mode = s.conn.active.as_deref()?;
    if d["unsafeFilter"].as_bool() == Some(true) {
        return Some("removefilter");
    }
    let replaced = d["mode"] == "replaced";
    if mode == "usbdirect" {
        if replaced {
            return None;
        }
        // A missing per-model package is built during the install.
        return Some(
            if s.drivers["libusb0"]["available"].as_bool() == Some(true) {
                "bind"
            } else {
                "package"
            },
        );
    }
    if replaced {
        return Some("restore");
    }
    // Issue #9: Windows sees the device, but without the Apple service Apple mode cannot.
    (!crate::apple::USBMUX_OK.load(Relaxed)).then_some("service")
}

/// Devices from Windows' own device list (no Apple service, no USB I/O).
fn windows_presence(s: &State) -> Vec<Value> {
    let usbdirect = s.conn.active.as_deref() == Some("usbdirect");
    s.drivers["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            let serial = d["serial"].as_str().unwrap_or_default();
            let id = if serial.len() == 24 { format!("{}-{}", &serial[..8], &serial[8..]) } else { serial.into() };
            let family = d["family"].as_str().unwrap_or("iPhone");
            let need = if s.conn.active.is_none() { Some("mode") } else { need(s, d) };
            json!({ "id": id, "family": family, "name": model_name(serial, family), "deviceName": null, "os": null,
                    "need": need, "ready": usbdirect && need.is_none() })
        })
        .collect()
}

fn refresh_drivers(s: &mut State) {
    s.drivers = crate::drivers::snapshot();
    s.drivers_at = Some(Instant::now());
}

fn snapshot(s: &mut State) -> Value {
    let stale = s
        .drivers_at
        .is_none_or(|t| t.elapsed() > Duration::from_secs(4));
    if stale && !CASTING.load(Relaxed) && !DRIVER_BUSY.load(Relaxed) {
        refresh_drivers(s);
    }
    let devices = match (s.conn.active.as_deref(), s.conn.link.as_str()) {
        // Studio over Wi-Fi: ZenLink is an AirPlay receiver; the device comes to us.
        (Some("usbdirect"), "wireless") => vec![json!({
            "id": "airplay", "family": "iOS device", "name": "AirPlay (Wi-Fi)", "deviceName": null, "os": null,
            "ready": true, "wifi": true, "airplay": true, "receiver": RECEIVER })],
        // Apple Driver: apple.rs lists the devices; these are the ones whose driver is wrong.
        (Some("apple"), _) => windows_presence(s)
            .into_iter()
            .filter(|d| !d["need"].is_null())
            .collect(),
        _ => windows_presence(s),
    };
    let d = load();
    let known = d
        .get("appleDevices")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let selected = d
        .get("appleDevice")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let present = |id: &str| devices.iter().any(|x| x["id"] == id);
    let known: Vec<Value> = known
        .iter()
        .map(|(id, info)| {
            let mut e = json!({ "id": id, "present": present(id) });
            if let (Some(o), Some(i)) = (e.as_object_mut(), info.as_object()) {
                o.extend(i.clone());
            }
            e
        })
        .collect();
    json!({
        "phase": if s.error.is_some() { "error" } else { "idle" },
        "detail": "", "device": null, "error": s.error, "rotation": null, "backend": null, "control": null,
        "devices": devices, "known": known, "selected": selected, "service": crate::apple::USBMUX_OK.load(Relaxed), "takenOver": false,
        "connection": conn_snapshot(&s.conn), "drivers": s.drivers, "capture": null, "nativeAudio": true,
    })
}

/// The whole state the window shows, before the sessions lay theirs over it.
pub fn state() -> Value {
    with(snapshot)
}

/// Called with the overlaid state: notes whether a session runs and keeps id, model and the
/// user's device name of trusted Apple devices (listed later even when not connected).
pub fn observe(st: &Value) {
    let phase = st["phase"].as_str().unwrap_or("idle");
    CASTING.store(
        matches!(phase, "connecting" | "activating" | "streaming"),
        Relaxed,
    );
    let devices: Vec<&Value> = st["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| {
            d["ready"] == true
                && d["id"]
                    .as_str()
                    .is_some_and(|i| !i.is_empty() && i != "airplay")
        })
        .collect();
    if devices.is_empty() {
        return;
    }
    let known = load().get("appleDevices").cloned().unwrap_or_default();
    let mut changed = Vec::new();
    for d in devices {
        let id = d["id"].as_str().unwrap();
        let old = &known[id];
        // Keep a name read earlier when this read had none.
        let pick = |k: &str| {
            if d[k].is_null() || d[k] == "" {
                old[k].clone()
            } else {
                d[k].clone()
            }
        };
        let merged = json!({ "name": pick("name"), "deviceName": pick("deviceName"), "family": pick("family") });
        if *old != merged {
            changed.push((id.to_string(), merged));
        }
    }
    if !changed.is_empty() {
        update(|data| {
            let k = data.entry("appleDevices").or_insert_with(|| json!({}));
            if !k.is_object() {
                *k = json!({});
            }
            for (id, v) in changed {
                k[id] = v;
            }
        });
    }
}

fn ok(v: Value) -> (u16, Value) {
    (200, v)
}

fn bad(code: u16, msg: &str) -> (u16, Value) {
    (code, json!({ "ok": false, "error": msg, "message": msg }))
}

/// One API request, as the HTTP server answered it.
pub fn handle(method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let null = Value::Null;
    let b = body.unwrap_or(&null);
    match (method, path) {
        ("GET", "/api/state") | ("POST", "/api/cast/stop") => ok(state()),
        ("POST", "/api/cast/start") => {
            // Every mode starts in Rust; only "no mode yet" reaches this.
            with(|s| s.error = s.conn.active.is_none().then(|| CHOOSE_MODE.to_string()));
            ok(state())
        }
        ("POST", "/api/device/select") => {
            let id = b["id"].as_str().unwrap_or("").to_string();
            let valid = (8..=64).contains(&id.len())
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
            if !id.is_empty() && !valid {
                return bad(400, "bad id");
            }
            let forget = b["forget"].as_bool() == Some(true);
            update(|d| {
                if forget {
                    if let Some(k) = d.get_mut("appleDevices").and_then(Value::as_object_mut) {
                        k.remove(&id);
                    }
                    if d.get("appleDevice").and_then(Value::as_str) == Some(&id) {
                        d.remove("appleDevice");
                    }
                } else if id.is_empty() {
                    d.remove("appleDevice");
                } else {
                    d.insert("appleDevice".into(), id.clone().into());
                }
            });
            ok(state())
        }
        ("GET", "/api/connection") => ok(with(|s| conn_snapshot(&s.conn))),
        ("POST", "/api/connection") => {
            if let (Some(link), None) = (b.get("link"), b.get("mode")) {
                let link = link.as_str().unwrap_or("");
                if !LINKS.contains(&link) {
                    return bad(400, &format!("Unknown link: {link}"));
                }
                // Wired <-> wireless ends the current session first.
                crate::cast_stop_all();
                with(|s| s.conn.link = link.into());
                update(|d| {
                    d.insert("appleLink".into(), link.into());
                });
                return ok(state());
            }
            let mode = b["mode"].as_str().unwrap_or("");
            if !MODES.contains(&mode) {
                return bad(400, &format!("Unknown connection mode: {mode}"));
            }
            let remember = b["remember"].as_bool() == Some(true);
            update(|d| {
                if remember {
                    d.insert("connectionMode".into(), mode.into());
                } else {
                    d.remove("connectionMode");
                }
                d.insert("rememberConnectionMode".into(), remember.into());
            });
            ok(with(|s| {
                let c = &mut s.conn;
                c.remember = remember;
                c.saved = remember.then(|| mode.to_string());
                // Never hot-swap: a change while a mode is active waits for a restart.
                match c.active.as_deref() {
                    None => (c.active, c.pending) = (Some(mode.into()), None),
                    Some(a) if a != mode => c.pending = Some(mode.into()),
                    _ => c.pending = None,
                }
                s.error = None;
                log::info!(
                    "connection mode: active={:?} pending={:?} remember={remember}",
                    c.active,
                    c.pending
                );
                conn_snapshot(c)
            }))
        }
        ("GET", "/api/drivers") => ok(with(|s| {
            refresh_drivers(s);
            s.drivers.clone()
        })),
        ("POST", "/api/drivers/action") => {
            if CASTING.load(Relaxed) {
                return bad(409, "Stop casting before changing drivers.");
            }
            if b["confirmed"].as_bool() != Some(true) {
                return bad(400, "The change was not confirmed.");
            }
            if DRIVER_BUSY.swap(true, Relaxed) {
                return bad(409, "Another driver change is running.");
            }
            let r = crate::drivers::run_action(
                b["action"].as_str().unwrap_or(""),
                b["instance"].as_str().unwrap_or(""),
            );
            DRIVER_BUSY.store(false, Relaxed);
            with(refresh_drivers);
            ok(r.unwrap_or_else(|m| json!({ "ok": false, "message": m })))
        }
        // The sessions read the sound settings from apple.rs.
        ("POST", "/api/app/restart") => {
            // What a service restart did: end casting, then the pending mode applies.
            crate::cast_stop_all();
            with(|s| {
                if let Some(m) = s.conn.pending.take() {
                    s.conn.active = Some(m);
                }
                s.error = None;
            });
            ok(json!({ "ok": true }))
        }
        _ => bad(404, "Not found."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(mode: Option<&str>, available: bool) -> State {
        State {
            conn: Conn {
                active: mode.map(String::from),
                pending: None,
                saved: None,
                remember: false,
                link: "wired".into(),
            },
            error: None,
            drivers: json!({ "libusb0": { "available": available } }),
            drivers_at: None,
        }
    }

    #[test]
    fn device_needs() {
        let apple = json!({ "mode": "apple", "unsafeFilter": false });
        let replaced = json!({ "mode": "replaced", "unsafeFilter": false });
        let coexist = json!({ "mode": "coexist", "unsafeFilter": true });
        let s = state_with(Some("usbdirect"), true);
        assert_eq!(need(&s, &replaced), None);
        assert_eq!(need(&s, &apple), Some("bind"));
        assert_eq!(need(&s, &coexist), Some("removefilter"));
        assert_eq!(
            need(&state_with(Some("usbdirect"), false), &apple),
            Some("package")
        );
        let s = state_with(Some("apple"), true);
        assert_eq!(need(&s, &apple), None);
        assert_eq!(need(&s, &replaced), Some("restore"));
        assert_eq!(need(&state_with(None, true), &apple), None);
    }
}
