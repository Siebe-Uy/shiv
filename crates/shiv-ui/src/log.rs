//! Diagnostics. Windows builds have no console, so messages that would go to
//! stderr are appended to `%LOCALAPPDATA%\shiv\shiv.log` instead; elsewhere
//! they stay on stderr.

use std::fmt::Display;

/// Start the log and record panics (on any thread) in it.
pub(crate) fn init() {
    #[cfg(windows)]
    file::start();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write(format_args!("panic: {info}"));
        previous(info);
    }));
}

/// One line of diagnostics.
pub(crate) fn line(message: impl Display) {
    write(format_args!("{message}"));
}

#[cfg(not(windows))]
fn write(message: std::fmt::Arguments<'_>) {
    eprintln!("{message}");
}

#[cfg(windows)]
fn write(message: std::fmt::Arguments<'_>) {
    file::append(message);
}

#[cfg(windows)]
mod file {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Older output is dropped once the file passes this size, at startup.
    const MAX_BYTES: u64 = 256 * 1024;

    static LOG: Mutex<Option<File>> = Mutex::new(None);

    fn path() -> Option<PathBuf> {
        shiv_core::perf::CatalogCache::default_dir().map(|d| d.join("shiv.log"))
    }

    pub(super) fn start() {
        let Some(path) = path() else { return };
        let Some(dir) = path.parent() else { return };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let oversized = fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(!oversized)
            .truncate(oversized)
            .open(&path)
            .ok();
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = file;
        append(format_args!("shiv {} started", env!("CARGO_PKG_VERSION")));
    }

    pub(super) fn append(message: std::fmt::Arguments<'_>) {
        let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(file) = guard.as_mut() {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let _ = writeln!(file, "[{secs}] {message}");
        }
    }
}
