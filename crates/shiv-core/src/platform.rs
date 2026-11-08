//! OS-specific code: process scanning and default Steam locations.

use std::path::PathBuf;

/// Names (process name and argv[0] basename) of every running process.
#[cfg(target_os = "linux")]
pub fn process_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let is_pid = file_name
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        let dir = entry.path();
        if let Ok(comm) = std::fs::read_to_string(dir.join("comm")) {
            names.push(comm.trim().to_string());
        }
        // Under Proton the game's argv[0] is a Windows path while comm may be truncated.
        if let Ok(cmdline) = std::fs::read(dir.join("cmdline"))
            && let Some(argv0) = cmdline.split(|b| *b == 0).next()
        {
            let argv0 = String::from_utf8_lossy(argv0);
            if let Some(base) = argv0.rsplit(['/', '\\']).next()
                && !base.is_empty()
            {
                names.push(base.to_string());
            }
        }
    }
    names
}

/// Process scanning is only implemented on Linux; other platforms report no processes.
#[cfg(not(target_os = "linux"))]
pub fn process_names() -> Vec<String> {
    Vec::new()
}

/// Steam installation roots to try when steamlocate finds none (Linux).
#[cfg(target_os = "linux")]
pub fn default_steam_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ]
}

/// No fallback Steam locations on other platforms; steamlocate's own lookup is used.
#[cfg(not(target_os = "linux"))]
pub fn default_steam_dirs() -> Vec<PathBuf> {
    Vec::new()
}
