# AUR packages

| Package | What it is |
|---|---|
| `shiv-bin` | The release AppImage, unpacked and installed system-wide. |
| `shiv-git` | Builds the latest `main` from source. |

Publishing needs an AUR account with your SSH key added. For a first release:

```sh
git clone ssh://aur@aur.archlinux.org/shiv-bin.git
cp shiv-bin/PKGBUILD shiv-bin/
cd shiv-bin
# Put the real checksums in sha256sums=(...): the AppImage's from the release's
# Shiv-<version>-x86_64.AppImage.sha256 asset, the LICENSE's from `sha256sum LICENSE` at the tag.
# Do not publish with 'SKIP'.
grep -n SKIP PKGBUILD && echo "fix the checksums first" && exit 1
makepkg --printsrcinfo > .SRCINFO
makepkg -si                         # test the build and install
git add PKGBUILD .SRCINFO && git commit -m "shiv-bin 0.1.0" && git push
```

Repeat with `shiv-git` (it builds from git, so it has no release checksum). After each release, bump
`pkgver` in `shiv-bin`, copy the new digests from the release assets, regenerate `.SRCINFO` and push.
