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
        "{dir} already holds {PAK_CAPACITY} paks; overflow goes to the next shard (`addons2` ..= `addons10`)"
    )]
    Full { dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVpk {
    pub file: PathBuf,
    /// `Some(NN)` for `pakNN_dir.vpk`; `None` for any other hand-added VPK.
    pub pak: Option<u32>,
    pub enabled: bool,
}

/// Parse a `*.vpk[.disabled]` file name into (pak number if `pakNN_dir.vpk`, enabled).
fn parse_name(name: &str) -> Option<(Option<u32>, bool)> {
    let (stem, enabled) = match name.strip_suffix(DISABLED_EXT) {
        Some(s) => (s, false),
        None => (name, true),
    };
    if !stem.ends_with(".vpk") || stem == ".vpk" {
        return None;
    }
    let pak = stem
        .strip_prefix("pak")
        .and_then(|r| r.strip_suffix("_dir.vpk"))
        .filter(|d| d.len() == 2 && d.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|d| d.parse().ok());
    Some((pak, enabled))
}

/// List VPK files directly in `addons` (pak-named or hand-added); other entries are ignored. A missing directory is empty.
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
        let is_file = entry.file_type().map_err(io_err)?.is_file();
        if is_file && let Some((pak, enabled)) = parse_name(&name) {
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

/// Enable or disable a VPK by renaming it in place; the stem (and pak number) never change.
///
/// The decision is made from what is on disk, not from `vpk.enabled`, so a stale
/// caller flag cannot cause a wrong rename.
pub fn set_enabled(vpk: &InstalledVpk, enabled: bool) -> Result<(), AddonsError> {
    let name = vpk
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AddonsError::NotAPak(vpk.file.clone()))?;
    let stem = name.strip_suffix(DISABLED_EXT).unwrap_or(name);
    let on_path = vpk.file.with_file_name(stem);
    let off_path = vpk.file.with_file_name(format!("{stem}{DISABLED_EXT}"));
    let (want, other) = if enabled {
        (on_path, off_path)
    } else {
        (off_path, on_path)
    };
    match (want.exists(), other.exists()) {
        (true, false) => Ok(()),
        (true, true) => Err(AddonsError::TargetExists { target: want }),
        (false, true) => fs::rename(&other, &want).map_err(|source| AddonsError::Io {
            path: other,
            source,
        }),
        (false, false) => Err(AddonsError::Io {
            path: other,
            source: io::Error::from(io::ErrorKind::NotFound),
        }),
    }
}

/// Lowest unused pak number in `addons` (1..=99), counting disabled paks as used.
/// Hand-added VPKs without a pak number are ignored.
///
/// Overflow rule (as in the old manager's shard.rs): the engine loads at most 99
/// `pakNN_dir.vpk` per search path. The old app uses shard directories `addons`,
/// `addons2` ..= `addons10` (MAX_SHARDS = 10), numbering from 01 in each shard. A full
/// directory returns [`AddonsError::Full`] and the caller moves to the next shard.
pub fn next_pak(addons: &Path) -> Result<u32, AddonsError> {
    let used: Vec<u32> = list(addons)?.into_iter().filter_map(|v| v.pak).collect();
    (1..=PAK_CAPACITY)
        .find(|n| !used.contains(n))
        .ok_or_else(|| AddonsError::Full {
            dir: addons.to_path_buf(),
        })
}
