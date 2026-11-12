//! The receiver's stable identity: an Ed25519 key and a locally administered MAC, kept in
//! `airplay.json` in the data folder (shared with the Python receiver, so iOS sees one receiver).

use std::path::Path;

use ed25519_dalek::SigningKey;
use serde_json::{json, Value};

pub struct Identity {
    pub key: SigningKey,
    /// "AA:BB:CC:DD:EE:FF"
    pub mac: String,
}

impl Identity {
    /// Reads `dir/airplay.json`, or makes a new identity and saves it (best effort).
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("airplay.json");
        let read = || -> Option<Self> {
            let v: Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            let seed: [u8; 32] = crate::hex(
                v.get("seed")?
                    .as_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))?,
            )
            .try_into()
            .ok()?;
            let mac = v
                .get("mac")?
                .as_str()
                .filter(|m| m.len() == 17 && m.is_ascii())?
                .to_string();
            Some(Self {
                key: SigningKey::from_bytes(&seed),
                mac,
            })
        };
        if let Some(id) = read() {
            return id;
        }
        let seed: [u8; 32] = rand::random();
        let mut mac: [u8; 6] = rand::random();
        mac[0] = (mac[0] | 0x02) & 0xFE; // locally administered, unicast
        let mac = mac
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
        if let Err(e) = std::fs::write(&path, json!({ "seed": hex, "mac": mac }).to_string()) {
            log::warn!("airplay: identity not saved: {e}");
        }
        Self {
            key: SigningKey::from_bytes(&seed),
            mac,
        }
    }

    /// The public key iOS pins (`pk`).
    pub fn pk(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn made_once_then_read_back() {
        let dir = std::env::temp_dir().join(format!("zl-airplay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = super::Identity::load(&dir);
        let b = super::Identity::load(&dir);
        assert_eq!(a.pk(), b.pk());
        assert_eq!(a.mac, b.mac);
        let first = u8::from_str_radix(&a.mac[..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_the_python_file() {
        let dir = std::env::temp_dir().join(format!("zl-airplay-py-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let seed: String = (0u8..32).map(|b| format!("{b:02x}")).collect();
        std::fs::write(
            dir.join("airplay.json"),
            format!(r#"{{"seed": "{seed}", "mac": "0A:11:22:33:44:55"}}"#),
        )
        .unwrap();
        let id = super::Identity::load(&dir);
        assert_eq!(id.pk().to_vec(), crate::hex(crate::vectors::SERVER_PK));
        assert_eq!(id.mac, "0A:11:22:33:44:55");
        let _ = std::fs::remove_dir_all(dir);
    }
}
