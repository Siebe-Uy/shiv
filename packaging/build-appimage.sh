#!/usr/bin/env bash
# Build dist/Shiv-<version>-x86_64.AppImage from a release build.
#
# Needs: cargo, rsvg-convert (librsvg), and linuxdeploy plus its appimage
# plugin on PATH (CI downloads them; see .github/workflows/release.yml).
# Build on an older distro (CI uses Ubuntu 22.04) so the AppImage runs on
# newer ones: it can only rely on the glibc it was built against.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/crates/shiv-ui/Cargo.toml" | head -1)}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cargo build --release -p shiv-ui --manifest-path "$root/Cargo.toml"

rsvg-convert -w 256 -h 256 "$root/style/svg/shiv-icon.svg" -o "$work/shiv.png"

mkdir -p "$root/dist"
cd "$work"
ARCH=x86_64 LINUXDEPLOY_OUTPUT_VERSION="$version" \
  linuxdeploy --appdir AppDir \
    --executable "$root/target/release/shiv" \
    --desktop-file "$root/packaging/shiv.desktop" \
    --icon-file "$work/shiv.png" \
    --output appimage

out="$root/dist/Shiv-$version-x86_64.AppImage"
mv ./*.AppImage "$out"
(cd "$root/dist" && sha256sum "$(basename "$out")" > "$(basename "$out").sha256")
echo "built $out"
