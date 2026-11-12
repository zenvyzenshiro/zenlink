//! FairPlay (fp-setup) for AirPlay mirroring. The four setup replies are fixed data (from
//! shairplay/UxPlay, GPL-3.0); decrypting the session key needs playfair, compiled from
//! ZenshiroDev/resources/windows/native/playfair into playfair.dll.

use std::path::Path;
use std::sync::OnceLock;

const REPLY_MESSAGES: [&str; 4] = [
    "46504c59030102000000008202000f9f3f9e0a2521dbdf312ab2bfb29e8d232b6376a8c818701d22ae93d82737feaf9db4fdf41c2dba9d1f49caaabf6591ac1f7bc6f7e0663d21afe01565953eab81f418ceed095adb7c3d0e254909a79831d49c3982973434facb42c63a1cd911a6fe941a8a6d4a743b46c3a7649e44c78955e49d8155009549c4e2f7a3f6d5ba",
    "46504c5903010200000000820201cf32a25714b2524f8aa0ad7af164e37bcf4424e200047efc0ad67afcd95ded1c2730bb591b962ed63a9c4ded88ba8fc78de64d91ccfd5c7b56da88e31f5cceafc7431995a01665a54e1939d25b94db64b9e45d8d063e1e6af07e9656162b0efa404275ea5a44d9591c7256b9fbe6513898b80227721988571650942ad946688a",
    "46504c5903010200000000820202c169a352eeed35b18cdd9c58d64f16c1519a89eb5317bd0d4336cd68f638ff9d016a5b52b7fa9216b2b65482c78444118121a2c7fed83db7119e9182aad7d18c7063e2a457555910af9e0efc76347d164043807f581ee4fbe42ca9dedc1b5eb2a3aa3d2ecd59e7eee70b3629f22afd161d877353ddb99adc8e07006e56f850ce",
    "46504c59030102000000008202039001e1727e0f57f9f5880db104a6257a23f5cfff1abbe1e93045251afb97eb9fc0011ebe0f3a81df5b691d76acb2f7a5c708e3d328f56bb39dbde5f29c8a17f481487e3ae863c678325422e6f78e166d18aa7fd636258bce28726f661f738893ce44311e4be6c0535193e5ef72e8686233729c227d820c999445d89246c8c359",
];
const FP_HEADER: &str = "46504c590301040000000014";

type Decrypt = unsafe extern "C" fn(*const u8, *const u8, *mut u8);

static DLL: OnceLock<(libloading::Library, Decrypt)> = OnceLock::new();

/// Loads playfair.dll once (the first path that loads wins; a failure is tried again
/// next time, for example after antivirus let the file go).
pub fn load(dll: &Path) -> Result<(), String> {
    if DLL.get().is_some() {
        return Ok(());
    }
    // SAFETY: playfair.dll is ZenLink's own build of the bundled playfair sources;
    // zl_playfair_decrypt(keymsg[164], ekey[72], out[16]) has no other side effects.
    let loaded = unsafe {
        let lib = libloading::Library::new(dll).map_err(|e| format!("{}: {e}", dll.display()))?;
        let f: Decrypt = *lib
            .get::<Decrypt>(b"zl_playfair_decrypt\0")
            .map_err(|e| e.to_string())?;
        (lib, f)
    };
    let _ = DLL.set(loaded);
    Ok(())
}

#[derive(Default)]
pub struct FairPlay {
    keymsg: Option<Vec<u8>>,
}

impl FairPlay {
    /// A `POST /fp-setup` body: 16 bytes (setup) or 164 (handshake). `None`: not supported.
    pub fn handle(&mut self, req: &[u8]) -> Option<Vec<u8>> {
        if req.len() < 5 || req[4] != 0x03 {
            return None;
        }
        match req.len() {
            16 => {
                self.keymsg = None;
                REPLY_MESSAGES.get(req[14] as usize).map(|m| crate::hex(m))
            }
            164 => {
                self.keymsg = Some(req.to_vec());
                let mut out = crate::hex(FP_HEADER);
                out.extend_from_slice(&req[144..164]);
                Some(out)
            }
            _ => None,
        }
    }

    /// The session key from SETUP's 72-byte `ekey`.
    pub fn decrypt(&self, ekey: &[u8]) -> Result<[u8; 16], String> {
        let keymsg = self
            .keymsg
            .as_ref()
            .ok_or("FairPlay handshake not finished")?;
        if ekey.len() != 72 {
            return Err("FairPlay key is not 72 bytes".into());
        }
        let (_, f) = DLL.get().ok_or("playfair.dll not loaded")?;
        let mut out = [0u8; 16];
        // SAFETY: keymsg is 164 bytes, ekey 72, out 16, as the function expects.
        unsafe { f(keymsg.as_ptr(), ekey.as_ptr(), out.as_mut_ptr()) };
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_replies_and_key_match_python() {
        let dll = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../resources/windows/native/playfair/playfair.dll");
        load(&dll).unwrap();
        let mut fp = FairPlay::default();
        let setup = [
            0x46, 0x50, 0x4C, 0x59, 3, 1, 1, 0, 0, 0, 0, 4, 2, 0, 2, 0xBB,
        ];
        assert_eq!(fp.handle(&setup).unwrap(), crate::hex(REPLY_MESSAGES[2]));
        let mut msg3 = vec![0x46, 0x50, 0x4C, 0x59, 3];
        msg3.resize(164, 0);
        assert_eq!(fp.handle(&msg3).unwrap().len(), 12 + 20);
        let ekey: Vec<u8> = (0..72).collect();
        assert_eq!(
            fp.decrypt(&ekey).unwrap().to_vec(),
            crate::hex(crate::vectors::FP_KEY)
        );
        assert!(fp.handle(&[0; 16]).is_none(), "not FairPlay v3");
    }
}
