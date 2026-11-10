//! Launch Deadlock without mods by taking the mod mounts out of `gameinfo.gi`
//! for one session.
//!
//! Mods load because `gameinfo.gi` mounts `citadel/addons` (and Shiv's own
//! `citadel/shiv_repair` overlay) as search paths. [`enable`] saves the
//! original next to it and writes a copy without those two lines;
//! [`restore`] puts the original back. Restoring is idempotent and safe to
//! call at any time, so Shiv also calls it at startup in case a session ended
//! without it (a crash, a power cut).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::game::GamePaths;

/// Search-path lines that mount mods.
static MOD_MOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*Game\s+citadel/(addons|shiv_repair)/?\s*(//.*)?$").expect("valid regex")
});

/// Where the original `gameinfo.gi` is kept while mods are off.
pub fn backup_path(paths: &GamePaths) -> PathBuf {
    let mut name = paths.gameinfo.as_os_str().to_owned();
    name.push(".shiv-vanilla");
    PathBuf::from(name)
}

/// Whether a vanilla session is in progress (its backup exists).
pub fn is_active(paths: &GamePaths) -> bool {
    backup_path(paths).is_file()
}

/// `text` without the mod mounts, line endings kept. `None` when it had none
/// (mods were not mounted, so there is nothing to turn off).
fn without_mounts(text: &str) -> Option<String> {
    let mut found = false;
    let out: String = text
        .split_inclusive('\n')
        .filter(|line| {
            let hit = MOD_MOUNT.is_match(line.trim_end_matches(['\r', '\n']));
            found |= hit;
            !hit
        })
        .collect();
    found.then_some(out)
}

/// Write the vanilla `gameinfo.gi`, keeping the original in [`backup_path`].
/// Does nothing when a session is already active.
pub fn enable(paths: &GamePaths) -> io::Result<()> {
    if is_active(paths) {
        return Ok(());
    }
    let original = fs::read_to_string(&paths.gameinfo)?;
    let Some(vanilla) = without_mounts(&original) else {
        return Ok(());
    };
    let backup = backup_path(paths);
    fs::write(&backup, &original)?;
    if let Err(e) = write_replace(&paths.gameinfo, &vanilla) {
        let _ = fs::remove_file(&backup);
        return Err(e);
    }
    Ok(())
}

/// Put the original `gameinfo.gi` back. If the file changed since [`enable`]
/// (a Steam update rewrote it), the newer file wins and the backup is dropped.
pub fn restore(paths: &GamePaths) -> io::Result<()> {
    let backup = backup_path(paths);
    let original = match fs::read_to_string(&backup) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let current = fs::read_to_string(&paths.gameinfo).ok();
    if current.as_deref() == without_mounts(&original).as_deref() {
        write_replace(&paths.gameinfo, &original)?;
    }
    fs::remove_file(&backup)
}

/// Replace `path` through a temp file so a crash never leaves it half-written.
fn write_replace(path: &Path, text: &str) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".shiv-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GI: &str = "GameInfo\r\n{\r\n\tFileSystem\r\n\t{\r\n\t\tSearchPaths\r\n\t\t{\r\n\t\t\tGame                citadel/dmm_localization_default\r\n\t\t\tGame                citadel/shiv_repair\r\n\t\t\tGame                citadel/addons\r\n\t\t\tMod                 citadel\r\n\t\t\tGame                citadel\r\n\t\t}\r\n\t}\r\n}\r\n";

    fn setup(text: &str) -> (tempfile::TempDir, GamePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        fs::create_dir_all(paths.gameinfo.parent().unwrap()).unwrap();
        fs::write(&paths.gameinfo, text).unwrap();
        (dir, paths)
    }

    #[test]
    fn strips_only_the_mod_mounts_and_keeps_line_endings() {
        let out = without_mounts(GI).unwrap();
        assert!(!out.contains("addons") && !out.contains("shiv_repair"));
        assert!(out.contains("dmm_localization_default"));
        assert!(out.contains("Game                citadel\r\n"));
        assert_eq!(without_mounts("Game citadel\n"), None);
    }

    #[test]
    fn enable_then_restore_round_trips() {
        let (_dir, paths) = setup(GI);
        enable(&paths).unwrap();
        assert!(is_active(&paths));
        assert!(
            !fs::read_to_string(&paths.gameinfo)
                .unwrap()
                .contains("citadel/addons")
        );
        enable(&paths).unwrap(); // idempotent: the backup keeps the original
        restore(&paths).unwrap();
        assert!(!is_active(&paths));
        assert_eq!(fs::read_to_string(&paths.gameinfo).unwrap(), GI);
        restore(&paths).unwrap(); // nothing to do
    }

    #[test]
    fn a_gameinfo_rewritten_meanwhile_is_not_overwritten() {
        let (_dir, paths) = setup(GI);
        enable(&paths).unwrap();
        fs::write(&paths.gameinfo, "NEW FROM STEAM").unwrap();
        restore(&paths).unwrap();
        assert_eq!(
            fs::read_to_string(&paths.gameinfo).unwrap(),
            "NEW FROM STEAM"
        );
        assert!(!is_active(&paths));
    }

    #[test]
    fn without_mounts_nothing_changes() {
        let (_dir, paths) = setup("Game citadel\n");
        enable(&paths).unwrap();
        assert!(!is_active(&paths));
    }
}
