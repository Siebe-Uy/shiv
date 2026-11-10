# Shiv: a fast native Deadlock mod manager

## Why

The current Deadlock Mod Manager (Tauri + React) idles at about 2.5 GB RAM. On
Linux (Arch) scrolling and browsing are laggy and unresponsive, because Tauri
renders through WebKitGTK. The owner also finds the UI looks like generated
"AI slop". A faster Rust backend would not fix the rendering path, so the
webview has to go.

## Goal

A small, native Rust desktop app (Linux first, Windows later) that does only:

1. Browse mods
2. Install mods
3. Enable and disable mods
4. Fix broken mods
5. Crosshairs
6. Performance configs

## Anti-goals

- Scrolling or browsing that stutters on Linux. This is the failure that
  matters most, even if every feature works.
- Idle RAM anywhere near the current app. Target: under 150 MB idle.
- A UI that looks templated: gradients, glow, emoji, stock card grids.

## Non-goals

Profiles, user accounts, Discord presence, match sync, crash dumps, auto
updater complexity, telemetry, i18n at launch.

## Decisions made by the owner

- Rust. Linux (Arch, including Steam installs) first. Windows is not needed on
  day one; keep paths and game detection platform-safe so it can follow.
- **Separate repo** at `~/Projects/shiv`, named Shiv (my name pick). Needed
  code (`vpkmanager`, parsers) is copied in from this repo, not path-linked,
  and trimmed of Tauri/`ts-rs` coupling.
- No "update available" detection yet. The sidecar still records mod id and
  version so it can be added later.
- Plain UI that still looks and feels premium: tight typography, restrained
  colour, smooth scrolling.
- Mod data comes from the project API (`api.deadlockmods.app`), not GameBanana
  directly.
- "Fix broken mods" covers both parts of what the current app does:
  - Mod folder health check, also run before launching the game, with a
    warning shown if something is wrong.
  - VPK content repair for mods that break after game updates.
- Performance: four presets (pretty, balanced, sweaty, potato; the catalog
  has no lean preset, so that tier was dropped on 2026-11-10) plus
  per-setting editing, using the API's perf catalog, written to the game's
  `GameInfo` file.
- Crosshair: a live preview and an apply step, producing the
  `citadel_crosshair_*` values.

## My calls (not yet confirmed by the owner)

- **UI toolkit: Slint.** Native retained-mode GPU UI, no webview, good for a
  polished look, runs on Windows and Linux. The repo is GPL-3.0, so Slint's
  GPL licence fits. egui is the fallback if Slint blocks something (e.g. a
  virtualised list with image thumbnails).
- **No manifest.** A mod is a VPK in `citadel/addons`. The app reads the
  folder as the source of truth. Enable and disable by renaming/moving files.
  This removes most of the manifest and staging failure states the current
  app has to repair. Any metadata (mod id, version) must live in a small
  sidecar file or be derivable, and that is left to the builder.
- **Copy existing Rust crates** (`vpkmanager`, plus `vpk-parser`/`kv-parser`
  where needed) into the new repo. Do not rewrite VPK or `GameInfo` parsing.
  Keep GPL-3.0 notices.

## More calls from the builder check (mine, not confirmed)

- **Toolkit spike first.** Before any feature work, build a throwaway
  1,000-row image list in Slint. Pass: 60 fps scrolling on the owner's Arch
  machine and under 150 MB RSS idle with the list loaded. Fail: switch to
  egui. Renderer (femtovg, skia, software) is chosen by that measurement.
- **Idle means** window open on the browse list, visible thumbnails loaded,
  thumbnails decoded at display size and held in a capped cache.
- **Install formats:** zip, rar, 7z, and multi-VPK archives as served by the
  API. `.dmodpkg` is out of scope.
- **Disabled mods** are renamed in place (extra extension), so enable and
  disable never changes load order. Install order follows `pakNN` naming.
- **Metadata:** a small sidecar file per mod ties it to its API mod id and
  version. Mods added by hand show by filename, with no thumbnail.
- **Health checks defined:** unreadable or truncated VPK, zero-byte or
  partial download, duplicate pak numbers, VPKs that fail the content-repair
  scan. Repair is user-triggered with a preview of what changes.
- **Launch:** the app has a Launch button (Steam URI). It runs the health
  check first and shows a warning with a "launch anyway" option. It does not
  block permanently. Edits to `addons` are refused while the game runs.
  After the link is opened the status says Launching until a Deadlock
  process appears, then Running until it exits; with no process after a
  timeout it says the launch could not be confirmed. Cancel launch stops
  waiting and stops the URI opener if it still runs; a launch Steam already
  accepted cannot be taken back, and the status says so.
- **Browse:** search, sort, category filter, pagination, thumbnails, and
  description from the existing v2 API. Rows show the last updated date
  (GameBanana update, else files update). NSFW mods are listed with a tag and
  a blurred thumbnail until Show NSFW is switched on (off every session).
  Install asks which file to use when a mod has more than one download.
  Clicking a row opens the mod's detail page inside Browse: name, author,
  category and hero, tags, downloads, likes, added and last updated dates,
  the description as plain text (HTML and BBCode removed, entities decoded,
  capped at 12,000 characters), an image gallery (large image with
  Previous/Next, arrow keys and a thumbnail strip; loaded lazily, fitted to
  960x540, at most four decoded images kept and all dropped on Back; a failed
  image shows a placeholder), "Open on GameBanana" (https links only, through
  the system opener) and the same Install/Cancel controls and file picker as
  the list, sharing its state. NSFW images stay blurred there too until Show
  NSFW is on. Back (or Escape) returns to the list with its search, filters
  and scroll position. API changes are out of scope unless
  the builder finds a blocker. Online only, no offline mode.
- **Crosshair:** apply path and controls follow the current app's behaviour
  (full parity with `config-generator.ts`); no community crosshair browser.

## Reference in the current code

- Folder health: `apps/desktop/src-tauri/src/commands/mod_folder_repair.rs`
  (manifest missing/corrupt, untracked VPKs, half-applied staging).
- Content repair: `packages/vpkmanager/src/material_repair.rs`.
- Game running guard: `apps/desktop/src-tauri/src/game_guard.rs` (do not
  modify `addons` while Deadlock is running).
- Crosshair values: `packages/crosshair/src/config-generator.ts`.
- Perf catalog schema: `packages/perf-catalog/src/schema.ts`, API route
  `apps/api/src/routers/v2/perf-catalog.ts`, Rust consumer
  `apps/desktop/src-tauri/src/mod_manager/perf_config/catalog.rs`.

## Left to the builder

Crate layout, download and extract pipeline, game folder detection on both
platforms, caching and thumbnail strategy, how the pre-launch check is
triggered, sidecar metadata format, packaging.
