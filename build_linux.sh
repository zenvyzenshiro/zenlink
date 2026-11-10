#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DIST="$HERE/dist/zenlink-linux-x86_64"

# Ensure fontconfig.pc exists for yeslogic-fontconfig-sys (use /tmp to avoid space-in-path issues)
if ! pkg-config --exists fontconfig 2>/dev/null; then
    mkdir -p "/tmp/zenlink_fontconfig"
    ln -sf /usr/lib/x86_64-linux-gnu/libfontconfig.so.1 "/tmp/zenlink_fontconfig/libfontconfig.so" 2>/dev/null || true
    cat << 'EOF' > "/tmp/zenlink_fontconfig/fontconfig.pc"
prefix=/usr
libdir=/tmp/zenlink_fontconfig
includedir=/usr/include

Name: Fontconfig
Description: Font configuration and customization library
Version: 2.15.0
Libs: -L/tmp/zenlink_fontconfig -lfontconfig
Cflags: -I/usr/include
EOF
    export PKG_CONFIG_PATH="/tmp/zenlink_fontconfig:${PKG_CONFIG_PATH:-}"
    export LIBRARY_PATH="/tmp/zenlink_fontconfig:${LIBRARY_PATH:-}"
fi

unset RUST_FONTCONFIG_DLOPEN

echo "==> Building ZenLink native binary (v2.5.13)..."
cargo build --release -p zenlink-app

echo "==> Packaging distribution to $DIST..."
rm -rf "$DIST"
mkdir -p "$DIST/bin" "$DIST/share/applications" "$DIST/share/pixmaps"

cp "$HERE/target/release/ZenLink" "$DIST/bin/zenlink"
chmod 755 "$DIST/bin/zenlink"

cp "$HERE/packaging/zenlink.desktop" "$DIST/share/applications/"
chmod 644 "$DIST/share/applications/zenlink.desktop"

cp "$HERE/packaging/zenlink.png" "$DIST/share/pixmaps/"
chmod 644 "$DIST/share/pixmaps/zenlink.png"

# Packaging tarball & zip
echo "==> Creating archives..."
cd "$HERE/dist"
tar -czvf "zenlink-v2.5.13-linux-x86_64.tar.gz" "zenlink-linux-x86_64"
zip -r "zenlink-v2.5.13-linux-x86_64.zip" "zenlink-linux-x86_64"

echo "==> Build complete!"
