// SPDX-License-Identifier: GPL-3.0-only
// Adapted from Deadlock Mod Manager's `mod_manager/repair_backup.rs` (GPL-3.0).
// Shiv has one flat addons folder, so the shard locators, profile folders and
// `VpkSnapshot` of the original are replaced by bare VPK file names and a local
// rollback list.

use crate::error::RepairError as Error;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

pub const REPAIR_BACKUP_PREFIX: &str = "addons-backup-repair-";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairBackupIndex {
    files: Vec<String>,
}

/// Accept only a bare file name (`pak03_dir.vpk`), never a path.
fn bare_file_name(value: &str) -> Result<&str, Error> {
    if value.is_empty()
        || Path::new(value).file_name().and_then(|name| name.to_str()) != Some(value)
    {
        return Err(Error::InvalidInput(format!(
            "Invalid VPK file name: {value}"
        )));
    }
    Ok(value)
}

/// Copy `files` (bare names inside `addons`) into a new, timestamped backup
/// folder under `directory`. The folder appears atomically: it is staged under
/// a temporary name and renamed once every copy is synced.
pub fn create_repair_backup(
    addons: &Path,
    directory: &Path,
    files: &[String],
) -> Result<PathBuf, Error> {
    fs::create_dir_all(directory)?;
    let name = format!(
        "{REPAIR_BACKUP_PREFIX}{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ")
    );
    let staging = tempfile::Builder::new()
        .prefix(".repair-")
        .tempdir_in(directory)?;
    let files_dir = staging.path().join("files");
    fs::create_dir_all(&files_dir)?;
    for file in files {
        let file = bare_file_name(file)?;
        let destination = files_dir.join(file);
        fs::copy(addons.join(file), &destination)?;
        OpenOptions::new()
            .write(true)
            .open(destination)?
            .sync_all()?;
    }
    let index = RepairBackupIndex {
        files: files.to_vec(),
    };
    let index_path = staging.path().join("repair.json");
    fs::write(
        &index_path,
        serde_json::to_vec_pretty(&index)
            .map_err(|error| Error::BackupCreationFailed(error.to_string()))?,
    )?;
    OpenOptions::new()
        .write(true)
        .open(index_path)?
        .sync_all()?;
    let destination = directory.join(name);
    let staged = staging.keep();
    if let Err(error) = fs::rename(&staged, &destination) {
        // Best effort: the staged copy is useless without its final name.
        let _ = fs::remove_dir_all(&staged);
        return Err(error.into());
    }
    Ok(destination)
}

/// Copy a backup's files back into `addons`. Refuses to overwrite anything:
/// the caller removes the repaired files first. On failure every file this
/// call created is removed again.
pub fn restore_repair_backup(backup: &Path, addons: &Path) -> Result<(), Error> {
    let index: RepairBackupIndex =
        serde_json::from_slice(&fs::read(backup.join("repair.json"))?)
            .map_err(|error| Error::BackupRestoreFailed(error.to_string()))?;
    let mut copies = Vec::new();
    for file in &index.files {
        let file = bare_file_name(file)?;
        let destination = addons.join(file);
        if destination.exists() {
            return Err(Error::BackupRestoreFailed(format!(
                "Repair restore would overwrite {}",
                destination.display()
            )));
        }
        copies.push((backup.join("files").join(file), destination));
    }
    fs::create_dir_all(addons)?;
    let mut created: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), Error> {
        for (source, destination) in &copies {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            created.push(destination.clone());
            std::io::copy(&mut fs::File::open(source)?, &mut output)?;
            output.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        for path in &created {
            let _ = fs::remove_file(path);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_then_restore_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let addons = root.path().join("addons");
        fs::create_dir_all(&addons).unwrap();
        fs::write(addons.join("pak01_dir.vpk"), b"original").unwrap();

        let backup = create_repair_backup(
            &addons,
            &root.path().join("backups"),
            &["pak01_dir.vpk".to_string()],
        )
        .unwrap();
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(REPAIR_BACKUP_PREFIX)
        );

        fs::remove_file(addons.join("pak01_dir.vpk")).unwrap();
        restore_repair_backup(&backup, &addons).unwrap();
        assert_eq!(fs::read(addons.join("pak01_dir.vpk")).unwrap(), b"original");
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let addons = root.path().join("addons");
        fs::create_dir_all(&addons).unwrap();
        fs::write(addons.join("pak01_dir.vpk"), b"original").unwrap();
        let backup = create_repair_backup(
            &addons,
            &root.path().join("backups"),
            &["pak01_dir.vpk".to_string()],
        )
        .unwrap();
        assert!(matches!(
            restore_repair_backup(&backup, &addons),
            Err(Error::BackupRestoreFailed(_))
        ));
    }

    #[test]
    fn rejects_paths_instead_of_file_names() {
        let root = tempfile::tempdir().unwrap();
        for bad in ["../pak01_dir.vpk", "sub/pak01_dir.vpk", ""] {
            assert!(matches!(
                create_repair_backup(root.path(), &root.path().join("b"), &[bad.to_string()]),
                Err(Error::InvalidInput(_))
            ));
        }
    }
}
