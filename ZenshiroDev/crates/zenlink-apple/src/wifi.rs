//! Apple Driver over Wi-Fi: find a paired device by its `_remotepairing._tcp` advert and open the
//! RemotePairing tunnel (pair-verify, TLS-PSK, CDTunnel) into idevice's in-process TCP stack.
//!
//! The pair records are the ones pymobiledevice3 wrote at USB pairing
//! (`%USERPROFILE%\.pymobiledevice3\remote_<udid>.plist`), so a device paired with ZenLink 2.5.x
//! works without pairing again. No Bonjour Service and no wintun are needed.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use idevice::{
    remote_pairing::{
        compute_auth_tag, connect_tls_psk_tunnel_native, RemotePairingClient, RpPairingFile,
        RpPairingSocket,
    },
    tcp::adapter::Adapter,
};
use tokio::net::TcpStream;

const SERVICE: &str = "_remotepairing._tcp.local.";

/// A remote pair record from pymobiledevice3.
pub struct Record {
    pub udid: String,
    /// The device's altIRK: it keys the authTag in the device's advert.
    pub peer_alt_irk: [u8; 16],
    private_key: Vec<u8>,
    public_key: Vec<u8>,
}

pub fn record_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".pymobiledevice3"))
}

/// Every readable `remote_<udid>.plist` in `dir`.
pub fn records(dir: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let data =
        |d: &plist::Dictionary, k: &str| d.get(k).and_then(|v| v.as_data()).map(<[u8]>::to_vec);
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let udid = name
                .strip_prefix("remote_")?
                .strip_suffix(".plist")?
                .to_owned();
            let d: plist::Dictionary = plist::from_file(e.path()).ok()?;
            Some(Record {
                udid,
                peer_alt_irk: data(&d, "peer_alt_irk")?.try_into().ok()?,
                private_key: data(&d, "private_key")?,
                public_key: data(&d, "public_key")?,
            })
        })
        .collect()
}

/// pymobiledevice3's `generate_host_id()`: the identifier this PC paired under.
pub fn host_identifier(hostname: &str) -> String {
    uuid::Uuid::new_v3(&uuid::Uuid::NAMESPACE_DNS, hostname.as_bytes())
        .to_string()
        .to_uppercase()
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// Whether an advert's base64 `authTag` was made with this altIRK.
pub fn auth_tag_matches(alt_irk: &[u8; 16], identifier: &str, auth_tag: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(auth_tag)
        .is_ok_and(|t| t == compute_auth_tag(alt_irk, identifier))
}

/// A paired device seen on the network.
#[derive(Debug, Clone)]
pub struct Found {
    pub udid: String,
    pub addr: SocketAddr,
}

/// Browse `_remotepairing._tcp` for `wait` and return the devices whose advert matches a record.
pub async fn browse(records: &[Record], wait: Duration) -> Result<Vec<Found>, String> {
    let mdns = mdns_sd::ServiceDaemon::new().map_err(|e| format!("mDNS: {e}"))?;
    let rx = mdns
        .browse(SERVICE)
        .map_err(|e| format!("mDNS browse: {e}"))?;
    let mut found: Vec<Found> = Vec::new();
    let end = tokio::time::Instant::now() + wait;
    while let Ok(Ok(event)) = tokio::time::timeout_at(end, rx.recv_async()).await {
        let mdns_sd::ServiceEvent::ServiceResolved(info) = event else {
            continue;
        };
        let props = &info.txt_properties;
        let (Some(ident), Some(tag)) = (
            props.get_property_val_str("identifier"),
            props.get_property_val_str("authTag"),
        ) else {
            continue;
        };
        let Some(ip) = info.get_addresses_v4().into_iter().next() else {
            continue;
        };
        if let Some(r) = records
            .iter()
            .find(|r| auth_tag_matches(&r.peer_alt_irk, ident, tag))
        {
            if !found.iter().any(|f| f.udid == r.udid) {
                found.push(Found {
                    udid: r.udid.clone(),
                    addr: SocketAddr::new(IpAddr::V4(ip), info.get_port()),
                });
            }
        }
    }
    let _ = mdns.shutdown();
    Ok(found)
}

/// An open Wi-Fi tunnel. Keep `control` alive: the device closes the tunnel when it goes.
pub struct Tunnel {
    pub adapter: Adapter,
    pub rsd_port: u16,
    pub control: RemotePairingClient<RpPairingSocket<TcpStream>>,
}

/// Pair-verify with the stored record (never a new pairing: that would ask on the device),
/// then the TLS-PSK tunnel.
pub async fn connect(addr: SocketAddr, record: &Record) -> Result<Tunnel, String> {
    let host = hostname();
    let mut file = rp_file(record, &host_identifier(&host))?;
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let mut control = RemotePairingClient::new(RpPairingSocket::new(stream), &host);
    control
        .attempt_pair_verify()
        .await
        .map_err(|e| format!("pair-verify start: {e:?}"))?;
    control
        .validate_pairing(&mut file)
        .await
        .map_err(|e| format!("pair-verify (pair over USB again?): {e:?}"))?;
    let port = control
        .create_tcp_listener()
        .await
        .map_err(|e| format!("tunnel listener: {e:?}"))?;
    let data = TcpStream::connect(SocketAddr::new(addr.ip(), port))
        .await
        .map_err(|e| format!("connect tunnel port {port}: {e}"))?;
    zenlink_core::quick_ack(&data);
    let _ = data.set_nodelay(true); // touches and keys go out at once
    let tunnel = connect_tls_psk_tunnel_native(data, control.encryption_key())
        .await
        .map_err(|e| format!("TLS-PSK tunnel: {e:?}"))?;
    let info = tunnel.info.clone();
    let ours: IpAddr = info
        .client_address
        .parse()
        .map_err(|e| format!("tunnel address: {e}"))?;
    let theirs: IpAddr = info
        .server_address
        .parse()
        .map_err(|e| format!("tunnel address: {e}"))?;
    // Same as CoreDeviceProxy::create_software_tunnel for the USB tunnel.
    let mut adapter = Adapter::new(Box::new(tunnel.into_inner()), ours, theirs);
    adapter.set_mss((info.mtu as usize).saturating_sub(60));
    Ok(Tunnel {
        adapter,
        rsd_port: info.server_rsd_port,
        control,
    })
}

/// One-time Wi-Fi setup over the cable (port of backend apple/wifi.py `prepare`): turns on
/// Wi-Fi connections and, on iOS 17+, writes the RemotePairing record the Wi-Fi tunnel needs
/// (pymobiledevice3's format, so the backend's Hybrid reads it too). The device may ask to
/// Trust. Returns whether a RemotePairing record is in place (iOS < 17 has none).
pub async fn prepare(udid: &str) -> Result<bool, String> {
    use idevice::{
        lockdown::LockdownClient, provider::IdeviceProvider as _,
        remote_pairing::RemotePairingLockdownService, IdeviceService as _,
    };
    let provider = crate::link::provider(udid).await?;
    let mut lockdown = LockdownClient::connect(&provider)
        .await
        .map_err(|e| format!("lockdown: {e:?}"))?;
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(|e| format!("pair record (Trust this computer): {e:?}"))?;
    lockdown
        .start_session(&pairing)
        .await
        .map_err(|e| format!("lockdown session: {e:?}"))?;
    lockdown
        .set_value(
            "EnableWifiConnections",
            true.into(),
            Some("com.apple.mobile.wireless_lockdown"),
        )
        .await
        .map_err(|e| format!("Wi-Fi connections: {e:?}"))?;
    let host = hostname();
    let id = host_identifier(&host);
    let dir = record_dir().ok_or("no user profile")?;
    let old = records(&dir).into_iter().find(|r| r.udid == udid);
    let mut file = match &old {
        Some(r) => rp_file(r, &id)?,
        None => {
            let mut f = RpPairingFile::generate(&host);
            f.identifier = id;
            f
        }
    };
    let paired = async {
        let mut client = RemotePairingLockdownService::connect(&provider)
            .await
            .map_err(|e| format!("{e:?}"))?
            .into_client(&host)
            .map_err(|e| format!("{e:?}"))?;
        client
            // idevice slices pin[..6] and panics on a shorter one (#9, iOS 27); a
            // wrong PIN only fails SRP. "000000" is what idevice itself uses over USB.
            .connect(&mut file, || async { "000000".to_string() })
            .await
            .map_err(|e| format!("{e:?}"))
    }
    .await;
    if let Err(e) = paired {
        log::info!("wifi: RemotePairing record not written: {e}");
        return Ok(false);
    }
    if old
        .as_ref()
        .is_some_and(|r| r.public_key == file.public_key_bytes())
    {
        return Ok(true); // verified with the stored record
    }
    let Some(irk) = file.alt_irk().map(<[u8]>::to_vec) else {
        return Err("pairing gave no altIRK".into());
    };
    let mut d = plist::Dictionary::new();
    d.insert("peer_alt_irk".into(), plist::Value::Data(irk));
    d.insert(
        "private_key".into(),
        plist::Value::Data(file.private_key_bytes()),
    );
    d.insert(
        "public_key".into(),
        plist::Value::Data(file.public_key_bytes()),
    );
    d.insert("remote_unlock_host_key".into(), "".into());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    plist::to_file_xml(dir.join(format!("remote_{udid}.plist")), &d).map_err(|e| e.to_string())?;
    Ok(true)
}

fn rp_file(r: &Record, identifier: &str) -> Result<RpPairingFile, String> {
    let mut d = plist::Dictionary::new();
    d.insert(
        "public_key".into(),
        plist::Value::Data(r.public_key.clone()),
    );
    d.insert(
        "private_key".into(),
        plist::Value::Data(r.private_key.clone()),
    );
    d.insert("identifier".into(), identifier.into());
    d.insert(
        "alt_irk".into(),
        plist::Value::Data(r.peer_alt_irk.to_vec()),
    );
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, &d).map_err(|e| format!("pair record: {e}"))?;
    RpPairingFile::from_bytes(&bytes).map_err(|e| format!("pair record: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_identifier_matches_pymobiledevice3() {
        // python: str(uuid.uuid3(uuid.NAMESPACE_DNS, "ZenvyChan")).upper()
        assert_eq!(host_identifier("ZenvyChan"), PY_HOST_ID);
    }

    #[test]
    fn auth_tag_round_trip() {
        let irk: [u8; 16] = core::array::from_fn(|i| i as u8);
        let ident = "D45C330B-B1B7-4E60-842C-C3ED8C81C91B";
        let tag = base64::engine::general_purpose::STANDARD.encode(compute_auth_tag(&irk, ident));
        // python: base64(pymobiledevice3.remote.siphash.compute_auth_tag(bytes(range(16)), ident))
        assert_eq!(tag, "MxMIjju5");
        assert!(auth_tag_matches(&irk, ident, &tag));
        assert!(!auth_tag_matches(&[0; 16], ident, &tag));
        assert!(!auth_tag_matches(&irk, ident, "!!"));
    }

    const PY_HOST_ID: &str = "25C323A2-B6C8-3641-8C3D-C99D5184F57E";
}
