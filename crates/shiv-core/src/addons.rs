//! The addons directory model: `pakNN_dir.vpk` files, optionally `.disabled`.

use std::path::{Path, PathBuf};
use std::{fs, io};

/// Extension appended to a pak file's name while it is disabled.
const DISABLED_EXT: &str = ".disabled";
/// The engine loads at most 99 paks per search path.
pub const PAK_CAPACITY: u32 = 99;

#[derive(Debug, thiserror::Error)]
pub enum AddonsError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0} is not a pakNN_dir.vpk file")]
    NotAPak(PathBuf),
    #[error("{target} already exists")]
    TargetExists { target: PathBuf },
    #[error(
        "{dir} already holds {PAK_CAPACITY} paks; overflow goes to the sibling `addons2` directory"
    )]
    Full { dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVpk {
    pub file: PathBuf,
    pub pak: u32,
    pub enabled: bool,
}

/// Parse `pakNN_dir.vpk[.disabled]` into (pak, enabled).
fn parse_name(name: &str) -> Option<(u32, bool)> {
    let (stem, enabled) = match name.strip_suffix(DISABLED_EXT) {
        Some(s) => (s, false),
        None => (name, true),
    };
    let digits = stem.strip_prefix("pak")?.strip_suffix("_dir.vpk")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, enabled))
}

/// List pak files in `addons`; other files are ignored. A missing directory is empty.
pub fn list(addons: &Path) -> Result<Vec<InstalledVpk>, AddonsError> {
    let io_err = |source| AddonsError::Io {
        path: addons.to_path_buf(),
        source,
    };
    let entries = match fs::read_dir(addons) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some((pak, enabled)) = parse_name(&name) {
            out.push(InstalledVpk {
                file: entry.path(),
                pak,
                enabled,
            });
        }
    }
    out.sort_by_key(|v| (v.pak, !v.enabled));
    Ok(out)
}

/// Enable or disable a pak by renaming it in place. The pak number never changes.
pub fn set_enabled(vpk: &InstalledVpk, enabled: bool) -> Result<(), AddonsError> {
    if vpk.enabled == enabled {
        return Ok(());
    }
    let name = vpk
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AddonsError::NotAPak(vpk.file.clone()))?;
    let new_name = if enabled {
        name.strip_suffix(DISABLED_EXT)
            .ok_or_else(|| AddonsError::NotAPak(vpk.file.clone()))?
            .to_string()
    } else {
        format!("{name}{DISABLED_EXT}")
    };
    let target = vpk.file.with_file_name(new_name);
    if target.exists() {
        return Err(AddonsError::TargetExists { target });
    }
    fs::rename(&vpk.file, &target).map_err(|source| AddonsError::Io {
        path: vpk.file.clone(),
        source,
    })
}

/// Lowest unused pak number in `addons` (1..=99), counting disabled paks as used.
///
/// Overflow rule (as in the old manager's shard.rs): the engine loads at most 99
/// `pakNN_dir.vpk` per search path, so a full directory returns [`AddonsError::Full`]
/// and the caller places the pak in the sibling `addons2` directory (then `addons3`,
/// and so on), numbering from 01 again there.
pub fn next_pak(addons: &Path) -> Result<u32, AddonsError> {
    let used: Vec<u32> = list(addons)?.into_iter().map(|v| v.pak).collect();
    (1..=PAK_CAPACITY)
        .find(|n| !used.contains(n))
        .ok_or_else(|| AddonsError::Full {
            dir: addons.to_path_buf(),
        })
}
