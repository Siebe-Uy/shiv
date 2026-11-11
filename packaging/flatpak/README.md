# Flatpak

App ID: `io.github.Siebe_Uy.shiv` (GitHub-hosted, so Flathub verifies it by GitHub login).

Release CI builds the `.flatpak` bundle from `io.github.Siebe_Uy.shiv.yml`, which builds
the local checkout. Build it yourself with `packaging/build-flatpak.sh`.

## Submitting to Flathub

Flathub builds from a tagged source, not a local directory, so the submitted manifest differs:

1. Fork <https://github.com/flathub/flathub>, branch from `new-pr`.
2. Copy into the branch: the manifest, `.desktop`, `.metainfo.xml`, and a `cargo-sources.json`
   generated with `flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json`.
3. In the copied manifest, replace the `type: dir` source with
   `type: git`, `url: https://github.com/Siebe-Uy/shiv.git`, `tag: v<version>`, `commit: <sha>`,
   and change the `install` paths that start with `packaging/flatpak/` to the file names in the
   Flathub repo root.
4. Add a `<screenshots>` section to the metainfo (Flathub requires at least one) and the new
   release entry for the tagged version.
5. Open the PR and answer the reviewers; the release must exist as a git tag first.
