//! An open RSD link to a device: the USB tunnel (CoreDeviceProxy) or the Wi-Fi tunnel
//! (RemotePairing), both into idevice's in-process TCP stack, plus the Developer Disk Image.

use idevice::{
    amfi::AmfiClient,
    core_device_proxy::CoreDeviceProxy,
    cryptexd::{install_ddi, installed_ddi, Cryptex1Assets},
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider as _, UsbmuxdProvider},
    remote_pairing::{RemotePairingClient, RpPairingSocket},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    usbmuxd::{UsbmuxdAddr, UsbmuxdConnection},
    IdeviceService, RsdService as _,
};
use tokio::net::TcpStream;

use crate::ddi::{self, Kind};
use crate::wifi;

pub const DISPLAY_SERVICE: &str = "com.apple.coredevice.displayservice";

/// What lockdown says about a device (USB only).
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub name: String,
    pub product_type: String,
    pub version: String,
    pub device_class: String,
}

pub struct Link {
    pub adapter: AdapterHandle,
    pub rsd: RsdHandshake,
    rsd_port: u16,
    pub wifi: bool,
    /// USB: the usbmuxd provider (the image mounter needs lockdown).
    provider: Option<UsbmuxdProvider>,
    /// Wi-Fi: the RemotePairing connection; the device drops the tunnel when it closes.
    _control: Option<RemotePairingClient<RpPairingSocket<TcpStream>>>,
}

fn err(what: &str) -> impl Fn(idevice::IdeviceError) -> String + '_ {
    move |e| format!("{what}: {e:?}")
}

/// Prefix of the error when usbmuxd (Apple Mobile Device Service) cannot be reached.
pub const SERVICE_ERR: &str = "Apple Mobile Device Service:";

pub async fn usbmux() -> Result<UsbmuxdConnection, String> {
    UsbmuxdConnection::default()
        .await
        .map_err(|e| format!("{SERVICE_ERR} {e:?}"))
}

/// `ensure_ddi`'s error when Developer Mode is off (the Developer Disk Image needs it).
pub const DEV_MODE_OFF: &str = "Developer Mode is off";

/// Developer Mode on? When it is off, its switch is revealed in Settings › Privacy & Security
/// (iOS hides it until a developer tool asks). An unknown state (AMFI unavailable) counts as on,
/// so the mount itself reports what is wrong.
async fn developer_mode(provider: &UsbmuxdProvider) -> bool {
    let Ok(mut amfi) = AmfiClient::connect(provider).await else {
        return true;
    };
    match amfi.get_developer_mode_status().await {
        Ok(false) => {
            if let Err(e) = amfi.reveal_developer_mode_option_in_ui().await {
                log::warn!("amfi: reveal Developer Mode: {e:?}");
            }
            false
        }
        _ => true,
    }
}

/// Devices on USB with whether this PC has their pair record (Trust done).
pub async fn usb_devices() -> Result<Vec<(String, bool)>, String> {
    let mut mux = usbmux().await?;
    let list = mux.get_devices().await.map_err(err("device list"))?;
    let mut out = Vec::new();
    for d in list
        .into_iter()
        .filter(|d| d.connection_type == idevice::usbmuxd::Connection::Usb)
    {
        let paired = mux.get_pair_record(&d.udid).await.is_ok();
        out.push((d.udid, paired));
    }
    Ok(out)
}

/// The device's names and iOS version over lockdown. Needs Trust (a pair record).
pub async fn info(udid: &str) -> Result<Info, String> {
    let provider = provider(udid).await?;
    let mut lockdown = LockdownClient::connect(&provider)
        .await
        .map_err(err("lockdown"))?;
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(err("pair record (Trust this computer)"))?;
    lockdown
        .start_session(&pairing)
        .await
        .map_err(err("lockdown session"))?;
    Ok(read_info(&mut lockdown).await)
}

async fn read_info(lockdown: &mut LockdownClient) -> Info {
    let mut get = async |k: &str| -> String {
        lockdown
            .get_value(Some(k), None)
            .await
            .ok()
            .and_then(|v| v.as_string().map(str::to_owned))
            .unwrap_or_default()
    };
    Info {
        name: get("DeviceName").await,
        product_type: get("ProductType").await,
        version: get("ProductVersion").await,
        device_class: get("DeviceClass").await,
    }
}

pub(crate) async fn provider(udid: &str) -> Result<UsbmuxdProvider, String> {
    let mut mux = usbmux().await?;
    let dev = mux.get_device(udid).await.map_err(err("device"))?;
    let addr = UsbmuxdAddr::from_env_var().map_err(|e| format!("usbmuxd address: {e}"))?;
    Ok(dev.to_provider(addr, "zenlink"))
}

impl Link {
    pub async fn usb(udid: &str) -> Result<Self, String> {
        let provider = provider(udid).await?;
        let proxy = CoreDeviceProxy::connect(&provider)
            .await
            .map_err(err("CoreDeviceProxy (iOS 17.4 or later?)"))?;
        let rsd_port = proxy.tunnel_info().server_rsd_port;
        let adapter = proxy
            .create_software_tunnel()
            .map_err(err("tunnel"))?
            .to_async_handle();
        Self::finish(adapter, rsd_port, false, Some(provider), None).await
    }

    pub async fn wifi(addr: std::net::SocketAddr, record: &wifi::Record) -> Result<Self, String> {
        let t = wifi::connect(addr, record).await?;
        Self::finish(
            t.adapter.to_async_handle(),
            t.rsd_port,
            true,
            None,
            Some(t.control),
        )
        .await
    }

    async fn finish(
        mut adapter: AdapterHandle,
        rsd_port: u16,
        wifi: bool,
        provider: Option<UsbmuxdProvider>,
        control: Option<RemotePairingClient<RpPairingSocket<TcpStream>>>,
    ) -> Result<Self, String> {
        let s = adapter
            .connect(rsd_port)
            .await
            .map_err(|e| format!("RSD connect: {e:?}"))?;
        let rsd = RsdHandshake::new(s).await.map_err(err("RSD handshake"))?;
        Ok(Self {
            adapter,
            rsd,
            rsd_port,
            wifi,
            provider,
            _control: control,
        })
    }

    /// The device's names and iOS version over the link (trusted lockdown through RSD), so a
    /// device first seen on Wi-Fi gets its name too.
    pub async fn info(&mut self) -> Result<Info, String> {
        let mut lockdown = LockdownClient::connect_rsd(&mut self.adapter, &mut self.rsd)
            .await
            .map_err(err("lockdown over RSD"))?;
        Ok(read_info(&mut lockdown).await)
    }

    /// The screen's size in pixels, portrait (lockdown's iTunes domain).
    pub async fn screen(&mut self) -> Option<(u32, u32)> {
        let mut lockdown = LockdownClient::connect_rsd(&mut self.adapter, &mut self.rsd)
            .await
            .ok()?;
        let domain = Some("com.apple.mobile.iTunes");
        let num = |v: Result<plist::Value, _>| {
            let v = v.ok()?;
            v.as_unsigned_integer()
                .or_else(|| v.as_signed_integer().map(|i| i as u64))
        };
        let w = num(lockdown.get_value(Some("ScreenWidth"), domain).await)?;
        let h = num(lockdown.get_value(Some("ScreenHeight"), domain).await)?;
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    pub fn display_offered(&self) -> bool {
        self.rsd.services.contains_key(DISPLAY_SERVICE)
    }

    /// Makes sure the Developer Disk Image is there (pymobiledevice3's order: already there?
    /// then Apple's TSS). `progress` gets a short note before a download-sized step.
    /// Installing needs USB; over Wi-Fi a missing DDI is an error.
    pub async fn ensure_ddi(
        &mut self,
        version: &str,
        mut progress: impl FnMut(&str),
    ) -> Result<(), String> {
        if self.display_offered() {
            return Ok(());
        }
        let kind = Kind::for_version(version).ok_or("iOS 17 or later is needed")?;
        if kind == Kind::Cryptex
            && installed_ddi(&mut self.adapter, &mut self.rsd)
                .await
                .map_err(err("cryptexd"))?
                .is_some()
        {
            return Ok(());
        }
        let Some(provider) = self.provider.as_ref() else {
            return Err("Developer Disk Image missing: connect the cable once".into());
        };
        if !developer_mode(provider).await {
            return Err(DEV_MODE_OFF.into());
        }
        let dirs = ddi::search_dirs(kind);
        let dir = ddi::find(kind, &dirs).ok_or("Developer Disk Image files not found")?;
        progress("Preparing the device (first time)");
        let f = ddi::read(kind, &dir).map_err(|e| format!("read DDI: {e}"))?;
        match kind {
            Kind::Cryptex => {
                let manifest: plist::Dictionary = plist::from_bytes(&f.build_manifest)
                    .map_err(|e| format!("BuildManifest: {e}"))?;
                let identity = idevice::tss::select_cryptex_build_identity(&manifest)
                    .map_err(err("build identity"))?
                    .clone();
                let assets = Cryptex1Assets::from_parts(
                    f.image,
                    f.trustcache,
                    f.cryptex_info,
                    f.root_hash,
                    identity,
                );
                install_ddi(&mut self.adapter, &mut self.rsd, &assets)
                    .await
                    .map_err(err("cryptex install"))?;
            }
            Kind::Personalized => {
                let mut mounter = ImageMounter::connect(provider)
                    .await
                    .map_err(err("image mounter"))?;
                let mut lockdown = LockdownClient::connect(provider)
                    .await
                    .map_err(err("lockdown"))?;
                let ecid = lockdown
                    .get_value(Some("UniqueChipID"), None)
                    .await
                    .ok()
                    .and_then(|v| v.as_unsigned_integer())
                    .ok_or("UniqueChipID")?;
                mounter
                    .mount_personalized(
                        provider,
                        f.image,
                        f.trustcache,
                        &f.build_manifest,
                        None,
                        ecid,
                    )
                    .await
                    .map_err(err("personalized mount"))?;
            }
        }
        // The display service appears in a fresh RSD handshake after the install.
        let s = self
            .adapter
            .connect(self.rsd_port)
            .await
            .map_err(|e| format!("RSD reconnect: {e:?}"))?;
        self.rsd = RsdHandshake::new(s).await.map_err(err("RSD handshake"))?;
        if self.display_offered() {
            Ok(())
        } else {
            Err("the screen service is still missing after the Developer Disk Image".into())
        }
    }
}
