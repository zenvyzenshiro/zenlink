#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DIST="$HERE/dist/zenlink-linux-x86_64"
OUTPUT_DIR="$HERE/dist"
PKG_VERSION="2.5.13"
PKG_REL="1"
PKG_NAME="zenlink-bin"
ARCH="x86_64"
BUILD_DATE="$(date +%s)"

if [ ! -f "$DIST/bin/zenlink" ]; then
    echo "Running build_linux.sh first..."
    bash "$HERE/build_linux.sh"
fi

STAGING="/tmp/zenlink_arch_staging"
rm -rf "$STAGING"
mkdir -p "$STAGING/opt/zenlink/bin" "$STAGING/usr/bin" "$STAGING/usr/share/applications" "$STAGING/usr/share/pixmaps"

# Copy distribution contents to /opt/zenlink
cp -r "$DIST/"* "$STAGING/opt/zenlink/"

# Create /usr/bin/zenlink symlink/wrapper
cat << 'WRAPPER' > "$STAGING/usr/bin/zenlink"
#!/usr/bin/env bash
exec /opt/zenlink/bin/zenlink "$@"
WRAPPER

# Copy desktop and icon
cp "$DIST/share/applications/zenlink.desktop" "$STAGING/usr/share/applications/"
cp "$DIST/share/pixmaps/zenlink.png" "$STAGING/usr/share/pixmaps/"

# Ensure standard 755 directory permissions and 644/755 file permissions
find "$STAGING" -type d -exec chmod 755 {} +
find "$STAGING/opt" -type f -exec chmod 644 {} +
find "$STAGING/usr" -type f -exec chmod 644 {} +

chmod 755 "$STAGING/usr/bin/zenlink" \
          "$STAGING/opt/zenlink/bin/zenlink"

chmod 644 "$STAGING/usr/share/applications/zenlink.desktop" \
          "$STAGING/usr/share/pixmaps/zenlink.png"

# Calculate installed size
INSTALLED_SIZE=$(du -sb "$STAGING" | awk '{print $1}')

# Create .PKGINFO
cat << PKGINFO > "$STAGING/.PKGINFO"
# Generated for Arch Linux pacman
pkgname = ${PKG_NAME}
pkgbase = ${PKG_NAME}
pkgver = ${PKG_VERSION}-${PKG_REL}
pkgdesc = Mirror and control Android, iPhone, and iPad on Linux (Native Rust)
url = https://github.com/zenvyzenshiro/zenlink
builddate = ${BUILD_DATE}
packager = imanecdoche
size = ${INSTALLED_SIZE}
arch = ${ARCH}
license = GPL3
depend = android-tools
depend = ffmpeg
provides = zenlink
conflict = zenlink
PKGINFO

# Create .BUILDINFO
cat << BUILDINFO > "$STAGING/.BUILDINFO"
format = 2
pkgname = ${PKG_NAME}
pkgbase = ${PKG_NAME}
pkgver = ${PKG_VERSION}-${PKG_REL}
pkgarch = ${ARCH}
packager = imanecdoche
builddate = ${BUILD_DATE}
buildenv = !distcc
buildenv = color
options = !strip
options = docs
BUILDINFO

chmod 644 "$STAGING/.PKGINFO" "$STAGING/.BUILDINFO"

# Create .pkg.tar.zst archive
PKG_FILE="${OUTPUT_DIR}/${PKG_NAME}-${PKG_VERSION}-${PKG_REL}-${ARCH}.pkg.tar.zst"
cd "$STAGING"
tar -c --zstd --owner=0 --group=0 --numeric-owner -f "$PKG_FILE" .PKGINFO .BUILDINFO opt usr

echo "Successfully built Arch package: $PKG_FILE"
ls -lh "$PKG_FILE"
