#!/usr/bin/env bash
# Build dist/Shiv-<version>-x86_64.flatpak from packaging/flatpak.
#
# Needs: flatpak, flatpak-builder, uv, and the freedesktop 25.08 runtime, SDK
# and rust-stable SDK extension from Flathub (CI installs them).
# FLATPAK_CARGO_GENERATOR must point at flatpak-cargo-generator.py (CI downloads
# a pinned copy; see .github/workflows/release.yml).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/crates/shiv-ui/Cargo.toml" | head -1)}"
id=io.github.Siebe_Uy.shiv
dir="$root/packaging/flatpak"
work="$(mktemp -d)"
trap 'rm -rf "$work" "$dir/cargo-sources.json"' EXIT

uv run --quiet --with aiohttp --with tomlkit --with PyYAML \
  python -I "${FLATPAK_CARGO_GENERATOR:?set FLATPAK_CARGO_GENERATOR}" \
  "$root/Cargo.lock" -o "$dir/cargo-sources.json"

flatpak-builder --force-clean --user --disable-rofiles-fuse \
  --install-deps-from=flathub \
  --repo="$work/repo" "$work/build" "$dir/$id.yml"

mkdir -p "$root/dist"
out="$root/dist/Shiv-$version-x86_64.flatpak"
flatpak build-bundle "$work/repo" "$out" "$id"
(cd "$root/dist" && sha256sum "$(basename "$out")" > "$(basename "$out").sha256")
echo "built $out"
