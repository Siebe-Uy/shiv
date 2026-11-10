// SPDX-License-Identifier: GPL-3.0-only
//! Mod health checks and the repair engine.
//!
//! Ported from Deadlock Mod Manager (GPL-3.0,
//! `apps/desktop/src-tauri/src/mod_manager/`): `mod_health.rs`,
//! `vdata_history.rs`, `repair_backup.rs` and `localization_overlay/`. Tauri
//! events and `ts-rs` exports were removed and the app-wide error type was
//! replaced by [`RepairError`]. See `docs/repair-port.md` for what changed.

pub mod api;
pub mod error;
pub mod gameinfo;
pub mod localization_overlay;
pub mod mod_health;
pub mod repair_backup;
pub mod vdata_history;

pub use api::{
    FixReport, ModHealth, ModId, OverlayState, RemoveReport, RepairStatus, RestoreReport, analyze,
    apply, apply_with, can_undo, game_build, gameinfo_backup_path, has_package, overlay_dir,
    overlay_state, remove_package, remove_package_with, restore_last, restore_last_with,
};
pub use error::RepairError;
