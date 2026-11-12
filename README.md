<p align="center">
  <img src="ZenshiroDev/resources/assets/logo.png" width="180" height="180" alt="ZenLink logo" style="border-radius: 40px; box-shadow: 0 16px 36px rgba(255, 79, 163, 0.35);">
</p>

<h1 align="center">ZenLink</h1>

<h3 align="center">Your phone, on your PC.</h3>

<p align="center">
  <b>iPhone, iPad &amp; Android screen mirroring for Windows</b> — native Rust, hardware-decoded H.264 / H.265, smooth in OBS.<br>
  60 fps on iPhone &amp; iPad, up to 120 fps on Android. Cable or Wi-Fi. No app on Android. Free and open source.
</p>

<p align="center">
  <a href="../../releases/latest"><img src="https://img.shields.io/badge/⬇_Download-v2.5.14-FF4FA3?style=for-the-badge" alt="Download v2.5.14"></a>
  <a href="https://zenvyzenshiro.github.io/zenlink/"><img src="https://img.shields.io/badge/Website-zenlink-1f1f2e?style=for-the-badge&logo=googlechrome&logoColor=white" alt="Website"></a>
</p>

<p align="center">
  <code>iPhone</code> · <code>iPad</code> · <code>Android</code> · <code>USB</code> · <code>Wi-Fi</code> · <code>AirPlay</code> · <code>scrcpy</code> · <code>OBS</code> · <code>OCR Alt+C</code> · <code>Mouse &amp; keyboard</code> · <code>Synced sound</code>
</p>

<p align="center">
  <a href="https://zenvyzenshiro.github.io/zenlink/"><img src="https://img.shields.io/badge/Website-zenvyzenshiro.github.io%2Fzenlink-FF4FA3?style=flat-square&logo=googlechrome&logoColor=white" alt="Website"></a>
  <a href="https://github.com/zenvyzenshiro"><img src="https://img.shields.io/badge/Developer-Zenvy%20Zenshiro-FF4FA3?style=flat-square&logo=github" alt="Developer: Zenvy Zenshiro"></a>
  <a href="https://sociabuzz.com/zenvy/tribe" target="_blank"><img src="https://img.shields.io/badge/♥_Support_on-Sociabuzz-FF4FA3?style=flat-square&logo=buymeacoffee&logoColor=white" alt="Support on Sociabuzz"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-GPL--3.0-green.svg?style=flat-square" alt="License: GPL-3.0"></a>
  <img src="https://img.shields.io/badge/Verified%20OS-Windows%2011%20(Build%2026200%2C%2064--bit)-0078D4.svg?style=flat-square&logo=windows" alt="Verified OS: Windows 11 build 26200">
  <img src="https://img.shields.io/badge/Verified%20Device-iPad%20(11th%20gen%2C%20iOS%2027.0.1)-000000.svg?style=flat-square&logo=apple" alt="Verified device: iPad (11th gen), iOS 27.0.1">
  <a href="../../releases/latest"><img src="https://img.shields.io/github/v/release/zenvyzenshiro/zenlink?style=flat-square&color=FF4FA3" alt="Latest release"></a>
  <a href="../../releases"><img src="https://img.shields.io/github/downloads/zenvyzenshiro/zenlink/total?style=flat-square&color=FF4FA3&label=Downloads" alt="Total downloads"></a>
  <a href="../../stargazers"><img src="https://img.shields.io/github/stars/zenvyzenshiro/zenlink?style=flat-square&color=FF4FA3" alt="GitHub stars"></a>
</p>

> ### 🛡️ Tested & Verified Environment
> - 📱 **Device**: **iPad (11th generation)**, physical device running **iOS 27.0.1**
> - 💻 **PC**: **Windows 11 Pro**, build 26200, 64-bit
> - 🚀 **Measured**: 60 fps on iPhone / iPad and up to 120 fps on Android (tested by the developer on real devices); ZenLink Studio cable 52–56 fps at 12–30 Mbps; ZenLink Studio AirPlay 3108×2160 at ~46 fps with ~16 ms picture delay; native sound output ~23 ms (WASAPI)

---

### 🌟 Author, Attribution & Open-Source Lineage
> - **Creator & Lead Developer**: **Zenvy Zenshiro** ([GitHub `@zenvyzenshiro`](https://github.com/zenvyzenshiro) · [Sociabuzz](https://sociabuzz.com/zenvy/tribe)).
>   Designed and built ZenLink: the Apple connection modes with a driver manager and safe rollback, ZenLink Studio (QuickTime capture over libusb0, and an AirPlay screen-mirroring receiver), Wi-Fi for Apple Driver mode without Bonjour, the Android client, hardware-decoded low-latency playback, native WASAPI sound with lip sync and a Wi-Fi jitter buffer, OCR copy, stealth dim, wake & unlock, the system tray, and the black + pink interface in Indonesian and English (more languages as files).
> - **Original open-source sources ZenLink builds on** (all used under their own licenses):
>   - [**pane-iphone-mirror-windows**](https://github.com/teyboxv1/pane-iphone-mirror-windows) by **teyboxv1** (GPL-3.0) — the original Pane project: iPhone mirroring on Windows through CoreDevice.
>   - [**pymobiledevice3**](https://github.com/doronz88/pymobiledevice3) by **Doron Zavelevsky** (GPL-3.0) — RemoteXPC / RSD tunnel, CoreDevice screen stream, lockdown and pairing. Used up to 2.5.11; since 2.5.12 the Rust crate `idevice` does this work inside ZenLink.exe.
>   - [**quicktime_video_hack**](https://github.com/danielpaulus/quicktime_video_hack) by **Daniel Paulus** (MIT) — reference for the QuickTime screen-capture USB protocol used by ZenLink Studio (cable).
>   - [**libusb-win32**](https://sourceforge.net/projects/libusb-win32/) by **Stephan Meyer & Travis Lee Robinson** (driver GPL, library LGPL) — the libusb0 driver bundled in `drivers/libusb-win32` (unmodified, signed binaries; source at the link).
>   - [**libwdi / Zadig**](https://github.com/pbatard/libwdi) by **Pete Batard** (LGPL / GPL) — the driver-package approach ZenLink follows; Zadig itself is not bundled.
>   - [**scrcpy**](https://github.com/Genymobile/scrcpy) by **Genymobile & Romain Vimont** (Apache-2.0) — the official scrcpy server (v5.0.1, unmodified) runs on the Android phone; ZenLink's Android client is its own Rust code.
>   - **Android SDK Platform-Tools `adb`** by **Google** (Android Open Source Project, Apache-2.0; `NOTICE.txt` included) — not bundled: ZenLink downloads Google's official, unmodified zip on request (SHA-256 checked), see [ZenshiroDev/docs/THIRD_PARTY.md](ZenshiroDev/docs/THIRD_PARTY.md).
>   - [**UxPlay**](https://github.com/FDH2/UxPlay) by **F. Duncanh** and [**RPiPlay / shairplay**](https://github.com/FD-/RPiPlay) (GPL-3.0) — the AirPlay mirroring protocol reference. ZenLink's AirPlay receiver is written in Rust; the **playfair** FairPlay code in `native/playfair` is used unmodified (GPL-3.0).

---

## 🚀 Key Features

| Feature | Description |
| :--- | :--- |
| **🤖 Android (new in v2.5)** | Android 5.0+ over USB, or Wi-Fi (one click from a USB connection, or Android 11+ wireless debugging: scan a QR code shown by ZenLink, or enter a pairing code). Picture, sound (Android 11+), mouse/keyboard control, clipboard, real device names, per-device settings, video profiles and Smart Encoder Detection (H.264 by default; H.265 only when the phone and Windows both decode it, with automatic fallback). Uses Google's official `adb` (downloaded by ZenLink on first use, about 8 MB) and the official scrcpy server (bundled). |
| **🪶 One program, small download (v2.5.12+)** | Everything runs inside one `ZenLink.exe` (Rust): Apple Driver, ZenLink Studio, AirPlay, Android and the driver settings. No Python helper. Setup is 9.3 MB (it was 60.8 MB). |
| **📱 Device picker** | Lists every connected iPhone / iPad (USB or Wi-Fi, iOS version, status) and the ones you used before; since v2.5.13 Android phones work the same way. Click a device, then press Start; with nothing picked, ZenLink asks you to pick one first. |
| **🧩 Android apps launcher (new in v2.5.1)** | **Apps** in the dock: a searchable grid of the phone's apps. Open one on the phone's screen or, on Android 10+, in its own window. |
| **📂 Drop files on the window (new in v2.5.1)** | Android: an `.apk` is installed, other files go to Download. iPhone / iPad (Apple Driver): files land in an app with File Sharing (VLC, Documents …). |
| **⬆️ In-app updates (new in v2.5.1)** | Settings › Updates checks GitHub (Stable or Beta), downloads with a SHA-256 check and updates in place, keeping your settings. |
| **🔌 Two Apple connection modes** | Choose before anything connects: **Apple Driver** (Apple's own driver, full control) or **ZenLink Studio** (for live streaming, like TikTok LIVE Studio). Each works **wired (USB)** or over **Wi-Fi**. "Remember my choice" skips the question next time. |
| **📡 ZenLink Studio over Wi-Fi (AirPlay)** | ZenLink is an AirPlay receiver: on the iPhone/iPad open **Control Center → Screen Mirroring → ZenLinkStudio**. No driver, no cable. The device's own stream is passed through untouched (HEVC or H.264, up to its native resolution); the how-to is shown on screen. |
| **🎥 ZenLink Studio over the cable** | QuickTime capture over USB in H.264 at the device's full native resolution and high bitrate (measured 2360×1640 on iPad 11th gen, 12–30 Mbps, up to ~56 fps). Picture and sound only. |
| **📶 Apple Driver over Wi-Fi** | Plug the device in once in Apple Driver mode and ZenLink sets up Wi-Fi by itself; afterwards choose **Wi-Fi**. ZenLink finds the device on the network itself, even where Windows gets no Bonjour replies. |
| **📊 Live stats** | Measured decode and display FPS next to the device name (switchable in Settings). |
| **⚡ Native window (Rust)** | Media Foundation hardware decoding (H.264 / HEVC) into a Direct3D 11 swap chain, one frame per refresh, Liquid Glass UI (Slint). No WebView2, no Visual C++ runtime needed. |
| **🎬 OBS ready** | Game Capture of the ZenLink window stays smooth and survives rotation (Settings › Record / stream in OBS). |
| **🔊 Synced sound** | Native WASAPI output (~22 ms) with an adaptive buffer and lip sync, so picture and sound arrive together. |
| **🖱️ Touch, keyboard & buttons** | Mouse as touch, PC keyboard typing, scrolling, Home / Recent Apps / Notifications / Control Center or Quick Settings, hardware buttons — follows rotation. |
| **📋 OCR copy & paste** | `Alt+C` copies the text on the device screen to the PC (Windows OCR, stitches scrolled pages). Paste the PC clipboard: `Alt+V` (Apple), `Ctrl+V` (Android). |
| **🌑 Stealth dim, wake & lock** | `Alt+D` dims the device to 0% while the picture stays bright; `Alt+U` wakes and types your passcode (DPAPI-encrypted); `Alt+L` locks. |
| **🛠️ Safe driver manager** | Install / restore drivers only after you confirm, with UAC, a backup and automatic rollback. Never stacks a libusb0 filter on Apple's driver. |
| **📥 Closing, tray & start with Windows** | The close button asks whether to keep ZenLink in the notification area (the session keeps running) or quit, with "Remember my choice"; Settings › Startup & closing changes it later. Start with Windows is off by default and follows Task Manager › Startup apps. Quitting also stops the `adb` server ZenLink started. One ZenLink at a time: opening it again brings the window back. |
| **🌐 Languages as files** | Bahasa Indonesia and English built in, following your Windows language. Anyone can add a language with one `.po` file, no build needed ([how](#-add-a-language-tambah-bahasa)). Liquid Glass Dark by default; Light and System too. |

---

## ⌨️ Keyboard Shortcuts

The same list is in **Settings › Keyboard shortcuts**. Click the picture first so the keys go to ZenLink; while the keyboard types to the device, plain keys go to the device and the Alt / Ctrl shortcuts stay with ZenLink.

| Shortcut | Action | Platform |
| :---: | :--- | :--- |
| **`Alt + M`** | Sound on the PC on / off | both |
| **`F11`** · **`Esc`** · **`Ctrl + ,`** | Full screen · leave full screen · Settings | both |
| **`Alt + H`** | Rotate the view (Auto, 90°, 180°, 270°, 0°) | both |
| **`Alt + ←`** | Back | both |
| **`Alt + C`** · **`Alt + R`** | Copy the text on the screen to the PC (OCR) · clear the copied-text history | both |
| **`Alt + L`** | Lock (Android: power button) | both |
| **`Alt + U`** · **`Alt + D`** · **`Alt + V`** | Wake & unlock (saved passcode) · stealth dim · type the PC clipboard | Apple Driver |
| **`Ctrl + K`** · **`Ctrl + C / X / V / A / Z`** | On-screen keyboard · copy, cut, paste, select all, undo in iOS | Apple Driver |
| **`Ctrl + V`** · **`Ctrl + C / X / A / Z`** | Paste the PC clipboard · copy, cut, select all, undo on the phone | Android |
| **`Alt + N`** · **`Alt + Q`** · **`Alt + S`** | Notifications · Quick Settings · Recent Apps | Android |
| **`Alt + ↑ / ↓`** | Phone volume up / down | Android |

---

## 🧭 First time? The flow in 5 steps (Pertama kali? Alurnya)

1. **Install and open ZenLink** (Setup.exe is easiest, see Quick Start below). The app follows your Windows language: Bahasa Indonesia if Windows lists it, English otherwise; switch any time in Settings.
2. **Pick your device type** on the welcome screen: **iPhone / iPad** or **Android Smartphone / Tablet**, then press **Lanjut / Continue**.
3. **Prepare the device once:**
   - *iPhone / iPad:* plug in the cable and tap **Trust This Computer**. For touch control (Apple Driver mode, iOS 17+), turn on **Developer Mode** (Settings → Privacy & Security). Without it, use **ZenLink Studio** (picture and sound).
   - *Android:* turn on **USB debugging** (Settings › About phone › tap Build number 7×, then Developer options › USB debugging), plug in the cable and tap **Allow** on the phone. The first time, ZenLink asks to **Install adb** (Google's official tool) and shows a progress bar.
4. **Press "Mulai mirroring / Start mirroring".** The device's screen appears; click and type on it to control it (Apple Driver mode and Android).
5. **Optional:** choose where the sound plays (Windows, device or both), switch to Wi-Fi so you can unplug the cable, save a passcode for auto unlock, or add the window to OBS. Each settings card opens and closes with its header.

Stuck? Every screen says what is missing (driver, Developer Mode, USB debugging) and how to fix it. Logs: `%LOCALAPPDATA%\ZenLink-Next\logs`.

## 📦 Quick Start

### 📋 Requirements (Persyaratan)

- **Windows 10 (1809+) or Windows 11, 64-bit.** No WebView2 and no Visual C++ runtime needed. Windows "N" editions need Microsoft's free Media Feature Pack (ZenLink tells you and opens the page).
- **Android:** Android 5.0+ with **USB debugging** on (Settings › About phone › tap Build number 7×, then Developer options › USB debugging), a data cable, and **Allow** on the phone. Sound needs Android 11+. ZenLink downloads Google's official `adb` the first time you pick Android (one click, about 8 MB); some phones (for example Samsung) need the maker's USB driver, which Windows Update usually installs.
- **iPhone / iPad: Apple Devices** (Microsoft Store) or iTunes, for Apple's USB driver and Apple Mobile Device Service.
- **Apple Driver mode** needs **iOS 17 or newer** with **Developer Mode** on: connect the cable, tap **Trust This Computer**, then **Settings → Privacy & Security → Developer Mode → On** and restart. *(If the toggle is missing, opening a screen-mirroring tool once — e.g. 3uTools → Realtime Screen — makes it appear.)* On iOS 17–26 it runs **Hybrid Control** (see the matrix below).
- **ZenLink Studio** works without Developer Mode (cable or AirPlay). AirPlay needs the PC and the device on the same network; the installer (or the first run of `Start ZenLink.bat`) offers to allow ZenLink through Windows Firewall.

### 📊 iOS compatibility matrix

| iOS | Apple Driver mode | Picture | Control (touch, keyboard, buttons) | Tested on a real device |
| :--- | :--- | :--- | :--- | :--- |
| **27 and newer** | **Native Control** | CoreDevice DisplayService (HEVC, cable or Wi-Fi) | CoreDevice HID | ✅ iPad (11th gen), iOS 27.0.1 |
| **17 – 26** | **Hybrid Control** *(Experimental)* | AirPlay to ZenLink (same Wi-Fi) | CoreDevice HID over the Apple driver (cable, or Wi-Fi after setup) | ⚠️ Not yet: simulated tests only |
| **16 and older** | Unsupported | Use ZenLink Studio | — | — |
| **Any (ZenLink Studio)** | — | QuickTime capture over libusb0 (cable) or AirPlay (Wi-Fi) | Mirroring only | ✅ Studio cable and AirPlay |

**How Hybrid Control works.** Apple's screen service only sends frames from iOS 27, but older iOS still accepts the stream request, and the CoreDevice HID services only deliver touches while such a stream is open. Hybrid opens that stream just to hold the input path open (its data is discarded), shows the picture from AirPlay instead, and maps touches through the device's orientation. Both paths run at once: AirPlay uses the network, control uses Apple's own USB driver; libusb0 is never involved. ZenLink checks the device's own service list before it claims control; if control can't be opened (for example Developer Mode is off), the picture keeps running and the app shows **Mirroring Only** with the reason.

**Not available on any version:** forcing the device to rotate (ZenLink follows the device's orientation instead) and Bluetooth HID control from Windows (Windows doesn't let apps act as a Bluetooth keyboard or mouse).

### Option 1A: Installer (Recommended)
1. Download **`ZenLink-vX.Y.Z-Setup.exe`** from [**Releases**](../../releases/latest).
2. Run it and follow the steps (Bahasa Indonesia or English). ZenLink appears in the Start Menu and, if you want, on the Desktop.
   Windows 11 Smart App Control blocks it? Use **`ZenLink-vX.Y.Z-Setup-Direct.exe`** instead (19.6 MB): ZenLink.exe installing itself, no helper program. Click **Yes**, and it installs for your user and starts.
3. Open **ZenLink**, choose **iPhone / iPad** or **Android Smartphone / Tablet**, plug in your device and press **Mulai / Start**.

### Option 1B: Portable (one .exe, no installation)
1. Download **`ZenLink-vX.Y.Z-Portable.exe`** from [**Releases**](../../releases/latest) and put it anywhere.
2. Double-click it. Nothing is installed; settings and logs are kept in `%LOCALAPPDATA%\ZenLink-Next`.

### Option 2: Manual (source code, no git)
Download **`ZenLink-vX.Y.Z-Manual.zip`** from [**Releases**](../../releases/latest), extract it and run **`Start ZenLink.bat`** (same first-run setup as below).

### Option 3: Run from Source
```powershell
git clone https://github.com/zenvyzenshiro/zenlink.git
cd zenlink
.\"Start ZenLink.bat"
```
The first run builds `ZenLink.exe` (needs Rust: rustup), then offers Apple Devices (for Apple Driver mode) and the firewall permission for AirPlay. For Android it offers to download Google's official `adb` (platform-tools 37.0.1, about 8 MB, checksum and Google signature checked), unless an Android SDK is already there (`ANDROID_HOME`). Setup.exe and Portable.exe download it inside the app ("Install adb") the first time Android is chosen. Later runs open straight away. **`ZenshiroDev\tooling\windows\scripts\run.bat`** starts ZenLink with a visible console log. Settings of a source checkout are kept in `data\` (ignored by git).

Tests: `cargo test --workspace` (no device needed; `ZenshiroDev\tooling\dev\airplay_sim.py` simulates an iPhone mirroring over AirPlay).

To build the release files yourself: `powershell -ExecutionPolicy Bypass -File .\build_portable_release.ps1` (Rust, Inno Setup 6 for the installer). Everything it builds goes to `out\`.

### Repository layout

| Folder | What it holds |
|---|---|
| `apps/desktop/` | `ZenLink.exe`: the Rust + Slint app (crate `zenlink-app`): window, Apple modes, ZenLink Studio, AirPlay, drivers. |
| `ZenshiroDev/crates/` | Shared Rust libraries: `zenlink-core` (in-process API, updates), `zenlink-apple` (Apple Driver, USB Direct), `zenlink-airplay`, `zenlink-video` (decode + D3D11), `zenlink-audio`, `zenlink-android` (ADB, scrcpy client). |
| `ZenshiroDev/resources/` | `assets/` (icons, logos), `third_party/` (pinned scrcpy server; adb is downloaded here), `windows/drivers` (libusb-win32), `windows/native` (playfair, zlaac: sources and DLLs). |
| `ZenshiroDev/tooling/` | `windows/packaging` (installer, MSIX), `windows/tools` (PowerShell helpers), `windows/scripts` (console run, desktop shortcut), `dev/` (window build helper, AirPlay simulator, fixture generator). |
| `ZenshiroDev/docs/` | Technical notes: Android, Rust, size audit, third-party notices, release notes. |
| `docs/` | The website (GitHub Pages). |

`Cargo.toml` at the root is the Rust workspace. The root also holds the three entry scripts: `Start ZenLink.bat`, `Uninstall-ZenLink.bat` and `build_portable_release.ps1`.

Dev outputs go to ignored folders in the checkout: `target/` (Rust), `data/` (settings of a source run) and `out/` (release build). To keep them outside, put a `Start ZenLink.local.bat` next to `Start ZenLink.bat` or in its parent folder that sets `CARGO_TARGET_DIR`, `ZENLINK_DATA_DIR` and `ZENLINK_OUT_DIR`. Installed and portable copies never write into the checkout.

---

## 🌐 Add a language (Tambah bahasa)

Each language is one text file (`<code>.po`, for example `ja.po`). No programming and no build are needed.

**With the installer or Portable (easiest):**
1. Get the template `zenlink.pot`: in the install folder (`C:\Program Files\ZenLink\lang\`), or [download it](https://raw.githubusercontent.com/zenvyzenshiro/zenlink/main/ZenshiroDev/resources/lang/zenlink.pot) (right-click → Save link as).
2. Open it in [Poedit](https://poedit.net/) (free): **File → New from POT/PO file…**, pick your language, save as `<code>.po`.
3. Put your language's own name in the header: `"Language-Name: 日本語\n"`.
4. Translate each text. Keep `{placeholders}` such as `{device}` exactly, and keep names (ZenLink, AirPlay, iPhone, USB, Wi-Fi…) in English. An empty translation shows English, so you can do it bit by bit.
5. Put the file in `%LOCALAPPDATA%\ZenLink-Next\lang\` (create the `lang` folder; it survives updates), restart ZenLink, and pick the language in **Settings → Language**.
6. Share it: open an issue and attach the file (zip it if GitHub refuses `.po`), or send a pull request.

**From the repository (source):** add `ZenshiroDev/resources/lang/<code>.po` (copied from `zenlink.pot`), test it as in step 5 or with `Start ZenLink.bat`, and open a pull request. To build it into `ZenLink.exe`, also add it to `BUILT_IN` in `apps/desktop/src/i18n.rs`. After changing texts in the code, `python ZenshiroDev/tooling/dev/extract_texts.py` updates the template and every `.po` file.

Full guide, including right-to-left languages: [`ZenshiroDev/resources/lang/README.md`](ZenshiroDev/resources/lang/README.md).

## 🔌 Connection Modes

Five ways to connect. Pick one on the home screen and switch any time in **Settings → Connection & Drivers**.

| Mode | Driver / tool | Video | Control | Good to know |
| :--- | :--- | :--- | :--- | :--- |
| **Apple Driver** (Standard) · wired or Wi-Fi | Apple's own driver, nothing to install | HEVC, about 4–6 Mbps (device limit) | Touch, keyboard, gestures, buttons | iOS 17+ with Developer Mode. Wi-Fi sets itself up once over the cable. Apple apps (iTunes, Apple Devices) keep working. |
| **ZenLink Studio · Wired** (USB) | libusb0 as the device's driver, installed by ZenLink after you confirm | H.264 at native resolution, 12–30 Mbps, up to ~56 fps | Picture and sound only | No Developer Mode needed. Apple apps can't see the device until you restore Apple's driver. |
| **ZenLink Studio · Wi-Fi** (AirPlay) | None | The device's own H.264 / HEVC, up to native resolution | Picture and sound only | No Developer Mode needed. On the device: Control Center → Screen Mirroring → ZenLinkStudio, on the same network as the PC. |
| **Android · Wired** (USB) | Google's `adb`, downloaded by ZenLink once | H.264, or H.265 when both sides support it; up to 120 fps | Mouse, keyboard, clipboard, buttons | Android 5.0+ with USB debugging. Sound on Android 11+. Nothing is installed on the phone. |
| **Android · Wi-Fi** | Google's `adb` | Same as wired | Same as wired | One click from a USB connection, or Android 11+ wireless debugging (QR code or pairing code). |

Changing the Apple driver asks for administrator permission, saves a backup and rolls back on failure; unplug and reconnect the cable afterwards.

--- | :--- | :--- | :--- |
| Driver | Apple's own, nothing to install | libusb0 as the device's driver (installed by ZenLink after you confirm) | None |
| Video | HEVC, about 4–6 Mbps (device limit) | H.264 at native resolution, 12–30 Mbps, up to ~56 fps | The device's own HEVC / H.264, up to native resolution |
| Control | Touch, keyboard, gestures, buttons | Picture and sound only | Picture and sound only |
| Wi-Fi | Yes (set up automatically once over the cable) | — | Yes (Control Center → Screen Mirroring) |
| Apple apps (iTunes, Apple Devices) | Keep working | Can't see the device until you restore Apple's driver | Keep working |

Switch any time in **Settings → Connection & Drivers**. Changing the driver asks for administrator permission, saves a backup and rolls back on failure; unplug and reconnect the cable afterwards.

---

## 🛠️ Under the Hood

```
iPhone / iPad ─ Apple Driver (RemoteXPC / RSD tunnel) ─┐
              ─ Studio: QuickTime over libusb0, AirPlay ┤
                                                        │
Android ─ adb (Google) + scrcpy server ─────────────────┼─ ZenLink.exe (Rust)
                                                        │   • Slint UI, Liquid Glass
                                                        │   • Media Foundation decode → Direct3D 11
                                                        │   • WASAPI sound, OBS-friendly frame pacing
```

1. **ZenLink.exe (Rust)**: one process for the window, settings, hardware decoding, rendering, Android client (ADB host protocol + scrcpy protocol) and sound.
2. **Apple**: `idevice` for Apple Driver mode (CoreDevice tunnel and DisplayService stream), the QuickTime USB protocol (libusb0) for ZenLink Studio over the cable, an AirPlay receiver for Studio over Wi-Fi. All inside ZenLink.exe.
3. **Android**: ZenLink pushes the official scrcpy server (v5.0.1) to the phone and reads its video, audio and control sockets through `adb`. Nothing is installed on the phone; the server is removed when the session ends.
4. **Local only**: video, sound, OCR and keystrokes never leave your PC. No telemetry.

---

## 🔒 Privacy & Clean Uninstall

- The passcode for `Alt+U` is encrypted with Windows DPAPI for your account only; remove it any time in Settings.
- **Installer version**: uninstall from **Windows Settings → Apps**.
- **Portable exe**: delete the `.exe` and the `%LOCALAPPDATA%\ZenLink-Next` folder.
- **Source**: run **`Uninstall-ZenLink.bat`** — it closes ZenLink, deletes its settings, passcode and logs, removes the shortcuts and can delete the folder.
- If you used ZenLink Studio over the cable, press **Restore Apple driver** in Settings first so Apple apps see the device again.

---

## 💖 Support This Project (Dukung ZenLink)

Kalau **ZenLink** membantu streaming, kerja, atau main game kamu di Windows, kamu bisa mendukung pengembangannya lewat **Sociabuzz**:

<p align="left">
  <a href="https://sociabuzz.com/zenvy/tribe" target="_blank">
    <img src="https://img.shields.io/badge/♥_Dukung_Zenvy_Zenshiro_di_Sociabuzz-sociabuzz.com%2Fzenvy%2Ftribe-FF4FA3?style=for-the-badge&logoColor=white" alt="Support Zenvy Zenshiro on Sociabuzz">
  </a>
</p>

* 🔗 **Sociabuzz:** [**https://sociabuzz.com/zenvy/tribe**](https://sociabuzz.com/zenvy/tribe)

---

## 📄 License & Credits

- **Creator & Lead Developer**: [**Zenvy Zenshiro**](https://github.com/zenvyzenshiro).
- **License**: ZenLink — Copyright © 2026 Zenvy Zenshiro — is free software under the GNU General Public License v3.0 or later ([GPL-3.0](LICENSE)). It comes with ABSOLUTELY NO WARRANTY.
- **Built on**: [pane-iphone-mirror-windows](https://github.com/teyboxv1/pane-iphone-mirror-windows) (teyboxv1, GPL-3.0) · [pymobiledevice3](https://github.com/doronz88/pymobiledevice3) (Doron Zavelevsky, GPL-3.0) · [quicktime_video_hack](https://github.com/danielpaulus/quicktime_video_hack) (Daniel Paulus, MIT) · [libusb-win32](https://sourceforge.net/projects/libusb-win32/) (GPL / LGPL) · [libwdi](https://github.com/pbatard/libwdi) (Pete Batard) · [UxPlay](https://github.com/FDH2/UxPlay) / playfair (GPL-3.0) · [scrcpy](https://github.com/Genymobile/scrcpy) server (Genymobile, Apache-2.0) · Android platform-tools `adb` (Google, AOSP Apache-2.0).
- **Libraries**: [Slint](https://github.com/slint-ui/slint) (GPL-3.0 option) · [idevice](https://github.com/jkcoxson/idevice) (MIT) · [tokio](https://github.com/tokio-rs/tokio) (MIT) · [mdns-sd](https://github.com/keepsimple1/mdns-sd) (Apache-2.0 / MIT) · [RustCrypto](https://github.com/RustCrypto) and [dalek](https://github.com/dalek-cryptography) (Apache-2.0 / MIT / BSD-3) · [plist](https://github.com/ebarnard/rust-plist) (MIT) · [windows-rs](https://github.com/microsoft/windows-rs) (Apache-2.0 / MIT).
- Android is a trademark of Google LLC. iPhone, iPad and iOS are trademarks of Apple Inc. AirPlay is a trademark of Apple Inc. ZenLink is not affiliated with Apple, Google or TikTok.
