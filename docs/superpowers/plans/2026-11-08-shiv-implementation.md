# Shiv Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build Shiv, a native Rust Deadlock mod manager (browse, install, enable, repair, crosshair, performance configs) in a new repo at `~/Projects/shiv`.

**Architecture:** A Cargo workspace. UI-free library crates hold all behavior and are tested without a window. A thin Slint binary renders them. Parsers and repair code are copied from the old repo (`/home/siebe/Projects/deadlock-mod-manager`, referred to below as `OLD`), not rewritten.

**Tech Stack:** Rust 2024, Slint (spike-gated, egui fallback), `ureq` + `serde` for the API, `steamlocate`, `zip`/`unrar`/`sevenz-rust2` for archives, `tempfile` for tests.

**Spec:** `OLD/docs/superpowers/specs/2026-11-08-lite-mod-manager-design.md`

## Global Constraints

- Linux first; Windows not needed on day one, but no Linux-only paths or APIs outside a `platform` module.
- UI is native and GPU-rendered. No webview.
- Idle RAM under 150 MB (window open on browse list, visible thumbnails loaded). Scrolling at 60 fps on the owner's Arch machine.
- Mod data from `https://api.deadlockmods.app` (`/v2/mods`, `/v2/mods/{id}`, `/v2/mods/{id}/downloads`, `/v2/perf-catalog`).
- No manifest: a mod is a VPK in `<game>/game/citadel/addons`. Disabled mods are renamed in place (extra extension). Mods are linked to API id and version by a sidecar file.
- No "update available" detection. No profiles, accounts, Discord presence, match sync, crash dumps, `.dmodpkg`, i18n, telemetry.
- Install formats: zip, rar, 7z, multi-VPK archives.
- Never modify `addons` while Deadlock is running.
- Repair is user-triggered with a preview. Launch runs the health check first and warns with "launch anyway".
- Rust error handling with `thiserror`; no `unwrap` outside tests. Keep GPL-3.0 headers/notices on copied code.
- UI look: restrained colour, tight typography, no gradients, glow or emoji.

## Review Focus

- Archive entry with `../` path (zip-slip): must be rejected, nothing written outside the target.
- Download interrupted or zero bytes: no half file left in `addons`, error shown.
- Archive containing no VPK: install fails cleanly with a message, `addons` unchanged.
- API unreachable on startup: browse screen shows an error with retry; installed, crosshair and performance screens still work.
- `gameinfo.gi` read-only or missing: performance and health check report it, nothing is truncated.

## File Structure (`~/Projects/shiv`)

```
Cargo.toml                    workspace
crates/vpk-parser/            copied from OLD/packages/vpk-parser/src-rs (rlib only)
crates/kv-parser/             copied from OLD/packages/kv-parser/src-rs
crates/vpkmanager/            copied from OLD/packages/vpkmanager
crates/shiv-core/             game locate, addons model, sidecar, installer, health, launch, crosshair, perf
crates/shiv-api/              API client and DTOs
crates/shiv-repair/           copied mod health + repair engine (Task 9)
crates/shiv-ui/               Slint binary
spike/                        throwaway list benchmark (Task 0), deleted after
```

---

### Task 0: Toolkit spike (gate)

**Files:**
- Create: `spike/Cargo.toml`, `spike/src/main.rs`, `spike/ui.slint`

**Interfaces:**
- Produces: a decision recorded in `docs/toolkit.md`: Slint (with renderer name) or egui.

- [ ] **Step 1:** Build a window with a scrolling list of 1,000 rows, each with a 240x135 generated image and two text lines. Renderer selectable by env var (femtovg, skia, software).
- [ ] **Step 2:** Measure on the owner's machine for each renderer: RSS after loading (`/proc/self/status` VmRSS) and scroll smoothness (owner confirms by feel; also log frame times over a scripted 10 s scroll).
- [ ] **Step 3:** Pass: 60 fps scrolling and under 150 MB RSS. Pick the lowest-RSS renderer that passes. If no Slint renderer passes, repeat Steps 1-3 with egui (`eframe`, wgpu).
- [ ] **Step 4:** Write `docs/toolkit.md` with the numbers and the decision. Ask the owner to confirm before Task 1.

### Task 1: Repo scaffold and copied crates

**Files:**
- Create: `~/Projects/shiv/` (git init), root `Cargo.toml`, `crates/*` as in File Structure, `LICENSE` (GPL-3.0), `.gitignore`
- Delete: `spike/`

**Interfaces:**
- Produces: workspace that builds with `cargo build --workspace`; crates named `vpk_parser`, `kv_parser`, `vpkmanager`, `shiv_core`, `shiv_api`, `shiv_ui`.

- [ ] **Step 1:** `git init`, copy `LICENSE` and the three crates; strip `cdylib`, `ts-rs`, and FFI modules from the copied `vpk-parser`/`kv-parser` so they are `rlib` only.
- [ ] **Step 2:** Add empty `shiv-core`, `shiv-api`, `shiv-ui` crates; set `[profile.release]` to `lto = true, opt-level = 3, codegen-units = 1, panic = "abort", strip = true`.
- [ ] **Step 3:** Run `cargo build --workspace && cargo test -p vpk_parser -p kv_parser -p vpkmanager`. Expected: builds, copied tests pass (delete any test that needed removed FFI code).
- [ ] **Step 4:** Commit `chore: scaffold workspace with copied parser crates`.

### Task 2: Game location and addons model

**Files:**
- Create: `crates/shiv-core/src/game.rs`, `crates/shiv-core/src/addons.rs`, `crates/shiv-core/src/lib.rs`
- Test: `crates/shiv-core/tests/addons.rs`

**Interfaces:**
- Produces:
  - `game::locate() -> Result<GamePaths, GameError>` (uses `steamlocate`; reference `OLD/apps/desktop/src-tauri/src/mod_manager/steam_manager.rs` `find_game`), `GamePaths { root: PathBuf, addons: PathBuf, gameinfo: PathBuf, autoexec: PathBuf }`
  - `game::is_running() -> bool` (process scan for `deadlock`; reference `OLD/.../game_process_manager.rs`)
  - `addons::list(addons: &Path) -> Result<Vec<InstalledVpk>, AddonsError>`; `InstalledVpk { file: PathBuf, pak: u32, enabled: bool }`
  - `addons::set_enabled(vpk: &InstalledVpk, enabled: bool) -> Result<(), AddonsError>`; disabled = filename gains `.disabled`; never changes `pak` number
  - `addons::next_pak(addons: &Path) -> Result<u32, AddonsError>`
  - Pak files are named `pakNN_dir.vpk`. Check `OLD/.../mod_manager/shard.rs` for how the old app handles more than 99 paks (`addons2` overflow) and implement the same rule in `next_pak`.

- [ ] **Step 1:** Write failing tests in a tempdir: `lists_enabled_and_disabled`, `disable_then_enable_roundtrips_filename`, `next_pak_skips_used_numbers_including_disabled`, `hand_added_vpk_is_listed`.
- [ ] **Step 2:** Run `cargo test -p shiv-core --test addons`. Expected: FAIL (not defined).
- [ ] **Step 3:** Implement the functions above.
- [ ] **Step 4:** Run tests. Expected: PASS.
- [ ] **Step 5:** Commit `feat(core): game location and addons model`.

### Task 3: Sidecar metadata

**Files:**
- Create: `crates/shiv-core/src/sidecar.rs`
- Test: `crates/shiv-core/tests/sidecar.rs`

**Interfaces:**
- Consumes: `InstalledVpk` (Task 2)
- Produces: `Sidecar { mod_id: String, name: String, version: Option<String>, thumbnail_url: Option<String>, vpks: Vec<String> }`; `sidecar::read(addons: &Path, mod_id_or_vpk: &str)`, `sidecar::write(addons: &Path, &Sidecar)`, `sidecar::group(addons: &Path) -> Vec<InstalledMod>` where `InstalledMod { sidecar: Option<Sidecar>, vpks: Vec<InstalledVpk> }`. Files live in `<addons>/.shiv/<mod_id>.json`. A VPK with no sidecar becomes its own `InstalledMod` with `sidecar: None`.

- [ ] **Step 1:** Failing tests: `write_then_read_roundtrips`, `group_joins_vpks_to_their_mod`, `unknown_vpk_groups_alone`, `corrupt_sidecar_is_ignored_not_fatal`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement with `serde_json`; write atomically (temp file + rename). **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Commit `feat(core): sidecar metadata and mod grouping`.

### Task 4: API client

**Files:**
- Create: `crates/shiv-api/src/lib.rs`, `src/dto.rs`
- Test: `crates/shiv-api/tests/client.rs` (local `tiny_http` server)

**Interfaces:**
- Produces: `Client::new(base_url: &str) -> Client`; `Client::list_mods(&self, q: &ModQuery) -> Result<ModPage, ApiError>`; `Client::downloads(&self, id: &str) -> Result<Vec<DownloadFile>, ApiError>`; `Client::fetch(&self, url: &str, dest: &Path, progress: impl FnMut(u64, Option<u64>)) -> Result<(), ApiError>`; `Client::perf_catalog(&self, etag: Option<&str>) -> Result<Option<(String, Vec<u8>)>, ApiError>` (`None` on 304). `ModQuery { search, sort, category, page }`. DTO field names come from `OLD/packages/shared` and the handlers in `OLD/apps/api/src/routers/v2/mods.ts`; read them for exact shapes.

- [ ] **Step 1:** Failing tests against a stub server: `list_mods_parses_page`, `perf_catalog_304_returns_none`, `fetch_writes_file_and_reports_progress`, `fetch_removes_partial_file_on_error`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement with `ureq`; `fetch` writes to `dest.with_extension("part")` and renames on success. **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Commit `feat(api): client for mods, downloads and perf catalog`.

### Task 5: Installer

**Files:**
- Create: `crates/shiv-core/src/install.rs`
- Test: `crates/shiv-core/tests/install.rs` with small fixture archives built in the test

**Interfaces:**
- Consumes: `addons::next_pak`, `sidecar::write`, `game::is_running`, `shiv_api::Client::fetch`
- Produces: `install::install(archive: &Path, meta: ModMeta, paths: &GamePaths) -> Result<Sidecar, InstallError>`; `install::uninstall(m: &InstalledMod) -> Result<(), InstallError>`. `InstallError::GameRunning` is returned before any write.

- [ ] **Step 1:** Failing tests: `installs_zip_with_one_vpk`, `installs_multi_vpk_with_consecutive_paks`, `rejects_path_traversal_entry`, `archive_without_vpk_leaves_addons_unchanged`, `refuses_while_game_running` (inject the running check via a trait or fn parameter), `rar_and_7z_extract` (small checked-in fixtures).
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Extract into a temp dir inside `addons/.shiv/tmp`, validate every entry stays inside it, find `*.vpk`, rename each to the next `pakNN_dir.vpk`, then write the sidecar last. On any failure remove temp and partially moved files. **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Commit `feat(core): archive install and uninstall`.

### Task 6: UI shell and browse screen

**Files:**
- Create: `crates/shiv-ui/src/main.rs`, `ui/app.slint`, `ui/browse.slint`, `src/thumbs.rs`
- Test: `crates/shiv-ui/src/thumbs.rs` unit tests

**Interfaces:**
- Consumes: `shiv_api::Client`, `install::install`
- Produces: `thumbs::Cache::new(max_bytes: usize)`; `Cache::get_or_fetch(url) -> Option<Image>` decoding to display size (240x135) and evicting LRU. Navigation with five screens: Browse, Installed, Crosshair, Performance, Repair. Network work runs on a worker thread; the UI thread never blocks.

- [ ] **Step 1:** Failing tests: `cache_evicts_least_recently_used_over_budget`, `cache_decodes_to_display_size`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement the cache and the shell with the visual language from Global Constraints (one accent colour, one typeface, 8 px spacing grid). **Step 4:** Run tests, expect PASS.
- [ ] **Step 5:** Build the Browse screen: search box, sort, category filter, virtualised list, Install button with progress, error state with Retry when the API is unreachable.
- [ ] **Step 6:** Run `cargo run -p shiv-ui --release`. Check manually: scrolling is smooth, idle RSS under 150 MB (`ps -o rss`), API-down shows Retry.
- [ ] **Step 7:** Commit `feat(ui): shell and browse screen`.

### Task 7: Installed screen

**Files:**
- Modify: `crates/shiv-ui/ui/app.slint`; Create `ui/installed.slint`

**Interfaces:**
- Consumes: `sidecar::group`, `addons::set_enabled`, `install::uninstall`, `game::is_running`

- [ ] **Step 1:** List installed mods (name and thumbnail from sidecar; hand-added VPKs by filename), with enable toggle and uninstall. While the game is running, controls are disabled with an explanation.
- [ ] **Step 2:** Manual check: toggle a mod off and on, confirm the file gains and loses `.disabled` and its `pak` number is unchanged; uninstall removes VPKs and sidecar.
- [ ] **Step 3:** Commit `feat(ui): installed mods screen`.

### Task 8: Health check and launch

**Files:**
- Create: `crates/shiv-core/src/health.rs`, `src/launch.rs`
- Test: `crates/shiv-core/tests/health.rs`

**Interfaces:**
- Produces: `health::check(paths: &GamePaths) -> Vec<Finding>`; `Finding { kind: FindingKind, file: Option<PathBuf>, detail: String }`; `FindingKind::{UnreadableVpk, TruncatedVpk, ZeroByte, PartialDownload, DuplicatePak, MissingSearchPath, GameinfoUnwritable}`. `launch::launch(paths: &GamePaths) -> Result<(), LaunchError>` opens the Steam URI (reference `OLD/.../steam_uri_launcher.rs`). The UI calls `health::check` on Launch click and on app start, showing findings with Launch anyway / Cancel.
- `MissingSearchPath`: `gameinfo.gi` lacks the `Game citadel/addons` search path (see `OLD/.../gameinfo_search_paths.rs`); use `kv_parser`'s KeyValues inspection, not string matching.

- [ ] **Step 1:** Failing tests per `FindingKind` using tempdirs, plus `clean_folder_has_no_findings`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement; VPK validity via `vpk_parser`. **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Wire Launch button and findings dialog in the UI; manual check by planting a zero-byte VPK.
- [ ] **Step 6:** Commit `feat: health check and launch`.

### Task 9: Repair engine

**Files:**
- Create: `crates/shiv-repair/` copied and adapted from `OLD/apps/desktop/src-tauri/src/mod_manager/mod_health.rs` and `OLD/.../localization_overlay/` (about 20k lines including tests and a 2.5k-line corpus; copy source and the tests that run without large fixtures first)
- Modify: `crates/shiv-ui/ui/repair.slint`

**Interfaces:**
- Produces: `repair::analyze(paths: &GamePaths, build_id: u64) -> Vec<ModHealth>` and `repair::apply(paths: &GamePaths, mods: &[ModId]) -> Result<FixReport, RepairError>`, mirroring `ModHealth`/`FixModsResult` in the old `mod_health.rs`. `apply` writes a backup of changed VPKs first (see `OLD/.../repair_backup.rs`) and is a no-op while the game runs. `analyze` returns what would change without writing, which is the preview.
- This engine depends on `VdataHistoryIndex` (`OLD/.../vdata_history.rs`) and the `errors` module; copy those too and replace Tauri/`ts-rs` types with plain structs.

- [ ] **Step 1:** Copy sources; get `cargo build -p shiv-repair` green by deleting `ts-rs`/Tauri attributes and replacing `crate::errors::Error` with a local `thiserror` enum.
- [ ] **Step 2:** Run the copied tests: `cargo test -p shiv-repair`. Expected: PASS. Any test that needs missing fixtures is copied with its fixtures or removed with a note in `docs/repair-port.md`.
- [ ] **Step 3:** Failing test `preview_does_not_modify_files`: run `analyze` on a copy of a mod fixture and assert VPK bytes are unchanged. Then `apply_creates_backup_and_changes_file`.
- [ ] **Step 4:** Implement the thin `analyze`/`apply` API over the copied code. Run, expect PASS.
- [ ] **Step 5:** Repair screen: list mods with status (works, fixable, needs update), preview of what changes, Apply with backup confirmation.
- [ ] **Step 6:** Commit `feat: mod repair engine and screen`.

### Task 10: Crosshair

**Files:**
- Create: `crates/shiv-core/src/crosshair.rs`, `crates/shiv-ui/ui/crosshair.slint`
- Test: `crates/shiv-core/tests/crosshair.rs`

**Interfaces:**
- Produces: `CrosshairConfig { color: Rgb, pip_border: bool, pip_gap_static: bool, pip_opacity: f32, width: f32, height: f32, gap: f32, dot_opacity: f32, dot_outline_opacity: f32 }`; `crosshair::to_commands(&CrosshairConfig) -> String` and `crosshair::parse(&str) -> Option<CrosshairConfig>`, matching `OLD/packages/crosshair/src/config-generator.ts` exactly; `crosshair::apply(&GamePaths, &CrosshairConfig) -> Result<(), CrosshairError>` writing into the managed section of `autoexec.cfg` (reference `OLD/.../autoexec_manager.rs` `apply_crosshair`, preserving user lines outside the managed section).

- [ ] **Step 1:** Failing tests: `to_commands_matches_reference_string` (copy the expected string from the TS tests in `config-generator.test.ts`), `parse_roundtrips`, `apply_preserves_user_lines`, `apply_replaces_previous_managed_section`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement. **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Screen with sliders and a live preview drawn from the config (reference `OLD/packages/crosshair/src/renderer.ts` for geometry). Manual check: change a slider, preview updates; Apply writes `autoexec.cfg`.
- [ ] **Step 6:** Commit `feat: crosshair editor`.

### Task 11: Performance configs

**Files:**
- Create: `crates/shiv-core/src/perf.rs`, `crates/shiv-ui/ui/performance.slint`
- Test: `crates/shiv-core/tests/perf.rs`

**Interfaces:**
- Consumes: `shiv_api::Client::perf_catalog`, `kv_parser`
- Produces: `perf::Catalog::parse(&[u8]) -> Result<Catalog, PerfError>` (shape per `OLD/packages/perf-catalog/src/schema.ts` and `OLD/.../perf_config/catalog.rs`; reject a different `SCHEMA_VERSION`); `Catalog::preset(Tier) -> Vec<ConfigEntry>` with `Tier::{Pretty, Balanced, Lean, Sweaty, Potato}`; `perf::read_current(&GamePaths) -> Result<Vec<ConfigEntry>, PerfError>`; `perf::apply(&GamePaths, &[ConfigEntry]) -> Result<(), PerfError>` writing only the listed paths in `gameinfo.gi`, `value: None` commenting the key out. `ConfigEntry { path: Vec<String>, value: Option<String> }`. Writes are atomic with a `.bak` copy first. Catalog cached on disk with its etag.

- [ ] **Step 1:** Failing tests: `parse_rejects_wrong_schema_version`, `preset_returns_entries_for_each_tier`, `apply_changes_only_listed_keys`, `apply_none_comments_key_out`, `apply_fails_cleanly_on_readonly_gameinfo` (file unchanged), `apply_keeps_backup`.
- [ ] **Step 2:** Run, expect FAIL. **Step 3:** Implement; edit `gameinfo.gi` through `kv_parser` source-preserving edits (see how `OLD/.../perf_config/patch.rs` does it and copy that approach). **Step 4:** Run, expect PASS.
- [ ] **Step 5:** Screen: five preset buttons, a searchable per-setting editor using each convar's kind, range and help text, a diff of pending changes, Apply. Manual check against a copy of `gameinfo.gi`.
- [ ] **Step 6:** Commit `feat: performance presets and per-setting editor`.

### Task 12: Release verification

**Files:**
- Create: `docs/perf-results.md`

- [ ] **Step 1:** `cargo build --release -p shiv-ui`; record binary size.
- [ ] **Step 2:** With the Browse list showing thumbnails, record RSS after 60 s idle and after scrolling the whole list. Pass: under 150 MB.
- [ ] **Step 3:** Owner confirms scrolling feels smooth on Arch (Wayland and, if available, X11).
- [ ] **Step 4:** Run `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`. Expected: clean.
- [ ] **Step 5:** Commit `docs: release performance results`.
