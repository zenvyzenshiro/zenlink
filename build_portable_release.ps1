<#
.SYNOPSIS
  Builds the ZenLink release files into out\release\ (work folders: out\build, out\dist):

    ZenLink-v<ver>-Portable.exe           one ZenLink.exe, nothing to install
    ZenLink-v<ver>-Setup.exe              installer (Inno Setup 6)
    ZenLink-v<ver>-Setup-Direct.exe       the portable exe that installs itself (R61), if SAC blocks Setup
    ZenLink-v<ver>-Manual.zip             manual install: ZenLink.exe + source + Start ZenLink.bat

  ZenLink is one Rust program (apps\desktop, ZenshiroDev\crates\); the Python backend was
  removed in v2.5.12. Needs cargo (rustup).

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\build_portable_release.ps1
#>
param(
    [string]$Version = "",
    # Output folder. Default: $env:ZENLINK_OUT_DIR, else out\ in the repository root.
    [string]$Out = "",
    # Test build: Cargo profile "fast" (thin LTO, all cores). Larger exe; never for a public release.
    [switch]$Fast
)
$ErrorActionPreference = "Stop"
# This script lives in the repository root; all paths below are relative to it.
$root = $PSScriptRoot
Set-Location $root

if (-not $Version) {
    $Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
}
# Numeric part only ("2.5.0-beta.1" -> "2.5.0"): Windows version fields are n.n.n.n.
$VersionNum = ($Version -split "-")[0]

# Android: no package carries adb. ZenLink downloads Google's official
# platform-tools zip on request (apps\desktop\src\android.rs ADB_ZIP_*), and
# Start ZenLink.bat names the same pin for ZenshiroDev\tooling\windows\tools\get-adb.ps1.
# Both pins must name the same zip.
$pinRev = (Select-String -Path "Start ZenLink.bat" -Pattern '^set "ZL_ADB_REV=(.+)"$').Matches[0].Groups[1].Value
$pinSha = (Select-String -Path "Start ZenLink.bat" -Pattern '^set "ZL_ADB_SHA256=(.+)"$').Matches[0].Groups[1].Value
$rs = Get-Content -Raw apps\desktop\src\android.rs
if ($rs -notmatch [regex]::Escape("platform-tools_r$pinRev-win.zip") -or $rs -notmatch [regex]::Escape($pinSha)) {
    throw "adb pin in apps\desktop\src\android.rs differs from Start ZenLink.bat (r$pinRev)."
}

# Every build output goes to out\ (ignored by git), or to -Out / ZENLINK_OUT_DIR.
$out = if ($Out) { $Out } elseif ($env:ZENLINK_OUT_DIR) { $env:ZENLINK_OUT_DIR } else { Join-Path $root "out" }
$out = [IO.Path]::GetFullPath($out)
$release = Join-Path $out "release"
$build = Join-Path $out "build"
$dist = Join-Path $out "dist"
Remove-Item -Recurse -Force $release, $build, $dist -ErrorAction SilentlyContinue
$app = Join-Path $dist "ZenLink"
New-Item -ItemType Directory -Force $release, $build, $app | Out-Null

# Code signing (GitHub issue #2: Smart App Control blocks unsigned native
# modules without reputation). Off until a trusted certificate exists. Set
# ZENLINK_SIGN_ARGS to the signtool options after "sign", e.g. for Azure
# Trusted Signing: /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256
#   /dlib <path>\Azure.CodeSigning.Dlib.dll /dmdf <path>\metadata.json
# Files that already carry a valid signature (libusb0) are kept.
function Invoke-Sign([string[]]$files) {
    if (-not $env:ZENLINK_SIGN_ARGS) { return }
    $todo = @($files | Where-Object { (Get-AuthenticodeSignature $_).Status -ne "Valid" })
    if (-not $todo) { return }
    $signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source
    if (-not $signtool) { throw "ZENLINK_SIGN_ARGS is set but signtool.exe is not on PATH" }
    # Batches keep the command line short.
    for ($i = 0; $i -lt $todo.Count; $i += 50) {
        $batch = $todo[$i..([Math]::Min($i + 49, $todo.Count - 1))]
        & $signtool sign ($env:ZENLINK_SIGN_ARGS -split " ") $batch
        if ($LASTEXITCODE) { throw "signtool failed" }
    }
    Write-Host "    signed $($todo.Count) file(s)" -ForegroundColor Green
}
if (-not $env:ZENLINK_SIGN_ARGS) { Write-Warning "ZENLINK_SIGN_ARGS not set: release files are NOT code-signed (Smart App Control may block them)." }

Write-Host "[1/3] Building ZenLink.exe..." -ForegroundColor Magenta
foreach ($f in "LICENSE", "README.md", "Start ZenLink.bat") {
    Copy-Item $f $app
}
# Uninstall-ZenLink.bat is in the root; run.bat and Create-Desktop-Shortcut.bat
# live in ZenshiroDev\tooling\windows\scripts. A packaged copy keeps all three
# next to ZenLink.exe (each script finds its root either way).
Copy-Item "Uninstall-ZenLink.bat" $app
foreach ($f in "run.bat", "Create-Desktop-Shortcut.bat") {
    Copy-Item (Join-Path "ZenshiroDev\tooling\windows\scripts" $f) $app
}
# Android: the scrcpy server is built into ZenLink.exe, its licence ships
# beside it. adb is downloaded by ZenLink on request (not packaged).
$scrcpyDest = Join-Path $app "third_party\scrcpy"
New-Item -ItemType Directory -Force $scrcpyDest | Out-Null
Copy-Item "ZenshiroDev\resources\third_party\scrcpy\LICENSE" $scrcpyDest
# Credits and licences of everything bundled (scrcpy, adb, FFmpeg, ...).
Copy-Item "ZenshiroDev\docs\THIRD_PARTY.md" $app
$cargoProfile = if ($Fast) { "fast" } else { "release" }
& cargo build --profile $cargoProfile --manifest-path Cargo.toml -p zenlink-app
if ($LASTEXITCODE) { throw "cargo failed" }
# CARGO_TARGET_DIR lets the release build while a dev ZenLink.exe from
# target\ is open (Windows locks a running exe).
$rustTarget = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root "target" }
$rustExe = Join-Path $rustTarget "$cargoProfile\ZenLink.exe"
Copy-Item $rustExe $app
# Sound (AAC) and the AirPlay session key, beside the exe (Portable.exe packs them in).
Copy-Item "$root\ZenshiroDev\resources\windows\native\zlaac\zlaac.dll" $app
Copy-Item "$root\ZenshiroDev\resources\windows\native\playfair\playfair.dll" $app
# Driver changes (Settings > Connection & Drivers): tools\usb_filter.ps1 next to drivers\.
Copy-Item -Recurse "$root\ZenshiroDev\tooling\windows\tools" (Join-Path $app "tools")
Copy-Item -Recurse "$root\ZenshiroDev\resources\windows\drivers" (Join-Path $app "drivers")
# Language files: Indonesian, the template and how to add a language. ZenLink reads
# any <code>.po here (src/i18n.rs); Indonesian is also built into ZenLink.exe.
$langDest = Join-Path $app "lang"
New-Item -ItemType Directory -Force $langDest | Out-Null
foreach ($f in @("id.po", "zenlink.pot", "README.md")) {
    Copy-Item "$root\ZenshiroDev\resources\lang\$f" $langDest
}
Invoke-Sign @(Get-ChildItem $app -Recurse -Include *.exe, *.dll | ForEach-Object FullName)

Write-Host "[2/3] Building the portable single-file ZenLink.exe..." -ForegroundColor Magenta
# Same app with the DLLs and the driver kit packed in; settings in %LOCALAPPDATA%\ZenLink-Next.
# Kept outside out\build (emptied each run), so only the changed crates rebuild.
$portableTarget = Join-Path $rustTarget "portable"
& cargo build --profile $cargoProfile --manifest-path Cargo.toml -p zenlink-app --features portable --target-dir $portableTarget
if ($LASTEXITCODE) { throw "cargo (portable) failed" }
Copy-Item (Join-Path $portableTarget "$cargoProfile\ZenLink.exe") (Join-Path $release "ZenLink-v$Version-Portable.exe")
Invoke-Sign @(Join-Path $release "ZenLink-v$Version-Portable.exe")

Write-Host "[3/3] Building the installers and the source zip..." -ForegroundColor Magenta
$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 not found (winget install JRSoftware.InnoSetup)." }
# Top-level entries of the install folder: the next Setup removes exactly
# these before copying (clean update, ZenshiroDev/tooling/windows/packaging/ZenLink.iss).
# lang\ is left out: languages a user added there survive an update (Setup
# still overwrites the files it ships).
$skip = @("Start ZenLink.bat", "run.bat", "Create-Desktop-Shortcut.bat", "Uninstall-ZenLink.bat", "portable.txt", "install-manifest.txt", "lang")
$names = @(Get-ChildItem -Force $app | Where-Object { $skip -notcontains $_.Name } | ForEach-Object { $_.Name }) + "install-manifest.txt"
[IO.File]::WriteAllLines((Join-Path $app "install-manifest.txt"), [string[]]$names)
& $iscc "/DAppVersion=$Version" "/DAppVersionNum=$VersionNum" "/DSourceDir=$app" "/O$release" "ZenshiroDev\tooling\windows\packaging\ZenLink.iss"
if ($LASTEXITCODE) { throw "Inno Setup failed" }
Invoke-Sign @(Get-ChildItem $release -Filter "*-Setup.exe" | ForEach-Object FullName)
# Setup-Direct.exe: the portable exe under another name installs itself (apps\desktop\src\install.rs).
# Twice the size, but no Setup.tmp: for PCs where Smart App Control blocks Inno's (B-63).
Copy-Item (Join-Path $release "ZenLink-v$Version-Portable.exe") (Join-Path $release "ZenLink-v$Version-Setup-Direct.exe")
if (git status --porcelain) { Write-Warning "Uncommitted changes: Manual.zip holds the last commit (HEAD), not the working tree." }
$srcZip = Join-Path $release "ZenLink-v$Version-Manual.zip"
git archive --format=zip --prefix="ZenLink-v$Version/" -o $srcZip HEAD
# Ready-built ZenLink.exe next to Start ZenLink.bat (no Rust toolchain needed).
# adb is not packed: ZenLink downloads Google's official copy on request
# (ZenshiroDev\tooling\windows\tools\get-adb.ps1 by hand). The website, agent notes and the scrcpy
# server (built into ZenLink.exe) are export-ignore in .gitattributes.
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::Open($srcZip, "Update")
try {
    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, (Join-Path $app "ZenLink.exe"), "ZenLink-v$Version/ZenLink.exe")
} finally { $zip.Dispose() }

Get-ChildItem $release | ForEach-Object {
    "{0,-45} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB)
}
Write-Host "Done: $release" -ForegroundColor Green
