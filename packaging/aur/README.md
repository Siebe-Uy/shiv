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
updpkgsums                          # fills in the real checksums
makepkg --printsrcinfo > .SRCINFO
makepkg -si                         # test the build and install
git add PKGBUILD .SRCINFO && git commit -m "shiv-bin 0.1.0" && git push
```

Repeat with `shiv-git` (no checksums to update). After each release, bump `pkgver` in
`shiv-bin`, run `updpkgsums`, regenerate `.SRCINFO` and push.
