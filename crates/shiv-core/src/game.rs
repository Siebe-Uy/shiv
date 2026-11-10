//! Locating the Deadlock install and detecting a running game.

use std::path::{Path, PathBuf};

use steamlocate::SteamDir;

use crate::platform;

/// Steam app id of Deadlock.
const DEADLOCK_APP_ID: u32 = 1_422_450;

#[derive(Debug, thiserror::Error)]
pub enum GameError {
    #[error("Deadlock was not found in any Steam library")]
    NotFound,
    #[error("failed to read Steam libraries: {0}")]
    Steam(#[from] steamlocate::Error),
}

/// Key locations inside a Deadlock install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GamePaths {
    pub root: PathBuf,
    pub addons: PathBuf,
    pub gameinfo: PathBuf,
    pub autoexec: PathBuf,
}

impl GamePaths {
    /// Derive all paths from the install root (the `Deadlock` folder).
    pub fn from_root(root: PathBuf) -> Self {
        let citadel = root.join("game").join("citadel");
        Self {
            addons: citadel.join("addons"),
            gameinfo: citadel.join("gameinfo.gi"),
            autoexec: citadel.join("cfg").join("autoexec.cfg"),
            root,
        }
    }
}

fn find_in(steam: &SteamDir) -> Result<Option<PathBuf>, GameError> {
    let Some((app, library)) = steam.find_app(DEADLOCK_APP_ID)? else {
        return Ok(None);
    };
    Ok(Some(library.resolve_app_dir(&app)))
}

/// Find Deadlock via steamlocate, falling back to well-known Steam directories.
pub fn locate() -> Result<GamePaths, GameError> {
    locate_with(
        || find_in(&SteamDir::locate()?),
        &platform::default_steam_dirs(),
    )
}

/// `primary` errors or finding nothing both fall through to `fallback_dirs`.
fn locate_with(
    primary: impl FnOnce() -> Result<Option<PathBuf>, GameError>,
    fallback_dirs: &[PathBuf],
) -> Result<GamePaths, GameError> {
    if let Ok(Some(root)) = primary() {
        return Ok(GamePaths::from_root(root));
    }
    for dir in fallback_dirs {
        if let Some(root) = locate_in_steam_dir(dir)? {
            return Ok(GamePaths::from_root(root));
        }
    }
    Err(GameError::NotFound)
}

fn locate_in_steam_dir(dir: &Path) -> Result<Option<PathBuf>, GameError> {
    if !dir.join("steamapps").is_dir() {
        return Ok(None);
    }
    match SteamDir::from_dir(dir) {
        Ok(steam) => find_in(&steam),
        Err(_) => Ok(None),
    }
}

/// True when any name is the Deadlock executable (native or Proton, case-insensitive).
pub fn any_deadlock_process<'a>(names: impl Iterator<Item = &'a str>) -> bool {
    names
        .into_iter()
        .any(|n| n.eq_ignore_ascii_case("deadlock.exe") || n.eq_ignore_ascii_case("deadlock"))
}

/// Whether [`is_running`] can tell: false where process scanning is not
/// implemented, so `is_running` always says "not running" there.
pub fn can_detect() -> bool {
    platform::CAN_SCAN_PROCESSES
}

/// True while a Deadlock process is running.
pub fn is_running() -> bool {
    let names = platform::process_names();
    any_deadlock_process(names.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_deadlock_names() {
        assert!(any_deadlock_process(["bash", "deadlock.exe"].into_iter()));
        assert!(any_deadlock_process(["Deadlock.EXE"].into_iter()));
        assert!(!any_deadlock_process(
            ["python3", "deadlock-mod-manager"].into_iter()
        ));
        assert!(!any_deadlock_process(std::iter::empty()));
    }

    #[test]
    fn locate_falls_through_when_primary_errors() {
        let err = locate_with(|| Err(GameError::NotFound), &[]);
        assert!(matches!(err, Err(GameError::NotFound)));
        let ok = locate_with(|| Ok(Some(PathBuf::from("/g/Deadlock"))), &[]).unwrap();
        assert_eq!(ok.root, PathBuf::from("/g/Deadlock"));
        // A missing fallback dir is skipped, not fatal.
        let none = locate_with(
            || Err(GameError::NotFound),
            &[PathBuf::from("/nonexistent")],
        );
        assert!(matches!(none, Err(GameError::NotFound)));
    }

    #[test]
    fn detection_is_available_only_where_processes_are_scanned() {
        assert_eq!(can_detect(), cfg!(target_os = "linux"));
    }

    #[test]
    fn paths_derive_from_root() {
        let p = GamePaths::from_root(PathBuf::from("/g/Deadlock"));
        assert_eq!(p.addons, PathBuf::from("/g/Deadlock/game/citadel/addons"));
        assert_eq!(
            p.gameinfo,
            PathBuf::from("/g/Deadlock/game/citadel/gameinfo.gi")
        );
    }
}
