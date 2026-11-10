//! Open a web page (a mod's GameBanana page) in the user's browser.
//!
//! Only `https://` links are opened: the URL comes from the mod catalog, and a
//! `file:`, `steam:` or custom-scheme link handed to the system opener could
//! start other programs. The opener receives the URL as a single argument
//! (no shell), as for the Steam launch.

use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::launch::{self, OpenerOutcome};
use crate::platform;

/// How long to wait for the opener before treating the link as handed over.
const OPENER_WAIT: Duration = Duration::from_secs(10);
/// Longest link accepted.
const MAX_LEN: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("only https links are opened")]
    NotHttps,
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("{program} could not open the link ({status})")]
    OpenerFailed { program: String, status: String },
}

/// True for an `https://` URL with a host and no whitespace or control
/// characters (nothing an opener could read as a second argument or option).
pub fn is_web_link(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    url.len() <= MAX_LEN
        && !host.is_empty()
        && !host.starts_with('-')
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The opener command for `url`, not yet spawned.
pub fn command(url: &str) -> Command {
    let (program, args) = platform::uri_opener();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Open `url` in the browser. Blocks for up to 10 s; call it from a worker.
pub fn open_web_link(url: &str) -> Result<(), LinkError> {
    open_web_link_with(url, |cmd| launch::run_opener(cmd, OPENER_WAIT))
}

/// [`open_web_link`] with an injectable runner, so tests open nothing.
pub fn open_web_link_with(
    url: &str,
    run: impl FnOnce(Command) -> io::Result<OpenerOutcome>,
) -> Result<(), LinkError> {
    if !is_web_link(url) {
        return Err(LinkError::NotHttps);
    }
    let cmd = command(url);
    let program = cmd.get_program().to_string_lossy().into_owned();
    match run(cmd) {
        Err(source) => Err(LinkError::Spawn { program, source }),
        Ok(OpenerOutcome::Exited(status))
            if !status.success() && platform::opener_exit_status_is_meaningful() =>
        {
            Err(LinkError::OpenerFailed {
                program,
                status: status.to_string(),
            })
        }
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_https_links_are_web_links() {
        assert!(is_web_link("https://gamebanana.com/mods/738243"));
        assert!(is_web_link("https://gamebanana.com/sounds/95493?x=1#top"));
        for bad in [
            "http://gamebanana.com/mods/1",
            "file:///etc/passwd",
            "steam://run/1422450//",
            "javascript:alert(1)",
            "https://",
            "https:///path",
            "https://-oProxyCommand=x/",
            "https://x.test/a b",
            "https://x.test/a\nb",
            " https://x.test/",
            "HTTPS//x",
            "",
        ] {
            assert!(!is_web_link(bad), "{bad:?}");
        }
        assert!(!is_web_link(&format!(
            "https://x.test/{}",
            "a".repeat(MAX_LEN)
        )));
    }

    #[test]
    fn opens_with_the_url_as_one_argument_and_refuses_others() {
        let mut seen = None;
        open_web_link_with("https://gamebanana.com/mods/1", |cmd| {
            seen = Some(
                cmd.get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            );
            Ok(OpenerOutcome::StillRunning)
        })
        .expect("opened");
        let args = seen.expect("runner called");
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://gamebanana.com/mods/1")
        );

        let mut called = false;
        let refused = open_web_link_with("file:///etc/passwd", |_| {
            called = true;
            Ok(OpenerOutcome::StillRunning)
        });
        assert!(matches!(refused, Err(LinkError::NotHttps)));
        assert!(!called);

        let spawn = open_web_link_with("https://x.test/", |_| {
            Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
        });
        assert!(matches!(spawn, Err(LinkError::Spawn { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn opener_failure_is_reported() {
        use std::os::unix::process::ExitStatusExt;
        let failed = open_web_link_with("https://x.test/", |_| {
            Ok(OpenerOutcome::Exited(std::process::ExitStatus::from_raw(
                3 << 8,
            )))
        });
        assert!(matches!(failed, Err(LinkError::OpenerFailed { .. })));
    }
}
