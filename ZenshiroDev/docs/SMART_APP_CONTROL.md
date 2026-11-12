# Smart App Control and ZenLink (GitHub issue #2)

## The report

ZenLink v2.5.1 (Portable) on Windows 11 with Smart App Control (SAC) on,
iPhone 17, Apple Driver over USB. The cast stopped at "Secure connection"
(25 %) with:

    DLL load failed while importing lzss: The file was blocked by an application control policy.

The Code Integrity log showed event 3077 (block), 3089 (signature) and 3118
(SAC block details). The service process was
`%LOCALAPPDATA%\ZenLink-Next\service\ZenLinkService-2.5.1-*.exe`, and the
blocked file was `%TEMP%\_MEI*\lzss.cp313-win_amd64.pyd`.

## Root cause

1. **Why `lzss` loads.** The Apple Driver path imports
   `pymobiledevice3.services.mobile_image_mounter` (the developer image).
   That module imports `restore.tss`, which imports `restore.img4`, which
   imports `pyimg4`. `pyimg4.parser` imports `lzss` (pylzss) and `lzfse` at
   import time. Mounting the developer image never compresses or decompresses
   anything with them: only the import is needed.
2. **Why it was blocked.** SAC allows a binary only if it carries a trusted
   signature or Microsoft's cloud service knows it as reputable. Almost every
   `.pyd` in the bundle is unsigned (aiohttp, pycryptodome, cryptography,
   zeroconf and others). The log shows only `lzss` was blocked, and
   connection detection (which loads many of those modules) worked. This
   points to reputation: widely downloaded wheels are known to the cloud
   service, while the rare pylzss build is not. `lzfse` is imported on the
   next line, and is just as rare.
3. **Why `_MEI`.** The Portable build runs ZenLinkService as a PyInstaller
   one-file exe, which unpacks its modules to `%TEMP%\_MEI*` on every start.
   The location is not the cause. SAC judges the file (signature or
   reputation), not the folder, and the Setup (folder) build loads the same
   unsigned `lzss.pyd`.

The cause is the combination: an unsigned native module without reputation
is loaded on the device path.

## Fix in the source (this change)

- `zenlink/_lzss.py` holds pure-Python stand-ins:
  - `lzss` is Apple's LZSS (xnu `lzss.c`, N=4096, F=18). It is
    byte-compatible with pylzss (checked both ways in
    `tests/test_lzss.py`).
  - `lzfse` is a stub that raises only if it is called.
- `zenlink/__init__.py` registers both in `sys.modules`, so the native
  modules are never loaded, in source mode as well.
- `build_portable_release.ps1` excludes `lzss` and `lzfse` from both
  PyInstaller builds, and fails if their `.pyd` files reappear.
- The frozen self-test (`launch.py --selftest`) fails if a native `lzss` or
  `lzfse` gets loaded.

This removes the file named in the report. No feature changes: nothing in
ZenLink compresses IMG4 payloads.

## What is not solved without a certificate

Every other module is still unsigned: other `.pyd` files, `zlaac.dll`,
`playfair.dll` (AirPlay), `ZenLinkService.exe` and `ZenLink.exe`. Today they
pass only on reputation, and SAC may block any of them on a given PC. This is
most likely for ZenLink's own files on a new release, before it has built up
reputation. The permanent fix is code signing with a certificate that
chains to Microsoft's trusted root program:

- **Azure Trusted Signing** (Microsoft, about USD 10 a month; needs identity
  validation).
- **SignPath Foundation** (free for open-source projects; signs in CI from a
  GitHub build).
- A classic OV/EV code-signing certificate.

Self-signed certificates do not help: SAC does not trust them.

## Signing pipeline (ready, off by default)

`build_portable_release.ps1` signs when `ZENLINK_SIGN_ARGS` is set. It holds
the `signtool sign` options, for example for Azure Trusted Signing:

    $env:ZENLINK_SIGN_ARGS = "/fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 /dlib <path>\Azure.CodeSigning.Dlib.dll /dmdf <path>\metadata.json"

When the variable is set, the script signs:

- every unsigned `.exe`, `.dll` and `.pyd` in the folder build (Setup);
- `ZenLink.exe`;
- the one-file service;
- the Portable exe;
- the Setup exe.

Without the variable it prints a warning and builds unsigned, as before.

Limitation: the one-file service packs files from site-packages, not from the
signed folder. Its unpacked `_MEI` copies stay unsigned. Once a certificate
exists, the Portable exe should embed the signed folder build and unpack it
once to `%LOCALAPPDATA%\ZenLink-Next\service\<version>\`, instead of a
one-file exe.

## Testing

- `tests/test_lzss.py` covers:
  - a round trip;
  - byte compatibility with pylzss;
  - that pyimg4 gets the pure modules.
- The full backend suite passes.
- The release build's frozen self-test imports every device-path module.
- Not tested: a PC with SAC on. SAC is off on the development PC, and it
  cannot be turned back on without reinstalling Windows. The reporter's test
  is the proof.
- Users are never asked to turn SAC, Defender or other protections off.

## 2.5.13: the installer itself is blocked (2026-11-12)

Since 2.5.12 there is no Python service: ZenLink is one native `ZenLink.exe`
(plus `zlaac.dll` and `playfair.dll`), so the `lzss`/`_MEI` cause above no
longer applies. A user with SAC on reported a new block on the installer:
"Part of this app has been blocked ... ZenLink-v2.5.13-Setup.tmp". Inno Setup
copies itself to `%TEMP%\is-*\<name>.tmp` and runs from there; that copy is
unsigned like the Setup exe, so SAC stops the installer before any ZenLink
code runs. The installer extracts no other helper DLL at setup time.

Nothing inside ZenLink can fix this: SAC trusts only a signature that chains
to Microsoft's root program, or cloud reputation that a new release does not
have yet. The Portable exe is unsigned too and is blocked the same way. The
only fix is signing (see above). Until then the website FAQ says so plainly.
