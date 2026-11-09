//! Start Deadlock through Steam by opening its `steam://run` URI.
//!
//! The URI matches the old manager's `steam_manager.rs`. The opener comes from
//! the platform module and receives the URI as a single argument (no shell).

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::game::GamePaths;
use crate::platform;

/// `steam://run/<appid>//`: start Deadlock (app 1422450) with no extra arguments.
pub const STEAM_URI: &str = "steam://run/1422450//";

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("Deadlock was not found at {0}")]
    GameMissing(PathBuf),
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
}

/// The opener command for [`STEAM_URI`], not yet spawned.
pub fn command() -> Command {
    let (program, args) = platform::uri_opener();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .arg(STEAM_URI)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Open the Steam URI for Deadlock. Returns once the opener has started; Steam
/// itself starts the game.
pub fn launch(paths: &GamePaths) -> Result<(), LaunchError> {
    launch_with(paths, spawn_detached)
}

/// [`launch`] with an injectable spawner, so tests never start Steam.
pub fn launch_with(
    paths: &GamePaths,
    spawn: impl FnOnce(Command) -> io::Result<()>,
) -> Result<(), LaunchError> {
    if !paths.root.is_dir() {
        return Err(LaunchError::GameMissing(paths.root.clone()));
    }
    let cmd = command();
    let program = cmd.get_program().to_string_lossy().into_owned();
    spawn(cmd).map_err(|source| LaunchError::Spawn { program, source })
}

/// Spawn and reap on a helper thread, so the opener never lingers as a zombie
/// and the caller is not blocked while it hands the URI over.
fn spawn_detached(mut cmd: Command) -> io::Result<()> {
    let mut child = cmd.spawn()?;
    let _ = std::thread::Builder::new()
        .name("shiv-launch-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn game() -> (tempfile::TempDir, GamePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        (dir, paths)
    }

    #[test]
    fn command_passes_the_uri_as_one_argument() {
        let cmd = command();
        let (program, lead) = platform::uri_opener();
        assert_eq!(cmd.get_program(), program);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some(STEAM_URI));
        assert_eq!(args.len(), lead.len() + 1);
        #[cfg(target_os = "linux")]
        assert_eq!(program, "xdg-open");
    }

    #[test]
    fn launch_hands_the_command_to_the_spawner() {
        let (_dir, paths) = game();
        let seen = RefCell::new(None);
        launch_with(&paths, |cmd| {
            *seen.borrow_mut() = Some(cmd.get_program().to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.into_inner().unwrap(), platform::uri_opener().0);
    }

    #[test]
    fn spawn_failure_is_reported() {
        let (_dir, paths) = game();
        let err =
            launch_with(&paths, |_| Err(io::Error::from(io::ErrorKind::NotFound))).unwrap_err();
        assert!(matches!(err, LaunchError::Spawn { .. }), "{err}");
        assert!(err.to_string().starts_with("could not run "));
    }

    #[test]
    fn missing_game_is_refused_before_spawning() {
        let paths = GamePaths::from_root(PathBuf::from("/nonexistent/shiv/Deadlock"));
        let err = launch_with(&paths, |_| panic!("must not spawn")).unwrap_err();
        assert!(matches!(err, LaunchError::GameMissing(_)));
    }
}
