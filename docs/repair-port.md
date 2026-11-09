# Repair engine port

`crates/shiv-repair` holds the repair engine copied from Deadlock Mod Manager
(GPL-3.0), `apps/desktop/src-tauri/src/mod_manager/` at commit `aeca8fa7`.
`crates/source2-model` is a copy of that repo's `packages/source2-model`, which
the engine uses to decode compiled Source 2 resources.

## What was copied

| Old path | New path |
| --- | --- |
| `mod_manager/mod_health.rs` | `crates/shiv-repair/src/mod_health.rs` |
| `mod_manager/vdata_history.rs` | `crates/shiv-repair/src/vdata_history.rs` |
| `mod_manager/localization_overlay.rs` and `localization_overlay/` | `crates/shiv-repair/src/localization_overlay{.rs,/}` |
| `mod_manager/repair_backup.rs` | `crates/shiv-repair/src/repair_backup.rs` (rewritten, see below) |
| `src-tauri/assets/vdata-history.bin`, `localization-history.bin` (+ `.md`), `hero-model-bindings.json` | `crates/shiv-repair/assets/` (embedded with `include_bytes!`) |
| `packages/source2-model` | `crates/source2-model` |

## Adaptations

- `ts-rs` derives and `#[ts(...)]` attributes removed.
- `RepairPlanProgress::emit` (a Tauri event) removed. Progress is still
  reported through the `&dyn Fn(RepairPlanProgress)` callbacks.
- `crate::errors::Error` replaced by `shiv_repair::RepairError`, which keeps
  only the variants the engine uses (`Io`, `ModInvalid`, `InvalidInput`,
  `BackupCreationFailed`, `BackupRestoreFailed`) with the same messages.
- `crate::mod_manager::` paths shortened to `crate::`.
- Entry points that were `pub(crate)` (consumed by the old app's
  `manager/localization/` layer) are now `pub` so the crate builds without dead
  code warnings and part (b) can call them.
- `repair_backup.rs` rewritten for Shiv's single flat addons folder: shard
  locators and profile folders are gone, files are bare VPK names (paths are
  rejected), and the old `VpkSnapshot` rollback is a local list of created
  files. Three tests added.
- Two bare `unwrap()` calls in non-test code replaced with error-safe forms
  (`animation_contract.rs`, `animation_skeleton.rs`).
- `cargo fmt` applied (the old repo used 2-space indentation).
- `source2-model` and `vpkmanager`: mechanical fixes for clippy lints this
  toolchain (clippy 1.98) reports (`chunks_exact_to_as_chunks`,
  `needless_late_init`, `collapsible_if`, `double_must_use`). No behaviour change.
- Test-only dev-dependency `hex` added (used by the ignored corpus tests).

## Removed tests

None. Every copied test compiles. 30 tests stay `#[ignore]`d exactly as in the
old repo because they need a real game install and local mod corpora through
`DMM_*` environment variables (`DMM_REAL_CITADEL`, `DMM_COMPAT_CORPUS`,
`DMM_*_VPK`, ...): 24 in `compatibility_corpus.rs`, 2 in
`profile_benchmark.rs`, 2 in `material_compatibility/tests.rs`, 1 in
`camera_attachment_frame/tests.rs`, 1 in `localization_overlay.rs`. They read
game files only; never point them at a real addons folder you care about.

`source2-model` keeps its 4 ignored tests (they need game VPKs via
`DMM_VIEWER_TEST_VPK` and similar).

## Not copied

The orchestration layer that drove the engine in the old app,
`mod_manager/manager/localization/` (`health.rs`, `receipt.rs`, `review.rs`,
`history.rs`, ...), was not copied. It depends on the old manifest, profiles
and app-data layout. Shiv's `analyze`/`apply` API (Task 9b) replaces it.

## Shiv API (Task 9b)

`crates/shiv-repair/src/api.rs` replaces the old orchestration layer:

- `analyze(paths, build_id) -> Vec<ModHealth>` checks every installed mod
  (sidecar groups, enabled or disabled) on its own against the installed game,
  as the old `health.rs::check_mod` did, and returns its status (`Works`,
  `Fixable`, `NeedsUpdate`, `Fixed`, `NotChecked`), a summary line, one
  "what changes" line per repair, and the remaining problems. It only reads.
  `build_id` is `game_build(paths)` (`ClientVersion` from `steam.inf`).
- `apply(paths, mods) -> Result<FixReport, RepairError>` refuses while the
  game runs, then builds one package for all loaded mods (engine order: pak
  number) with repairs on for exactly `mods`, the way the old app did
  (`build_learning_history`), and writes it to
  `game/citadel/shiv_repair/pak01_dir.vpk` with a receipt
  (`shiv-repair.json`) and learned game data (`observed-history.bin`) beside
  it. `gameinfo.gi` gains one `Game citadel/shiv_repair` line directly before
  `Game citadel/addons` (old app: `citadel/dmm_localization_*`, mounted first).
  Mod VPKs are never modified. Existing files that are replaced or removed
  (package, receipt, learned data, `gameinfo.gi`) are copied first with
  `repair_backup::create_backup_set` into one folder under
  `addons/.shiv/repair-backups/`. The index (`repair.json`, version 2)
  records each file's target folder and whether it existed, so files `apply`
  created are removed on restore. If a write fails after the package is in
  place, `apply` restores that set and returns the error. An empty `mods`
  list removes the package (after a backup). `apply` creates `shiv_repair/`
  only when it writes a package.
- `restore_last(paths)` undoes the last apply from its backup set (refuses
  while the game runs) and renames the set `restored-...`, so the next call
  goes one apply further back. Restored files get their original bytes,
  permissions and modification time.
- The old review dialog has no Shiv equivalent: the package keeps the game's
  load order for choices between mods, and `FixReport::notes` lists what the
  old app would have asked about.
- `overlay_state(paths)` says whether the package still matches the installed
  mods and game build.
