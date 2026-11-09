// SPDX-License-Identifier: GPL-3.0-only
//! Mod health checks and the repair engine.
//!
//! Ported from Deadlock Mod Manager (GPL-3.0,
//! `apps/desktop/src-tauri/src/mod_manager/`): `mod_health.rs`,
//! `vdata_history.rs`, `repair_backup.rs` and `localization_overlay/`. Tauri
//! events and `ts-rs` exports were removed and the app-wide error type was
//! replaced by [`RepairError`]. See `docs/repair-port.md` for what changed.

pub mod error;
pub mod localization_overlay;
pub mod mod_health;
pub mod repair_backup;
pub mod vdata_history;

pub use error::RepairError;
