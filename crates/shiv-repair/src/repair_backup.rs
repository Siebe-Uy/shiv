// SPDX-License-Identifier: GPL-3.0-only
// Adapted from Deadlock Mod Manager's `mod_manager/repair_backup.rs` (GPL-3.0).
// Shiv has one flat addons folder, so the shard locators, profile folders and
// `VpkSnapshot` of the original are replaced by bare VPK file names and a local
// rollback list.

use crate::error::RepairError as Error;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const REPAIR_BACKUP_PREFIX: &str = "addons-backup-repair-";

/// Index format written by this version. Version 1 (no `version` field)
/// listed bare `files` only; version 2 adds `entries` with each file's target
/// folder, so a backup set can be restored without knowing where it came from.
const INDEX_VERSION: u32 = 2;
const INDEX_NAME: &str = "repair.json";

fn legacy_version() -> u32 {
    1
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupEntry {
    name: String,
    /// Folder the file lives in.
    target: PathBuf,
    /// Whether the file existed when the backup was taken. Restoring removes
    /// files that did not.
    existed: bool,
    /// Whether `target` existed; restoring removes it again when it is empty.
    target_existed: bool,
    #[serde(default)]
    modified: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairBackupIndex {
    #[serde(default = "legacy_version")]
    version: u32,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    entries: Vec<BackupEntry>,
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
    let targets: Vec<(&Path, &str)> = files.iter().map(|f| (addons, f.as_str())).collect();
    write_backup(directory, &targets, true, &[])
}

/// Back up `targets` (folder, bare file name), which may live in different
/// folders, into one new timestamped folder under `directory`. Files that do
/// not exist are recorded as absent, so [`restore_backup_set`] removes them
/// again; their names must still be unique within the set. `new_dirs` are
/// target folders the caller created for the change being backed up; a
/// restore removes them again once they are empty.
pub fn create_backup_set(
    directory: &Path,
    targets: &[(&Path, &str)],
    new_dirs: &[&Path],
) -> Result<PathBuf, Error> {
    write_backup(directory, targets, false, new_dirs)
}

fn write_backup(
    directory: &Path,
    targets: &[(&Path, &str)],
    require_existing: bool,
    new_dirs: &[&Path],
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
    let mut entries: Vec<BackupEntry> = Vec::new();
    for (target, file) in targets {
        let file = bare_file_name(file)?;
        if entries.iter().any(|entry| entry.name == file) {
            return Err(Error::InvalidInput(format!(
                "Backup lists {file} more than once"
            )));
        }
        let source = target.join(file);
        let metadata = match fs::metadata(&source) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => {
                return Err(Error::InvalidInput(format!(
                    "{} is not a file",
                    source.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !require_existing => None,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_some() {
            let destination = files_dir.join(file);
            fs::copy(&source, &destination)?;
            OpenOptions::new()
                .write(true)
                .open(destination)?
                .sync_all()?;
        }
        entries.push(BackupEntry {
            name: file.to_owned(),
            target: target.to_path_buf(),
            existed: metadata.is_some(),
            target_existed: target.is_dir() && !new_dirs.contains(target),
            modified: metadata.and_then(|metadata| metadata.modified().ok()),
        });
    }
    let index = RepairBackupIndex {
        version: INDEX_VERSION,
        files: targets.iter().map(|(_, file)| (*file).to_owned()).collect(),
        entries,
    };
    let index_path = staging.path().join(INDEX_NAME);
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

fn read_index(backup: &Path) -> Result<RepairBackupIndex, Error> {
    serde_json::from_slice(&fs::read(backup.join(INDEX_NAME))?)
        .map_err(|error| Error::BackupRestoreFailed(error.to_string()))
}

/// What [`restore_backup_set`] put back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Files written back from the backup.
    pub restored: Vec<PathBuf>,
    /// Files removed because they did not exist when the backup was taken.
    pub removed: Vec<PathBuf>,
}

/// Put every file of a [`create_backup_set`] backup back where it was,
/// overwriting the current file, and remove files that did not exist then.
/// Files whose bytes already match are left alone (their timestamp is reset).
/// Every copy is staged and synced beside its target before the first one
/// replaces anything, and each replacement is a rename. Restored files get
/// their original modification time.
pub fn restore_backup_set(backup: &Path) -> Result<RestoreOutcome, Error> {
    let index = read_index(backup)?;
    if index.version < INDEX_VERSION || index.entries.is_empty() {
        return Err(Error::BackupRestoreFailed(format!(
            "{} does not record where its files belong",
            backup.display()
        )));
    }
    let mut staged = Vec::new();
    let mut touch = Vec::new();
    let mut removals = Vec::new();
    for entry in &index.entries {
        let name = bare_file_name(&entry.name)?;
        let destination = entry.target.join(name);
        if !entry.existed {
            if destination.exists() {
                removals.push(destination);
            }
            continue;
        }
        let source = backup.join("files").join(name);
        let saved = fs::read(&source)?;
        if fs::read(&destination).is_ok_and(|current| current == saved) {
            touch.push((destination, entry.modified));
            continue;
        }
        fs::create_dir_all(&entry.target)?;
        let mut file = tempfile::NamedTempFile::new_in(&entry.target)?;
        std::io::Write::write_all(&mut file, &saved)?;
        fs::set_permissions(file.path(), fs::metadata(&source)?.permissions())?;
        if let Some(modified) = entry.modified {
            file.as_file().set_modified(modified)?;
        }
        file.as_file().sync_all()?;
        staged.push((file, destination));
    }
    let mut outcome = RestoreOutcome::default();
    for (file, destination) in staged {
        file.persist(&destination)
            .map_err(|error| Error::BackupRestoreFailed(error.error.to_string()))?;
        outcome.restored.push(destination);
    }
    for (destination, modified) in touch {
        if let Some(modified) = modified
            && fs::metadata(&destination)?.modified().ok() != Some(modified)
        {
            OpenOptions::new()
                .write(true)
                .open(&destination)?
                .set_modified(modified)?;
        }
    }
    for destination in removals {
        fs::remove_file(&destination)?;
        outcome.removed.push(destination);
    }
    for entry in index.entries.iter().filter(|entry| !entry.target_existed) {
        // Fails while anything else is inside, which is what we want.
        let _ = fs::remove_dir(&entry.target);
    }
    Ok(outcome)
}

/// The newest restorable [`create_backup_set`] backup under `directory`.
pub fn latest_backup_set(directory: &Path) -> Result<Option<PathBuf>, Error> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_backup = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(REPAIR_BACKUP_PREFIX));
        if is_backup && path.is_dir() {
            candidates.push(path);
        }
    }
    // The timestamp in the name is fixed width, so names sort by time.
    candidates.sort();
    Ok(candidates.into_iter().rev().find(|path| {
        read_index(path)
            .is_ok_and(|index| index.version >= INDEX_VERSION && !index.entries.is_empty())
    }))
}

/// Copy a backup's files back into `addons`. Refuses to overwrite anything:
/// the caller removes the repaired files first. On failure every file this
/// call created is removed again.
pub fn restore_repair_backup(backup: &Path, addons: &Path) -> Result<(), Error> {
    let index = read_index(backup)?;
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

    #[test]
    fn backup_sets_restore_over_current_files_across_folders() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("kept.txt"), b"original").unwrap();
        let backup = create_backup_set(
            &root.path().join("backups"),
            &[(&a, "kept.txt"), (&b, "new.vpk")],
            &[],
        )
        .unwrap();
        let modified = fs::metadata(a.join("kept.txt"))
            .unwrap()
            .modified()
            .unwrap();

        fs::write(a.join("kept.txt"), b"changed").unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("new.vpk"), b"created later").unwrap();
        let outcome = restore_backup_set(&backup).unwrap();
        assert_eq!(outcome.restored, [a.join("kept.txt")]);
        assert_eq!(outcome.removed, [b.join("new.vpk")]);
        assert_eq!(fs::read(a.join("kept.txt")).unwrap(), b"original");
        assert_eq!(
            fs::metadata(a.join("kept.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
        // `b` did not exist when the backup was taken, so it goes again.
        assert!(!b.exists());
        // Restoring twice changes nothing.
        assert_eq!(
            restore_backup_set(&backup).unwrap(),
            RestoreOutcome::default()
        );
        assert_eq!(
            latest_backup_set(&root.path().join("backups")).unwrap(),
            Some(backup)
        );
    }

    #[test]
    fn version_one_backups_restore_only_through_the_addons_path() {
        let root = tempfile::tempdir().unwrap();
        let backup = root.path().join(format!("{REPAIR_BACKUP_PREFIX}old"));
        fs::create_dir_all(backup.join("files")).unwrap();
        fs::write(backup.join("files").join("pak01_dir.vpk"), b"v1").unwrap();
        fs::write(
            backup.join("repair.json"),
            br#"{"files":["pak01_dir.vpk"]}"#,
        )
        .unwrap();
        assert!(matches!(
            restore_backup_set(&backup),
            Err(Error::BackupRestoreFailed(_))
        ));
        assert_eq!(latest_backup_set(root.path()).unwrap(), None);
        let addons = root.path().join("addons");
        restore_repair_backup(&backup, &addons).unwrap();
        assert_eq!(fs::read(addons.join("pak01_dir.vpk")).unwrap(), b"v1");
    }

    #[test]
    fn backup_sets_reject_duplicate_names() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        assert!(matches!(
            create_backup_set(root.path(), &[(&a, "x.vpk"), (&b, "x.vpk")], &[]),
            Err(Error::InvalidInput(_))
        ));
    }
}
