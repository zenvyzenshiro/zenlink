# Changelog

Full notes for each version are on the [Releases](https://github.com/zenvyzenshiro/zenlink/releases) page.

## 2.5.14

### Added
- **Ghost mode:** press **F4** (or F6 / F8 / F9, Settings › Advanced) while ZenLink is active. The window stays on top, fades to the opacity you pick (20–100 %), and mouse clicks go to the app behind it, for example a trading chart. Press the key again, or click the tray icon, to use ZenLink again. The key is only taken from other apps while Ghost mode is on. Idea by irulestar.

### Fixed
- **ZenLink Studio over the cable:** the Trust check over USB timed out, so the iPhone never sent its screen (no Trust prompt, "Check trust" failed, then `-22` on every retry). Fixed in the USB connection itself; a session that gets no picture for 30 s now says to unlock the device or Trust this PC.
- **Black window on iOS 27 (Apple Driver):** iOS 27 sends H.265 only. When Windows has no HEVC decoder, ZenLink now explains it and opens "HEVC Video Extensions" in the Microsoft Store.
- **OBS / TikTok LIVE Studio Game Capture showed the whole window:** the animated background kept the window redrawing, and Game Capture hooked it instead of the picture. The background now pauses while ZenLink is in the background.
- **Stability:** Stop now ends iOS 17–26 (Hybrid) sessions; a damaged `airplay.json` no longer crashes ZenLink; Android and Apple connections no longer hang without a timeout; the picture comes back after a slow Wi-Fi start; Wi-Fi setup no longer asks the device again every minute.
- **Smart App Control (Windows 11):** the website explains why unsigned installers are blocked.

### Removed
- Unused source: the unfinished Android companion app (`apps/mobile`), old release notes and internal audit documents, and an unused window effect (`glass.rs`).

## 2.5.13

### Changed
- **Android device list like iPhone / iPad:** click a phone to pick it (it is highlighted), then press the one Start button. A phone on USB and Wi-Fi at once gets a USB and a Wi-Fi Start.
- **Start needs a picked device:** pressing Start with no device picked shows "Pick a device first" instead of doing nothing (Android and iPhone / iPad; ZenLink Studio over Wi-Fi still starts the receiver).
- **Language list:** Indonesia first, then English, then other languages. With more than 6 languages the picker becomes a dropdown.
- **Installer in Indonesian and English:** a language page opens first and follows the Windows display language (Indonesian on an Indonesian Windows, English otherwise). The last page has a **Support / Donate** button.
- **`lang` folder in the install folder** with `id.po`, the `zenlink.pot` template and a how-to. Languages you add there survive updates.

### Fixed
- **ZenLink closed by itself while idle** when an iPhone / iPad was found on Wi-Fi: reading its name started a timer outside the network runtime. A crash now also writes its reason to the log.
- **ZenLink Studio over Wi-Fi looked choppy at a good frame rate:** Windows held TCP acknowledgements, so the iPhone sent small frames in bursts (gaps up to ~500 ms on a still or slow screen). The video connection, and the Apple Driver Wi-Fi tunnel, now acknowledge at once.

## 2.5.12

### Changed
- **One program, no Python:** Apple Driver mode, ZenLink Studio (cable and Wi-Fi), AirPlay and the driver settings now run inside `ZenLink.exe` (Rust). The `ZenLinkService.exe` helper and its PyInstaller runtime are gone, so downloads are smaller and start faster. Running from source needs only Rust.
- **Languages are files:** English is built in, Bahasa Indonesia is `lang/id.po`. A new language is one gettext `.po` file in a `lang` folder next to ZenLink.exe (or in `%LOCALAPPDATA%\ZenLink-Next\lang`); Settings lists it without a new build. See `ZenshiroDev/resources/lang/README.md`.
- **Connect steps are numbered** (1, 2, 3) for iPhone / iPad and Android, so a first-time user can follow them.

### Fixed
- **AirPlay over Wi-Fi stuck on "connecting":** ZenLink now sees when Windows Firewall blocks it (for example after a dismissed Windows prompt on a Public network) and shows an "Allow ZenLink on the network" button.
- The device menu highlights the iPad, not the iPhone, while the iPad is mirrored.
- ZenLink Studio remembers the device model and shows short, plain error messages.

## 2.5.11

### Added
- **Closing ZenLink:** the close button asks whether to keep ZenLink in the tray or quit, with "Remember my choice". Settings › Startup & closing changes the choice later. Quit from the tray never asks.
- **Start with Windows** is off by default. The Settings toggle shows the real state, including a change made in Task Manager › Startup apps.
- **Popped-out phone windows** have the same title bar, the full Android dock (Back, Home, Recents, notifications, volume, power) and their own full screen. They open centred on the screen.
- **Device menu:** a ☰ button next to Settings opens a menu on the left with Home and every Android phone and iPhone / iPad, each with a status dot. The selected device has **Screen** and **Files** panels that open beside the menu.
- **Home without pop-ups:** pick Android or iPhone / iPad, the connection mode and the driver right in the main area.
- **Phone file manager (Android):** browse the phone, copy to the PC, delete, new folder, drop files to upload into the open folder.
- **Several phones at once:** pop a phone out into its own window, and **Join** it back into the main window.
- While mirroring, the area around the picture keeps the glass look instead of black.
- **Sound route per mode:** Windows only, device only, or both. Apple Driver mode turns the device volume down for "Windows only" (iPad volume buttons follow the orientation); Android 13+ can play on both at once; ZenLink Studio plays on Windows or the device.
- **Unlock again when the screen locks** (Apple Driver mode, opt-in): when the device locks while mirroring, ZenLink wakes it and enters the saved passcode. On iPad it no longer depends on the orientation.
- **Install adb from the app:** Android needs Google's adb; ZenLink downloads the official platform-tools (about 8 MB, SHA-256 checked) with a progress bar, once. Setup.exe and Portable.exe are smaller because they no longer carry adb.
- **Language follows Windows:** Bahasa Indonesia when Windows lists it, English otherwise (the website follows the browser). A language you pick is kept.
- **Collapsible settings cards** and a 5-step beginner flow in the README.

### Fixed
- Quitting ZenLink also stops the adb server when ZenLink started it. A server started by another tool (Android Studio, scrcpy) keeps running.
- Phone file manager: files that run code (`.exe`, `.bat`, `.ps1` and similar) copied from the phone are shown in their folder, never started.
- Sound route: changing the route no longer turns sound on or off by itself; turning sound on or off no longer changes the device volume.
- **Smaller downloads:** the service no longer carries a second copy of its Python sources (Setup.exe about 1 MB smaller).
- **Apple Driver mode with Windows Smart App Control** ([#2](https://github.com/zenvyzenshiro/zenlink/issues/2)): the connection stopped at "Secure connection" with "DLL load failed while importing lzss" because Smart App Control blocked an unsigned helper module. ZenLink no longer ships or loads that module (a built-in Python version replaces it). Other unsigned files may still be blocked until the releases are code-signed.
- After switching the connection mode or driver, ZenLink asks to restart instead of switching silently.
- A restart of the service no longer opens a console window.
- ZenLink Studio explains "driver installed but not attached yet: unplug and plug in again" instead of asking to install the driver.
- Apple Driver mode: a session whose USB tunnel closed now ends with a message instead of staying on "streaming"; harmless disconnect errors are no longer logged as crashes.
- The welcome screen shows the real version.

### Changed
- **Repository layout:** monorepo with `apps/desktop` (window and Python backend) and `ZenshiroDev/` (shared crates, resources, tooling, technical docs). Build outputs go to `out/`; dev outputs can live outside the checkout (`Start ZenLink.local.bat`).

## 2.5.1

### Added
- **Device picker:** the Apple screen lists every connected iPhone / iPad (USB or Wi-Fi, iOS version, status) and the ones used before.
- **Android apps launcher:** a searchable grid of the phone's apps; open one on the phone or (Android 10+) in its own window.
- **Drop files on the window:** Android installs an `.apk` or saves other files to Download; iPhone / iPad (Apple Driver mode) sends files to an app with File Sharing.
- **In-app updates:** Settings › Updates (Stable or Beta channel), SHA-256 checked, settings kept.
- **Logs in one place:** `%LOCALAPPDATA%\ZenLink-Next\logs`, opened from Settings › About.
- **Microsoft Store edition:** MSIX package (`packaging/build_msix.py`); the Store version gets updates from the Store.

### Fixed
- ZenLink Studio over Wi-Fi no longer looks stuck on "Connecting…".
- "Backend not found" says what is missing and what to do.
- The Android app list no longer fails with "os error 10060" on slower phones.
- The installer's single firewall prompt also covers the bundled `adb`.

## 2.5.0

### Added
- **Android, no app on the phone:** Android 5.0+ over USB or Wi-Fi (QR or pairing code) with the official scrcpy server (v5.0.1) and Google's `adb`. Picture, sound (Android 11+), mouse and keyboard, clipboard.
- **Smart Encoder Detection:** H.264 by default, H.265 only when verified, with fallback; video profiles and per-device settings.
- **Native Rust window:** Media Foundation hardware decoding into Direct3D 11, Liquid Glass UI (Slint). No WebView2, no Visual C++ runtime.
- OCR copy on Apple and Android, tray and start with Windows, auto unlock (Apple Driver mode), first screen to choose iPhone / iPad or Android.

### Fixed
- Portable.exe no longer needs the Visual C++ Redistributable; Windows N editions get a clear Media Feature Pack message; the installer no longer warns about WebView2.
- OBS Game Capture is smoother and survives rotation; Android sound no longer crackles.

## 2.2.2

### Added
- **Hybrid Control for iOS 17–26 (Experimental).** In Apple Driver mode, an iOS 17–26 device mirrors its picture and sound to ZenLink over AirPlay, while touch, keyboard, gestures and buttons go over Apple's own driver through CoreDevice HID. Both run at once without touching the USB driver. Not yet confirmed on a real iOS 17–26 device.
- **Control level, from what actually opened:** Native Control, Hybrid Control (Experimental), Mirroring Only or Unsupported. Shown on the device chip, in the idle screen and under Settings → Connection & Drivers → Control level.
- **AirPlay guide for Hybrid:** while ZenLink waits for the device to mirror, the step-by-step guide is shown in the app.
- **Capability check on the device:** before claiming control, ZenLink checks that the device offers the display and HID services (mounting the developer image when needed). When they're missing, the picture keeps running and the reason is shown.
- iOS compatibility matrix in the README.

### Changed
- Apple Driver mode now starts on iOS 17 and later; iOS 16 and older get a clear message pointing to ZenLink Studio.
- iOS 27 and later keep the native Apple mode unchanged.

### Fixed
- **Apple Driver mode in the exe failed at "Secure connection"** with "No package metadata was found for pyimg4". The build left out a package's metadata that the developer-image code reads, and also `wintun.dll`, which the tunnel code loads. Both are included now, and every build checks itself (`ZenLink.exe --selftest`) before it can be released.

### Not changed
- ZenLink Studio (cable and AirPlay), the USB driver switch, restore, automatic rollback and the error 259 handling.

## 2.2.1
- Fixed switching to ZenLink Studio failing with "A parameter cannot be found that matches parameter name 'DeleteKey'".
- Driver switching works on Windows in any language, survives Apple Mobile Device Service holding the device (error 259), says when a restart is needed, and undoes a failed switch cleanly.
- Apple Driver mode on iOS 26 and older showed a clear message instead of a black screen.

## 2.2
- Apple mode sound no longer crackles; App Switcher, Notifications and Control Center buttons toggle; double-click no longer goes full screen in Apple mode.
- Language choice in the first menu, stats beside the device name, single Portable.exe, opening ZenLink again shows its window.
