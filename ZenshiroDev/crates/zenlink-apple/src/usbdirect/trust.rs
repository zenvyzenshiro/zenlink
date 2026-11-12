//! Trust check over a usbmux stream (port of backend usbdirect/lockdown.py): a TLS lockdown
//! session with the Windows pair record, started and stopped again. qcast does the same; without
//! it the device never starts screen capture.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use idevice::{lockdown::LockdownClient, pairing_file::PairingFile, Idevice};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::mux::MuxStream;

pub const NOT_TRUSTED: &str =
    "This PC isn't trusted by the device yet. Use Apple mode once (or iTunes / Apple Devices), \
                               tap Trust on the device, then switch back to USB Direct.";

fn lockdown_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into()))
        .join("Apple")
        .join("Lockdown")
}

/// `<udid>.plist`, dashed after 8 characters for the 24-character UDIDs of newer devices.
pub fn pair_record_path(udid: &str) -> PathBuf {
    let udid = udid.trim_matches('\0');
    let name = if udid.len() == 24 && !udid.contains('-') {
        format!("{}-{}", &udid[..8], &udid[8..])
    } else {
        udid.to_string()
    };
    lockdown_dir().join(format!("{name}.plist"))
}

/// The pair record iTunes / Apple Devices wrote, completed with what Windows keeps elsewhere.
pub fn load_pair_record(udid: &str) -> Option<PairingFile> {
    let mut record = plist::Value::from_file(pair_record_path(udid))
        .ok()?
        .into_dictionary()?;
    if !record.contains_key("SystemBUID") {
        let buid = plist::Value::from_file(lockdown_dir().join("SystemConfiguration.plist"))
            .ok()
            .and_then(|v| {
                v.into_dictionary()?
                    .get("SystemBUID")?
                    .as_string()
                    .map(String::from)
            })
            .unwrap_or_default();
        record.insert("SystemBUID".into(), buid.into());
    }
    // Windows records may lack it; lockdown never checks it.
    if !record.contains_key("WiFiMACAddress") {
        record.insert("WiFiMACAddress".into(), "00:00:00:00:00:00".into());
    }
    let mut bytes = Vec::new();
    plist::Value::Dictionary(record)
        .to_writer_xml(&mut bytes)
        .ok()?;
    PairingFile::from_bytes(&bytes).ok()
}

/// StartSession + StopSession over `stream`; errors carry lockdown's own error names
/// (PasswordProtected, InvalidHostID, ...). `Ok` carries the ProductType (e.g. "iPad13,18") when
/// lockdown told it.
pub fn check(stream: MuxStream, udid: &str) -> Result<Option<String>, String> {
    let pairing = load_pair_record(udid).ok_or(NOT_TRUSTED)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let writer = stream.writer();
    let done = Arc::new(AtomicBool::new(false));
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (mut from_lockdown, mut to_lockdown) = tokio::io::split(theirs);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // The mux stream blocks, so one plain thread owns it and pulls from the device; sending goes
    // through its own writer, never through a lock the reader holds.
    let reader = {
        let (mut stream, done) = (stream, done.clone());
        std::thread::spawn(move || {
            while !done.load(Relaxed) {
                match stream.recv(Duration::from_millis(50)) {
                    Ok(Some(d)) => {
                        if tx.send(d).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => {} // timeout: let a sender take the lock
                }
            }
        })
    };
    let result = rt.block_on(async {
        let down = tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                if to_lockdown.write_all(&d).await.is_err() {
                    break;
                }
            }
        });
        let up = tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            while let Ok(n) = from_lockdown.read(&mut buf).await {
                if n == 0 || writer.send(&buf[..n]).is_err() {
                    break;
                }
            }
        });
        let session = async {
            let mut client = LockdownClient::new(Idevice::new(Box::new(ours), "ZenLink"));
            // Readable before a session; the exact model for the device list (session.py).
            let product = tokio::time::timeout(
                Duration::from_secs(4),
                client.get_value(Some("ProductType"), None),
            )
            .await
            .map_err(|_| "timed out waiting for lockdown (no answer to GetValue)".to_string())?
            .ok()
            .and_then(|v| v.as_string().map(String::from));
            log::info!("usb direct: lockdown answered (ProductType {product:?})");
            client
                .start_session(&pairing)
                .await
                .map_err(|e| format!("{e:?}"))?;
            let _ = client.stop_session().await;
            Ok::<_, String>(product)
        };
        let r = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .unwrap_or_else(|_| Err("timed out waiting for lockdown".into()));
        up.abort();
        down.abort();
        r
    });
    done.store(true, Relaxed);
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn pair_record_names() {
        let p = super::pair_record_path("00008110001A2B3C4D5E6F70");
        assert!(p.ends_with("00008110-001A2B3C4D5E6F70.plist"), "{p:?}");
        let p = super::pair_record_path("abcdef0123456789abcdef0123456789abcdef01");
        assert!(p.ends_with("abcdef0123456789abcdef0123456789abcdef01.plist"));
    }
}
