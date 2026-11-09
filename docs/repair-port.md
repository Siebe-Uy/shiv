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
