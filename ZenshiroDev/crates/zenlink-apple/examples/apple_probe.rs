//! Checks one iPhone / iPad end to end with the zenlink-apple library.
//!
//! cargo run -p zenlink-apple --example apple_probe -- [--udid X] [--wifi] [--stream SECONDS] [--out FILE]
//!
//! USB by default: device, iOS, tunnel, Developer Disk Image (installed when missing),
//! display service. --wifi finds the device over Wi-Fi (RemotePairing) instead.
//! --stream receives the screen for SECONDS and reports fps; --out saves the HEVC.

use std::io::Write as _;
use std::time::{Duration, Instant};

use idevice::usbmuxd::Connection;
use zenlink_apple::{link, mirror, wifi};

#[derive(Default)]
struct Args {
    udid: Option<String>,
    wifi: bool,
    stream: Option<u64>,
    out: Option<String>,
}

fn args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--udid" => a.udid = it.next(),
            "--wifi" => a.wifi = true,
            "--stream" => a.stream = it.next().and_then(|v| v.parse().ok()),
            "--out" => a.out = it.next(),
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

macro_rules! step {
    ($($t:tt)*) => {{ println!("[{:>6.2}s] {}", START.with(|s| s.elapsed().as_secs_f32()), format!($($t)*)); }};
}
thread_local! { static START: Instant = Instant::now(); }

#[tokio::main]
async fn main() {
    if let Err(e) = run(args()).await {
        step!("FAILED: {e}");
        std::process::exit(1);
    }
    step!("done");
}

async fn run(a: Args) -> Result<(), String> {
    let mut l = if a.wifi {
        let dir = wifi::record_dir().ok_or("no USERPROFILE")?;
        let records: Vec<_> = wifi::records(&dir)
            .into_iter()
            .filter(|r| a.udid.as_ref().is_none_or(|u| u == &r.udid))
            .collect();
        step!("{} remote pair record(s)", records.len());
        let found = wifi::browse(&records, Duration::from_secs(4)).await?;
        for f in &found {
            step!("Wi-Fi: {} at {}", f.udid, f.addr);
        }
        let f = found
            .first()
            .ok_or("no paired device advertises RemotePairing")?;
        let r = records.iter().find(|r| r.udid == f.udid).ok_or("record")?;
        link::Link::wifi(f.addr, r).await?
    } else {
        let devs = link::usbmux()
            .await?
            .get_devices()
            .await
            .map_err(|e| format!("device list: {e:?}"))?;
        let dev = devs
            .iter()
            .filter(|d| d.connection_type == Connection::Usb)
            .find(|d| a.udid.as_ref().is_none_or(|u| u == &d.udid))
            .ok_or("no device on USB")?;
        let info = link::info(&dev.udid).await?;
        step!(
            "{} ({}), iOS {}",
            info.name,
            info.product_type,
            info.version
        );
        let mut l = link::Link::usb(&dev.udid).await?;
        step!("tunnel up: {} RSD services", l.rsd.services.len());
        l.ensure_ddi(&info.version, |n| step!("{n}")).await?;
        l
    };
    step!("display service offered: {}", l.display_offered());

    let Some(secs) = a.stream else { return Ok(()) };
    let mut out = a
        .out
        .as_ref()
        .map(|p| std::fs::File::create(p).expect("create --out"));
    let (mut frames, mut bytes) = (0usize, 0usize);
    let end = Instant::now() + Duration::from_secs(secs);
    let mut first: Option<Instant> = None;
    step!("streaming for {secs}s (move something on the screen)");
    let mut audio = 0usize;
    let key = std::sync::atomic::AtomicBool::new(false);
    mirror::stream(
        &mut l,
        &key,
        false,
        |au, _rebuild| {
            first.get_or_insert_with(Instant::now);
            frames += 1;
            bytes += au.len();
            if let Some(f) = out.as_mut() {
                let _ = f.write_all(&au);
            }
            Instant::now() < end
        },
        |_| audio += 1,
    )
    .await?;
    step!("{audio} sound packets (AAC-ELD)");
    let fps = first.map_or(0.0, |t| {
        frames as f32 / t.elapsed().as_secs_f32().max(0.001)
    });
    step!(
        "{frames} frames (~{fps:.1} fps), {:.1} MB HEVC",
        bytes as f32 / 1e6
    );
    if frames == 0 {
        return Err("no video frames received".into());
    }
    Ok(())
}
