//! Start Deadlock through Steam by opening its `steam://run` URI.
//!
//! The URI matches the old manager's `steam_manager.rs`. The opener comes from
//! the platform module and receives the URI as a single argument (no shell).

use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::game::GamePaths;
use crate::platform;

/// `steam://run/<appid>//`: start Deadlock (app 1422450) with no extra arguments.
pub const STEAM_URI: &str = "steam://run/1422450//";

/// How long to wait for the opener to report. Openers that hand the URI over
/// exit at once; one that runs the handler in the foreground (Steam itself,
/// when it was not running) can stay up for the whole session, so after this
/// long it is taken to have succeeded and is reaped in the background.
const OPENER_WAIT: Duration = Duration::from_secs(10);
const OPENER_POLL: Duration = Duration::from_millis(50);

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
    #[error("{program} could not open the Steam link ({status}); is Steam installed?")]
    OpenerFailed { program: String, status: String },
}

/// What the opener did within [`OPENER_WAIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenerOutcome {
    Exited(ExitStatus),
    /// Still running when the wait ended (handed off, reaped in the background).
    StillRunning,
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

/// Open the Steam URI for Deadlock and report whether the opener accepted it.
/// Blocks for up to [`OPENER_WAIT`]; call it from a worker thread.
pub fn launch(paths: &GamePaths) -> Result<(), LaunchError> {
    launch_with(paths, |cmd| run_opener(cmd, OPENER_WAIT))
}

/// [`launch`] with an injectable runner, so tests never start Steam.
pub fn launch_with(
    paths: &GamePaths,
    run: impl FnOnce(Command) -> io::Result<OpenerOutcome>,
) -> Result<(), LaunchError> {
    if !paths.root.is_dir() {
        return Err(LaunchError::GameMissing(paths.root.clone()));
    }
    let cmd = command();
    let program = cmd.get_program().to_string_lossy().into_owned();
    match run(cmd) {
        Err(source) => Err(LaunchError::Spawn { program, source }),
        Ok(OpenerOutcome::Exited(status))
            if !status.success() && platform::opener_exit_status_is_meaningful() =>
        {
            Err(LaunchError::OpenerFailed {
                program,
                status: status.to_string(),
            })
        }
        Ok(_) => Ok(()),
    }
}

/// Spawn `cmd` and wait up to `wait` for it to exit. A child still running
/// after that is reaped on a helper thread so it never lingers as a zombie.
pub fn run_opener(mut cmd: Command, wait: Duration) -> io::Result<OpenerOutcome> {
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(OpenerOutcome::Exited(status));
        }
        if start.elapsed() >= wait {
            let _ = std::thread::Builder::new()
                .name("shiv-launch-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return Ok(OpenerOutcome::StillRunning);
        }
        std::thread::sleep(OPENER_POLL);
    }
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

    #[cfg(unix)]
    fn exit(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
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

    #[cfg(unix)]
    #[test]
    fn launch_hands_the_command_to_the_runner() {
        let (_dir, paths) = game();
        let seen = RefCell::new(None);
        launch_with(&paths, |cmd| {
            *seen.borrow_mut() = Some(cmd.get_program().to_owned());
            Ok(OpenerOutcome::Exited(exit(0)))
        })
        .unwrap();
        assert_eq!(seen.into_inner().unwrap(), platform::uri_opener().0);
    }

    #[cfg(unix)]
    #[test]
    fn opener_failure_is_reported() {
        let (_dir, paths) = game();
        let err = launch_with(&paths, |_| Ok(OpenerOutcome::Exited(exit(4)))).unwrap_err();
        assert!(matches!(err, LaunchError::OpenerFailed { .. }), "{err}");
        assert!(err.to_string().contains("could not open the Steam link"));
        assert!(err.to_string().contains('4'), "{err}");
    }

    #[test]
    fn opener_still_running_counts_as_handed_off() {
        let (_dir, paths) = game();
        launch_with(&paths, |_| Ok(OpenerOutcome::StillRunning)).unwrap();
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

    /// The real runner against harmless stand-ins for the opener.
    #[cfg(unix)]
    #[test]
    fn run_opener_reports_exit_status_and_hand_off() {
        let failed = run_opener(Command::new("false"), Duration::from_secs(10)).unwrap();
        assert!(matches!(failed, OpenerOutcome::Exited(s) if !s.success()));
        let ok = run_opener(Command::new("true"), Duration::from_secs(10)).unwrap();
        assert!(matches!(ok, OpenerOutcome::Exited(s) if s.success()));
        let mut slow = Command::new("sleep");
        slow.arg("2");
        let start = Instant::now();
        let running = run_opener(slow, Duration::from_millis(100)).unwrap();
        assert_eq!(running, OpenerOutcome::StillRunning);
        assert!(start.elapsed() < Duration::from_secs(2));
        let missing = run_opener(
            Command::new("/nonexistent/shiv-opener"),
            Duration::from_secs(1),
        );
        assert!(missing.is_err());
    }
}
