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
    /// `pakNN_<digits>.vpk[.disabled]` chunk files that belong to this dir VPK.
    pub chunks: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PakKind {
    Dir,
    /// The digits of `pakNN_<digits>.vpk`, kept verbatim.
    Chunk(String),
}

/// Parse an enabled-form file name `pakNN_dir.vpk` / `pakNN_<digits>.vpk`
/// (case-sensitive, lowercase `pak`).
pub(crate) fn parse_pak(stem: &str) -> Option<(u32, PakKind)> {
    let rest = stem.strip_prefix("pak")?.strip_suffix(".vpk")?;
    let (num, tail) = rest.split_once('_')?;
    if num.len() != 2 || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = num.parse().ok()?;
    if tail == "dir" {
        Some((number, PakKind::Dir))
    } else if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
        Some((number, PakKind::Chunk(tail.to_owned())))
    } else {
        None
    }
}

/// Split `*.vpk[.disabled]` into (enabled-form stem, enabled).
fn split_name(name: &str) -> Option<(&str, bool)> {
    let (stem, enabled) = match name.strip_suffix(DISABLED_EXT) {
        Some(s) => (s, false),
        None => (name, true),
    };
    (stem.ends_with(".vpk") && stem != ".vpk").then_some((stem, enabled))
}

/// List VPK files directly in `addons` (pak-named or hand-added); other entries are ignored.
/// Chunk files are folded into their `pakNN_dir` entry; a chunk without a dir file stays
/// as an unknown row. A missing directory is empty.
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
    let mut out: Vec<InstalledVpk> = Vec::new();
    let mut chunks: Vec<(u32, InstalledVpk)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type().map_err(io_err)?.is_file() {
            continue;
        }
        let Some((stem, enabled)) = split_name(&name) else {
            continue;
        };
        let (pak, chunk_of) = match parse_pak(stem) {
            Some((n, PakKind::Dir)) => (Some(n), None),
            Some((n, PakKind::Chunk(_))) => (None, Some(n)),
            None => (None, None),
        };
        let vpk = InstalledVpk {
            file: entry.path(),
            pak,
            enabled,
            chunks: Vec::new(),
        };
        match chunk_of {
            Some(n) => chunks.push((n, vpk)),
            None => out.push(vpk),
        }
    }
    chunks.sort_by(|a, b| a.1.file.cmp(&b.1.file));
    for (n, chunk) in chunks {
        // Prefer the enabled dir file when both forms exist.
        let owner = out
            .iter_mut()
            .filter(|v| v.pak == Some(n))
            .min_by_key(|v| !v.enabled);
        match owner {
            Some(dir) => dir.chunks.push(chunk.file),
            None => out.push(chunk),
        }
    }
    out.sort_by_key(|v| (v.pak, !v.enabled));
    Ok(out)
}

/// Enable or disable a VPK and its chunk files by renaming in place; the stems (and
/// pak number) never change.
///
/// The decision is made from what is on disk, not from `vpk.enabled`, so a stale
/// caller flag cannot cause a wrong rename. Every target is checked before the first
/// rename and renames already done are undone if a later one fails.
pub fn set_enabled(vpk: &InstalledVpk, enabled: bool) -> Result<(), AddonsError> {
    let name = vpk
        .file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AddonsError::NotAPak(vpk.file.clone()))?;
    let stem = name.strip_suffix(DISABLED_EXT).unwrap_or(name);
    let dir = vpk.file.parent().map(Path::to_path_buf).unwrap_or_default();

    let mut stems = vec![stem.to_owned()];
    if let Some((number, PakKind::Dir)) = parse_pak(stem) {
        let io_err = |source| AddonsError::Io {
            path: dir.clone(),
            source,
        };
        for entry in fs::read_dir(&dir).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let Some(n) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if let Some((s, _)) = split_name(&n)
                && matches!(parse_pak(s), Some((c, PakKind::Chunk(_))) if c == number)
                && !stems.iter().any(|x| x == s)
            {
                stems.push(s.to_owned());
            }
        }
    }

    let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (i, s) in stems.iter().enumerate() {
        let on_path = dir.join(s);
        let off_path = dir.join(format!("{s}{DISABLED_EXT}"));
        let (want, other) = if enabled {
            (on_path, off_path)
        } else {
            (off_path, on_path)
        };
        match (want.exists(), other.exists()) {
            (true, false) => {}
            (true, true) => return Err(AddonsError::TargetExists { target: want }),
            (false, true) => renames.push((other, want)),
            (false, false) if i == 0 => {
                return Err(AddonsError::Io {
                    path: other,
                    source: io::Error::from(io::ErrorKind::NotFound),
                });
            }
            (false, false) => {}
        }
    }
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for r in &renames {
        if let Err(source) = fs::rename(&r.0, &r.1) {
            for (from, to) in done.into_iter().rev() {
                let _ = fs::rename(to, from);
            }
            return Err(AddonsError::Io {
                path: r.0.clone(),
                source,
            });
        }
        done.push(r);
    }
    Ok(())
}

/// Rename files in two phases (everything to a temporary name, then to its
/// target) so moves may swap or chain names. Every source must exist and every
/// target must be free or itself be moved; renames already done are undone if
/// a later one fails.
pub(crate) fn apply_moves(moves: &[(PathBuf, PathBuf)]) -> Result<(), AddonsError> {
    for (from, to) in moves {
        if !from.exists() {
            return Err(AddonsError::Io {
                path: from.clone(),
                source: io::Error::from(io::ErrorKind::NotFound),
            });
        }
        if to.exists() && !moves.iter().any(|(f, _)| f == to) {
            return Err(AddonsError::TargetExists { target: to.clone() });
        }
    }
    let tmp = |p: &Path| {
        let mut name = p.as_os_str().to_owned();
        name.push(".reorder");
        PathBuf::from(name)
    };
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut step = |from: PathBuf, to: PathBuf| match fs::rename(&from, &to) {
        Ok(()) => {
            done.push((from, to));
            Ok(())
        }
        Err(source) => Err(AddonsError::Io { path: from, source }),
    };
    let result = moves
        .iter()
        .try_for_each(|(from, _)| step(from.clone(), tmp(from)))
        .and_then(|()| {
            moves
                .iter()
                .try_for_each(|(from, to)| step(tmp(from), to.clone()))
        });
    if result.is_err() {
        for (from, to) in done.into_iter().rev() {
            let _ = fs::rename(to, from);
        }
    }
    result
}

/// The file `vpk` (a dir file or chunk) would have under pak number `number`.
fn renumbered(file: &Path, number: u32) -> Option<PathBuf> {
    let name = file.file_name()?.to_str()?;
    let (stem, _) = split_name(name)?;
    parse_pak(stem)?;
    // `pakNN` is always the first five bytes (checked by `parse_pak`).
    Some(file.with_file_name(format!("pak{number:02}{}", name.get(5..)?)))
}

/// What [`plan_reorder`] decided: the renames, and `(old, new)` enabled-form
/// dir file names of every VPK that moves.
pub(crate) struct ReorderPlan {
    pub moves: Vec<(PathBuf, PathBuf)>,
    pub names: Vec<(String, String)>,
}

fn enabled_name(path: &Path) -> String {
    let n = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    n.strip_suffix(DISABLED_EXT).unwrap_or(n).to_owned()
}

/// Plan giving the pak-numbered VPKs in `order` (dir files with their chunks,
/// enabled or not) the pak numbers they hold now, redistributed in `order`:
/// the first VPK gets the lowest number and so on. Gaps stay and VPKs whose
/// number does not change are left alone. VPKs without a pak number are
/// skipped.
pub(crate) fn plan_reorder(order: &[InstalledVpk]) -> Result<ReorderPlan, AddonsError> {
    let ranked: Vec<&InstalledVpk> = order.iter().filter(|v| v.pak.is_some()).collect();
    let mut numbers: Vec<u32> = ranked.iter().filter_map(|v| v.pak).collect();
    numbers.sort_unstable();
    let mut plan = ReorderPlan {
        moves: Vec::new(),
        names: Vec::new(),
    };
    for (vpk, number) in ranked.into_iter().zip(numbers) {
        if vpk.pak == Some(number) {
            continue;
        }
        for (i, file) in std::iter::once(&vpk.file).chain(&vpk.chunks).enumerate() {
            let to = renumbered(file, number).ok_or_else(|| AddonsError::NotAPak(file.clone()))?;
            if i == 0 {
                plan.names.push((enabled_name(file), enabled_name(&to)));
            }
            plan.moves.push((file.clone(), to));
        }
    }
    Ok(plan)
}

/// Pak numbers carried by any file in `addons` (dir, chunk, enabled or disabled).
pub(crate) fn used_pak_numbers(addons: &Path) -> Result<Vec<u32>, AddonsError> {
    let io_err = |source| AddonsError::Io {
        path: addons.to_path_buf(),
        source,
    };
    let entries = match fs::read_dir(addons) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(e)),
    };
    let mut used = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        if let Some(n) = entry.file_name().to_str()
            && let Some((stem, _)) = split_name(n)
            && let Some((num, _)) = parse_pak(stem)
        {
            used.push(num);
        }
    }
    Ok(used)
}

/// Lowest unused pak number in `addons` (1..=99), counting disabled paks and chunk
/// files as used. Hand-added VPKs without a pak number are ignored.
///
/// Overflow rule (as in the old manager's shard.rs): the engine loads at most 99
/// `pakNN_dir.vpk` per search path. The old app uses shard directories `addons`,
/// `addons2` ..= `addons10` (MAX_SHARDS = 10), numbering from 01 in each shard. A full
/// directory returns [`AddonsError::Full`] and the caller moves to the next shard.
pub fn next_pak(addons: &Path) -> Result<u32, AddonsError> {
    let used = used_pak_numbers(addons)?;
    (1..=PAK_CAPACITY)
        .find(|n| !used.contains(n))
        .ok_or_else(|| AddonsError::Full {
            dir: addons.to_path_buf(),
        })
}
