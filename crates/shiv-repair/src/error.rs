// SPDX-License-Identifier: GPL-3.0-only
// Replaces the subset of Deadlock Mod Manager's `crate::errors::Error` that the
// repair engine uses.

/// Errors the repair engine returns.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Mod is invalid: {0}")]
    ModInvalid(String),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Failed to create backup: {0}")]
    BackupCreationFailed(String),
    #[error("Failed to restore backup: {0}")]
    BackupRestoreFailed(String),
    #[error("Deadlock is running; close the game before repairing mods")]
    GameRunning,
    #[error("{0}")]
    Gameinfo(String),
}
